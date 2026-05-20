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
    config::{
        ConfigError, DisplaySettings, HistorySettings, RuntimeConfig, TestSettings, TestTarget,
    },
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
    pub display: DisplaySettings,
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
        display: state.config.display.clone(),
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
    fn client_config_should_include_display_settings() {
        let payload = super::ClientConfig {
            targets: Vec::new(),
            test: crate::config::TestSettings::default(),
            history: crate::config::HistorySettings::default(),
            display: crate::config::DisplaySettings {
                show_domains: false,
            },
        };

        let json = serde_json::to_value(payload).expect("client config should serialize");

        assert_eq!(json["display"]["show_domains"], false);
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
        assert!(!html.contains("renderDownloadStateLabel"));
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
        assert!(html.contains("/cdn-cgi/trace"));
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
    fn embedded_index_html_should_compare_cloudflare_trace_client_ip() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("const CLOUDFLARE_TRACE_URL = \"/cdn-cgi/trace\";"),
            "主页应从当前站点同源的 /cdn-cgi/trace 读取 Cloudflare Trace 信息"
        );
        assert!(
            html.contains("id=\"client-ip-mismatch-alert\" hidden")
                && html.contains(
                    "clientIpMismatchAlert: document.getElementById(\"client-ip-mismatch-alert\"),"
                )
                && html.contains(".client-ip-mismatch-alert {")
                && html.contains(".client-ip-mismatch-alert[hidden] {"),
            "用户信息和线路标题之间应预留一个默认隐藏的红色异常标识"
        );
        assert!(
            html.contains("currentClientIpMismatch: false")
                && html.contains("function updateClientIpMismatchAlert()")
                && html
                    .contains("els.clientIpMismatchAlert.hidden = !state.currentClientIpMismatch;"),
            "IP 来源不一致状态应集中保存，并同步到异常标识显示状态"
        );
        assert!(
            html.contains("async function fetchCloudflareTraceInfo()")
                && html.contains(
                    "const response = await fetch(CLOUDFLARE_TRACE_URL, { cache: \"no-store\" });"
                )
                && html.contains("function parseCloudflareTrace(text)")
                && html.contains("const separatorIndex = line.indexOf(\"=\");")
                && html.contains("ip: cleanText(fields.ip),")
                && html.contains("loc: cleanText(fields.loc),"),
            "Cloudflare Trace 文本应按 key=value 解析出 ip 和 loc"
        );
        assert!(
            html.contains("function shouldUseCloudflareTraceInfo(publicIp, traceIp)")
                && html.contains("function ipv4PrefixForComparison(value)")
                && html.contains("return parts.slice(0, 2).join(\".\");")
                && html.contains(
                    "return Boolean(publicPrefix && tracePrefix && publicPrefix !== tracePrefix);"
                ),
            "只有双方都是 IPv4 且前两个段不一致时，才应判定为同用户 IP 不一致"
        );
        assert!(
            html.contains("fetchPublicIpInfo().catch(() => null),")
                && html.contains("fetchCloudflareTraceInfo().catch(() => null),")
                && html.contains("applyClientInfo(publicInfo, traceInfo);"),
            "IPIP 和 Cloudflare Trace 查询应互不阻塞，最后统一合并展示结果"
        );
        assert!(
            html.contains("const shouldUseTraceInfo = shouldUseCloudflareTraceInfo(publicInfo?.ip, traceInfo?.ip);")
                && html.contains("state.currentClientIpMismatch = shouldUseTraceInfo;")
                && html.contains("state.currentClientIp = traceInfo.ip;")
                && html.contains("country: traceInfo.loc,")
                && html.contains("isp: \"\","),
            "IPv4 前缀不一致时，应切换主页用户 IP 和位置为 Cloudflare Trace 来源"
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
    fn embedded_index_html_should_keep_compact_target_rows_stable_at_breakpoint() {
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
            "640px 是顶部说明开始换行的位置，应切到手机紧凑布局"
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
    fn embedded_index_html_should_use_two_row_target_cards_on_small_screens() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("class=\"target-main\""),
            "测速卡片应给线路信息区独立类名，便于窄屏布局精确控制"
        );
        assert!(
            html.contains(
                ".target-row {\n        grid-template-columns: minmax(0, 1fr) minmax(82px, .8fr) 86px;\n        gap: 6px 8px;\n        align-items: stretch;\n        padding: 8px;"
            ),
            "640px 以下测速卡片应压缩为两行：线路信息一行，延迟/下载/操作一行"
        );
        assert!(
            html.contains(".target-main {\n        grid-column: 1 / -1;"),
            "线路信息区应横跨整行，给长域名保留完整可省略空间"
        );
        assert!(
            html.contains(
                ".target-main {\n        grid-column: 1 / -1;\n        display: grid;\n        grid-template-columns: minmax(0, 1fr) minmax(0, max-content);"
            ),
            "窄屏线路信息区应把 CNAME 放到线路名同一行并按内容自适应右对齐"
        );
        assert!(
            html.contains(
                ".target-cname {\n        grid-column: 2;\n        grid-row: 1;\n        justify-self: end;"
            ),
            "窄屏 CNAME 应位于线路名右侧"
        );
        assert!(
            html.contains("max-width: min(34vw, 168px);") && html.contains("font-size: .58rem;"),
            "窄屏 CNAME 应进一步缩小字号和最大宽度，节省水平空间"
        );
        assert!(
            html.contains(".target-host-text {\n        grid-column: 1;\n        grid-row: 2;"),
            "窄屏主域名文本应留在左列，避免和右侧 CNAME 或节点徽标重叠"
        );
        assert!(
            html.contains(
                ".target-row button {\n        min-height: 38px;\n        padding: 8px 10px;"
            ),
            "窄屏操作按钮应降低高度但保留基本触控面积"
        );
        assert!(
            html.contains(".metric {\n        align-items: flex-start;\n        text-align: left;")
                && html.contains(".download-metric {\n        align-items: flex-end;\n        text-align: right;")
                && html.contains(
                    ".target-speed-value,\n      .target-download-status {\n        margin-left: auto;\n        margin-right: 0;\n        text-align: right;"
                ),
            "窄屏下载速度区域应保持右对齐，避免从旧版右贴齐表现退化为左对齐"
        );
        assert!(
            html.contains(".target-main {\n        grid-column: 1 / -1;\n        display: grid;")
                && html.contains("gap: 2px 8px;")
                && html.contains(
                    ".target-host {\n        display: contents;\n        font-size: .72rem;"
                ),
            "窄屏域名辅助信息应收紧间距和字号"
        );
    }

    #[test]
    fn embedded_index_html_should_render_cname_without_visual_prefix_and_with_more_room() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(
                ".target-host {\n      display: grid;\n      grid-template-columns: max-content minmax(0, max-content) minmax(0, 1fr) max-content;"
            ),
            "桌面端域名行应优先给主域名完整宽度，CNAME 自适应，地区固定在右侧"
        );
        assert!(
            html.contains("max-width: min(32ch, 100%);")
                && html.contains("justify-self: start;")
                && !html.contains("flex: 0 1 240px;")
                && !html.contains("max-width: 240px;"),
            "桌面端 CNAME 应按内容自适应，不应继续占固定长槽"
        );
        assert!(
            html.contains("min-height: 17px;")
                && html.contains(".target-host {\n      display: grid;")
                && html.contains(".target-colo {\n      grid-column: 4;")
                && html.contains("box-sizing: border-box;")
                && html.contains("line-height: 1.15;"),
            "节点徽标从无到有时应预留稳定高度，避免 HTTPS 预热完成后线路内容上下抖动"
        );
        assert!(
            html.contains(
                ".target-host-text {\n      grid-column: 1;\n      max-width: 100%;\n      line-height: 17px;"
            ),
            "主域名文本也应使用稳定行高，避免窄屏 display: contents 时父级最小高度失效"
        );
        assert!(
            html.contains(
                "return `<span class=\"target-cname\" title=\"${escapeHtml(displayCname)}\" aria-label=\"CNAME ${escapeHtml(displayCname)}\">${escapeHtml(displayCname)}</span>`;"
            ),
            "CNAME 徽标视觉文本应只显示域名本身"
        );
        assert!(
            !html.contains(">CNAME ${escapeHtml(displayCname)}</span>"),
            "CNAME 徽标不应继续在视觉文本前显示 CNAME 前缀"
        );
    }

    #[test]
    fn embedded_index_html_should_not_privacy_mask_cname_badges() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("const displayCname = cleanText(cname);"),
            "CNAME 只用于辅助诊断展示，不应复用主域名隐私脱敏函数"
        );
        assert!(
            !html.contains("const displayCname = formatTargetHostForDisplay(cname);"),
            "CNAME 不应在隐私模式下被 maskDomainForPrivacy 处理"
        );
        assert!(
            html.contains("${renderCnameBadge(target.cname)}"),
            "测速列表仍应渲染 CNAME 徽标"
        );
    }

    #[test]
    fn embedded_index_html_should_hide_domain_text_when_config_disables_domains() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("display: {\n        show_domains: true,\n      },"),
            "前端状态应默认显示域名，保持旧配置行为"
        );
        assert!(
            html.contains("state.display = {\n          ...state.display,\n          ...(config.display || {}),\n        };"),
            "前端应从 /api/config 合并 display 设置"
        );
        assert!(
            html.contains("function shouldShowDomains()")
                && html.contains("function hiddenDomainText() {\n      return \"-\";\n    }"),
            "域名可见性判断和占位符应集中封装"
        );
        assert!(
            html.contains("if (!shouldShowDomains()) return hiddenDomainText();"),
            "主页域名和历史域名值应在关闭显示时返回 -"
        );
        assert!(
            html.contains("return formatHistoryTargetLabelForDisplay(target, record);")
                && html.contains("function formatHistoryTargetLabelForDisplay(target, record)")
                && html.contains("looksLikeDomainLabel(label, target?.host || record?.domain)"),
            "历史记录应保留非域名线路名，只隐藏看起来像域名的线路名"
        );
        assert!(
            html.contains("if (!shouldShowDomains()) return \"\";")
                && html.contains("function renderCnameBadge(cname)"),
            "关闭域名显示时主页不应继续显示 CNAME"
        );
        assert!(
            html.contains("els.historyDomain.disabled = false;")
                && html.contains("function historyDomainOptionLabel(target, index)")
                && html.contains("const fallbackLabel = `线路 ${index + 1}`;")
                && html.contains("return shouldShowDomains() ? `${label} / ${host}` : label;"),
            "关闭域名显示时历史筛选下拉仍应保留线路名选择能力，但不能直接显示域名"
        );
    }

    #[test]
    fn embedded_index_html_should_keep_colo_badge_right_aligned_without_stretching() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(".target-host-text {\n      grid-column: 1;\n      max-width: 100%;"),
            "主域名应固定在域名行第一列，避免被 CNAME 或地区抢占优先级"
        );
        assert!(
            html.contains(".target-cname {\n      display: inline-flex;")
                && html.contains("grid-column: 2;"),
            "CNAME 应固定在主域名之后的自适应列"
        );
        assert!(
            html.contains(".target-colo {\n      grid-column: 4;")
                && html.contains("justify-self: end;")
                && html.contains("width: max-content;"),
            "地区徽标应固定在右侧内容宽度列，不能被拉伸或跟随主域名"
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
        assert!(html.contains("opacity .2s ease,\n        visibility 0s linear .2s"));
        assert!(html.contains(".modal-overlay.active .modal-content {\n      opacity: 1;\n      transform: scale(1);\n    }"));
        assert!(
            html.contains(".modal-overlay.is-entering .modal-content {\n      animation: popIn")
        );
        assert!(
            html.contains(".modal-overlay.is-leaving .modal-content {\n      animation: popOut")
        );
        assert!(html.contains("@keyframes popIn"));
        assert!(html.contains("@keyframes popOut"));
        assert!(!html.contains(".modal-overlay.active .modal-content {\n      opacity: 1;\n      transform: scale(1);\n      animation:"));
        assert!(!html.contains("historyDialogIn"));
        assert!(!html.contains("historyDialogOut"));
        assert!(html.contains("transform: scale(0.96)"));
        assert!(html.contains("transform: scale(1)"));
        assert!(html.contains("@media (prefers-reduced-motion: reduce)"));
        assert!(html.contains(".modal-overlay:focus"));
        assert!(html.contains("id=\"close-history-btn\""));
        assert!(html.contains("const HISTORY_OPEN_ANIMATION_MS = 300"));
        assert!(html.contains("const HISTORY_CLOSE_ANIMATION_MS = 200"));
        assert!(!html.contains("historyOpenTimer"));
        assert!(!html.contains("historyCloseTimer"));
        assert!(html.contains("let historyAnimationTimer = 0"));
        assert!(html.contains("els.historyModal.classList.add(\"is-entering\")"));
        assert!(html.contains("els.historyModal.classList.add(\"is-leaving\")"));
        assert!(html.contains("finishHistoryAnimation(\"is-entering\""));
        assert!(html.contains("finishHistoryAnimation(\"is-leaving\""));
        assert!(!html.contains("els.historyModal.classList.add(\"closing\")"));
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
            html.contains("document.body.classList.toggle(\"keyboard-focus\", true);")
                && html.contains("document.body.classList.toggle(\"keyboard-focus\", false);")
                && html.contains(
                    "body:not(.keyboard-focus) button:focus-visible {\n      outline: none;\n    }"
                ),
            "指针点击触发的按钮焦点不应显示默认轮廓，避免移动端出现椭圆粗线"
        );
        assert!(
            html.contains("body.keyboard-focus button:focus")
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
            html.contains("els.historyBtn.addEventListener(\"click\", openHistory);")
                && html.contains(
                    "els.closeHistoryBtn.addEventListener(\"click\", () => toggleHistory(false));"
                )
                && !html.contains("runAfterButtonClickFeedback(() => toggleHistory(false))"),
            "打开和关闭测速记录都应立即切换弹窗，避免移动端把按钮反馈误看成弹窗闪烁"
        );
    }

    #[test]
    fn embedded_index_html_should_start_dynamic_download_button_action_immediately() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(r#"onclick="startDownloadTest('${escapeHtml(target.key)}')""#),
            "动态渲染的测速按钮应立即触发测速动作，避免开始和停止测速慢半拍"
        );
        assert!(
            !html.contains("function startDownloadTestAfterClick(targetKey)")
                && !html.contains(
                    r#"onclick="startDownloadTestAfterClick('${escapeHtml(target.key)}')""#
                ),
            "测速按钮不应再通过点击反馈定时器延迟真实动作"
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
    fn embedded_index_html_should_offer_memory_backed_history_group_and_speed_sort_controls() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("const HISTORY_COLLAPSE_STORAGE_KEY = \"web-speed-history-collapse\";")
                && html.contains(
                    "const HISTORY_SPEED_SORT_STORAGE_KEY = \"web-speed-history-speed-sort\";"
                ),
            "测速记录折叠和下载速度排序偏好应使用独立 localStorage key 记忆"
        );
        assert!(
            html.contains(
                "historyCollapseEnabled: readHistoryPreference(HISTORY_COLLAPSE_STORAGE_KEY),"
            ) && html.contains(
                "historySpeedSortEnabled: readHistoryPreference(HISTORY_SPEED_SORT_STORAGE_KEY),"
            ),
            "前端状态应从本地偏好恢复折叠和排序开关"
        );
        assert!(
            html.contains(
                "<button class=\"btn-secondary history-control-btn history-icon-btn\" id=\"history-collapse-toggle\" type=\"button\""
            ) && html.contains(
                "<button class=\"history-speed-sort-btn\" id=\"history-speed-sort-btn\" type=\"button\" aria-pressed=\"false\""
            ),
            "历史记录应提供折叠同 IP 按钮，并把下载速度表头设计成排序按钮"
        );
        assert!(
            html.contains("els.historyCollapseToggle.addEventListener(\"click\", toggleHistoryCollapse);")
                && html.contains(
                    "els.historySpeedSortBtn.addEventListener(\"click\", toggleHistorySpeedSort);"
                )
                && html.contains(
                    "writeHistoryPreference(HISTORY_COLLAPSE_STORAGE_KEY, state.historyCollapseEnabled);"
                )
                && html.contains(
                    "writeHistoryPreference(HISTORY_SPEED_SORT_STORAGE_KEY, state.historySpeedSortEnabled);"
                ),
            "折叠和排序按钮应只切换前端状态、重新渲染当前查询结果，并记忆开关偏好"
        );
    }

    #[test]
    fn embedded_index_html_should_group_history_records_by_client_ip_and_sort_by_speed() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("historyRecords: [],")
                && html.contains("expandedHistoryGroups: new Set(),")
                && html.contains("state.historyRecords = records;")
                && html.contains("renderHistory();"),
            "查询返回的记录应只保存在当前页面内存中，后续折叠和排序复用这次查询结果"
        );
        assert!(
            html.contains("function historyRowsForRender()")
                && html.contains("function groupHistoryRecordsByClientIp(records)")
                && html.contains("const key = historyClientGroupKey(record);")
                && html.contains("bestRecord: bestHistoryRecord(groupRecords),"),
            "折叠模式应按后端返回的脱敏 client_ip 分组，并用组内最快记录作为外层展示"
        );
        assert!(
            html.contains("function sortHistoryRecordsBySpeed(records)")
                && html.contains("function historyDownloadSpeed(record)")
                && html.contains("return Number.isFinite(speed) ? speed : 0;")
                && html.contains(
                    "state.historySpeedSortEnabled ? sortHistoryGroupsBySpeed(groups) : groups;"
                ),
            "下载速度排序应复用同一套速度解析逻辑，并且在折叠模式下按组内最高速度排序"
        );
        assert!(
            html.contains("data-history-group-toggle")
                && html.contains("function toggleHistoryGroup(groupKey)")
                && html.contains(
                    "group.records.forEach(record => pushHistoryRow(record, { isChild: true }));"
                ),
            "折叠后的分组行应支持点击展开，并展示该 IP 下的全部测速记录"
        );
        assert!(
            !html.contains("localStorage.setItem(\"historyRecords\"")
                && !html.contains("localStorage.setItem(\"records\""),
            "查询结果不能持久化到 localStorage，只允许记忆排序和折叠开关"
        );
    }

    #[test]
    fn embedded_index_html_should_render_icon_only_history_collapse_control() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("class=\"btn-secondary history-control-btn history-icon-btn\"")
                && html.contains("aria-label=\"折叠同 IP\"")
                && html.contains("class=\"history-collapse-icon\"")
                && html.contains(
                    "<rect x=\"5\" y=\"4.5\" width=\"14\" height=\"3\" rx=\"1.3\"></rect>"
                )
                && html.contains(
                    "<rect x=\"5\" y=\"10.5\" width=\"14\" height=\"3\" rx=\"1.3\"></rect>"
                )
                && html.contains(
                    "<rect x=\"5\" y=\"16.5\" width=\"14\" height=\"3\" rx=\"1.3\"></rect>"
                ),
            "折叠同 IP 控制应使用固定尺寸的水平三层图标按钮，避免文字变化导致宽度抖动"
        );
        assert!(
            html.contains(
                "els.historyCollapseToggle.setAttribute(\"aria-label\", historyCollapseLabel);"
            ) && html.contains(
                "els.historyCollapseToggle.setAttribute(\"title\", historyCollapseLabel);"
            ),
            "折叠按钮应通过可访问名称和 title 表达状态，而不是切换可见文字"
        );
        assert!(
            !html.contains("els.historyCollapseToggle.textContent"),
            "折叠按钮不应通过 textContent 在折叠/取消折叠之间切换文案"
        );
    }

    #[test]
    fn embedded_index_html_should_paint_history_rows_across_grid_gaps_and_keep_scrollbar_stable() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(".table-container {\n      flex: 1;\n      overflow: auto;\n      scrollbar-gutter: stable;"),
            "历史表格容器应预留滚动条槽位，避免展开后出现滚动条导致列宽重算抖动"
        );
        assert!(
            html.contains(".history-table tbody .history-record-row::before")
                && html.contains("grid-column: 1 / -1;")
                && html.contains("grid-row: var(--history-row);")
                && html.contains(".history-table tbody .history-record-row > td {\n      grid-row: var(--history-row);"),
            "历史记录每行应绘制跨越所有列的底色层，覆盖列间空白，避免颜色断层"
        );
        assert!(
            html.contains(".history-table tr > :nth-child(1) {\n      grid-column: 1;")
                && html.contains(".history-table tr > :nth-child(5) {\n      grid-column: 5;"),
            "历史表格在显式指定 grid-row 后，也必须固定每个单元格的 grid-column，避免自动流排到隐式列"
        );
        assert!(
            html.contains(".history-child-row {\n      --history-row-bg: #f3f4f6;")
                && html.contains("--history-row-hover-bg: #eceff3;")
                && html.contains(".history-group-row:hover,")
                && html.contains(".history-group-row:has(> td:hover) {")
                && html.contains("--history-row-bg: #fff4ed;")
                && html.contains("--history-row-ring: rgba(218, 119, 86, .22);")
                && !html.contains("--history-row-fg"),
            "折叠展开后的明细行应使用灰底，分组摘要行 hover 只能提供整行底色和边框反馈"
        );
        assert!(
            html.contains("style=\"--history-row:${options.rowIndex};\""),
            "渲染历史记录时应给每一行写入稳定 grid 行号，供整行底色层覆盖列间空白"
        );
    }

    #[test]
    fn embedded_index_html_should_measure_all_history_records_before_collapsed_expansion() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("${renderHistoryMeasureRow()}${historyRowsForRender().join(\"\")}")
                && html.contains("function renderHistoryMeasureRow()"),
            "折叠状态也应插入零高度测量行，让所有查询结果提前参与列宽计算"
        );
        assert!(
            html.contains("class=\"history-record-row history-measure-row\"")
                && html.contains("style=\"--history-row:2;\"")
                && html.contains("aria-hidden=\"true\""),
            "测量行应固定在第 2 个 grid 行，并对辅助技术隐藏"
        );
        assert!(
            html.contains(".history-measure-row > td {\n      min-height: 0;")
                && html
                    .contains(".history-measure-stack {\n      display: grid;\n      height: 0;"),
            "测量行应只贡献列宽，不增加可见行高"
        );
        assert!(
            html.contains("{ ...options, rowIndex: rows.length + 3 }"),
            "真实数据行应从测量行之后开始编号，避免和测量行重叠"
        );
    }

    #[test]
    fn embedded_index_html_should_expand_collapsed_history_group_from_entire_row() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("data-history-group-row")
                && html.contains("data-history-group-key=\"${escapeHtml(options.group.key)}\""),
            "折叠分组摘要行本身应携带分组 key，让整行都可以点击展开"
        );
        assert!(
            html.contains("const row = target.closest(\"[data-history-group-row]\");")
                && html.contains(
                    "const groupKey = toggle?.dataset.historyGroupKey || row?.dataset.historyGroupKey;"
                ),
            "历史记录点击处理应同时识别时间按钮和整行点击"
        );
        assert!(
            html.contains(".history-group-row td {\n      cursor: pointer;"),
            "折叠分组摘要行应通过整行指针样式表达可点击"
        );
        assert!(
            !html.contains(".history-group-row:hover .history-group-toggle")
                && !html.contains(".history-group-row:has(> td:hover) .history-group-toggle")
                && !html.contains(".history-group-row:hover > td")
                && !html.contains(".history-group-row:has(> td:hover) > td")
                && !html.contains(".history-group-toggle:hover:not(:disabled),\n    .history-group-toggle:active:not(:disabled),\n    .history-group-toggle.click-feedback:not(:disabled) {\n      color: var(--accent);\n      background: #fff4ed;"),
            "分组摘要 hover 样式应只由整行底色提供，时间按钮和单元格文字不应单独变色"
        );
        assert!(
            html.contains("if (button.matches(\"[data-history-group-toggle]\")) return;")
                && !html.contains(".history-group-row:focus-within")
                && !html.contains(".history-group-toggle:hover:not(:disabled),"),
            "折叠摘要行里的时间按钮不应再继承按钮点击反馈或 focus-within 高亮，避免展开时单独闪一下"
        );
    }

    #[test]
    fn embedded_index_html_should_keep_active_speed_sort_button_frame_visible() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(
                ".history-speed-sort-btn[aria-pressed=\"true\"] {\n      color: var(--accent);\n      background: #fff4ed;\n      border-color: rgba(218, 119, 86, .22);"
            ),
            "下载速度排序激活后应保持 hover 时的边框和底色，避免状态反馈只在鼠标悬停时出现"
        );
        assert!(
            html.contains("@media (hover: hover)")
                && html.contains(".history-speed-sort-btn:hover:not(:disabled)")
                && !html.contains(
                    ".history-speed-sort-btn:hover:not(:disabled),\n    .history-speed-sort-btn:active:not(:disabled)"
                ),
            "移动端关闭下载速度排序后不应被粘滞 hover 留住激活样式"
        );
    }

    #[test]
    fn embedded_index_html_should_use_stable_history_group_row_feedback() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("toggleHistoryGroup(groupKey);")
                && !html.contains("function runAfterHistoryRowClickFeedback(row, action)")
                && !html.contains("playHistoryRowClickFeedback(row);")
                && !html.contains(".history-group-row.click-feedback {")
                && html.contains(".history-group-row:active {"),
            "折叠行应立即展开或收起，按压反馈只能由 CSS 提供，不能用定时器阻塞渲染"
        );
        assert!(
            !html.contains(".history-group-row.click-feedback > td")
                && !html.contains(".history-group-row.click-feedback .history-group-toggle")
                && html.contains("-webkit-tap-highlight-color: transparent;")
                && html.contains("-webkit-user-select: none;")
                && html.contains("user-select: none;"),
            "折叠行点击反馈不应让时间文字变色，移动端也不应出现系统蓝色选中块"
        );
        assert!(
            html.contains("pointer-events: auto;")
                && html.contains("@media (hover: hover)")
                && html.contains(".history-group-row:has(> td:hover) {"),
            "整行背景层应能接收列间空白点击，hover 反馈也应只在支持 hover 的设备上启用"
        );
    }

    #[test]
    fn embedded_index_html_should_keep_mobile_history_actions_on_one_row() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(".history-toolbar {\n        grid-template-columns: minmax(0, 1fr) 42px;")
                && html.contains(".history-toolbar .search-select,\n      .history-toolbar .search-input {\n        grid-column: 1 / -1;")
                && html.contains("#query-history-btn {\n        grid-column: 1;")
                && html.contains("#history-collapse-toggle {\n        grid-column: 2;"),
            "移动端历史查询工具栏应让查询按钮和折叠按钮保持同一行，避免折叠按钮被挤到下一行"
        );
    }

    #[test]
    fn embedded_index_html_should_open_history_modal_without_delayed_feedback() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("els.historyBtn.addEventListener(\"click\", openHistory);")
                && !html.contains("runAfterButtonClickFeedback(openHistory)"),
            "测速记录按钮应立即打开弹窗，避免先播放按钮反馈再打开导致移动端闪一下"
        );
    }

    #[test]
    fn embedded_index_html_should_render_continuous_history_header_background_and_border() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(".history-table::before {\n      content: \"\";")
                && html.contains("grid-column: 1 / -1;")
                && html.contains("grid-row: 1;")
                && html.contains("border-bottom: 1px solid #eee7df;"),
            "历史表头应使用跨列背景层绘制连续下边线，避免列间空白处边框断开"
        );
        assert!(
            html.contains(".history-table thead tr > th {\n      grid-row: 1;\n      z-index: 2;")
                && html.contains("th {\n      position: sticky;")
                && html.contains("background: transparent;"),
            "表头单元格应位于跨列背景层上方，自身不再单独绘制背景造成断层"
        );
    }

    #[test]
    fn embedded_index_html_should_keep_history_row_height_and_time_stable() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(".history-table th,\n    .history-table td {\n      padding: 8px;"),
            "测速记录表格单元格 padding 应固定，避免行高随视口宽度变化"
        );
        assert!(
            html.contains(".history-table tbody td {\n      min-height: 54px;\n    }"),
            "历史记录每行应有稳定高度，避免收窄时视觉抖动"
        );
        assert!(
            html.contains("<th class=\"col-time\">时间</th>")
                && html.contains(
                    "<td class=\"col-time\">${renderHistoryTimeCell(record, options)}</td>"
                )
                && html.contains("if (!options.group) return formatTime(record.created_at);")
                && html.contains(".col-time {\n      font-family: var(--mono-stack);\n      white-space: nowrap;\n    }"),
            "历史记录时间列应有语义 class 并禁止换行"
        );
        assert!(
            !html.contains("padding: clamp(8px, 1.1vw, 10px);"),
            "历史记录不应再用随宽度变化的 padding 影响行高"
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
            html.contains("<th class=\"col-domain\">域名</th>")
                && html.contains("<td class=\"col-domain\">")
                && html.contains(".history-table .col-domain,\n    .history-table .col-network {\n      padding-left: clamp(8px, 1.55vw, 14px);\n    }"),
            "历史记录域名列和访问来源列应通过连续留白拉开相邻列，避免断点处跳变"
        );
        assert!(
            !html.contains("@media (min-width: 760px)"),
            "历史记录列间距不应依赖 760px 硬断点，否则收窄到临界宽度会突然跳变"
        );
        assert!(
            !html.contains("<th style=\"text-align:right\">下载速度</th>"),
            "访问来源列应通过专属 class 控制左侧留白"
        );
    }

    #[test]
    fn embedded_index_html_should_show_history_colo_badge_after_location_and_isp() {
        let html = super::embedded_index_html();

        assert!(
            html.contains("function historyNetworkText(record)")
                && html.contains("const isp = cleanText(record.ip_isp);")
                && html
                    .contains("return location && isp ? `${location} · ${isp}` : location || isp;")
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
    fn embedded_index_html_should_use_responsive_history_table_columns() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(
                ".history-table {\n      --history-grid-columns: repeat(5, max-content);"
            ),
            "测速记录应让每列按内容宽度占位，避免内容较多的列被均分列宽挤压"
        );
        assert!(
            html.contains("width: max-content;")
                && html.contains("min-width: 100%;")
                && html.contains("justify-content: space-between;"),
            "历史表格应把空余宽度均分为列间距，并在内容总宽超过容器时横向滚动"
        );
        assert!(
            !html.contains("clamp(174px, 21vw, 200px)"),
            "访问来源列不应再设置固定上限，否则较长归属地仍会被省略"
        );
        assert!(
            !html.contains("@media (min-width: 820px)")
                && !html.contains("clamp(86px, 12vw, 118px)")
                && !html.contains("clamp(90px, 13vw, 126px)")
                && !html.contains("repeat(5, minmax(max-content, 1fr))"),
            "历史记录列宽不应再使用断点或视口 clamp 切换，避免收窄时跳跃"
        );
        assert!(
            html.contains(
                ".history-table thead,\n    .history-table tbody,\n    .history-table tr {\n      display: contents;\n    }"
            ),
            "历史表格行应交给同一套 grid 轨道排布，让表头和数据列保持对齐"
        );
        assert!(
            !html.contains("<colgroup>")
                && !html.contains("table-layout: fixed;")
                && !html.contains(".history-col-network {\n      width: auto;"),
            "历史记录不应继续依赖 fixed table 和 auto col，否则访问来源会承接多余空白"
        );
        assert!(
            html.contains(".history-table .empty-row {\n      grid-column: 1 / -1;\n    }"),
            "加载中和空状态行应跨越整个 grid 表格"
        );
    }

    #[test]
    fn embedded_index_html_should_keep_history_https_values_fully_visible() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(".history-latency-cell {\n      min-width: max-content;\n    }"),
            "HTTPS 指标列应按内容保留最小宽度，避免窄屏下数值被挤成省略号"
        );
        assert!(
            html.contains(".history-latency-main,\n    .history-latency-jitter {\n      overflow: visible;\n      text-overflow: clip;\n      white-space: nowrap;\n    }"),
            "HTTPS 主延迟和抖动值都应完整显示，不应使用省略号截断"
        );
        assert!(
            !html.contains(
                ".history-latency-jitter,\n    .history-network-location {\n      overflow: hidden;"
            ),
            "HTTPS 抖动值不应继续复用访问来源的省略号样式"
        );
    }

    #[test]
    fn embedded_index_html_should_limit_native_history_domain_select_width() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(
                "grid-template-columns: minmax(150px, 240px) minmax(220px, 1fr) auto auto;"
            ),
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
    fn embedded_index_html_should_keep_target_speed_value_visible_on_one_line() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(".target-speed-value {\n      display: block;")
                && html.contains("width: max-content;")
                && html.contains("white-space: nowrap;")
                && html.contains("overflow: visible;")
                && html.contains("text-overflow: clip;"),
            "主页下载速度值应强制单行完整显示，宽度不足时允许覆盖相邻内容而不是换行或省略"
        );
        assert!(
            html.contains(".download-metric {\n      position: relative;")
                && html.contains("z-index: 2;"),
            "下载速度容器应允许速度值溢出并压在同一行相邻元素之上"
        );
        assert!(
            html.contains(".target-download-status {\n      display: block;")
                && html.contains("width: max-content;")
                && html.contains("max-width: 100%;")
                && html.contains("margin-left: auto;")
                && html.contains("text-align: right;")
                && html.contains("white-space: nowrap;"),
            "下载状态文字应和速度值使用一致的右侧贴齐规则"
        );
    }

    #[test]
    fn embedded_index_html_should_align_target_metrics_with_shared_number_style() {
        let html = super::embedded_index_html();

        assert!(
            html.contains(
                "--number-stack: ui-monospace, \"SFMono-Regular\", Menlo, Monaco, \"Cascadia Mono\", \"Segoe UI Mono\", Consolas, \"Liberation Mono\", monospace;"
            ),
            "HTTPS 延迟和下载速度应使用同一套本机等宽数字字体栈"
        );
        assert!(
            html.contains(".metric,\n    .download-metric {\n      position: relative;")
                && html.contains("display: flex;")
                && html.contains("flex-direction: column;")
                && html.contains("align-items: flex-end;")
                && html.contains("font-family: var(--number-stack);")
                && html.contains("font-variant-numeric: tabular-nums;")
                && html.contains("font-feature-settings: \"tnum\" 1;"),
            "HTTPS 延迟和下载速度容器应共享同一套对齐与数字显示规则"
        );
        assert!(
            html.contains(".metric strong,\n    .target-speed-value {\n      display: block;")
                && html.contains("font-size: 1.06rem;")
                && html.contains("font-weight: 850;")
                && html.contains("line-height: 1.15;")
                && html.contains("width: max-content;")
                && html.contains("margin-left: auto;"),
            "HTTPS 延迟值和下载速度值应共享相同字号、行高和右贴齐盒模型"
        );
        assert!(
            html.contains(".metric span,\n    .target-download-status {\n      display: block;")
                && html.contains("line-height: 1.2;")
                && html.contains("width: max-content;")
                && html.contains("max-width: 100%;")
                && html.contains("margin-left: auto;"),
            "抖动说明和下载状态说明应共享相同行高和右贴齐盒模型"
        );
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
