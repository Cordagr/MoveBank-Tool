use crate::{DailyMovement, TagEffectError};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::collections::BTreeMap;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq)]
pub struct PeriodComparison {
    pub deployment_id: i64,
    pub individual_id: String,
    pub early_mean_km: f64,
    pub baseline_mean_km: f64,
    pub difference_km: f64,
    pub early_days_observed: u32,
    pub baseline_days_observed: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EffectSummary {
    pub individuals: usize,
    pub mean_difference_km: f64,
    pub confidence_interval: (f64, f64),
}

pub fn compare_periods(
    rows: &[DailyMovement],
    early: Range<u32>,
    baseline: Range<u32>,
    min_days: usize,
) -> Result<Vec<PeriodComparison>, TagEffectError> {
    let empty = |r: &Range<u32>| r.start >= r.end;
    let overlap = early.start < baseline.end && baseline.start < early.end;
    if empty(&early) || empty(&baseline) || overlap {
        return Err(TagEffectError::InvalidComparisonWindow);
    }
    if min_days == 0 {
        return Err(TagEffectError::InvalidMinimumCoverage);
    }

    let mut groups: BTreeMap<(i64, String), Vec<&DailyMovement>> = BTreeMap::new();
    for row in rows {
        groups
            .entry((row.deployment_id, row.individual_id.clone()))
            .or_default()
            .push(row);
    }

    let mut out = Vec::new();
    for ((deployment_id, individual_id), days) in groups {
        let Some((early_mean_km, early_days_observed)) = period_mean(&days, &early, min_days)
        else {
            continue;
        };
        let Some((baseline_mean_km, baseline_days_observed)) =
            period_mean(&days, &baseline, min_days)
        else {
            continue;
        };

        out.push(PeriodComparison {
            deployment_id,
            individual_id,
            early_mean_km,
            baseline_mean_km,
            difference_km: early_mean_km - baseline_mean_km,
            early_days_observed,
            baseline_days_observed,
        });
    }
    Ok(out)
}

fn period_mean(rows: &[&DailyMovement], days: &Range<u32>, min_days: usize) -> Option<(f64, u32)> {
    let vals: Vec<f64> = rows
        .iter()
        .filter(|r| days.contains(&r.day_since_deployment) && r.usable_segments > 0)
        .filter_map(|r| r.distance_km)
        .filter(|d| d.is_finite() && *d >= 0.0)
        .collect();

    if vals.len() < min_days {
        return None;
    }
    Some((vals.iter().sum::<f64>() / vals.len() as f64, vals.len() as u32))
}

pub fn summarize_effects(
    comparisons: &[PeriodComparison],
    iterations: usize,
    confidence_level: f64,
    seed: u64,
) -> Result<EffectSummary, TagEffectError> {
    if comparisons.is_empty() {
        return Err(TagEffectError::NoEffects);
    }

    // average across deployments first so each animal only counts once
    let mut per_individual: BTreeMap<&str, (f64, usize)> = BTreeMap::new();
    for c in comparisons.iter().filter(|c| c.difference_km.is_finite()) {
        let entry = per_individual.entry(&c.individual_id).or_insert((0.0, 0));
        entry.0 += c.difference_km;
        entry.1 += 1;
    }
    let effects: Vec<f64> = per_individual
        .values()
        .map(|(sum, n)| sum / *n as f64)
        .collect();

    match effects.len() {
        0 => return Err(TagEffectError::NoEffects),
        1 => return Err(TagEffectError::InsufficientIndividuals),
        _ => {}
    }

    let mean_difference_km = effects.iter().sum::<f64>() / effects.len() as f64;
    let confidence_interval =
        bootstrap_confidence_interval(&effects, iterations, confidence_level, seed)?;

    Ok(EffectSummary {
        individuals: effects.len(),
        mean_difference_km,
        confidence_interval,
    })
}

pub fn bootstrap_confidence_interval(
    effects: &[f64],
    iterations: usize,
    confidence_level: f64,
    seed: u64,
) -> Result<(f64, f64), TagEffectError> {
    let bad_level = !(confidence_level > 0.0 && confidence_level < 1.0);
    if effects.is_empty() || iterations == 0 || bad_level || effects.iter().any(|e| !e.is_finite())
    {
        return Err(TagEffectError::InvalidBootstrapOptions);
    }

    let n = effects.len();
    let mut rng = StdRng::seed_from_u64(seed);
    let mut means: Vec<f64> = (0..iterations)
        .map(|_| (0..n).map(|_| effects[rng.gen_range(0..n)]).sum::<f64>() / n as f64)
        .collect();
    means.sort_by(f64::total_cmp);

    let tail = (1.0 - confidence_level) / 2.0;
    Ok((percentile(&means, tail), percentile(&means, 1.0 - tail)))
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let pos = p * (sorted.len() - 1) as f64;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    sorted[lo] + (pos - lo as f64) * (sorted[hi] - sorted[lo])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(day: u32, km: f64) -> DailyMovement {
        DailyMovement {
            deployment_id: 1,
            individual_id: "individual-1".to_owned(),
            day_since_deployment: day,
            distance_km: Some(km),
            usable_segments: 1,
        }
    }

    fn cmp(deployment_id: i64, id: &str, diff: f64) -> PeriodComparison {
        PeriodComparison {
            deployment_id,
            individual_id: id.to_owned(),
            early_mean_km: 0.0,
            baseline_mean_km: 0.0,
            difference_km: diff,
            early_days_observed: 1,
            baseline_days_observed: 1,
        }
    }

    #[test]
    fn period_means() {
        let rows = [row(0, 10.0), row(1, 20.0), row(7, 5.0), row(8, 7.0)];
        let out = compare_periods(&rows, 0..2, 7..9, 2).unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].early_mean_km, 15.0);
        assert_eq!(out[0].baseline_mean_km, 6.0);
        assert_eq!(out[0].difference_km, 9.0);
    }

    #[test]
    fn skips_sparse_deployments() {
        let rows = [row(0, 10.0), row(7, 5.0)];
        assert!(compare_periods(&rows, 0..2, 7..9, 2).unwrap().is_empty());
    }

    #[test]
    fn rejects_overlapping_windows() {
        assert!(compare_periods(&[], 0..5, 3..8, 1).is_err());
    }

    #[test]
    fn individuals_counted_once() {
        let comps = vec![
            cmp(1, "individual-1", 4.0),
            cmp(2, "individual-1", 8.0),
            cmp(3, "individual-2", 1.0),
        ];
        let s = summarize_effects(&comps, 500, 0.95, 4).unwrap();

        assert_eq!(s.individuals, 2);
        assert_eq!(s.mean_difference_km, 3.5);
    }

    #[test]
    fn bootstrap_same_seed_same_result() {
        let effects = [-2.0, 1.0, 3.0, 5.0];
        let a = bootstrap_confidence_interval(&effects, 1_000, 0.95, 42).unwrap();
        let b = bootstrap_confidence_interval(&effects, 1_000, 0.95, 42).unwrap();
        assert_eq!(a, b);
    }
}