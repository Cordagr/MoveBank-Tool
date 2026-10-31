use chrono::NaiveDateTime;
use mb_client::{Client, MovebankError};
use mb_taxonomy::{primary_class, resolve_taxon, GbifClient, GbifError, Overrides, TaxonClass};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub struct StudyRow {
    pub id: i64,
    pub name: String,
    pub taxon_raw: String,
    pub class: TaxonClass,
    pub n_individuals: Option<u32>,
    pub n_locations: Option<u64>,
    pub duration_days: Option<f64>,
    pub downloadable: bool,
    pub neglect_score: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    pub class: Option<TaxonClass>,
    pub min_individuals: Option<u32>,
    pub min_days: Option<f64>,
    pub downloadable: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum NeglectError {
    #[error("Movebank request failed: {0}")]
    Movebank(#[from] MovebankError),
    #[error("taxon resolution failed: {0}")]
    Taxonomy(#[from] GbifError),
    #[error("weight for class {class:?} must be finite and nonnegative")]
    InvalidWeight { class: TaxonClass },
}

impl StudyRow {
    pub fn new(id: i64, name: String) -> Self {
        Self {
            id,
            name,
            taxon_raw: String::new(),
            class: TaxonClass::Unknown,
            n_individuals: None,
            n_locations: None,
            duration_days: None,
            downloadable: false,
            neglect_score: 0.0,
        }
    }

    pub fn duration_days(first: Option<NaiveDateTime>, last: Option<NaiveDateTime>) -> Option<f64> {
        let (Some(first), Some(last)) = (first, last) else {
            return None;
        };
        if last < first {
            return None;
        }
        Some((last - first).num_milliseconds() as f64 / 86_400_000.0)
    }

    pub async fn build_study_table(
        client: &Client,
        overrides: &Overrides,
        gbif: Option<&GbifClient>,
    ) -> Result<Vec<StudyRow>, NeglectError> {
        let studies = client.list_studies().await?;
        let mut rows = Vec::with_capacity(studies.len());

        for study in studies {
            let individuals = client.get_individuals(study.id).await?;
            let mut seen_names = HashSet::new();
            let mut taxon_names = Vec::new();
            for name in individuals
                .into_iter()
                .filter_map(|individual| individual.individual_taxon_canonical_name)
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
            {
                if seen_names.insert(name.to_ascii_lowercase()) {
                    taxon_names.push(name);
                }
            }

            let mut resolutions = Vec::with_capacity(taxon_names.len());
            for name in &taxon_names {
                resolutions.push(resolve_taxon(name, overrides, gbif).await?);
            }

            let mut row = StudyRow::new(study.id, study.name);
            row.taxon_raw = if taxon_names.is_empty() {
                study.taxon_ids.unwrap_or_default()
            } else {
                taxon_names.join("; ")
            };
            row.class = primary_class(&resolutions);
            row.n_individuals = study.number_of_individuals;
            row.n_locations = study.number_of_deployed_locations;
            row.duration_days = Self::duration_days(
                study.timestamp_first_deployed_location,
                study.timestamp_last_deployed_location,
            );
            row.downloadable = study.i_have_download_access;
            rows.push(row);
        }

        Ok(rows)
    }

    pub fn class_share(rows: &[StudyRow]) -> HashMap<TaxonClass, f64> {
        let mut counts: HashMap<TaxonClass, usize> = HashMap::new();
        for row in rows {
            if row.class != TaxonClass::Unknown {
                *counts.entry(row.class.clone()).or_default() += 1;
            }
        }

        let total: usize = counts.values().sum();
        if total == 0 {
            return HashMap::new();
        }

        counts
            .into_iter()
            .map(|(class, count)| (class, count as f64 / total as f64))
            .collect()
    }

    pub fn neglect_score(
        rows: &mut [StudyRow],
        weights: Option<&HashMap<TaxonClass, f64>>,
    ) -> Result<(), NeglectError> {
        let shares = Self::class_share(rows);
        let mut individuals_by_class: HashMap<TaxonClass, Vec<u32>> = HashMap::new();
        let mut durations_by_class: HashMap<TaxonClass, Vec<f64>> = HashMap::new();

        for row in rows.iter() {
            if row.class == TaxonClass::Unknown {
                continue;
            }
            if let Some(value) = row.n_individuals {
                individuals_by_class
                    .entry(row.class.clone())
                    .or_default()
                    .push(value);
            }
            if let Some(value) = row
                .duration_days
                .filter(|value| value.is_finite() && *value >= 0.0)
            {
                durations_by_class
                    .entry(row.class.clone())
                    .or_default()
                    .push(value);
            }
        }
        for values in individuals_by_class.values_mut() {
            values.sort_unstable();
        }
        for values in durations_by_class.values_mut() {
            values.sort_by(f64::total_cmp);
        }

        let mut raw_scores = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            if row.class == TaxonClass::Unknown {
                raw_scores.push(0.0);
                continue;
            }

            let mut percentiles = Vec::with_capacity(2);
            if let Some(value) = row.n_individuals {
                if let Some(values) = individuals_by_class.get(&row.class) {
                    if let Some(percentile) = percentile_u32(values, value) {
                        percentiles.push(percentile);
                    }
                }
            }
            if let Some(value) = row
                .duration_days
                .filter(|value| value.is_finite() && *value >= 0.0)
            {
                if let Some(values) = durations_by_class.get(&row.class) {
                    if let Some(percentile) = percentile_f64(values, value) {
                        percentiles.push(percentile);
                    }
                }
            }
            if percentiles.is_empty() {
                raw_scores.push(0.0);
                continue;
            }

            let share = shares.get(&row.class).copied().unwrap_or_default();
            let weight = weights
                .and_then(|weights| weights.get(&row.class).copied())
                .unwrap_or_else(|| if share > 0.0 { 1.0 / share } else { 0.0 });
            if !weight.is_finite() || weight < 0.0 {
                return Err(NeglectError::InvalidWeight {
                    class: row.class.clone(),
                });
            }

            let percentile_mean = percentiles.iter().sum::<f64>() / percentiles.len() as f64;
            raw_scores.push(weight * percentile_mean);
        }

        let max_score = raw_scores.iter().copied().fold(0.0_f64, f64::max);
        for (row, raw_score) in rows.iter_mut().zip(raw_scores) {
            row.neglect_score = if max_score > 0.0 {
                raw_score / max_score * 100.0
            } else {
                0.0
            };
        }
        Ok(())
    }

    pub fn filter_studies(rows: &[StudyRow], filter: &Filter) -> Vec<StudyRow> {
        rows.iter()
            .filter(|row| {
                let class_matches = match &filter.class {
                    Some(class) => row.class == *class,
                    None => true,
                };
                let individuals_match = match filter.min_individuals {
                    Some(minimum) => row.n_individuals.is_some_and(|count| count >= minimum),
                    None => true,
                };
                let duration_matches = match filter.min_days {
                    Some(minimum) => row
                        .duration_days
                        .is_some_and(|days| days.is_finite() && days >= minimum),
                    None => true,
                };
                let download_matches = !filter.downloadable || row.downloadable;

                class_matches && individuals_match && duration_matches && download_matches
            })
            .cloned()
            .collect()
    }
}

fn percentile_u32(sorted_values: &[u32], value: u32) -> Option<f64> {
    if sorted_values.is_empty() {
        return None;
    }
    let lower = sorted_values.partition_point(|candidate| *candidate < value);
    let upper = sorted_values.partition_point(|candidate| *candidate <= value);
    let equal = upper - lower;
    Some((lower as f64 + equal as f64 / 2.0) / sorted_values.len() as f64)
}

fn percentile_f64(sorted_values: &[f64], value: f64) -> Option<f64> {
    if sorted_values.is_empty() || !value.is_finite() {
        return None;
    }
    let lower = sorted_values.partition_point(|candidate| candidate.total_cmp(&value).is_lt());
    let upper = sorted_values.partition_point(|candidate| !candidate.total_cmp(&value).is_gt());
    let equal = upper - lower;
    Some((lower as f64 + equal as f64 / 2.0) / sorted_values.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, NaiveDate};

    fn row(class: TaxonClass, individuals: Option<u32>, days: Option<f64>) -> StudyRow {
        StudyRow {
            id: 1,
            name: "test".to_owned(),
            taxon_raw: String::new(),
            class,
            n_individuals: individuals,
            n_locations: None,
            duration_days: days,
            downloadable: true,
            neglect_score: 0.0,
        }
    }

    #[test]
    fn duration_days_preserves_fractions_and_rejects_invalid_ranges() {
        let first = NaiveDate::from_ymd_opt(2024, 1, 1)
            .expect("valid date")
            .and_hms_opt(0, 0, 0)
            .expect("valid time");
        let last = first + Duration::hours(36);

        assert_eq!(StudyRow::duration_days(Some(first), Some(last)), Some(1.5));
        assert_eq!(StudyRow::duration_days(Some(first), None), None);
        assert_eq!(StudyRow::duration_days(Some(last), Some(first)), None);
    }

    #[test]
    fn class_share_excludes_unknown_and_returns_proportions() {
        let rows = [
            row(TaxonClass::Aves, None, None),
            row(TaxonClass::Aves, None, None),
            row(TaxonClass::Mammalia, None, None),
            row(TaxonClass::Unknown, None, None),
        ];
        let shares = StudyRow::class_share(&rows);

        assert_eq!(shares.get(&TaxonClass::Aves), Some(&(2.0 / 3.0)));
        assert_eq!(shares.get(&TaxonClass::Mammalia), Some(&(1.0 / 3.0)));
        assert!(!shares.contains_key(&TaxonClass::Unknown));
    }

    #[test]
    fn class_share_is_empty_when_no_known_classes_exist() {
        assert!(StudyRow::class_share(&[row(TaxonClass::Unknown, None, None)]).is_empty());
    }

    #[test]
    fn neglect_score_rewards_underrepresented_non_bird_classes_for_equal_metrics() {
        let mut rows = vec![
            row(TaxonClass::Aves, Some(10), Some(30.0)),
            row(TaxonClass::Aves, Some(10), Some(30.0)),
            row(TaxonClass::Aves, Some(10), Some(30.0)),
            row(TaxonClass::Mammalia, Some(10), Some(30.0)),
            row(TaxonClass::Mammalia, Some(10), Some(30.0)),
            row(TaxonClass::Mammalia, Some(10), Some(30.0)),
            row(TaxonClass::Reptilia, Some(10), Some(30.0)),
        ];

        StudyRow::neglect_score(&mut rows, None).expect("derived class weights are valid");

        assert!(rows[6].neglect_score > rows[0].neglect_score);
        assert!(rows[6].neglect_score > rows[3].neglect_score);
        assert!(rows
            .iter()
            .all(|row| (0.0..=100.0).contains(&row.neglect_score)));
    }

    #[test]
    fn neglect_score_uses_available_metrics_and_unknown_scores_zero() {
        let mut rows = vec![
            row(TaxonClass::Aves, Some(10), None),
            row(TaxonClass::Aves, None, Some(30.0)),
            row(TaxonClass::Unknown, Some(10), Some(30.0)),
            row(TaxonClass::Mammalia, None, None),
        ];

        StudyRow::neglect_score(&mut rows, None).expect("derived class weights are valid");

        assert!(rows[0].neglect_score > 0.0);
        assert!(rows[1].neglect_score > 0.0);
        assert_eq!(rows[2].neglect_score, 0.0);
        assert_eq!(rows[3].neglect_score, 0.0);
    }

    #[test]
    fn invalid_explicit_weights_are_reported() {
        let mut rows = vec![row(TaxonClass::Aves, Some(1), Some(1.0))];
        let weights = HashMap::from([(TaxonClass::Aves, f64::NAN)]);

        assert!(matches!(
            StudyRow::neglect_score(&mut rows, Some(&weights)),
            Err(NeglectError::InvalidWeight {
                class: TaxonClass::Aves
            })
        ));
    }

    #[test]
    fn filter_applies_all_requested_criteria() {
        let rows = vec![
            row(TaxonClass::Aves, Some(10), Some(30.0)),
            row(TaxonClass::Mammalia, Some(5), Some(90.0)),
            row(TaxonClass::Mammalia, Some(10), Some(100.0)),
        ];
        let mut downloadable_rows = rows.clone();
        downloadable_rows[1].downloadable = false;
        let filtered = StudyRow::filter_studies(
            &downloadable_rows,
            &Filter {
                class: Some(TaxonClass::Mammalia),
                min_individuals: Some(5),
                min_days: Some(90.0),
                downloadable: true,
            },
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].n_individuals, Some(10));
        assert_eq!(filtered[0].duration_days, Some(100.0));
    }
}
