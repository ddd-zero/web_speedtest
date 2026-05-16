# 多域名测速网站 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 构建一个 Rust + SQLite 后端驱动的多域名测速网站，前端自动多轮 HTTPS 延迟对比，用户点击域名后执行单线程下载测速，并支持历史记录查询与 IP 脱敏。

**Architecture:** 后端使用 Axum 提供静态页面、域名配置、测速记录保存/更新/查询接口，SQLite 通过 `rusqlite` 直接持久化。前端保留现有暖色卡片、曲线、进度和弹窗风格，真实测速仍在用户浏览器中完成，后端只保存结果和提供查询。

**Tech Stack:** Rust 2024、axum、tokio、rusqlite、serde、uuid、chrono、HTML/CSS/原生 JavaScript、SQLite。

---

## 文件结构

- Create: `src/lib.rs`，导出后端模块，便于测试。
- Create: `src/config.rs`，集中维护测速域名配置。
- Create: `src/ip.rs`，负责 IP 解析与脱敏。
- Create: `src/models.rs`，定义请求、响应和记录结构。
- Create: `src/store.rs`，封装 SQLite 初始化、插入、更新和查询。
- Create: `src/app.rs`，组装 Axum 路由和 HTTP handlers。
- Modify: `src/main.rs`，启动 HTTP 服务。
- Modify: `Cargo.toml`，增加后端依赖。
- Replace: `index.html`，保留现有视觉语言并实现多域名测速 UI。

## Task 1: 依赖与 IP 脱敏

**Files:**
- Modify: `Cargo.toml`
- Create: `src/lib.rs`
- Create: `src/ip.rs`

- [ ] **Step 1: 写失败测试**

在 `src/ip.rs` 新建测试：

```rust
#[cfg(test)]
mod tests {
    use super::mask_ip_for_public;

    #[test]
    fn mask_ip_should_keep_ipv4_24_prefix() {
        assert_eq!(mask_ip_for_public("1.2.3.4"), Some("1.2.3.*".to_string()));
    }

    #[test]
    fn mask_ip_should_keep_ipv6_48_prefix_with_compact_suffix() {
        assert_eq!(
            mask_ip_for_public("2001:db8:abcd:1234::1"),
            Some("2001:db8:abcd::*".to_string())
        );
    }

    #[test]
    fn mask_ip_should_convert_ipv4_mapped_ipv6_to_ipv4_mask() {
        assert_eq!(
            mask_ip_for_public("::ffff:192.0.2.33"),
            Some("192.0.2.*".to_string())
        );
    }

    #[test]
    fn mask_ip_should_hide_invalid_input() {
        assert_eq!(mask_ip_for_public("not-an-ip"), None);
    }
}
```

- [ ] **Step 2: 运行失败测试**

Run: `cargo test ip::tests -- --nocapture`

Expected: 编译失败或测试失败，原因是 `mask_ip_for_public` 尚未实现。

- [ ] **Step 3: 增加依赖和最小实现**

`Cargo.toml` 增加：

```toml
[dependencies]
axum = "0.8"
chrono = { version = "0.4", features = ["serde"] }
rusqlite = { version = "0.37", features = ["bundled"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "net"] }
uuid = { version = "1", features = ["v4"] }
```

`src/lib.rs`：

```rust
pub mod ip;
```

`src/ip.rs`：

```rust
use std::net::{IpAddr, Ipv6Addr};

pub fn mask_ip_for_public(input: &str) -> Option<String> {
    let ip = input.trim().parse::<IpAddr>().ok()?;
    match ip {
        IpAddr::V4(ipv4) => {
            let octets = ipv4.octets();
            Some(format!("{}.{}.{}.*", octets[0], octets[1], octets[2]))
        }
        IpAddr::V6(ipv6) => {
            if let Some(mapped) = ipv4_mapped(ipv6) {
                let octets = mapped.octets();
                return Some(format!("{}.{}.{}.*", octets[0], octets[1], octets[2]));
            }
            let segments = ipv6.segments();
            Some(format!("{:x}:{:x}:{:x}::*", segments[0], segments[1], segments[2]))
        }
    }
}

fn ipv4_mapped(ipv6: Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    ipv6.to_ipv4_mapped()
}
```

- [ ] **Step 4: 运行通过测试**

Run: `cargo test ip::tests -- --nocapture`

Expected: 4 个测试通过。

## Task 2: 域名配置与 SQLite 存储

**Files:**
- Create: `src/config.rs`
- Create: `src/models.rs`
- Create: `src/store.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: 写失败测试**

在 `src/store.rs` 新建测试：

```rust
#[cfg(test)]
mod tests {
    use super::{QueryFilter, SaveResultInput, SpeedStore};
    use crate::models::{LatencyStats, ResultStatus};

    fn sample_input(status: ResultStatus) -> SaveResultInput {
        SaveResultInput {
            id: None,
            update_token: None,
            domain_key: "a1".to_string(),
            https_latency: LatencyStats {
                median_ms: 42.0,
                min_ms: 38.0,
                max_ms: 51.0,
                samples_ms: vec![38.0, 42.0, 51.0],
            },
            partial_download_mbps: Some(80.0),
            final_download_mbps: None,
            status,
            client_ip: "1.2.3.4".to_string(),
            location: None,
            isp: None,
            colo: Some("SJC".to_string()),
        }
    }

    #[test]
    fn store_should_insert_running_record_and_query_masked_ip() {
        let store = SpeedStore::in_memory().expect("store should initialize");
        let saved = store.save_result(sample_input(ResultStatus::Running)).expect("save should work");

        let records = store.query_results(QueryFilter::default()).expect("query should work");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, saved.id);
        assert_eq!(records[0].client_ip, "1.2.3.*");
    }

    #[test]
    fn store_should_update_existing_record_with_token() {
        let store = SpeedStore::in_memory().expect("store should initialize");
        let saved = store.save_result(sample_input(ResultStatus::Running)).expect("save should work");
        let mut update = sample_input(ResultStatus::Completed);
        update.id = Some(saved.id);
        update.update_token = Some(saved.update_token);
        update.final_download_mbps = Some(120.0);

        store.save_result(update).expect("update should work");
        let records = store.query_results(QueryFilter::default()).expect("query should work");

        assert_eq!(records[0].status, ResultStatus::Completed);
        assert_eq!(records[0].final_download_mbps, Some(120.0));
    }

    #[test]
    fn store_should_reject_update_when_token_is_wrong() {
        let store = SpeedStore::in_memory().expect("store should initialize");
        let saved = store.save_result(sample_input(ResultStatus::Running)).expect("save should work");
        let mut update = sample_input(ResultStatus::Completed);
        update.id = Some(saved.id);
        update.update_token = Some("wrong-token".to_string());

        let err = store.save_result(update).expect_err("wrong token should fail");

        assert!(err.to_string().contains("更新令牌无效"));
    }
}
```

- [ ] **Step 2: 运行失败测试**

Run: `cargo test store::tests -- --nocapture`

Expected: 编译失败，原因是 `SpeedStore`、`SaveResultInput`、`ResultStatus` 等类型尚未实现。

- [ ] **Step 3: 实现配置、模型和存储**

实现以下边界：

```rust
// src/config.rs
#[derive(Debug, Clone, serde::Serialize)]
pub struct TestTarget {
    pub key: &'static str,
    pub label: &'static str,
    pub host: &'static str,
    pub trace_url: &'static str,
    pub download_url: &'static str,
}

pub const TEST_TARGETS: &[TestTarget] = &[
    TestTarget {
        key: "a1",
        label: "a1",
        host: "a1.steinsgate.eu.org",
        trace_url: "https://a1.steinsgate.eu.org/cdn-cgi/trace",
        download_url: "https://a1.steinsgate.eu.org/200mb.test",
    },
    TestTarget {
        key: "a2",
        label: "a2",
        host: "a2.steinsgate.eu.org",
        trace_url: "https://a2.steinsgate.eu.org/cdn-cgi/trace",
        download_url: "https://a2.steinsgate.eu.org/200mb.test",
    },
];

pub fn find_target(key: &str) -> Option<&'static TestTarget> {
    TEST_TARGETS.iter().find(|target| target.key == key)
}
```

```rust
// src/models.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResultStatus {
    Running,
    Completed,
    Failed,
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
```

`src/store.rs` 使用 `rusqlite` 参数化 SQL 初始化 `speed_results` 表，插入时生成 `uuid::Uuid::new_v4()` 作为 `update_token`，查询时调用 `mask_ip_for_public`。

- [ ] **Step 4: 运行通过测试**

Run: `cargo test store::tests -- --nocapture`

Expected: 3 个测试通过。

## Task 3: Axum API 与服务入口

**Files:**
- Create: `src/app.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: 写失败测试**

在 `src/app.rs` 新建测试：

```rust
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
}
```

- [ ] **Step 2: 运行失败测试**

Run: `cargo test app::tests -- --nocapture`

Expected: 编译失败，原因是 `extract_client_ip` 尚未实现。

- [ ] **Step 3: 实现路由**

`src/app.rs` 实现：

- `GET /` 返回 `Html(include_str!("../index.html"))`
- `GET /api/config` 返回 `TEST_TARGETS`
- `POST /api/results` 创建或更新记录
- `GET /api/results` 查询记录
- JSON 错误格式 `{ "error": "..." }`

`src/main.rs` 启动：

```rust
#[tokio::main]
async fn main() -> Result<(), web_speed::app::AppError> {
    web_speed::app::serve().await
}
```

- [ ] **Step 4: 运行通过测试**

Run: `cargo test app::tests -- --nocapture`

Expected: 2 个测试通过。

## Task 4: 前端页面

**Files:**
- Replace: `index.html`

- [ ] **Step 1: 保留现有视觉语言并重写结构**

实现一个单页应用：

- 首屏显示标题、IP 查询占位、域名延迟对比列表。
- 页面加载后请求 `/api/config`。
- 对每个域名并发执行 5 轮 HTTPS 延迟测试。
- 显示每个域名的中位数、最小值、状态和推荐标记。
- 点击域名按钮后执行单线程下载测速。
- 测速半程调用 `POST /api/results` 创建记录。
- 测速完成后调用 `POST /api/results` 更新记录。
- 历史弹窗调用 `GET /api/results` 查询并展示脱敏 IP。

- [ ] **Step 2: 手动检查关键 JS 函数**

确认存在以下函数：

```javascript
loadConfig()
runLatencyRounds()
measureHttpsLatency(target)
startDownloadTest(targetKey)
saveRunningResult(targetKey, speed)
finalizeResult(targetKey, speed, status)
loadHistory()
```

- [ ] **Step 3: 运行格式和编译验证**

Run: `cargo fmt --check`

Expected: 格式检查通过。

Run: `cargo test`

Expected: 所有 Rust 测试通过。

## Task 5: 全量验证

**Files:**
- No file changes expected.

- [ ] **Step 1: 运行格式检查**

Run: `cargo fmt --check`

Expected: 退出码 0。

- [ ] **Step 2: 运行 Clippy**

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: 退出码 0，零警告。

- [ ] **Step 3: 运行测试**

Run: `cargo test`

Expected: 所有测试通过。

- [ ] **Step 4: 浏览器验证**

Run: `cargo run`

Open: `http://127.0.0.1:3000`

Expected:

- 页面加载后自动显示两个域名。
- HTTPS 延迟多轮测试会更新状态。
- 历史记录弹窗可打开并查询。
- 如果 `200mb.test` 缺少 CORS，页面展示清晰的跨域提示。

完成后停止本地服务；任务完成不要求保持项目运行。
