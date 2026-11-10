use crate::{daily_distances, DailyMovement, DeploymentEvents, MatchReport, TagEffectError};
use chrono::Duration;
use mb_client::{Client, Deployment, EventQuery, Fix, MovebankError};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct StudyAnalysis {
    pub daily_movements: Vec<DailyMovement>,
    pub match_report: MatchReport,
}

pub fn analyze_records(
    deployments: &[Deployment],
    fixes: &[Fix],
    max_gap: Duration,
) -> Result<StudyAnalysis, TagEffectError> {
    if max_gap <= Duration::zero() {
        return Err(TagEffectError::InvalidMaxGap);
    }

    let match_report = crate::match_fixes_to_deployments(deployments, fixes);
    let mut groups: BTreeMap<i64, DeploymentEvents> = BTreeMap::new();

    for (fix, deployment) in &match_report.matches {
        groups
            .entry(deployment.deployment_id)
            .or_insert_with(|| DeploymentEvents {
                deployment: deployment.clone(),
                fixes: Vec::new(),
            })
            .fixes
            .push(fix.clone());
    }

    let daily_movements = groups
        .values()
        .map(|input| daily_distances(input, max_gap))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();

    Ok(StudyAnalysis {
        daily_movements,
        match_report,
    })
}

pub async fn fetch_study_analysis<F>(
    client: &Client,
    query: &EventQuery,
    max_gap: Duration,
    mut approve_license: F,
) -> Result<StudyAnalysis, TagEffectError>
where
    F: FnMut(i64, &[u8]) -> Result<bool, std::io::Error>,
{
    if query.study_id <= 0 {
        return Err(TagEffectError::InvalidStudyId);
    }
    if max_gap <= Duration::zero() {
        return Err(TagEffectError::InvalidMaxGap);
    }
    let (Some(start), Some(end)) = (query.start, query.end) else {
        return Err(TagEffectError::MissingEventQueryBounds);
    };
    if start >= end {
        return Err(TagEffectError::InvalidEventQueryRange);
    }

    let deployments = match client.get_deployments(query.study_id).await {
        Ok(deployments) => deployments,
        Err(MovebankError::LicenseRequired { terms }) => {
            if approve_license(query.study_id, &terms).map_err(TagEffectError::LicensePrompt)? {
                client
                    .get_deployments_after_accepting(query.study_id, &terms)
                    .await?
            } else {
                return Err(TagEffectError::LicenseNotAccepted {
                    study_id: query.study_id,
                });
            }
        }
        Err(error) => return Err(error.into()),
    };

    let fixes = match client.get_events(query).await {
        Ok(fixes) => fixes,
        Err(MovebankError::LicenseRequired { terms }) => {
            if approve_license(query.study_id, &terms).map_err(TagEffectError::LicensePrompt)? {
                client.get_events_after_accepting(query, &terms).await?
            } else {
                return Err(TagEffectError::LicenseNotAccepted {
                    study_id: query.study_id,
                });
            }
        }
        Err(error) => return Err(error.into()),
    };

    analyze_records(&deployments, &fixes, max_gap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveDateTime};

    fn timestamp(day: u32, hour: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2024, 1, day)
            .expect("valid date")
            .and_hms_opt(hour, 0, 0)
            .expect("valid time")
    }

    #[test]
    fn analysis_groups_uniquely_matched_fixes_and_keeps_match_diagnostics() {
        let deployments = [Deployment {
            deployment_id: 11,
            individual_id: Some("individual-1".to_owned()),
            start_timestamp: Some(timestamp(1, 0)),
            end_timestamp: Some(timestamp(1, 3)),
        }];
        let fixes = vec![
            Fix {
                individual_id: "individual-1".to_owned(),
                deployment_id: Some(11),
                timestamp: timestamp(1, 0),
                lat: 0.0,
                lon: 0.0,
                outlier: None,
            },
            Fix {
                individual_id: "individual-1".to_owned(),
                deployment_id: Some(11),
                timestamp: timestamp(1, 1),
                lat: 0.0,
                lon: 1.0,
                outlier: None,
            },
            Fix {
                individual_id: "other".to_owned(),
                deployment_id: None,
                timestamp: timestamp(1, 1),
                lat: 0.0,
                lon: 1.0,
                outlier: None,
            },
        ];

        let analysis =
            analyze_records(&deployments, &fixes, Duration::hours(2)).expect("valid records");

        assert_eq!(analysis.daily_movements.len(), 1);
        assert_eq!(analysis.daily_movements[0].usable_segments, 1);
        assert_eq!(analysis.match_report.unmatched_fixes.len(), 1);
    }
}
