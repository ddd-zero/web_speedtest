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
    config::{ConfigError, RuntimeConfig, TestSettings, TestTarget},
    models::{LatencyStats, ResultStatus, SaveResultResponse},
    store::{QueryFilter, SaveResultInput, SpeedStore, StoreError},
};

#[derive(Clone)]
pub struct AppState {
    store: SpeedStore,
    config: RuntimeConfig,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("未知测速线路: {0}")]
    UnknownTarget(String),
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
            Self::Store(StoreError::MissingUpdateToken)
            | Self::Store(StoreError::InvalidUpdateToken)
            | Self::UnknownTarget(_) => StatusCode::BAD_REQUEST,
            Self::Store(StoreError::Json(_))
            | Self::Store(StoreError::Database(_))
            | Self::Store(StoreError::LockPoisoned)
            | Self::Config(_)
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

#[derive(Debug, Serialize)]
pub struct ClientConfig {
    pub targets: Vec<TestTarget>,
    pub test: TestSettings,
}

pub fn build_router(store: SpeedStore, runtime_config: RuntimeConfig) -> Router {
    let state = AppState {
        store,
        config: runtime_config,
    };
    Router::new()
        .route("/", get(index))
        .route("/api/config", get(client_config))
        .route("/api/results", get(results).post(save_result))
        .with_state(state)
}

pub async fn serve() -> Result<(), AppError> {
    let config_path = std::env::var("WEB_SPEED_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config.toml"));
    let config = RuntimeConfig::load(config_path)?;
    let listener = tokio::net::TcpListener::bind(config.listen_addr()).await?;
    let store = SpeedStore::open(&config.database.path)?;

    axum::serve(
        listener,
        build_router(store, config).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(embedded_index_html())
}

async fn client_config(State(state): State<AppState>) -> Json<ClientConfig> {
    Json(ClientConfig {
        targets: state.config.targets.clone(),
        test: state.config.test.clone(),
    })
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
    let target = state
        .config
        .targets
        .iter()
        .find(|target| target.key == payload.domain_key)
        .cloned()
        .ok_or_else(|| AppError::UnknownTarget(payload.domain_key.clone()))?;
    let client_ip = extract_client_ip(&headers, addr, payload.client_ip.as_deref());
    let input = SaveResultInput {
        id: payload.id,
        update_token: payload.update_token,
        domain_key: target.key,
        domain_host: target.host,
        trace_url: target.trace_url,
        download_url: target.download_url,
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
        assert!(html.contains("width: min(920px, 100%)"));
        assert!(html.contains("@media (max-width: 640px)"));
        assert!(!html.contains("@media (max-width: 560px)"));
        assert!(!html.contains("@media (max-width: 720px)"));
        assert!(!html.contains("@media (max-width: 860px)"));
        assert!(html.contains("loadConfig()"));
        assert!(html.contains("target-speed-value"));
        assert!(html.contains("target-row-progress"));
        assert!(html.contains("target-colo"));
        assert!(html.contains("sortedTargetsForRender()"));
        assert!(html.contains("COLO_PALETTE"));
        assert!(html.contains("colorForColo(colo)"));
        assert!(html.contains("stopDownloadTest()"));
        assert!(html.contains("patchDownloadRow(targetKey)"));
        assert!(html.contains("renderDownloadStatusText(targetState)"));
        assert!(!html.contains("target-download-strip"));
        assert!(html.contains("MAX_CONSECUTIVE_LATENCY_FAILURES = 3"));
        assert!(html.contains("consecutiveLatencyFailures"));
        assert!(html.contains("latencySkipped"));
        assert!(html.contains("progress_save_ratio"));
        assert!(!html.contains("保存状态"));
        assert!(!html.contains("已保存"));
        assert!(!html.contains("colo-info"));
        assert!(!html.contains("边缘节点"));
        assert!(!html.contains("searchParams.set(\"_\""));
    }

    #[test]
    fn embedded_index_html_should_keep_compact_target_rows_single_line() {
        let html = super::embedded_index_html();

        assert!(
            !html.contains("grid-template-columns: minmax(0, 1fr) auto auto;"),
            "窄屏列表不应出现三列两行的中间态，避免 560-720px 宽度行高跳变"
        );
        assert!(
            !html.contains("@media (max-width: 720px)"),
            "720px 是平板/窄桌面宽度，不应触发测速列表布局突变"
        );
        assert!(
            html.contains("@media (max-width: 640px)"),
            "640px 是顶部说明开始换行的位置，应直接切到手机单列布局"
        );
        assert!(
            html.contains("target-host-text"),
            "紧凑列表需要给主机名单独挂载可省略的样式类"
        );
        assert!(
            html.contains("text-overflow: ellipsis"),
            "紧凑列表中的长主机名应省略显示，避免撑高行内容"
        );
    }

    #[test]
    fn embedded_index_html_should_keep_main_page_scroll_locked_to_target_list() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("height: 100dvh;"),
            "页面根容器应锁定到动态视口高度，避免 1920x921 等桌面视口出现外层滚动"
        );
        assert!(
            html.contains("overflow: hidden;"),
            "外层页面不应参与滚动，滚动应保留在测速列表内部"
        );
        assert!(
            html.contains("display: flex;\n      flex-direction: column;\n      min-height: 0;"),
            "主卡片应使用纵向 flex 布局，让列表吃掉剩余空间"
        );
        assert!(
            html.contains("flex: 1 1 auto;\n      min-height: 0;\n      overflow: auto;"),
            "测速列表应作为唯一滚动容器，并允许在 flex 容器内收缩"
        );
        assert!(
            html.contains(".target-list {\n      display: flex;\n      flex-direction: column;"),
            "测速列表容器应使用纵向 flex 堆叠行，避免父级 grid 隐式行高压缩测速行"
        );
        assert!(
            !html.contains(".target-list {\n      display: grid;"),
            "测速列表容器不应再作为父级 grid，否则行高可能低于测速行内部内容"
        );
        assert!(
            html.contains(".target-list-header,\n    .target-row {\n      flex: 0 0 auto;"),
            "列表表头和测速行不应在滚动容器内收缩，否则会压扁行高"
        );
        assert!(
            !html.contains("max-height: 66vh;"),
            "固定 66vh 未扣除标题和信息区高度，会导致页面整体超出视口"
        );
    }

    #[test]
    fn embedded_index_html_should_use_responsive_history_window() {
        let html = super::embedded_index_html();

        assert!(html.contains("role=\"dialog\""));
        assert!(html.contains("aria-modal=\"true\""));
        assert!(html.contains("history-shell"));
        assert!(html.contains("class=\"history-fab\""));
        assert!(html.contains("position: fixed"));
        assert!(html.contains("right: 20px"));
        assert!(html.contains("border-radius: 999px"));
        assert!(html.contains("class=\"history-toolbar\""));
        assert!(html.contains("class=\"history-status-pill"));
        assert!(html.contains("width: min(980px, calc(100vw - 48px))"));
        assert!(html.contains("height: min(760px, calc(100dvh - 48px))"));
        assert!(html.contains("      box-sizing: border-box;\n      overflow: hidden;"));
        assert!(html.contains("scrollbar-gutter: stable"));
        assert!(html.contains("@media (max-width: 640px)"));
        assert!(html.contains("width: 100vw"));
        assert!(html.contains("height: 100dvh"));
        assert!(html.contains("        scrollbar-gutter: auto;"));
        assert!(html.contains("        border: none;\n      }\n\n      .history-toolbar"));
        assert!(html.contains("@keyframes popIn"));
        assert!(html.contains("animation: popIn 0.3s cubic-bezier(0.18, 0.89, 0.32, 1.28)"));
        assert!(html.contains("transform: scale(0.95)"));
        assert!(html.contains("transform: scale(1)"));
        assert!(html.contains(".modal-overlay:focus"));
        assert!(html.contains("id=\"close-history-btn\""));
        assert!(html.contains("els.closeHistoryBtn.addEventListener"));
        assert!(!html.contains("clip-path: circle"));
        assert!(!html.contains("backdrop-filter"));
        assert!(!html.contains("history-bounce-in"));
        assert!(!html.contains("<button class=\"btn-secondary\" id=\"history-btn\""));
    }
}
