use mb_client::MovebankError;

#[derive(Debug, thiserror::Error)]
pub enum TagEffectError {
    #[error("maximum time gap must be greater than zero")]
    InvalidMaxGap,
    #[error("deployment is missing its start timestamp")]
    MissingDeploymentStart,
    #[error("deployment is missing its individual ID")]
    MissingIndividualId,
    #[error("deployment end timestamp is before its start timestamp")]
    InvalidDeploymentWindow,
    #[error("event query must include both start and end timestamps")]
    MissingEventQueryBounds,
    #[error("event query start timestamp must be before its end timestamp")]
    InvalidEventQueryRange,
    #[error("study ID must be positive")]
    InvalidStudyId,
    #[error("deployment day index exceeds the supported range")]
    DayIndexOverflow,
    #[error("comparison day ranges must be nonempty and must not overlap")]
    InvalidComparisonWindow,
    #[error("minimum observed days must be greater than zero")]
    InvalidMinimumCoverage,
    #[error("bootstrap requires at least one effect, at least one iteration, and a confidence level between 0 and 1")]
    InvalidBootstrapOptions,
    #[error("no individual effects are available to summarize")]
    NoEffects,
    #[error("at least two individuals are required to estimate a confidence interval")]
    InsufficientIndividuals,
    #[error("license terms for study {study_id} were not accepted")]
    LicenseNotAccepted { study_id: i64 },
    #[error("Movebank request failed: {0}")]
    Movebank(#[from] MovebankError),
    #[error("could not read license confirmation: {0}")]
    LicensePrompt(#[from] std::io::Error),
}
