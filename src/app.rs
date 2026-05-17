use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use axum::{
    Json, Router,
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::get,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    config::{ConfigError, HistorySettings, RuntimeConfig, TestSettings, TestTarget},
    models::SaveResultResponse,
    store::{QueryFilter, SaveResultInput, SpeedStore, StoreError},
};

const IN_MEMORY_UPDATE_TOKEN_TTL_SECS: i64 = 30;

#[derive(Clone)]
pub struct AppState {
    store: SpeedStore,
    config: RuntimeConfig,
    update_tokens: UpdateTokenStore,
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
            Self::Store(StoreError::InvalidUpdateToken) | Self::UnknownTarget(_) => {
                StatusCode::BAD_REQUEST
            }
            Self::Store(StoreError::Database(_))
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

pub fn exit_code_for_error(error: &AppError) -> ExitCode {
    match error {
        AppError::Config(_) => ExitCode::from(78),
        _ => ExitCode::FAILURE,
    }
}

#[derive(Debug, Deserialize)]
pub struct SaveResultRequest {
    pub id: Option<i64>,
    pub update_token: Option<String>,
    pub domain: String,
    pub https_latency_ms: f64,
    pub https_jitter_ms: f64,
    pub download_mbps: f64,
    pub client_ip: Option<String>,
    pub ip_country: Option<String>,
    pub ip_region: Option<String>,
    pub ip_city: Option<String>,
    pub ip_isp: Option<String>,
    pub colo: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResultsQuery {
    pub domain: Option<String>,
    pub q: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ClientConfig {
    pub targets: Vec<TestTarget>,
    pub test: TestSettings,
    pub history: HistorySettings,
}

pub fn build_router(store: SpeedStore, runtime_config: RuntimeConfig) -> Router {
    let state = AppState {
        store,
        config: runtime_config,
        update_tokens: UpdateTokenStore::default(),
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
        history: state.config.history.clone(),
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
        .find(|target| target.host == payload.domain)
        .cloned()
        .ok_or_else(|| AppError::UnknownTarget(payload.domain.clone()))?;
    let _ = (headers, addr);
    let client_ip = payload.client_ip.as_deref().and_then(parse_ip_candidate);
    let input = SaveResultInput {
        id: payload.id,
        domain: target.host,
        https_latency_ms: payload.https_latency_ms,
        https_jitter_ms: payload.https_jitter_ms,
        download_mbps: payload.download_mbps,
        client_ip,
        ip_country: clean_optional(payload.ip_country),
        ip_region: clean_optional(payload.ip_region),
        ip_city: clean_optional(payload.ip_city),
        ip_isp: clean_optional(payload.ip_isp),
        colo: payload.colo,
    };

    if let Some(id) = payload.id {
        let token = payload.update_token.as_deref().unwrap_or_default();
        state.update_tokens.validate_and_remove(id, token)?;
        state.store.save_result(input)?;
        Ok(Json(SaveResultResponse {
            id,
            update_token: token.to_string(),
        }))
    } else {
        let id = state.store.save_result(input)?;
        Ok(Json(SaveResultResponse {
            id,
            update_token: state.update_tokens.create(id),
        }))
    }
}

async fn results(
    State(state): State<AppState>,
    Query(query): Query<ResultsQuery>,
) -> Result<Json<Vec<crate::models::PublicSpeedRecord>>, AppError> {
    let limit = state.config.history.resolve_limit(query.limit);
    Ok(Json(state.store.query_results(QueryFilter {
        domain: query.domain,
        q: query.q,
        limit,
    })?))
}

#[derive(Clone, Default)]
struct UpdateTokenStore {
    tokens: Arc<Mutex<HashMap<i64, PendingUpdateToken>>>,
    sequence: Arc<AtomicU64>,
}

struct PendingUpdateToken {
    token: String,
    expires_at: DateTime<Utc>,
}

impl UpdateTokenStore {
    fn create(&self, id: i64) -> String {
        let mut tokens = self.tokens.lock().expect("更新令牌锁不应损坏");
        Self::cleanup_expired_locked(&mut tokens);
        let token = self.next_token();
        tokens.insert(
            id,
            PendingUpdateToken {
                token: token.clone(),
                expires_at: Utc::now() + Duration::seconds(IN_MEMORY_UPDATE_TOKEN_TTL_SECS),
            },
        );
        token
    }

    fn validate_and_remove(&self, id: i64, token: &str) -> Result<(), StoreError> {
        let mut tokens = self.tokens.lock().map_err(|_| StoreError::LockPoisoned)?;
        Self::cleanup_expired_locked(&mut tokens);
        let Some(current) = tokens.get(&id) else {
            return Err(StoreError::InvalidUpdateToken);
        };
        if current.token != token {
            return Err(StoreError::InvalidUpdateToken);
        }
        tokens.remove(&id);
        Ok(())
    }

    fn next_token(&self) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let nanos = Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_else(|| Utc::now().timestamp_micros());
        format!("{nanos:x}{sequence:x}")
    }

    fn cleanup_expired_locked(tokens: &mut HashMap<i64, PendingUpdateToken>) {
        let now = Utc::now();
        tokens.retain(|_, pending| pending.expires_at > now);
    }

    #[cfg(test)]
    fn create_expired_for_test(&self, id: i64) -> String {
        let token = self.next_token();
        let mut tokens = self.tokens.lock().expect("更新令牌锁不应损坏");
        tokens.insert(
            id,
            PendingUpdateToken {
                token: token.clone(),
                expires_at: Utc::now() - Duration::seconds(1),
            },
        );
        token
    }
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

fn clean_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{AppError, exit_code_for_error, extract_client_ip};
    use crate::config::ConfigError;
    use axum::http::HeaderMap;
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        process::ExitCode,
    };

    #[test]
    fn exit_code_for_error_should_return_config_code_for_domain_config_errors() {
        let error = AppError::Config(ConfigError::MissingDomainPath);

        let code = exit_code_for_error(&error);

        assert_eq!(code, ExitCode::from(78));
    }

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
        assert!(html.contains("width: min(835px, 100%)"));
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
        assert!(html.contains("https://myip.ipip.net/json"));
        assert!(html.contains("runLatencyRounds({ discardFirstRound: true })"));
        assert!(html.contains("https_jitter_ms"));
        assert!(html.contains("download_mbps"));
        assert!(html.contains("renderHistoryNetwork(record)"));
        assert!(html.contains("搜索域名 / IP / 位置 / 运营商 / 节点"));
        assert!(!html.contains("partial_download_mbps"));
        assert!(!html.contains("final_download_mbps"));
        assert!(!html.contains("https_latency_median_ms"));
        assert!(!html.contains("min ${stats.min_ms"));
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
    fn embedded_index_html_should_show_latency_reachability_summary() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("function renderLatencySummary()"),
            "延迟状态应由统一函数渲染，避免完成态和初始化态文案漂移"
        );
        assert!(
            html.contains("reachableCount"),
            "延迟状态应统计 HTTPS 成功的线路数量"
        );
        assert!(
            html.contains("`共 ${state.targets.length} 条线路，测试成功 ${reachableCount} 条`"),
            "状态栏应展示总线路数和 HTTPS 可通线路数"
        );
        assert!(
            html.contains("els.latencyStatus.textContent = renderLatencySummary();"),
            "配置加载和测试结束后都应刷新统计摘要"
        );
        assert!(
            !html.contains("延迟测试完成"),
            "完成态文案信息量较低，应替换为线路统计摘要"
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
        assert!(!html.contains("class=\"history-status-pill"));
        assert!(html.contains("width: min(980px, calc(100vw - 48px))"));
        assert!(html.contains("height: min(760px, calc(100dvh - 48px))"));
        assert!(html.contains("      box-sizing: border-box;\n      overflow: hidden;"));
        assert!(html.contains("scrollbar-gutter: stable"));
        assert!(html.contains("@media (max-width: 640px)"));
        assert!(html.contains("width: 100vw"));
        assert!(html.contains("height: 100dvh"));
        assert!(html.contains("        scrollbar-gutter: auto;"));
        assert!(html.contains("        border: none;\n      }\n\n      .history-toolbar"));
        assert!(html.contains("opacity: 0;\n      transform: scale(0.96);"));
        assert!(html.contains("transition:\n        opacity .16s ease,\n        transform .18s cubic-bezier(.22, 1, .36, 1);"));
        assert!(html.contains("will-change: transform, opacity"));
        assert!(html.contains("opacity: 1;\n      transform: scale(1);\n      animation: popIn"));
        assert!(html.contains(".modal-overlay.closing"));
        assert!(html.contains("animation: popOut .2s cubic-bezier(.4, 0, .2, 1) both;"));
        assert!(html.contains("@keyframes popIn"));
        assert!(html.contains("@keyframes popOut"));
        assert!(html.contains("animation: popIn 0.3s cubic-bezier(0.18, 0.89, 0.32, 1.28)"));
        assert!(html.contains("transform: scale(0.95)"));
        assert!(html.contains("transform: scale(1)"));
        assert!(html.contains(".modal-overlay:focus"));
        assert!(html.contains("id=\"close-history-btn\""));
        assert!(html.contains("const HISTORY_CLOSE_ANIMATION_MS = 220"));
        assert!(html.contains("els.historyModal.classList.add(\"closing\")"));
        assert!(html.contains("els.closeHistoryBtn.addEventListener"));
        assert!(!html.contains("clip-path: circle"));
        assert!(!html.contains("backdrop-filter"));
        assert!(!html.contains("history-bounce-in"));
        assert!(!html.contains("<button class=\"btn-secondary\" id=\"history-btn\""));
    }

    #[test]
    fn embedded_index_html_should_center_history_close_button_and_add_press_motion() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(".close-btn {\n      display: inline-grid;\n      place-items: center;"),
            "关闭按钮应使用独立居中布局，让 × 位于圆形按钮正中心"
        );
        assert!(
            html.contains(".close-btn:hover:not(:disabled)")
                && html.contains("box-shadow: 0 8px 18px rgba(218, 119, 86, .16);"),
            "关闭按钮需要 hover 状态反馈"
        );
        assert!(
            html.contains(".close-btn:active:not(:disabled)")
                && html.contains("transform: translateY(0) scale(0.94);"),
            "关闭按钮需要点击按压反馈"
        );
    }

    #[test]
    fn embedded_index_html_should_use_five_pixel_padding_in_history_table_cells() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(
                ".history-table th,\n    .history-table td {\n      padding: 5px;\n    }"
            ),
            "测速记录表格的表头、内容和空状态单元格 padding 都应统一为 5px"
        );
    }

    #[test]
    fn embedded_index_html_should_show_history_colo_badge_after_location_and_isp() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("const networkText = [location, cleanText(record.ip_isp)]")
                && html.contains("<span class=\"history-network-meta\">")
                && html.contains("${renderHistoryColoBadge(record)}")
                && html.contains("function renderHistoryColoBadge(record)")
                && html.contains("class=\"target-colo history-network-colo\""),
            "历史记录网络环境应把运营商和 COLO 放在地理位置后面，并复用主页 COLO 标签样式"
        );
        assert!(
            !html.contains("history-network-isp"),
            "历史记录网络环境不应再把运营商和 COLO 单独渲染成第三行"
        );
    }

    #[test]
    fn embedded_index_html_should_use_fixed_widths_for_compact_history_metric_columns() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("<col class=\"history-col-time\">")
                && html.contains("<col class=\"history-col-domain\">")
                && html.contains("<col class=\"history-col-latency\">")
                && html.contains("<col class=\"history-col-speed\">")
                && html.contains("<col class=\"history-col-network\">"),
            "测速记录列宽应通过命名 col 控制，避免继续使用平均百分比分配"
        );
        assert!(
            html.contains(
                ".history-col-time {\n      width: 118px;\n    }\n\n    .history-col-latency {\n      width: 96px;\n    }\n\n    .history-col-speed {\n      width: 112px;\n    }"
            ),
            "时间、HTTPS 和下载速度列应使用更窄的固定宽度"
        );
        assert!(
            !html.contains("<col style=\"width: 16%\">"),
            "测速记录表格不应继续使用旧的百分比列宽"
        );
    }

    #[test]
    fn embedded_index_html_should_hide_internal_domain_key_in_history_rows() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("historyTargetLabel(record)")
                && html.contains("historyTargetHost(record)")
                && !html.contains("escapeHtml(record.domain_key)</span>"),
            "历史表应展示用户可识别的线路名称和域名，不应把内部 domain_key 当作可见信息"
        );
    }

    #[test]
    fn embedded_index_html_should_not_render_history_status_controls() {
        let html = super::embedded_index_html();

        assert!(
            !html.contains("id=\"history-status\"")
                && !html.contains("renderHistoryStatus")
                && !html.contains("historyStatusDescription")
                && !html.contains("params.set(\"status\""),
            "历史记录不应再暴露或查询运行/完成/失败状态"
        );
    }

    #[test]
    fn embedded_index_html_should_randomize_colo_tag_colors_without_gray_palette() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("randomColoPaletteIndex()")
                && html.contains("Math.random()")
                && !html.contains(r##"{ bg: "#f1f4f8", fg: "#506176", ring: "#d8e0ea" }"##)
                && !html.contains("function hashString"),
            "COLO 节点标签颜色应随机分配，且不再使用灰色标签配色"
        );
    }

    #[test]
    fn embedded_index_html_should_support_one_click_download_sequence() {
        let html = super::embedded_index_html();
        let one_click_button = html
            .find("id=\"one-click-download-btn\"")
            .expect("页面应渲染一键测速按钮");
        let refresh_button = html
            .find("id=\"refresh-latency-btn\"")
            .expect("页面应渲染重测延迟按钮");

        assert!(
            one_click_button < refresh_button,
            "一键测速按钮应放在重测延迟左侧"
        );
        assert!(html.contains("一键测速"));
        assert!(html.contains("const ONE_CLICK_DOWNLOAD_TEST_MS = 6000"));
        assert!(html.contains("runOneClickDownloadSequence()"));
        assert!(html.contains("oneClickDownloadTargets()"));
        assert!(html.contains("sortedTargetsForRender()"));
        assert!(html.contains("targetState.status === \"ready\""));
        assert!(html.contains("targetState?.status !== \"failed\""));
        assert!(html.contains("startDownloadTest(target.key, ONE_CLICK_DOWNLOAD_TEST_MS)"));
    }

    #[test]
    fn update_token_store_should_validate_once_and_release_token() {
        let store = super::UpdateTokenStore::default();
        let token = store.create(42);

        assert!(store.validate_and_remove(42, &token).is_ok());
        assert!(store.validate_and_remove(42, &token).is_err());
    }

    #[test]
    fn update_token_store_should_reject_expired_token() {
        let store = super::UpdateTokenStore::default();
        let token = store.create_expired_for_test(42);

        assert!(store.validate_and_remove(42, &token).is_err());
    }
}
