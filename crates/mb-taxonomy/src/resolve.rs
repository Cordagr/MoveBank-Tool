use crate::{GbifClient, GbifError, TaxonClass, TaxonResolution, TaxonSource};
use std::collections::HashMap;

pub async fn resolve_taxon(
    name: &str,
    overrides: &crate::Overrides,
    gbif: Option<&GbifClient>,
) -> Result<TaxonResolution, GbifError> {
    if let Some(resolution) = overrides.build_resolution(name) {
        return Ok(resolution);
    }

    let Some(client) = gbif else {
        return Ok(TaxonResolution::unresolved_for(name));
    };
    let normalized = crate::normalize_name(name);
    let Some(matched) = client.lookup(&normalized).await? else {
        return Ok(TaxonResolution::unresolved_for(name));
    };
    let Some(canonical) = matched
        .scientific_name
        .filter(|scientific_name| !scientific_name.trim().is_empty())
    else {
        return Ok(TaxonResolution::unresolved_for(name));
    };

    let confidence = matched.confidence.unwrap_or_default().clamp(0.0, 100.0) / 100.0;
    Ok(TaxonResolution {
        raw: name.to_owned(),
        canonical: Some(canonical),
        class: classify_gbif_taxon(matched.kingdom.as_deref(), matched.class.as_deref()),
        confidence,
        source: TaxonSource::Gbif,
    })
}

pub fn primary_class(resolutions: &[TaxonResolution]) -> TaxonClass {
    let mut counts = HashMap::new();
    let mut known_count = 0;
    for resolution in resolutions {
        if resolution.class != TaxonClass::Unknown {
            *counts.entry(resolution.class.clone()).or_insert(0usize) += 1;
            known_count += 1;
        }
    }
    if known_count == 0 {
        return TaxonClass::Unknown;
    }

    let mut ranked = counts.into_iter().collect::<Vec<_>>();
    ranked.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    let (class, count) = ranked[0].clone();
    if count * 2 > known_count {
        class
    } else {
        TaxonClass::Other
    }
}

fn classify_gbif_taxon(kingdom: Option<&str>, class: Option<&str>) -> TaxonClass {
    let Some(class) = class else {
        return TaxonClass::Unknown;
    };
    match class.to_ascii_lowercase().as_str() {
        "aves" => TaxonClass::Aves,
        "mammalia" => TaxonClass::Mammalia,
        "reptilia" => TaxonClass::Reptilia,
        "amphibia" => TaxonClass::Amphibia,
        "actinopterygii" | "sarcopterygii" | "chondrichthyes" | "myxini" | "petromyzontida"
        | "cephalaspidomorphi" | "hyperoartia" | "ostracodermi" | "placodermi" | "acanthodii" => {
            TaxonClass::Fish
        }
        _ if kingdom.is_some_and(|kingdom| kingdom.eq_ignore_ascii_case("Animalia")) => {
            TaxonClass::Invertebrate
        }
        _ => TaxonClass::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OverrideRecord, Overrides};

    #[tokio::test]
    async fn manual_override_resolves_without_gbif_client() {
        let mut overrides = Overrides::new();
        overrides.add(OverrideRecord {
            normalized_name: "Puma   concolor".to_owned(),
            canonical: "Puma concolor".to_owned(),
            class: TaxonClass::Mammalia,
        });

        let resolution = resolve_taxon("Puma concolor", &overrides, None)
            .await
            .expect("manual resolution should succeed");
        assert_eq!(resolution.class, TaxonClass::Mammalia);
        assert_eq!(resolution.source, TaxonSource::ManualOverride);
    }

    #[tokio::test]
    async fn missing_client_returns_unresolved_with_original_input() {
        let resolution = resolve_taxon("Unknown taxon", &Overrides::new(), None)
            .await
            .expect("missing optional client is not an error");
        assert_eq!(resolution.raw, "Unknown taxon");
        assert_eq!(resolution.class, TaxonClass::Unknown);
        assert_eq!(resolution.source, TaxonSource::Unresolved);
    }

    #[test]
    fn primary_class_requires_a_strict_majority() {
        let resolutions = [
            resolution(TaxonClass::Aves),
            resolution(TaxonClass::Aves),
            resolution(TaxonClass::Mammalia),
        ];
        assert_eq!(primary_class(&resolutions), TaxonClass::Aves);

        let mixed = [
            resolution(TaxonClass::Aves),
            resolution(TaxonClass::Mammalia),
        ];
        assert_eq!(primary_class(&mixed), TaxonClass::Other);
        assert_eq!(primary_class(&[]), TaxonClass::Unknown);
    }

    #[test]
    fn maps_gbif_classes_to_project_classes() {
        assert_eq!(
            classify_gbif_taxon(Some("Animalia"), Some("Actinopterygii")),
            TaxonClass::Fish
        );
        assert_eq!(
            classify_gbif_taxon(Some("Animalia"), Some("Insecta")),
            TaxonClass::Invertebrate
        );
        assert_eq!(
            classify_gbif_taxon(Some("Plantae"), Some("Magnoliopsida")),
            TaxonClass::Unknown
        );
    }

    #[tokio::test]
    async fn resolves_gbif_match_into_project_resolution() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/species/match"))
            .and(query_param("name", "puma concolor"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"usageKey":2435099,"scientificName":"Puma concolor","kingdom":"Animalia","class":"Mammalia","confidence":98,"matchType":"EXACT"}"#,
            ))
            .mount(&server)
            .await;

        let cache_dir = std::env::temp_dir().join(format!(
            "mb-taxonomy-resolve-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after Unix epoch")
                .as_nanos()
        ));
        let gbif = GbifClient::new()
            .expect("HTTP client should build")
            .with_cache_dir(&cache_dir)
            .with_base_url(format!("{}/v1/species/match", server.uri()));

        let resolution = resolve_taxon("Puma concolor", &Overrides::new(), Some(&gbif))
            .await
            .expect("GBIF resolution should succeed");
        assert_eq!(resolution.canonical.as_deref(), Some("Puma concolor"));
        assert_eq!(resolution.class, TaxonClass::Mammalia);
        assert_eq!(resolution.confidence, 0.98);
        assert_eq!(resolution.source, TaxonSource::Gbif);
        std::fs::remove_dir_all(cache_dir).expect("temporary cache should be removable");
    }

    fn resolution(class: TaxonClass) -> TaxonResolution {
        TaxonResolution {
            raw: String::new(),
            canonical: None,
            class,
            confidence: 1.0,
            source: TaxonSource::Gbif,
        }
    }
}
