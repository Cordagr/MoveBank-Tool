use serde::{Deserialize, Serialize};

#[derive(PartialEq, Debug, Clone, Serialize, Deserialize)]
pub struct TaxonResolution {
    pub raw: String,
    pub canonical: Option<String>,
    pub class: TaxonClass,
    pub confidence: f32, // 0.0 - 1.0 range //
    pub source: TaxonSource,
}

#[derive(PartialEq, Eq, Hash, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaxonClass {
    Aves,
    Mammalia,
    Reptilia,
    Amphibia,
    Fish,
    Invertebrate,
    Other,
    Unknown,
}

#[derive(PartialEq, Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaxonSource {
    ManualOverride,
    Gbif,
    Unresolved,
}

impl TaxonResolution {
    pub fn unresolved_resolution() -> Self {
        Self::unresolved_for("")
    }

    pub fn unresolved_for(raw: impl Into<String>) -> Self {
        Self {
            raw: raw.into(),
            canonical: None,
            class: TaxonClass::Unknown,
            confidence: 0.0,
            source: TaxonSource::Unresolved,
        }
    }
}
