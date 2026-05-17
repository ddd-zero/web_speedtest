#[derive(Debug, Clone, serde::Serialize)]
pub struct SaveResultResponse {
    pub id: i64,
    pub update_token: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PublicSpeedRecord {
    pub id: i64,
    pub domain: String,
    pub https_latency_ms: Option<f64>,
    pub https_jitter_ms: Option<f64>,
    pub download_mbps: Option<f64>,
    pub client_ip: String,
    pub ip_country: Option<String>,
    pub ip_region: Option<String>,
    pub ip_city: Option<String>,
    pub ip_isp: Option<String>,
    pub colo: Option<String>,
    pub created_at: String,
}
