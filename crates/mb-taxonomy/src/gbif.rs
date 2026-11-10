use md5::{Digest, Md5};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const GBIF_MATCH_URL: &str = "https://api.gbif.org/v1/species/match";
const CACHE_TTL_SECONDS: u64 = 24 * 60 * 60;
const MAX_ATTEMPTS: u32 = 3;
const REQUEST_INTERVAL: Duration = Duration::from_millis(100);
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
pub enum GbifError {
    #[error("GBIF HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("GBIF returned HTTP status {0}")]
    Status(u16),
    #[error("GBIF response could not be decoded: {0}")]
    Json(#[from] serde_json::Error),
    #[error("GBIF cache I/O error: {0}")]
    CacheIo(#[from] io::Error),
    #[error("GBIF cache entry could not be decoded: {0}")]
    CacheJson(serde_json::Error),
    #[error("system clock is before the Unix epoch")]
    Clock,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GbifMatch {
    pub usage_key: Option<u64>,
    pub scientific_name: Option<String>,
    pub kingdom: Option<String>,
    pub class: Option<String>,
    pub confidence: Option<f32>,
    pub match_type: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    key: String,
    expiry: u64,
    body: String,
}

pub struct GbifClient {
    http: reqwest::Client,
    base_url: String,
    cache_dir: PathBuf,
    last_request: tokio::sync::Mutex<Option<tokio::time::Instant>>,
}

impl GbifClient {
    pub fn new() -> Result<Self, GbifError> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            base_url: GBIF_MATCH_URL.to_owned(),
            cache_dir: PathBuf::from("data/cache/gbif"),
            last_request: tokio::sync::Mutex::new(None),
        })
    }

    pub fn with_cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.cache_dir = path.into();
        self
    }

    pub async fn lookup(&self, name: &str) -> Result<Option<GbifMatch>, GbifError> {
        let key = name.trim().to_ascii_lowercase();
        if key.is_empty() {
            return Ok(None);
        }

        let body = match self.read_cache(&key)? {
            Some(body) => body,
            None => {
                let response = self.request_with_retries(name).await?;
                let body = response.text().await?;
                let _: GbifMatch = serde_json::from_str(&body)?;
                self.write_cache(&key, &body)?;
                body
            }
        };

        let matched: GbifMatch = serde_json::from_str(&body)?;
        let is_match = matched.usage_key.is_some()
            && !matched
                .match_type
                .as_deref()
                .is_some_and(|match_type| match_type.eq_ignore_ascii_case("NONE"));
        Ok(is_match.then_some(matched))
    }

    async fn request_with_retries(&self, name: &str) -> Result<reqwest::Response, GbifError> {
        let mut attempts = 0;
        loop {
            attempts += 1;
            self.wait_for_request_slot().await;

            match self
                .http
                .get(&self.base_url)
                .query(&[("name", name)])
                .send()
                .await
            {
                Ok(response) if response.status() == StatusCode::OK => return Ok(response),
                Ok(response) => {
                    let status = response.status();
                    let retryable =
                        status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                    if !retryable || attempts >= MAX_ATTEMPTS {
                        return Err(GbifError::Status(status.as_u16()));
                    }

                    let delay = retry_delay(&response, attempts);
                    tokio::time::sleep(delay).await;
                }
                Err(error)
                    if (error.is_timeout() || error.is_connect()) && attempts < MAX_ATTEMPTS =>
                {
                    tokio::time::sleep(retry_backoff(attempts)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    async fn wait_for_request_slot(&self) {
        let mut last_request = self.last_request.lock().await;
        if let Some(previous) = last_request.as_ref() {
            tokio::time::sleep(REQUEST_INTERVAL.saturating_sub(previous.elapsed())).await;
        }
        *last_request = Some(tokio::time::Instant::now());
    }

    fn read_cache(&self, key: &str) -> Result<Option<String>, GbifError> {
        let path = self.cache_path(key);
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let entry: CacheEntry = serde_json::from_str(&contents).map_err(GbifError::CacheJson)?;
        if entry.key != key {
            return Ok(None);
        }
        if entry.expiry <= unix_now()? {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            return Ok(None);
        }
        Ok(Some(entry.body))
    }

    fn write_cache(&self, key: &str, body: &str) -> Result<(), GbifError> {
        fs::create_dir_all(&self.cache_dir)?;
        let entry = CacheEntry {
            key: key.to_owned(),
            expiry: unix_now()?.saturating_add(CACHE_TTL_SECONDS),
            body: body.to_owned(),
        };
        fs::write(self.cache_path(key), serde_json::to_vec(&entry)?)?;
        Ok(())
    }

    fn cache_path(&self, key: &str) -> PathBuf {
        self.cache_dir
            .join(format!("{:x}.json", Md5::digest(key.as_bytes())))
    }

    #[cfg(test)]
    pub(crate) fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

fn retry_delay(response: &reqwest::Response, attempt: u32) -> Duration {
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after);
    retry_after
        .unwrap_or_else(|| retry_backoff(attempt))
        .max(retry_backoff(attempt))
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }

    let retry_at = httpdate::parse_http_date(value).ok()?;
    Some(
        retry_at
            .duration_since(SystemTime::now())
            .unwrap_or_default(),
    )
}

fn retry_backoff(attempt: u32) -> Duration {
    INITIAL_RETRY_DELAY * 2_u32.saturating_pow(attempt.saturating_sub(1))
}

fn unix_now() -> Result<u64, GbifError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| GbifError::Clock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_cache_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "mb-taxonomy-gbif-test-{}-{nonce}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn lookup_parses_and_caches_gbif_match() {
        let server = MockServer::start().await;
        let cache_dir = test_cache_dir();
        let client = GbifClient::new()
            .expect("HTTP client should build")
            .with_cache_dir(&cache_dir);
        let client = GbifClient {
            base_url: format!("{}/v1/species/match", server.uri()),
            ..client
        };

        Mock::given(method("GET"))
            .and(path("/v1/species/match"))
            .and(query_param("name", "Puma concolor"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"usageKey":2435099,"scientificName":"Puma concolor","kingdom":"Animalia","class":"Mammalia","confidence":98,"matchType":"EXACT"}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let matched = client
            .lookup("Puma concolor")
            .await
            .expect("GBIF response should parse")
            .expect("exact match should be returned");
        assert_eq!(matched.scientific_name.as_deref(), Some("Puma concolor"));
        assert_eq!(matched.confidence, Some(98.0));

        let cached = client
            .lookup("Puma concolor")
            .await
            .expect("cached response should parse")
            .expect("cached match should be returned");
        assert_eq!(cached.usage_key, Some(2_435_099));
        fs::remove_dir_all(cache_dir).expect("temporary cache should be removable");
    }

    #[tokio::test]
    async fn lookup_returns_none_for_gbif_no_match() {
        let server = MockServer::start().await;
        let cache_dir = test_cache_dir();
        let client = GbifClient::new()
            .expect("HTTP client should build")
            .with_cache_dir(&cache_dir);
        let client = GbifClient {
            base_url: format!("{}/v1/species/match", server.uri()),
            ..client
        };

        Mock::given(method("GET"))
            .and(path("/v1/species/match"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"confidence":0,"matchType":"NONE"}"#),
            )
            .mount(&server)
            .await;

        assert!(client
            .lookup("not a taxon")
            .await
            .expect("valid no-match response should parse")
            .is_none());
        fs::remove_dir_all(cache_dir).expect("temporary cache should be removable");
    }

    #[tokio::test]
    async fn lookup_retries_temporary_status_and_succeeds() {
        let server = MockServer::start().await;
        let cache_dir = test_cache_dir();
        let client = GbifClient::new()
            .expect("HTTP client should build")
            .with_cache_dir(&cache_dir)
            .with_base_url(format!("{}/v1/species/match", server.uri()));
        let calls = Arc::new(AtomicUsize::new(0));
        let response_calls = Arc::clone(&calls);

        Mock::given(method("GET"))
            .and(path("/v1/species/match"))
            .and(query_param("name", "Puma concolor"))
            .respond_with(move |_request: &wiremock::Request| {
                if response_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(503)
                } else {
                    ResponseTemplate::new(200).set_body_string(
                        r#"{"usageKey":2435099,"scientificName":"Puma concolor","class":"Mammalia","confidence":98,"matchType":"EXACT"}"#,
                    )
                }
            })
            .expect(2)
            .mount(&server)
            .await;

        let matched = client
            .lookup("Puma concolor")
            .await
            .expect("temporary failure should be retried")
            .expect("second response should be returned");
        assert_eq!(matched.usage_key, Some(2_435_099));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        fs::remove_dir_all(cache_dir).expect("temporary cache should be removable");
    }

    #[tokio::test]
    async fn lookup_stops_after_retry_limit() {
        let server = MockServer::start().await;
        let cache_dir = test_cache_dir();
        let client = GbifClient::new()
            .expect("HTTP client should build")
            .with_cache_dir(&cache_dir)
            .with_base_url(format!("{}/v1/species/match", server.uri()));
        let calls = Arc::new(AtomicUsize::new(0));
        let response_calls = Arc::clone(&calls);

        Mock::given(method("GET"))
            .and(path("/v1/species/match"))
            .and(query_param("name", "Puma concolor"))
            .respond_with(move |_request: &wiremock::Request| {
                response_calls.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(503)
            })
            .expect(MAX_ATTEMPTS as u64)
            .mount(&server)
            .await;

        let error = client
            .lookup("Puma concolor")
            .await
            .expect_err("retries should stop at the configured limit");
        assert!(matches!(error, GbifError::Status(503)));
        assert_eq!(calls.load(Ordering::SeqCst), MAX_ATTEMPTS as usize);
        if cache_dir.exists() {
            fs::remove_dir_all(cache_dir).expect("temporary cache should be removable");
        }
    }
}
