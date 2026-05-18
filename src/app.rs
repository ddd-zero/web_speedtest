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
const CONFIG_PATH_ENV: &str = "WEB_SPEEDTEST_CONFIG";
const LEGACY_CONFIG_PATH_ENV: &str = "WEB_SPEED_CONFIG";

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
    let config_path = resolve_config_path_from_env(|name| std::env::var(name).ok());
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

fn resolve_config_path_from_env(mut read_env: impl FnMut(&str) -> Option<String>) -> PathBuf {
    // 新变量跟随项目名；保留旧变量是为了让已有部署升级后仍能找到配置文件。
    read_env(CONFIG_PATH_ENV)
        .or_else(|| read_env(LEGACY_CONFIG_PATH_ENV))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"))
}

async fn index() -> Html<String> {
    Html(embedded_index_html())
}

async fn client_config(State(state): State<AppState>) -> Json<ClientConfig> {
    Json(ClientConfig {
        targets: state.config.targets.clone(),
        test: state.config.test.clone(),
        history: state.config.history.clone(),
    })
}

pub fn embedded_index_html() -> String {
    include_str!("../frontend/index.html").replace("__APP_VERSION__", env!("CARGO_PKG_VERSION"))
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
        path::PathBuf,
        process::ExitCode,
    };

    #[test]
    fn exit_code_for_error_should_return_config_code_for_domain_config_errors() {
        let error = AppError::Config(ConfigError::MissingDomainPath);

        let code = exit_code_for_error(&error);

        assert_eq!(code, ExitCode::from(78));
    }

    #[test]
    fn config_path_should_prefer_current_project_env_var() {
        let path = super::resolve_config_path_from_env(|name| match name {
            "WEB_SPEEDTEST_CONFIG" => Some("new-config.toml".to_string()),
            "WEB_SPEED_CONFIG" => Some("legacy-config.toml".to_string()),
            _ => None,
        });

        assert_eq!(path, PathBuf::from("new-config.toml"));
    }

    #[test]
    fn config_path_should_fallback_to_legacy_env_var() {
        let path = super::resolve_config_path_from_env(|name| match name {
            "WEB_SPEED_CONFIG" => Some("legacy-config.toml".to_string()),
            _ => None,
        });

        assert_eq!(path, PathBuf::from("legacy-config.toml"));
    }

    #[test]
    fn config_path_should_use_default_config_when_env_vars_are_missing() {
        let path = super::resolve_config_path_from_env(|_| None);

        assert_eq!(path, PathBuf::from("config.toml"));
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

        assert!(html.contains("网络测速"));
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
        assert!(html.contains("搜索历史 空格分隔 IP 位置 运营商 节点"));
        assert!(!html.contains("partial_download_mbps"));
        assert!(!html.contains("final_download_mbps"));
        assert!(!html.contains("https_latency_median_ms"));
        assert!(!html.contains("min ${stats.min_ms"));
    }

    #[test]
    fn embedded_index_html_should_show_cargo_package_version_badge_near_title() {
        let html = super::embedded_index_html();
        let version = env!("CARGO_PKG_VERSION");

        assert!(
            html.contains("<h1>网络测速\n            <span class=\"version-badge\""),
            "版本徽标应紧贴主标题显示"
        );
        assert!(
            html.contains(&format!("aria-label=\"当前版本 {version}\"")),
            "版本徽标应提供完整可访问语义"
        );
        assert!(
            html.contains(&format!(">v{version}</span>")),
            "版本徽标应显示 Cargo 包版本"
        );
        assert!(
            html.contains(".version-badge {\n      display: inline-flex;"),
            "版本徽标应使用轻量 pill 样式"
        );
        assert!(
            !html.contains("__APP_VERSION__"),
            "页面输出不应泄露版本占位符"
        );
    }

    #[test]
    fn embedded_index_html_should_offer_main_page_privacy_toggle() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("id=\"privacy-toggle-btn\""),
            "主页顶部应提供隐私模式按钮"
        );
        assert!(
            html.contains("class=\"icon-btn privacy-toggle-btn\""),
            "隐私按钮应使用紧凑图标按钮样式，避免挤占主操作按钮"
        );
        assert!(
            html.contains("aria-pressed=\"false\""),
            "隐私按钮应通过 aria-pressed 暴露开关状态"
        );
        assert!(
            html.contains("privacyBtn: document.getElementById(\"privacy-toggle-btn\"),"),
            "脚本应缓存隐私按钮元素"
        );
        assert!(
            html.contains("isPrivacyMode: false"),
            "隐私模式默认应关闭，避免用户看到的主页面信息被意外脱敏"
        );
        assert!(
            html.contains("els.privacyBtn.addEventListener(\"click\", togglePrivacyMode);"),
            "点击隐私按钮应切换隐私模式"
        );
        assert!(
            html.contains("function togglePrivacyMode()"),
            "隐私模式切换逻辑应集中封装，避免散落在事件回调里"
        );
        assert!(
            html.contains(
                "els.privacyBtn.setAttribute(\"aria-pressed\", String(state.isPrivacyMode));"
            ),
            "切换后应同步按钮可访问状态"
        );
        assert!(
            html.contains("updateClientInfo();\n      renderTargetList();"),
            "切换隐私模式后应刷新用户 IP 和域名延迟列表"
        );
    }

    #[test]
    fn embedded_index_html_should_mask_home_ip_and_target_domains_in_privacy_mode() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("function formatClientIpForDisplay(value)")
                && html.contains("function maskIpForPrivacy(value)")
                && html.contains("function maskIpv4ForPrivacy(value)")
                && html.contains("function maskIpv6ForPrivacy(value)"),
            "用户 IP 应通过独立展示函数脱敏，保留原始 state 数据"
        );
        assert!(
            html.contains("return `${parts[0]}.${parts[1]}.*.*`;"),
            "IPv4 隐私展示应只保留前 16 位"
        );
        assert!(
            html.contains(
                "return `${Number.parseInt(segments[0], 16).toString(16)}:${Number.parseInt(segments[1], 16).toString(16)}:${Number.parseInt(segments[2], 16).toString(16)}::*`;"
            ),
            "IPv6 隐私展示应按 /48 保留前三段"
        );
        assert!(
            html.contains(
                "els.clientIp.textContent = formatClientIpForDisplay(state.currentClientIp);"
            ),
            "用户 IP 渲染路径应接入隐私展示函数"
        );
        assert!(
            html.contains("function formatTargetLabelForDisplay(target)")
                && html.contains("function formatTargetHostForDisplay(host)")
                && html.contains("function maskDomainForPrivacy(value)")
                && html.contains("function maskDomainLabel(label)"),
            "域名延迟对比应通过独立展示函数脱敏线路标签和主机名"
        );
        assert!(
            html.contains("${escapeHtml(formatTargetLabelForDisplay(target))}")
                && html.contains("${escapeHtml(formatTargetHostForDisplay(target.host))}"),
            "主页面线路渲染应使用脱敏后的标签和主机名"
        );
    }

    #[test]
    fn embedded_index_html_should_mask_last_client_location_part_in_privacy_mode() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("function formatClientLocationForDisplay()")
                && html.contains("function maskClientLocationForPrivacy(parts)"),
            "用户位置应通过独立展示函数脱敏，保留原始 state 数据"
        );
        assert!(
            html.contains("els.clientLocation.textContent = formatClientLocationForDisplay();"),
            "用户位置渲染路径应接入隐私展示函数"
        );
        assert!(
            html.contains("return [...locationParts.slice(0, -1), \"***\"].join(\" \");"),
            "隐私模式应直接把位置最后一段替换成 ***"
        );
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
    fn embedded_index_html_should_allow_root_pull_refresh_while_layout_stays_viewport_sized() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(
                "html {\n      min-height: 100%;\n      scrollbar-gutter: stable;\n    }"
            ),
            "根元素不应锁死滚动，应保留下拉刷新需要的顶层滚动链路"
        );
        assert!(
            html.contains(
                "body {\n      height: 100dvh;\n      margin: 0;\n      padding: 22px;\n      box-sizing: border-box;\n      overflow-x: hidden;"
            ),
            "body 应继续按动态视口精确计算高度，但只限制横向溢出"
        );
        assert!(
            !html.contains("html {\n      height: 100%;\n      overflow: hidden;"),
            "html 不应继续使用 overflow hidden 阻断移动端下拉刷新"
        );
        assert!(
            !html.contains(
                "body {\n      height: 100dvh;\n      margin: 0;\n      padding: 22px;\n      box-sizing: border-box;\n      overflow: hidden;"
            ),
            "body 不应继续锁定纵向滚动"
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
        assert!(html.contains("width: min(835px, calc(100vw - 48px))"));
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
    fn embedded_index_html_should_lock_history_fab_visual_width_with_border_box() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(
                ".history-fab {\n      position: fixed;\n      right: 20px;\n      top: 65%;"
            ),
            "测试应定位到测速记录浮动按钮样式"
        );
        assert!(
            html.contains("      width: 48px;\n      min-width: 48px;\n      max-width: 48px;"),
            "测速记录浮动按钮应使用固定视觉宽度，避免不同手机字体度量撑宽"
        );
        assert!(
            html.contains("      box-sizing: border-box;"),
            "固定宽度应包含内边距"
        );
        assert!(
            html.contains("      padding: 14px 8px;"),
            "固定宽度后应收窄横向内边距，保留舒适触控面积"
        );
        assert!(
            !html.contains(
                "      min-width: 46px;\n      min-height: 118px;\n      padding: 14px 10px;"
            ),
            "不应继续用内容最小宽度叠加 padding 的方式决定视觉宽度"
        );
    }

    #[test]
    fn embedded_index_html_should_stack_history_fab_label_without_writing_mode() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("<span class=\"history-fab-label\" aria-hidden=\"true\">")
                && html.contains("<span>测</span>")
                && html.contains("<span>速</span>")
                && html.contains("<span>记</span>")
                && html.contains("<span>录</span>"),
            "测速记录浮动按钮应使用显式逐字堆叠的视觉标签"
        );
        assert!(
            html.contains(
                ".history-fab-label {\n      display: inline-flex;\n      flex-direction: column;"
            ),
            "逐字标签应通过 flex 纵向排列，避免依赖 writing-mode"
        );
        assert!(
            html.contains(".history-fab-label span {\n      display: block;\n    }"),
            "每个中文字应作为独立块级单元参与布局"
        );
        assert!(
            html.contains("aria-label=\"打开测速记录\"") && html.contains("aria-hidden=\"true\""),
            "视觉逐字标签应对读屏隐藏，由按钮 aria-label 提供完整语义"
        );
        assert!(
            !html.contains("writing-mode: vertical-rl;") && !html.contains(">测速记录</button>"),
            "不应继续依赖原生 button 文本的 writing-mode 竖排"
        );
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
    fn embedded_index_html_should_suppress_mobile_button_tap_highlight_and_keep_keyboard_focus() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("-webkit-tap-highlight-color: transparent;"),
            "移动端按钮应禁用系统蓝色触摸高亮，避免点击后出现选中提示"
        );
        assert!(
            html.contains("button:focus:not(:focus-visible) {\n      outline: none;\n    }"),
            "指针点击触发的按钮焦点不应显示默认轮廓"
        );
        assert!(
            html.contains("button:focus-visible")
                && html.contains("outline: 3px solid rgba(218, 119, 86, .48);"),
            "键盘导航仍需要保留符合页面主题色的可见焦点"
        );
    }

    #[test]
    fn embedded_index_html_should_mirror_hover_feedback_for_touch_active_buttons() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(".btn-primary:hover:not(:disabled),")
                && html.contains(".btn-primary:active:not(:disabled),")
                && html.contains("background: var(--accent-hover);"),
            "主按钮在移动端按下时应复用桌面 hover 的变暗反馈"
        );
        assert!(
            html.contains(".history-fab:hover:not(:disabled),")
                && html.contains(".history-fab:active:not(:disabled),")
                && html.contains("transform: translateY(-50%) translateX(-3px);"),
            "测速记录浮动按钮在触摸按下时应复用 hover 的变暗反馈"
        );
        assert!(
            html.contains(".close-btn:hover:not(:disabled),")
                && html.contains(".close-btn:active:not(:disabled),")
                && html.contains("color: var(--accent);"),
            "关闭按钮在触摸按下时应复用 hover 的颜色反馈"
        );
    }

    #[test]
    fn embedded_index_html_should_play_click_feedback_after_touch_release() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("const BUTTON_CLICK_FEEDBACK_MS = 160"),
            "点击反馈应使用短时常量，避免弹窗切换吞掉点击动画"
        );
        assert!(
            html.contains("button.click-feedback:not(:disabled)")
                && html.contains("@keyframes buttonClickFeedback"),
            "按钮点击后应通过独立 click-feedback 动画呈现松手后的反馈"
        );
        assert!(
            html.contains("document.addEventListener(\"click\", event => {")
                && html.contains("playButtonClickFeedback(button);")
                && html.contains("}, true);"),
            "所有按钮点击都应在捕获阶段先播放点击反馈"
        );
        assert!(
            html.contains(
                "els.historyBtn.addEventListener(\"click\", () => runAfterButtonClickFeedback(openHistory));"
            ) && html.contains(
                "els.closeHistoryBtn.addEventListener(\"click\", () => runAfterButtonClickFeedback(() => toggleHistory(false)));"
            ),
            "打开和关闭测速记录应等待点击反馈可见后再切换弹窗"
        );
    }

    #[test]
    fn embedded_index_html_should_delay_dynamic_download_button_action_for_click_feedback() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(r#"onclick="startDownloadTestAfterClick('${escapeHtml(target.key)}')""#),
            "动态渲染的开始测速按钮应先播放点击反馈，再触发测速动作"
        );
        assert!(
            html.contains(
                "function startDownloadTestAfterClick(targetKey) {\n      runAfterButtonClickFeedback(() => startDownloadTest(targetKey));\n    }"
            ),
            "开始测速的延迟包装应复用统一点击反馈时长"
        );
        assert!(
            !html.contains(r#"onclick="startDownloadTest('${escapeHtml(target.key)}')""#),
            "开始测速按钮不应直接触发会立即重渲染列表的测速动作"
        );
    }

    #[test]
    fn embedded_index_html_should_submit_history_search_form_on_enter() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("<form class=\"history-toolbar\" id=\"history-form\">"),
            "测速记录筛选区应使用表单语义，让输入框回车可以提交搜索"
        );
        assert!(
            html.contains("<button class=\"btn-primary\" id=\"query-history-btn\" type=\"submit\">查询</button>"),
            "查询按钮应作为表单提交按钮，和回车搜索复用同一逻辑"
        );
        assert!(
            html.contains("historyForm: document.getElementById(\"history-form\"),"),
            "脚本应缓存历史搜索表单元素"
        );
        assert!(
            html.contains("els.historyForm.addEventListener(\"submit\", event => {")
                && html.contains("event.preventDefault();")
                && html.contains("loadHistory();"),
            "历史搜索表单提交时应阻止页面刷新并执行搜索"
        );
        assert!(
            !html.contains("els.queryHistoryBtn.addEventListener(\"click\", loadHistory);"),
            "不应只依赖查询按钮 click，否则输入框回车不会搜索"
        );
    }

    #[test]
    fn embedded_index_html_should_use_ten_pixel_padding_in_history_table_cells() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(
                ".history-table th,\n    .history-table td {\n      padding: 10px;\n    }"
            ),
            "测速记录表格的表头、内容和空状态单元格 padding 都应统一为 10px"
        );
    }

    #[test]
    fn embedded_index_html_should_label_and_space_history_source_column_semantically() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("<th class=\"col-network\">访问来源</th>")
                && html.contains("<td class=\"col-network\">${renderHistoryNetwork(record)}</td>"),
            "历史记录访问来源列应使用贴近内容的表头，并通过语义化 class 标记"
        );
        assert!(
            html.contains(".history-table .col-network {\n      padding-left: 13px;\n    }"),
            "访问来源列应通过专属 class 增加左侧留白，拉开与下载速度列的距离"
        );
    }

    #[test]
    fn embedded_index_html_should_show_history_colo_badge_after_location_and_isp() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("const isp = cleanText(record.ip_isp);")
                && html.contains("const networkText = location && isp ? `${location} · ${isp}` : location || isp;")
                && html.contains("<span class=\"history-network-meta\">")
                && html.contains("${renderHistoryColoBadge(record)}")
                && html.contains("function renderHistoryColoBadge(record)")
                && html.contains("class=\"target-colo history-network-colo\""),
            "历史记录访问来源应把运营商用中点接在地理位置后面，并复用主页 COLO 标签样式"
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
                ".history-col-time {\n      width: 118px;\n    }\n\n    .history-col-latency {\n      width: 96px;\n    }\n\n    .history-col-speed {\n      width: 100px;\n    }"
            ),
            "时间、HTTPS 和下载速度列应使用更窄的固定宽度"
        );
        assert!(
            !html.contains("<col style=\"width: 16%\">"),
            "测速记录表格不应继续使用旧的百分比列宽"
        );
    }

    #[test]
    fn embedded_index_html_should_limit_native_history_domain_select_width() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("grid-template-columns: minmax(150px, 240px) minmax(220px, 1fr) auto;"),
            "历史筛选栏应限制域名下拉框宽度，让搜索框承接剩余空间"
        );
        assert!(
            html.contains("box-sizing: border-box;")
                && html.contains("width: 100%;")
                && html.contains("min-width: 0;"),
            "筛选控件应在 grid 轨道内收缩，避免 Firefox 原生 select 撑出多余空白"
        );
        assert!(
            html.contains(".search-select {\n      appearance: none;")
                && html.contains("background-position:\n        calc(100% - 18px) 50%,\n        calc(100% - 12px) 50%;"),
            "域名下拉框应自绘箭头，减少 Chrome 与 Firefox 原生外观差异"
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
