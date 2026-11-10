use crate::cache::CacheError;
use crate::license::{LicenseStore, LicenseStoreError};
use crate::Cache;
use crate::{Deployment, EventQuery, Fix, Individual, Study};
use md5::{Digest, Md5};
use ratelimit::Ratelimiter;
use std::path::PathBuf;

const BASE_URL: &str = "https://www.movebank.org/movebank/service/direct-read";
const STUDY_ATTRIBUTES: &str = "id,name,taxon_ids,number_of_deployments,number_of_individuals,number_of_deployed_locations,timestamp_first_deployed_location,timestamp_last_deployed_location,license_type,i_have_download_access";
const INDIVIDUAL_ATTRIBUTES: &str =
    "id,local_identifier,individual_taxon_canonical_name,timestamp_start,timestamp_end";
const EVENT_ATTRIBUTES: &str =
    "individual_id,deployment_id,timestamp,location_long,location_lat,visible";

fn parse_individuals(csv_text: &str) -> Result<Vec<Individual>, MovebankError> {
    csv::Reader::from_reader(csv_text.as_bytes())
        .deserialize::<Individual>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn parse_deployments(csv_text: &str) -> Result<Vec<Deployment>, MovebankError> {
    csv::Reader::from_reader(csv_text.as_bytes())
        .deserialize::<Deployment>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn parse_events(csv_text: &str) -> Result<Vec<Fix>, MovebankError> {
    csv::Reader::from_reader(csv_text.as_bytes())
        .deserialize::<Fix>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn event_query_params(query: &EventQuery) -> Vec<(String, String)> {
    let mut params = vec![
        ("entity_type".to_owned(), "event".to_owned()),
        ("study_id".to_owned(), query.study_id.to_string()),
        ("attributes".to_owned(), EVENT_ATTRIBUTES.to_owned()),
    ];
    if let Some(individuals) = &query.individuals {
        params.push((
            "individual_local_identifier".to_owned(),
            individuals.join(","),
        ));
    }
    if let Some(sensor_type_id) = query.sensor_type_id {
        params.push(("sensor_type_id".to_owned(), sensor_type_id.to_string()));
    }
    if let Some(start) = query.start {
        params.push((
            "timestamp_start".to_owned(),
            start.format("%Y%m%d%H%M%S%3f").to_string(),
        ));
    }
    if let Some(end) = query.end {
        params.push((
            "timestamp_end".to_owned(),
            end.format("%Y%m%d%H%M%S%3f").to_string(),
        ));
    }
    params
}

#[derive(Debug, thiserror::Error)]
pub enum MovebankError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("CSV parsing error: {0}")]
    Csv(#[from] csv::Error),
    #[error("cache error: {0}")]
    Cache(#[from] CacheError),
    #[error("license store error: {0}")]
    LicenseStore(#[from] LicenseStoreError),
    #[error("bad credentials (401)")]
    BadCredentials,
    #[error("authentication is required for this request (401)")]
    AuthenticationRequired,
    #[error("no access to this data (403)")]
    Forbidden,
    #[error("license terms must be accepted first")]
    LicenseRequired { terms: Vec<u8> },
    #[error("unexpected status {0}")]
    Status(u16),
    #[error("missing env var {0}")]
    MissingEnv(&'static str),
    #[error("{entity} {id} was not found")]
    NotFound { entity: &'static str, id: i64 },
    #[error("study_id is required to persist license acceptance")]
    MissingStudyIdForLicense,
}

#[derive(Clone)]
pub enum Auth {
    Anonymous,
    Basic { user: String, password: String },
    Token(String),
}

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    auth: Auth,
    cache: Cache,
    license_store: LicenseStore,
    license_scope: String,
    ratelimiter: Ratelimiter,
}

impl Client {
    pub fn new(auth: Auth) -> Result<Self, MovebankError> {
        let ratelimiter = Ratelimiter::new(1);
        let cache_dir = PathBuf::from("data/cache");
        let license_scope = Self::license_scope_for(&auth);
        let http = reqwest::Client::builder()
            .cookie_store(true) // keeps the session across the license round-trip
            .build()?;
        Ok(Self {
            http,
            base_url: BASE_URL.into(),
            auth,
            cache: Cache::new(cache_dir.clone()),
            license_store: LicenseStore::new(cache_dir.join("licenses.json")),
            license_scope,
            ratelimiter,
        })
    }

    pub fn from_env() -> Result<Self, MovebankError> {
        let user = std::env::var("MOVEBANK_USER")
            .map_err(|_| MovebankError::MissingEnv("MOVEBANK_USER"))?;
        let password = std::env::var("MOVEBANK_PASSWORD")
            .map_err(|_| MovebankError::MissingEnv("MOVEBANK_PASSWORD"))?;
        Self::new(Auth::Basic { user, password })
    }

    /// Swap basic auth for an API token so the password isn't sent on every request.
    pub async fn authenticate_with_movebank_api(&mut self) -> Result<(), MovebankError> {
        let Auth::Basic { user, password } = &self.auth else {
            return Ok(());
        };
        let mut retries = 0;
        let resp = loop {
            self.rate_limit_func().await;
            let result = self
                .http
                .get(&self.base_url)
                .query(&[("service", "request-token")])
                .basic_auth(user, Some(password))
                .send()
                .await;

            match result {
                Ok(response)
                    if response.status().as_u16() == 429 || response.status().is_server_error() =>
                {
                    if retries == 3 {
                        return Err(MovebankError::Status(response.status().as_u16()));
                    }
                    retries += 1;
                }
                Ok(response) => break response,
                Err(error) if (error.is_timeout() || error.is_connect()) && retries < 3 => {
                    retries += 1;
                }
                Err(error) => return Err(error.into()),
            }

            let backoff_ms = 250 * 2_u64.pow(retries - 1);
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
        };

        match resp.status().as_u16() {
            200 => {}
            401 => return Err(MovebankError::BadCredentials),
            s => return Err(MovebankError::Status(s)),
        }
        let token = resp.text().await?.trim().to_string();
        self.auth = Auth::Token(token);
        Ok(())
    }

    pub async fn get_csv(&self, params: &[(&str, &str)]) -> Result<String, MovebankError> {
        let cache_key = self.cache_key(params);
        if let Some(cached) = self.cache.get(&cache_key)? {
            return Ok(cached);
        }

        let mut retries = 0;
        let resp = loop {
            let mut request = self.http.get(&self.base_url).query(params);
            match &self.auth {
                Auth::Anonymous => {}
                Auth::Basic { user, password } => {
                    request = request.basic_auth(user, Some(password))
                }
                Auth::Token(token) => request = request.query(&[("api-token", token.as_str())]),
            }

            self.rate_limit_func().await;
            match request.send().await {
                Ok(response)
                    if response.status().as_u16() == 429 || response.status().is_server_error() =>
                {
                    let status = response.status().as_u16();
                    if retries == 3 {
                        return Err(MovebankError::Status(status));
                    }
                    retries += 1;
                    let backoff_ms = 250 * 2_u64.pow(retries - 1);
                    let retry_after = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok())
                        .map(std::time::Duration::from_secs);
                    let backoff = std::time::Duration::from_millis(backoff_ms);
                    tokio::time::sleep(retry_after.unwrap_or(backoff).max(backoff)).await;
                }
                Ok(response) => break response,
                Err(error) if (error.is_timeout() || error.is_connect()) && retries < 3 => {
                    retries += 1;
                    let backoff_ms = 250 * 2_u64.pow(retries - 1);
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };

        match resp.status().as_u16() {
            200 => {}
            401 if matches!(&self.auth, Auth::Anonymous) => {
                return Err(MovebankError::AuthenticationRequired)
            }
            401 => return Err(MovebankError::BadCredentials),
            403 => return Err(MovebankError::Forbidden),
            status => return Err(MovebankError::Status(status)),
        }
        // Movebank signals "license not yet accepted" with this header; the body is the license text.
        let needs_license = resp
            .headers()
            .get("accept-license")
            .map_or(false, |v| v == "true");
        let body = resp.bytes().await?;
        if needs_license {
            return Err(MovebankError::LicenseRequired {
                terms: body.to_vec(),
            });
        }
        let csv = String::from_utf8_lossy(&body).into_owned();
        self.cache.store(&cache_key, &csv)?;
        Ok(csv)
    }

    /// Call only after the user has actually read and agreed to `terms`.
    pub async fn get_csv_after_accepting(
        &self,
        params: &[(&str, &str)],
        terms: &[u8],
    ) -> Result<String, MovebankError> {
        let study_id = params
            .iter()
            .find(|(name, _)| *name == "study_id")
            .and_then(|(_, value)| value.parse::<i64>().ok())
            .ok_or(MovebankError::MissingStudyIdForLicense)?;
        let md5 = format!("{:x}", Md5::digest(terms));
        self.license_store
            .record_acceptance(study_id, &self.license_scope, &md5)?;
        let mut p = params.to_vec();
        p.push(("license-md5", md5.as_str()));
        self.get_csv(&p).await
    }

    pub fn has_accepted_license(&self, study_id: i64, terms: &[u8]) -> Result<bool, MovebankError> {
        let md5 = format!("{:x}", Md5::digest(terms));
        Ok(self
            .license_store
            .check_acceptance_status(study_id, &self.license_scope, &md5)?)
    }

    pub async fn list_studies(&self) -> Result<Vec<Study>, MovebankError> {
        let csv_text = self
            .get_csv(&[("entity_type", "study"), ("attributes", STUDY_ATTRIBUTES)])
            .await?;

        let studies = csv::Reader::from_reader(csv_text.as_bytes())
            .deserialize::<Study>()
            .collect::<Result<Vec<_>, _>>()?;

        Ok(studies)
    }

    pub async fn get_study(&self, study_id: i64) -> Result<Study, MovebankError> {
        let id = study_id;
        let study_id = id.to_string();
        let csv_text = self
            .get_csv(&[
                ("entity_type", "study"),
                ("study_id", study_id.as_str()),
                ("attributes", STUDY_ATTRIBUTES),
            ])
            .await?;

        csv::Reader::from_reader(csv_text.as_bytes())
            .deserialize::<Study>()
            .next()
            .transpose()?
            .ok_or(MovebankError::NotFound {
                entity: "study",
                id,
            })
    }

    pub async fn get_individuals(&self, study_id: i64) -> Result<Vec<Individual>, MovebankError> {
        let id = study_id;
        let study_id = id.to_string();
        let params = [
            ("entity_type", "individual"),
            ("study_id", study_id.as_str()),
            ("attributes", INDIVIDUAL_ATTRIBUTES),
        ];

        let csv_text = match self.get_csv(&params).await {
            Ok(csv_text) => csv_text,
            Err(MovebankError::LicenseRequired { terms }) => {
                if self.has_accepted_license(id, &terms)? {
                    self.get_csv_after_accepting(&params, &terms).await?
                } else {
                    return Err(MovebankError::LicenseRequired { terms });
                }
            }
            Err(error) => return Err(error),
        };

        parse_individuals(&csv_text)
    }

    pub async fn get_individuals_after_accepting(
        &self,
        study_id: i64,
        terms: &[u8],
    ) -> Result<Vec<Individual>, MovebankError> {
        let study_id = study_id.to_string();
        let params = [
            ("entity_type", "individual"),
            ("study_id", study_id.as_str()),
            ("attributes", INDIVIDUAL_ATTRIBUTES),
        ];
        let csv_text = self.get_csv_after_accepting(&params, terms).await?;
        parse_individuals(&csv_text)
    }

    pub async fn get_deployments(&self, study_id: i64) -> Result<Vec<Deployment>, MovebankError> {
        let study_id_string = study_id.to_string();
        let params = [
            ("entity_type", "deployment"),
            ("study_id", study_id_string.as_str()),
        ];
        let csv_text = match self.get_csv(&params).await {
            Ok(csv_text) => csv_text,
            Err(MovebankError::LicenseRequired { terms })
                if self.has_accepted_license(study_id, &terms)? =>
            {
                self.get_csv_after_accepting(&params, &terms).await?
            }
            Err(error) => return Err(error),
        };
        parse_deployments(&csv_text)
    }

    pub async fn get_deployments_after_accepting(
        &self,
        study_id: i64,
        terms: &[u8],
    ) -> Result<Vec<Deployment>, MovebankError> {
        let study_id_string = study_id.to_string();
        let params = [
            ("entity_type", "deployment"),
            ("study_id", study_id_string.as_str()),
        ];
        let csv_text = self.get_csv_after_accepting(&params, terms).await?;
        parse_deployments(&csv_text)
    }

    pub async fn get_events(&self, query: &EventQuery) -> Result<Vec<Fix>, MovebankError> {
        let owned_params = event_query_params(query);
        let params: Vec<_> = owned_params
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let csv_text = match self.get_csv(&params).await {
            Ok(csv_text) => csv_text,
            Err(MovebankError::LicenseRequired { terms })
                if self.has_accepted_license(query.study_id, &terms)? =>
            {
                self.get_csv_after_accepting(&params, &terms).await?
            }
            Err(error) => return Err(error),
        };
        parse_events(&csv_text)
    }

    pub async fn get_events_after_accepting(
        &self,
        query: &EventQuery,
        terms: &[u8],
    ) -> Result<Vec<Fix>, MovebankError> {
        let owned_params = event_query_params(query);
        let params: Vec<_> = owned_params
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let csv_text = self.get_csv_after_accepting(&params, terms).await?;
        parse_events(&csv_text)
    }

    pub fn with_cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        self.cache = Cache::new(path.clone());
        self.license_store = LicenseStore::new(path.join("licenses.json"));
        self
    }

    fn license_scope_for(auth: &Auth) -> String {
        match auth {
            Auth::Anonymous => "anonymous".to_owned(),
            Auth::Basic { user, .. } => format!("user:{:x}", Md5::digest(user.as_bytes())),
            Auth::Token(token) => format!("token:{:x}", Md5::digest(token.as_bytes())),
        }
    }

    fn cache_key(&self, params: &[(&str, &str)]) -> String {
        let mut params = params.to_vec();
        params.sort_unstable();
        let params = params
            .into_iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("&");
        let auth_scope = match &self.auth {
            Auth::Anonymous => "anonymous".to_owned(),
            Auth::Basic { user, .. } => format!("basic:{:x}", Md5::digest(user.as_bytes())),
            Auth::Token(token) => format!("token:{:x}", Md5::digest(token.as_bytes())),
        };
        format!("{}|{}|{}", self.base_url, auth_scope, params)
    }

    pub async fn rate_limit_func(&self) {
        loop {
            match self.ratelimiter.try_wait() {
                Ok(()) => return,
                Err(wait) => tokio::time::sleep(wait).await,
            }
        }
    }
}

#[cfg(test)]
impl Client {
    fn change_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_list_studies() -> Result<(), MovebankError> {
        let mock_server = MockServer::start().await;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let cache_dir = std::env::temp_dir().join(format!(
            "mb-client-study-test-{}-{nonce}",
            std::process::id()
        ));
        let client = Client::new(Auth::Anonymous)?.with_cache_dir(&cache_dir);
        let client = client.change_url(mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("entity_type", "study"))
            .and(query_param(
                "attributes",
                "id,name,taxon_ids,number_of_deployments,number_of_individuals,number_of_deployed_locations,timestamp_first_deployed_location,timestamp_last_deployed_location,license_type,i_have_download_access",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/csv")
                    .set_body_string(
                        "id,name,i_have_download_access\n2911040,Galapagos Albatrosses,true",
                    ),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let studies = client.list_studies().await?;
        assert_eq!(studies.len(), 1);
        assert_eq!(studies[0].id, 2_911_040);
        assert_eq!(studies[0].name, "Galapagos Albatrosses");

        let cached_studies = client.list_studies().await?;
        assert_eq!(cached_studies.len(), 1);
        assert_eq!(cached_studies[0].id, 2_911_040);
        fs::remove_dir_all(cache_dir).expect("test cache directory should be removable");
        Ok(())
    }

    #[tokio::test]
    async fn retrieves_study_individuals_deployments_and_events() -> Result<(), MovebankError> {
        let mock_server = MockServer::start().await;
        let client = Client::new(Auth::Anonymous)?.change_url(mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("entity_type", "study"))
            .and(query_param("study_id", "2911040"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "id,name,i_have_download_access\n2911040,Galapagos Albatrosses,true",
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("entity_type", "individual"))
            .and(query_param("study_id", "2911040"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "id,local_identifier,individual_taxon_canonical_name,timestamp_start,timestamp_end\n2911059,4264-84830852,Phoebastria irrorata,2008-05-31 13:30:02.001,2008-11-06 18:00:55.998",
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("entity_type", "deployment"))
            .and(query_param("study_id", "2911040"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "id,individual_id,deploy_on_timestamp,deploy_off_timestamp\n9472219,2911059,2008-06-23 06:00:00.000,2008-11-06 18:00:55.998",
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("entity_type", "event"))
            .and(query_param("study_id", "2911040"))
            .and(query_param(
                "individual_local_identifier",
                "4264-84830852,3606-30668",
            ))
            .and(query_param("sensor_type_id", "653"))
            .and(query_param("timestamp_start", "20080604133045000"))
            .and(query_param("timestamp_end", "20080604133046000"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "individual_id,deployment_id,timestamp,location_long,location_lat,visible\n2911059,9472219,2008-06-04 13:30:45.999,-85.1520954,-1.9960011,false",
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let study = client.get_study(2_911_040).await?;
        assert_eq!(study.name, "Galapagos Albatrosses");

        let individuals = client.get_individuals(2_911_040).await?;
        assert_eq!(individuals.len(), 1);
        assert_eq!(
            individuals[0].local_identifier.as_deref(),
            Some("4264-84830852")
        );

        let deployments = client.get_deployments(2_911_040).await?;
        assert_eq!(deployments.len(), 1);
        assert_eq!(deployments[0].deployment_id, 9_472_219);
        assert_eq!(deployments[0].individual_id.as_deref(), Some("2911059"));

        let events = client
            .get_events(&EventQuery {
                study_id: 2_911_040,
                individuals: Some(vec!["4264-84830852".into(), "3606-30668".into()]),
                sensor_type_id: Some(653),
                start: Some(
                    chrono::NaiveDateTime::parse_from_str(
                        "2008-06-04 13:30:45.000",
                        "%Y-%m-%d %H:%M:%S%.f",
                    )
                    .expect("valid Movebank timestamp"),
                ),
                end: Some(
                    chrono::NaiveDateTime::parse_from_str(
                        "2008-06-04 13:30:46.000",
                        "%Y-%m-%d %H:%M:%S%.f",
                    )
                    .expect("valid Movebank timestamp"),
                ),
            })
            .await?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].individual_id, "2911059");
        assert_eq!(events[0].deployment_id, Some(9_472_219));
        assert_eq!(events[0].lon, -85.1520954);
        assert_eq!(events[0].lat, -1.9960011);
        assert_eq!(events[0].outlier, Some(true));

        Ok(())
    }

    #[tokio::test]
    async fn test_retry_on_503_success() -> Result<(), MovebankError> {
        let mock_server = MockServer::start().await;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let cache_dir = std::env::temp_dir().join(format!(
            "mb-client-retry-test-{}-{nonce}",
            std::process::id()
        ));
        let client = Client::new(Auth::Anonymous)?
            .with_cache_dir(&cache_dir)
            .change_url(mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "id,name,i_have_download_access\n2911040,Galapagos Albatrosses,true",
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let studies = client.list_studies().await?;
        assert_eq!(studies.len(), 1);
        assert_eq!(studies[0].id, 2_911_040);
        assert_eq!(studies[0].name, "Galapagos Albatrosses");

        fs::remove_dir_all(cache_dir).expect("test cache directory should be removable");
        Ok(())
    }

    #[tokio::test]
    async fn individual_license_terms_are_explicitly_accepted() -> Result<(), MovebankError> {
        let mock_server = MockServer::start().await;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let cache_dir = std::env::temp_dir().join(format!(
            "mb-client-individual-license-test-{}-{nonce}",
            std::process::id()
        ));
        let client = Client::new(Auth::Anonymous)?
            .with_cache_dir(&cache_dir)
            .change_url(mock_server.uri());
        let study_id = 2_911_040;
        let terms = b"Please accept these individual metadata terms.";
        let license_md5 = format!("{:x}", Md5::digest(terms));

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("entity_type", "individual"))
            .and(query_param("study_id", "2911040"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("accept-license", "true")
                    .set_body_bytes(terms.to_vec()),
            )
            .up_to_n_times(1)
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("entity_type", "individual"))
            .and(query_param("study_id", "2911040"))
            .and(query_param("license-md5", &license_md5))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "id,local_identifier,individual_taxon_canonical_name,timestamp_start,timestamp_end\n1,F1,Puma concolor,,",
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        assert!(matches!(
            client.get_individuals(study_id).await,
            Err(MovebankError::LicenseRequired { .. })
        ));

        let individuals = client
            .get_individuals_after_accepting(study_id, terms)
            .await?;
        assert_eq!(individuals.len(), 1);
        assert_eq!(
            individuals[0].individual_taxon_canonical_name.as_deref(),
            Some("Puma concolor")
        );
        assert!(client.has_accepted_license(study_id, terms)?);

        fs::remove_dir_all(cache_dir).expect("test cache directory should be removable");
        Ok(())
    }

    #[tokio::test]
    async fn event_license_terms_are_explicitly_accepted() -> Result<(), MovebankError> {
        let mock_server = MockServer::start().await;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let cache_dir = std::env::temp_dir().join(format!(
            "mb-client-event-license-test-{}-{nonce}",
            std::process::id()
        ));
        let client = Client::new(Auth::Anonymous)?
            .with_cache_dir(&cache_dir)
            .change_url(mock_server.uri());
        let study_id = 2_911_040;
        let terms = b"Please accept these event data terms.";
        let license_md5 = format!("{:x}", Md5::digest(terms));
        let query = EventQuery {
            study_id,
            individuals: None,
            sensor_type_id: Some(653),
            start: Some(
                chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
                    .expect("valid date")
                    .and_hms_opt(0, 0, 0)
                    .expect("valid time"),
            ),
            end: Some(
                chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                    .expect("valid date")
                    .and_hms_opt(0, 0, 0)
                    .expect("valid time"),
            ),
        };

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("entity_type", "event"))
            .and(query_param("study_id", "2911040"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("accept-license", "true")
                    .set_body_bytes(terms.to_vec()),
            )
            .up_to_n_times(1)
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("entity_type", "event"))
            .and(query_param("study_id", "2911040"))
            .and(query_param("sensor_type_id", "653"))
            .and(query_param("timestamp_start", "20240101000000000"))
            .and(query_param("timestamp_end", "20240102000000000"))
            .and(query_param("license-md5", &license_md5))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "individual_id,deployment_id,timestamp,location_long,location_lat,visible\n2911059,9472219,2024-01-01 01:00:00.000,-85.1,-1.9,true",
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        assert!(matches!(
            client.get_events(&query).await,
            Err(MovebankError::LicenseRequired { .. })
        ));

        let events = client.get_events_after_accepting(&query, terms).await?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].individual_id, "2911059");
        assert_eq!(events[0].deployment_id, Some(9_472_219));
        assert_eq!(events[0].outlier, Some(false));
        assert!(client.has_accepted_license(study_id, terms)?);

        fs::remove_dir_all(cache_dir).expect("test cache directory should be removable");
        Ok(())
    }

    #[tokio::test]
    async fn license_terms_can_be_accepted() -> Result<(), MovebankError> {
        let mock_server = MockServer::start().await;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let cache_dir = std::env::temp_dir().join(format!(
            "mb-client-license-test-{}-{nonce}",
            std::process::id()
        ));
        let client = Client::new(Auth::Anonymous)?
            .with_cache_dir(&cache_dir)
            .change_url(mock_server.uri());
        let params = [("entity_type", "event"), ("study_id", "2911040")];
        let terms = b"Please accept these study terms.";

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("entity_type", "event"))
            .and(query_param("study_id", "2911040"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("accept-license", "true")
                    .insert_header("set-cookie", "session-id=test-session; Path=/")
                    .set_body_bytes(terms.to_vec()),
            )
            .up_to_n_times(1)
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("entity_type", "event"))
            .and(query_param("study_id", "2911040"))
            .and(query_param(
                "license-md5",
                "0a82c1dab0353bb5abb28d2cca5aa846",
            ))
            .and(header("cookie", "session-id=test-session"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("event-id,timestamp\n1,2024-01-01 00:00:00"),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        match client.get_csv(&params).await {
            Err(MovebankError::LicenseRequired {
                terms: returned_terms,
            }) => assert_eq!(returned_terms, terms),
            result => panic!("expected license terms, got {result:?}"),
        }

        let csv = client.get_csv_after_accepting(&params, terms).await?;
        assert_eq!(csv, "event-id,timestamp\n1,2024-01-01 00:00:00");
        assert!(client.has_accepted_license(2_911_040, terms)?);
        assert!(!client.has_accepted_license(2_911_041, terms)?);

        fs::remove_dir_all(cache_dir).expect("test cache directory should be removable");
        Ok(())
    }

    #[tokio::test]
    async fn authentication_verify_request_token_storage() -> Result<(), MovebankError> {
        let mock_server = MockServer::start().await;
        let auth = Auth::Basic {
            user: "test_user".into(),
            password: "test_pass".into(),
        };
        let mut client = Client::new(auth)?.change_url(mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("service", "request-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("abc"))
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("api-token", "abc"))
            .and(query_param("entity_type", "study"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                // CSV //
                "id,name\n2911040,Galapagos Albatrosses",
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        client.authenticate_with_movebank_api().await?;
        let csv = client.get_csv(&[("entity_type", "study")]).await?;
        assert_eq!(csv, "id,name\n2911040,Galapagos Albatrosses");
        assert!(matches!(&client.auth, Auth::Token(token) if token == "abc"));

        Ok(())
    }

    #[tokio::test]
    async fn unauthorized_responses_return_bad_credentials() -> Result<(), MovebankError> {
        let mock_server = MockServer::start().await;
        let auth = Auth::Basic {
            user: "test_user".into(),
            password: "wrong_password".into(),
        };
        let mut client = Client::new(auth)?.change_url(mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("service", "request-token"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("entity_type", "study"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&mock_server)
            .await;

        assert!(matches!(
            client.authenticate_with_movebank_api().await,
            Err(MovebankError::BadCredentials)
        ));
        assert!(matches!(
            client.get_csv(&[("entity_type", "study")]).await,
            Err(MovebankError::BadCredentials)
        ));

        Ok(())
    }

    #[tokio::test]
    async fn forbidden_response_returns_forbidden() -> Result<(), MovebankError> {
        let mock_server = MockServer::start().await;
        let client = Client::new(Auth::Anonymous)?.change_url(mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("entity_type", "study"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&mock_server)
            .await;

        assert!(matches!(
            client.get_csv(&[("entity_type", "study")]).await,
            Err(MovebankError::Forbidden)
        ));

        Ok(())
    }

    #[tokio::test]
    async fn anonymous_unauthorized_response_reports_authentication_required(
    ) -> Result<(), MovebankError> {
        let mock_server = MockServer::start().await;
        let client = Client::new(Auth::Anonymous)?.change_url(mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("entity_type", "study"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&mock_server)
            .await;

        assert!(matches!(
            client.get_csv(&[("entity_type", "study")]).await,
            Err(MovebankError::AuthenticationRequired)
        ));

        Ok(())
    }

    #[tokio::test]
    async fn retry_exhaustion_returns_final_server_status() -> Result<(), MovebankError> {
        let mock_server = MockServer::start().await;
        let client = Client::new(Auth::Anonymous)?.change_url(mock_server.uri());

        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("entity_type", "study"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(4)
            .expect(4)
            .mount(&mock_server)
            .await;

        assert!(matches!(
            client.get_csv(&[("entity_type", "study")]).await,
            Err(MovebankError::Status(503))
        ));

        Ok(())
    }
}
