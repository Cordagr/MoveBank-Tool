use chrono::NaiveDateTime;
use serde::{Deserialize, Deserializer, Serialize};

// Movebank timestamps omit an offset and may represent UTC, GPS time, or locally supplied time.
// Preserve the supplied wall-clock value until its time basis is known.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LicenseStatus {
    Accepted,
    NotAccepted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Study {
    pub id: i64,
    pub name: String,
    pub taxon_ids: Option<String>,
    pub number_of_deployments: Option<u64>,
    pub number_of_individuals: Option<u32>,
    pub number_of_deployed_locations: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_movebank_datetime")]
    pub timestamp_first_deployed_location: Option<NaiveDateTime>,
    #[serde(default, deserialize_with = "deserialize_optional_movebank_datetime")]
    pub timestamp_last_deployed_location: Option<NaiveDateTime>,
    pub license_type: Option<String>,
    pub i_have_download_access: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventQuery {
    pub study_id: i64,
    pub individuals: Option<Vec<String>>,
    pub sensor_type_id: Option<i64>,
    pub start: Option<NaiveDateTime>,
    pub end: Option<NaiveDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fix {
    pub individual_id: String,
    #[serde(default)]
    pub deployment_id: Option<i64>,
    #[serde(deserialize_with = "deserialize_movebank_datetime")]
    pub timestamp: NaiveDateTime,
    #[serde(rename = "location_long")]
    pub lon: f64,
    #[serde(rename = "location_lat")]
    pub lat: f64,
    #[serde(
        default,
        rename = "visible",
        deserialize_with = "deserialize_outlier_from_visible"
    )]
    pub outlier: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deployment {
    #[serde(rename = "id")]
    pub deployment_id: i64,
    pub individual_id: Option<String>,
    #[serde(
        default,
        rename = "deploy_on_timestamp",
        deserialize_with = "deserialize_optional_movebank_datetime"
    )]
    pub start_timestamp: Option<NaiveDateTime>,
    #[serde(
        default,
        rename = "deploy_off_timestamp",
        deserialize_with = "deserialize_optional_movebank_datetime"
    )]
    pub end_timestamp: Option<NaiveDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Individual {
    pub id: i64,
    pub local_identifier: Option<String>,
    pub individual_taxon_canonical_name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_movebank_datetime")]
    pub timestamp_start: Option<NaiveDateTime>,
    #[serde(default, deserialize_with = "deserialize_optional_movebank_datetime")]
    pub timestamp_end: Option<NaiveDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LicenseTerms {
    pub text: String,
    pub identifier: String,
}

fn deserialize_outlier_from_visible<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<bool>::deserialize(deserializer)?.map(|visible| !visible))
}

fn deserialize_movebank_datetime<'de, D>(deserializer: D) -> Result<NaiveDateTime, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    parse_movebank_datetime(&value).map_err(serde::de::Error::custom)
}

fn deserialize_optional_movebank_datetime<'de, D>(
    deserializer: D,
) -> Result<Option<NaiveDateTime>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .filter(|value| !value.trim().is_empty())
        .map(|value| parse_movebank_datetime(&value).map_err(serde::de::Error::custom))
        .transpose()
}

fn parse_movebank_datetime(value: &str) -> Result<NaiveDateTime, chrono::ParseError> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_movebank_timestamps_with_milliseconds() {
        let timestamp = parse_movebank_datetime("2008-05-31 13:30:02.001")
            .expect("timestamp with milliseconds should parse");
        assert_eq!(
            timestamp.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
            "2008-05-31 13:30:02.001"
        );
    }

    #[test]
    fn parses_movebank_timestamps_without_milliseconds() {
        let timestamp = parse_movebank_datetime("2008-05-31 13:30:02")
            .expect("timestamp without milliseconds should parse");
        assert_eq!(
            timestamp.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
            "2008-05-31 13:30:02.000"
        );
    }

    #[test]
    fn empty_optional_timestamps_deserialize_as_none() {
        let csv = "id,local_identifier,individual_taxon_canonical_name,timestamp_start,timestamp_end\n7,,,,";
        let individual = csv::Reader::from_reader(csv.as_bytes())
            .deserialize::<Individual>()
            .next()
            .expect("CSV should contain an individual")
            .expect("empty timestamps should be accepted");

        assert_eq!(individual.timestamp_start, None);
        assert_eq!(individual.timestamp_end, None);
    }

    #[test]
    fn rejects_invalid_movebank_timestamps() {
        assert!(parse_movebank_datetime("not a timestamp").is_err());
    }
}
