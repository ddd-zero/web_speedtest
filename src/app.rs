use std::{net::SocketAddr, path::PathBuf};

use axum::{
    Json, Router,
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::{
    config::TEST_TARGETS,
    models::{LatencyStats, ResultStatus, SaveResultResponse},
    store::{QueryFilter, SaveResultInput, SpeedStore, StoreError},
};

#[derive(Clone)]
pub struct AppState {
    store: SpeedStore,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("服务启动失败: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let status = match &self {
            Self::Store(StoreError::UnknownDomain(_))
            | Self::Store(StoreError::MissingUpdateToken)
            | Self::Store(StoreError::InvalidUpdateToken) => StatusCode::BAD_REQUEST,
            Self::Store(StoreError::Json(_))
            | Self::Store(StoreError::Database(_))
            | Self::Store(StoreError::LockPoisoned)
            | Self::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        (
            status,
            Json(ErrorResponse {
                error: self.to_string(),
            }),
        )
            .into_response()
    }
}

#[derive(Debug, Deserialize)]
pub struct SaveResultRequest {
    pub id: Option<i64>,
    pub update_token: Option<String>,
    pub domain_key: String,
    pub https_latency: LatencyStats,
    pub partial_download_mbps: Option<f64>,
    pub final_download_mbps: Option<f64>,
    pub status: ResultStatus,
    pub client_ip: Option<String>,
    pub location: Option<String>,
    pub isp: Option<String>,
    pub colo: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResultsQuery {
    pub domain: Option<String>,
    pub status: Option<ResultStatus>,
    pub q: Option<String>,
    pub limit: Option<usize>,
}

pub fn build_router(store: SpeedStore) -> Router {
    let state = AppState { store };
    Router::new()
        .route("/", get(index))
        .route("/api/config", get(config))
        .route("/api/results", get(results).post(save_result))
        .with_state(state)
}

pub async fn serve() -> Result<(), AppError> {
    let db_path = std::env::var("WEB_SPEED_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("speed_results.sqlite3"));
    let bind_addr =
        std::env::var("WEB_SPEED_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let store = SpeedStore::open(db_path)?;

    axum::serve(
        listener,
        build_router(store).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(embedded_index_html())
}

async fn config() -> Json<&'static [crate::config::TestTarget]> {
    Json(TEST_TARGETS)
}

pub fn embedded_index_html() -> &'static str {
    include_str!("../frontend/index.html")
}

async fn save_result(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<SaveResultRequest>,
) -> Result<Json<SaveResultResponse>, AppError> {
    let client_ip = extract_client_ip(&headers, addr, payload.client_ip.as_deref());
    let input = SaveResultInput {
        id: payload.id,
        update_token: payload.update_token,
        domain_key: payload.domain_key,
        https_latency: payload.https_latency,
        partial_download_mbps: payload.partial_download_mbps,
        final_download_mbps: payload.final_download_mbps,
        status: payload.status,
        client_ip,
        location: payload.location,
        isp: payload.isp,
        colo: payload.colo,
    };

    Ok(Json(state.store.save_result(input)?))
}

async fn results(
    State(state): State<AppState>,
    Query(query): Query<ResultsQuery>,
) -> Result<Json<Vec<crate::models::PublicSpeedRecord>>, AppError> {
    Ok(Json(state.store.query_results(QueryFilter {
        domain: query.domain,
        status: query.status,
        q: query.q,
        limit: query.limit,
    })?))
}

pub fn extract_client_ip(
    headers: &HeaderMap,
    socket: SocketAddr,
    reported_ip: Option<&str>,
) -> String {
    // 前端 trace 上报的公网 IP 更接近用户出口；仍先解析校验，避免把任意字符串写入数据库。
    reported_ip
        .and_then(parse_ip_candidate)
        .or_else(|| header_ip(headers, "cf-connecting-ip"))
        .or_else(|| header_ip(headers, "x-real-ip"))
        .or_else(|| header_ip(headers, "x-forwarded-for"))
        .unwrap_or_else(|| socket.ip().to_string())
}

fn header_ip(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_ip_candidate)
}

fn parse_ip_candidate(value: &str) -> Option<String> {
    value
        .split(',')
        .next()
        .map(str::trim)
        .filter(|candidate| !candidate.is_empty())
        .and_then(|candidate| candidate.parse::<std::net::IpAddr>().ok())
        .map(|ip| ip.to_string())
}

#[cfg(test)]
mod tests {
    use super::extract_client_ip;
    use axum::http::HeaderMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[test]
    fn extract_client_ip_should_prefer_valid_reported_ip() {
        let headers = HeaderMap::new();
        let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);

        let ip = extract_client_ip(&headers, socket, Some("8.8.8.8"));

        assert_eq!(ip, "8.8.8.8");
    }

    #[test]
    fn extract_client_ip_should_fallback_to_socket_when_reported_ip_is_invalid() {
        let headers = HeaderMap::new();
        let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 8080);

        let ip = extract_client_ip(&headers, socket, Some("bad-ip"));

        assert_eq!(ip, "10.0.0.2");
    }

    #[test]
    fn embedded_index_html_should_be_loaded_from_frontend_dir() {
        assert!(std::path::Path::new("frontend/index.html").is_file());

        let html = super::embedded_index_html();

        assert!(html.contains("多域名网络测速"));
        assert!(html.contains("loadConfig()"));
        assert!(html.contains("target-speed-value"));
        assert!(html.contains("target-progress-fill"));
        assert!(html.contains("测速完成后显示在节点内"));
    }
}
