use mb_client::{Deployment, Fix};

#[derive(Debug, Clone)]
pub struct DeploymentEvents {
    pub deployment: Deployment,
    pub fixes: Vec<Fix>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DailyMovement {
    pub deployment_id: i64,
    pub individual_id: String,
    pub day_since_deployment: u32,
    pub distance_km: Option<f64>,
    pub usable_segments: u32,
}
