use crate::normalize_name;
use crate::{TaxonClass, TaxonResolution, TaxonSource};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct OverrideRecord {
    pub normalized_name: String,
    pub canonical: String,
    pub class: TaxonClass,
}

#[derive(Debug, thiserror::Error)]
pub enum OverrideLoadError {
    #[error("failed to read override CSV: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse override CSV: {0}")]
    Csv(#[from] csv::Error),
}

#[derive(Debug, Default)]
pub struct Overrides {
    mappings: HashMap<String, OverrideRecord>,
}

impl Overrides {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_reader(reader: impl Read) -> Result<Self, csv::Error> {
        let records = csv::Reader::from_reader(reader)
            .into_deserialize::<OverrideRecord>()
            .collect::<Result<Vec<_>, _>>()?;
        let mut overrides = Self::new();
        for record in records {
            overrides.add(record);
        }
        Ok(overrides)
    }

    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, OverrideLoadError> {
        Self::from_reader(std::fs::File::open(path)?).map_err(Into::into)
    }

    pub fn add(&mut self, mut record: OverrideRecord) {
        let key = normalize_name(&record.normalized_name);
        record.normalized_name = key.clone();
        self.mappings.insert(key, record);
    }

    pub fn normalize_input_name(&self, name: &str) -> Option<&OverrideRecord> {
        self.mappings.get(&normalize_name(name))
    }

    pub fn build_resolution(&self, name: &str) -> Option<TaxonResolution> {
        self.normalize_input_name(name)
            .map(|record| TaxonResolution {
                raw: name.to_owned(),
                canonical: Some(record.canonical.clone()),
                class: record.class.clone(),
                confidence: 1.0,
                source: TaxonSource::ManualOverride,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_csv_and_matches_normalized_names() {
        let overrides = Overrides::from_reader(
            "normalized_name,canonical,class\npuma concolor,Puma concolor,mammalia\n".as_bytes(),
        )
        .expect("valid override CSV should load");

        let resolution = overrides
            .build_resolution("  PUMA   CONCOLOR ")
            .expect("normalized override should match");
        assert_eq!(resolution.raw, "  PUMA   CONCOLOR ");
        assert_eq!(resolution.canonical.as_deref(), Some("Puma concolor"));
        assert_eq!(resolution.class, TaxonClass::Mammalia);
        assert_eq!(resolution.confidence, 1.0);
        assert_eq!(resolution.source, TaxonSource::ManualOverride);
    }

    #[test]
    fn invalid_override_class_is_reported() {
        let error = Overrides::from_reader(
            "normalized_name,canonical,class\npuma concolor,Puma concolor,mammal\n".as_bytes(),
        )
        .expect_err("unknown class should fail to parse");
        assert!(error.to_string().contains("unknown variant"));
    }
}
