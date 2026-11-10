use mb_client::{Deployment, Fix};

pub fn matches_deployment(fix: &Fix, deployment: &Deployment) -> bool {
    let Some(start) = deployment.start_timestamp else {
        return false;
    };

    let before_end = match deployment.end_timestamp {
        Some(end) => fix.timestamp <= end,
        None => true,
    };

    let within_deployment = fix.timestamp >= start && before_end;
    match fix.deployment_id {
        Some(deployment_id) => {
            let individual_matches = match deployment.individual_id.as_deref() {
                Some(individual_id) => fix.individual_id == individual_id,
                None => true,
            };
            deployment_id == deployment.deployment_id && within_deployment && individual_matches
        }
        None => {
            let Some(individual_id) = deployment.individual_id.as_deref() else {
                return false;
            };
            fix.individual_id == individual_id && within_deployment
        }
    }
}

pub fn match_fixes_to_deployments(deployments: &[Deployment], fixes: &[Fix]) -> MatchReport {
    let mut matches = Vec::new();
    let mut unmatched_fixes = Vec::new();
    let mut ambiguous_fixes = Vec::new();
    let mut deployment_match_counts = vec![0; deployments.len()];

    for fix in fixes {
        let matching_indices: Vec<usize> = deployments
            .iter()
            .enumerate()
            .filter_map(|(index, deployment)| matches_deployment(fix, deployment).then_some(index))
            .collect();

        for &index in &matching_indices {
            deployment_match_counts[index] += 1;
        }

        match matching_indices.as_slice() {
            [] => unmatched_fixes.push(fix.clone()),
            [index] => matches.push((fix.clone(), deployments[*index].clone())),
            _ => ambiguous_fixes.push(fix.clone()),
        }
    }

    let unmatched_deployments = deployments
        .iter()
        .zip(deployment_match_counts)
        .filter(|(_, count)| *count == 0)
        .map(|(deployment, _)| deployment.clone())
        .collect();

    MatchReport {
        matches,
        unmatched_fixes,
        ambiguous_fixes,
        unmatched_deployments,
    }
}

#[derive(Debug, Clone)]
pub struct MatchReport {
    pub matches: Vec<(Fix, Deployment)>,
    pub unmatched_fixes: Vec<Fix>,
    pub ambiguous_fixes: Vec<Fix>,
    pub unmatched_deployments: Vec<Deployment>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveDateTime};

    fn timestamp(day: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2024, 1, day)
            .expect("valid date")
            .and_hms_opt(12, 0, 0)
            .expect("valid time")
    }

    fn deployment(id: i64, individual_id: &str, start_day: u32, end_day: u32) -> Deployment {
        Deployment {
            deployment_id: id,
            individual_id: Some(individual_id.to_owned()),
            start_timestamp: Some(timestamp(start_day)),
            end_timestamp: Some(timestamp(end_day)),
        }
    }

    fn fix(individual_id: &str, day: u32) -> Fix {
        Fix {
            individual_id: individual_id.to_owned(),
            deployment_id: None,
            timestamp: timestamp(day),
            lon: -100.0,
            lat: 40.0,
            outlier: None,
        }
    }

    #[test]
    fn classifies_fixes_once_and_reports_unmatched_records() {
        let deployments = [
            deployment(1, "bird-1", 1, 3),
            deployment(2, "bird-1", 5, 7),
            deployment(3, "bird-2", 1, 3),
        ];
        let fixes = [
            fix("bird-1", 2),
            fix("bird-1", 4),
            fix("bird-2", 2),
            fix("bird-3", 2),
        ];

        let report = match_fixes_to_deployments(&deployments, &fixes);

        assert_eq!(report.matches.len(), 2);
        assert_eq!(report.unmatched_fixes.len(), 2);
        assert!(report.ambiguous_fixes.is_empty());
        assert_eq!(report.unmatched_deployments.len(), 1);
        assert_eq!(report.unmatched_deployments[0].deployment_id, 2);
    }

    #[test]
    fn overlapping_deployments_make_a_fix_ambiguous_not_duplicated() {
        let deployments = [deployment(1, "bird-1", 1, 4), deployment(2, "bird-1", 3, 5)];
        let fixes = [fix("bird-1", 3)];

        let report = match_fixes_to_deployments(&deployments, &fixes);

        assert!(report.matches.is_empty());
        assert!(report.unmatched_fixes.is_empty());
        assert_eq!(report.ambiguous_fixes.len(), 1);
        assert!(report.unmatched_deployments.is_empty());
    }

    #[test]
    fn missing_deployment_start_does_not_match() {
        let deployment = Deployment {
            deployment_id: 1,
            individual_id: Some("bird-1".to_owned()),
            start_timestamp: None,
            end_timestamp: None,
        };

        assert!(!matches_deployment(&fix("bird-1", 2), &deployment));
    }

    #[test]
    fn event_deployment_id_matches_when_deployment_has_no_individual_id() {
        let mut event = fix("bird-1", 2);
        event.deployment_id = Some(1);
        let deployment = Deployment {
            deployment_id: 1,
            individual_id: None,
            start_timestamp: Some(timestamp(1)),
            end_timestamp: Some(timestamp(3)),
        };

        assert!(matches_deployment(&event, &deployment));

        event.deployment_id = Some(2);
        assert!(!matches_deployment(&event, &deployment));
    }
}
