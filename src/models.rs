#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResultStatus {
    Running,
    Completed,
    Failed,
}

impl ResultStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn from_db(value: &str) -> Self {
        match value {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LatencyStats {
    pub median_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub samples_ms: Vec<f64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SaveResultResponse {
    pub id: i64,
    pub update_token: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PublicSpeedRecord {
    pub id: i64,
    pub domain_key: String,
    pub domain_host: String,
    pub https_latency_median_ms: Option<f64>,
    pub https_latency_min_ms: Option<f64>,
    pub https_latency_max_ms: Option<f64>,
    pub partial_download_mbps: Option<f64>,
    pub final_download_mbps: Option<f64>,
    pub status: ResultStatus,
    pub client_ip: String,
    pub location: Option<String>,
    pub isp: Option<String>,
    pub colo: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
