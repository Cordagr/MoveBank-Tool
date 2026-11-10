use crate::{DailyMovement, DeploymentEvents, TagEffectError};
use chrono::Duration;
use mb_client::Fix;
use std::collections::BTreeMap;

const EARTH_RADIUS_KM: f64 = 6_371.0;

pub fn is_usable_fix(fix: &Fix) -> bool {
    fix.outlier != Some(true)
        && fix.lat.is_finite()
        && fix.lon.is_finite()
        && (-90.0..=90.0).contains(&fix.lat)
        && (-180.0..=180.0).contains(&fix.lon)
}

pub fn haversine_km(a: &Fix, b: &Fix) -> Option<f64> {
    if !is_usable_fix(a) || !is_usable_fix(b) {
        return None;
    }

    let (lat1, lat2) = (a.lat.to_radians(), b.lat.to_radians());
    let dlat = lat2 - lat1;
    let dlon = (b.lon - a.lon).to_radians();

    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    let h = h.clamp(0.0, 1.0);

    Some(EARTH_RADIUS_KM * 2.0 * h.sqrt().atan2((1.0 - h).sqrt()))
}

fn non_blank(s: &str) -> bool {
    !s.trim().is_empty()
}

/// A segment belongs to the elapsed deployment day of its first fix.
pub fn daily_distances(
    input: &DeploymentEvents,
    max_gap: Duration,
) -> Result<Vec<DailyMovement>, TagEffectError> {
    if max_gap <= Duration::zero() {
        return Err(TagEffectError::InvalidMaxGap);
    }

    let dep = &input.deployment;

    // prefer the deployment's own id, fall back to the first named fix
    let id = dep
        .individual_id
        .as_deref()
        .filter(|s| non_blank(s))
        .or_else(|| {
            input
                .fixes
                .iter()
                .map(|f| f.individual_id.as_str())
                .find(|s| non_blank(s))
        })
        .ok_or(TagEffectError::MissingIndividualId)?;

    let start = dep
        .start_timestamp
        .ok_or(TagEffectError::MissingDeploymentStart)?;
    if dep.end_timestamp.is_some_and(|end| end < start) {
        return Err(TagEffectError::InvalidDeploymentWindow);
    }

    let mut fixes: Vec<&Fix> = input
        .fixes
        .iter()
        .filter(|f| {
            f.individual_id == id
                && f.timestamp >= start
                && dep.end_timestamp.map_or(true, |end| f.timestamp <= end)
        })
        .collect();
    fixes.sort_by_key(|f| f.timestamp);

    // day index -> (total km, segment count)
    let mut totals: BTreeMap<u32, (f64, u32)> = BTreeMap::new();
    for pair in fixes.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let gap = b.timestamp - a.timestamp;
        if gap <= Duration::zero() || gap > max_gap {
            continue;
        }

        // None if either fix is an outlier or has bad coordinates
        let Some(km) = haversine_km(a, b) else {
            continue;
        };

        let day: u32 = (a.timestamp - start)
            .num_days()
            .try_into()
            .map_err(|_| TagEffectError::DayIndexOverflow)?;
        let entry = totals.entry(day).or_default();
        entry.0 += km;
        entry.1 += 1;
    }

    Ok(totals
        .into_iter()
        .map(|(day, (km, segments))| DailyMovement {
            deployment_id: dep.deployment_id,
            individual_id: id.to_owned(),
            day_since_deployment: day,
            distance_km: Some(km),
            usable_segments: segments,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveDateTime};
    use mb_client::Deployment;

    fn ts(day: u32, hour: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2024, 1, day)
            .unwrap()
            .and_hms_opt(hour, 0, 0)
            .unwrap()
    }

    fn fix(lat: f64, lon: f64, day: u32, hour: u32) -> Fix {
        Fix {
            individual_id: "individual-1".to_owned(),
            deployment_id: None,
            timestamp: ts(day, hour),
            lat,
            lon,
            outlier: None,
        }
    }

    fn deployment(start: NaiveDateTime) -> Deployment {
        Deployment {
            deployment_id: 7,
            individual_id: Some("individual-1".to_owned()),
            start_timestamp: Some(start),
            end_timestamp: None,
        }
    }

    #[test]
    fn same_point_is_zero_distance() {
        let d = haversine_km(&fix(0.0, 0.0, 1, 0), &fix(0.0, 0.0, 1, 1));
        assert_eq!(d, Some(0.0));
    }

    #[test]
    fn one_degree_of_longitude_at_equator() {
        let d = haversine_km(&fix(0.0, 0.0, 1, 0), &fix(0.0, 1.0, 1, 1)).unwrap();
        assert!((d - 111.2).abs() < 0.2);
    }

    #[test]
    fn rejects_outliers_and_bad_coordinates() {
        let mut outlier = fix(0.0, 0.0, 1, 0);
        outlier.outlier = Some(true);

        assert!(!is_usable_fix(&outlier));
        assert_eq!(haversine_km(&outlier, &fix(0.0, 1.0, 1, 1)), None);
        assert!(!is_usable_fix(&fix(f64::NAN, 0.0, 1, 0)));
        assert!(!is_usable_fix(&fix(91.0, 0.0, 1, 0)));
        assert!(!is_usable_fix(&fix(0.0, 181.0, 1, 0)));
    }

    #[test]
    fn sorts_fixes_and_credits_midnight_crossing_to_first_day() {
        let input = DeploymentEvents {
            deployment: deployment(ts(1, 23)),
            fixes: vec![
                fix(0.0, 1.0, 2, 1),
                fix(0.0, 0.0, 1, 23),
                fix(0.0, 2.0, 2, 8), // 7h gap, too long
            ],
        };

        let rows = daily_distances(&input, Duration::hours(3)).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].day_since_deployment, 0);
        assert_eq!(rows[0].usable_segments, 1);
        assert!((rows[0].distance_km.unwrap() - 111.2).abs() < 0.2);
    }

    #[test]
    fn does_not_bridge_outliers_or_long_gaps() {
        let mut outlier = fix(0.0, 1.0, 1, 1);
        outlier.outlier = Some(true);
        let input = DeploymentEvents {
            deployment: deployment(ts(1, 0)),
            fixes: vec![
                fix(0.0, 0.0, 1, 0),
                outlier,
                fix(0.0, 2.0, 1, 2),
                fix(0.0, 3.0, 1, 8),
            ],
        };

        let rows = daily_distances(&input, Duration::hours(3)).unwrap();
        assert!(rows.is_empty());
    }
}