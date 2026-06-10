use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

#[derive(Debug, Clone, serde::Serialize)]
pub struct TestTarget {
    pub key: String,
    pub label: String,
    pub host: String,
    pub cname: String,
    pub trace_url: String,
    pub download_url: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RuntimeConfig {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub test: TestSettings,
    pub history: HistorySettings,
    pub display: DisplaySettings,
    pub targets: Vec<TestTarget>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DatabaseConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TestSettings {
    pub latency_rounds: usize,
    pub latency_timeout_ms: u64,
    pub download_test_ms: u64,
    pub download_file_path: String,
    pub progress_save_ratio: f64,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct FileTestSettings {
    #[serde(default = "default_latency_rounds")]
    latency_rounds: usize,
    #[serde(default = "default_latency_timeout_ms")]
    latency_timeout_ms: u64,
    #[serde(default = "default_download_test_ms")]
    download_test_ms: u64,
    download_file_path: String,
    #[serde(default = "default_progress_save_ratio")]
    progress_save_ratio: f64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HistorySettings {
    pub default_limit: usize,
    pub max_limit: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DisplaySettings {
    pub show_domains: bool,
}

#[derive(Debug, serde::Deserialize)]
struct FileConfig {
    #[serde(default)]
    server: ServerConfig,
    #[serde(default)]
    database: DatabaseConfig,
    #[serde(default)]
    speedtest: Option<FileTestSettings>,
    #[serde(default)]
    test: Option<FileTestSettings>,
    #[serde(default)]
    history: HistorySettings,
    #[serde(default)]
    display: DisplaySettings,
    domains: DomainConfig,
}

#[derive(Debug, serde::Deserialize)]
struct DomainConfig {
    path: PathBuf,
    #[serde(default)]
    blacklist: Vec<String>,
    #[serde(default)]
    overrides: Vec<DomainItem>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DomainItem {
    #[serde(default)]
    pub name: Option<String>,
    pub url: String,
    #[serde(default)]
    pub cname: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ExternalDomainsFile {
    #[serde(default)]
    domains: Vec<DomainItem>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("读取配置失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("配置文件不存在: {}", .0.display())]
    MissingConfig(PathBuf),
    #[error("外部线路文件不存在: {}", .0.display())]
    MissingDomainFile(PathBuf),
    #[error("TOML 配置解析失败: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("YAML 线路配置解析失败: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("配置中没有可用线路")]
    NoDomains,
    #[error("线路 URL 无效: {0}")]
    InvalidDomainUrl(String),
    #[error("重复线路域名: {0}")]
    DuplicateDomain(String),
    #[error("缺少 [speedtest] 配置段")]
    MissingSpeedtestSettings,
    #[error("speedtest.download_file_path 必须以 / 开头且不能只写 /: {0}")]
    InvalidDownloadFilePath(String),
    #[error("历史记录默认查询数量不能大于最大查询数量")]
    InvalidHistoryLimit,
}

impl RuntimeConfig {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let path = path.into();
        if path.is_file() {
            Self::from_toml_str(&std::fs::read_to_string(path)?)
        } else {
            Err(ConfigError::MissingConfig(path))
        }
    }

    pub fn from_toml_str(input: &str) -> Result<Self, ConfigError> {
        Self::from_file_config(toml::from_str::<FileConfig>(input)?)
    }

    fn from_file_config(file_config: FileConfig) -> Result<Self, ConfigError> {
        let FileConfig {
            server,
            database,
            speedtest,
            test,
            history,
            display,
            domains,
        } = file_config;
        let domain_items = load_domain_items(&domains)?;
        let test = select_file_test_settings(speedtest, test)?.normalized()?;
        let targets = build_targets(&domain_items, &test.download_file_path)?;
        if targets.is_empty() {
            return Err(ConfigError::NoDomains);
        }

        Ok(Self {
            server,
            database,
            test,
            history: history.normalized()?,
            display,
            targets,
        })
    }

    pub fn listen_addr(&self) -> String {
        format!("{}:{}", self.server.host, self.server.port)
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 3000,
        }
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("speed_results.sqlite3"),
        }
    }
}

fn select_file_test_settings(
    speedtest: Option<FileTestSettings>,
    legacy_test: Option<FileTestSettings>,
) -> Result<FileTestSettings, ConfigError> {
    // 新配置段命名为 speedtest；保留 test 作为旧配置兼容入口，避免升级时配置立即失效。
    speedtest
        .or(legacy_test)
        .ok_or(ConfigError::MissingSpeedtestSettings)
}

impl FileTestSettings {
    fn normalized(self) -> Result<TestSettings, ConfigError> {
        Ok(TestSettings {
            latency_rounds: self.latency_rounds.clamp(1, 20),
            latency_timeout_ms: self.latency_timeout_ms.clamp(300, 30_000),
            download_test_ms: self.download_test_ms.clamp(1_000, 120_000),
            download_file_path: normalize_download_file_path(&self.download_file_path)?,
            progress_save_ratio: self.progress_save_ratio.clamp(0.3, 0.95),
        })
    }
}

fn default_latency_rounds() -> usize {
    5
}

fn default_latency_timeout_ms() -> u64 {
    3500
}

fn default_download_test_ms() -> u64 {
    8500
}

fn default_progress_save_ratio() -> f64 {
    0.5
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            default_limit: 80,
            max_limit: 200,
        }
    }
}

impl HistorySettings {
    fn normalized(mut self) -> Result<Self, ConfigError> {
        self.default_limit = self.default_limit.clamp(1, 1000);
        self.max_limit = self.max_limit.clamp(1, 1000);
        if self.default_limit > self.max_limit {
            return Err(ConfigError::InvalidHistoryLimit);
        }
        Ok(self)
    }

    pub fn resolve_limit(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.default_limit)
            .clamp(1, self.max_limit)
    }
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self { show_domains: true }
    }
}

fn load_domain_items(config: &DomainConfig) -> Result<Vec<DomainItem>, ConfigError> {
    let path = &config.path;
    if !path.is_file() {
        return Err(ConfigError::MissingDomainFile(path.clone()));
    }

    let external = serde_yaml::from_str::<ExternalDomainsFile>(&std::fs::read_to_string(path)?)?;
    if external.domains.is_empty() {
        return Err(ConfigError::NoDomains);
    }

    let domains = merge_domain_overrides(external.domains, &config.overrides)?;
    filter_blacklisted_domains(domains, &config.blacklist)
}

fn merge_domain_overrides(
    mut domains: Vec<DomainItem>,
    overrides: &[DomainItem],
) -> Result<Vec<DomainItem>, ConfigError> {
    let mut index_by_url = HashMap::new();
    for (index, domain) in domains.iter().enumerate() {
        index_by_url.insert(normalize_base_url(&domain.url)?, index);
    }

    for override_item in overrides {
        let base_url = normalize_base_url(&override_item.url)?;
        if let Some(index) = index_by_url.get(&base_url).copied() {
            if let Some(name) = non_empty_string(override_item.name.as_deref()) {
                domains[index].name = Some(name.to_string());
            }
            if let Some(cname) = non_empty_string(override_item.cname.as_deref()) {
                domains[index].cname = Some(cname.to_string());
            }
        } else {
            index_by_url.insert(base_url, domains.len());
            domains.push(override_item.clone());
        }
    }

    Ok(domains)
}

fn filter_blacklisted_domains(
    domains: Vec<DomainItem>,
    blacklist: &[String],
) -> Result<Vec<DomainItem>, ConfigError> {
    if blacklist.is_empty() {
        return Ok(domains);
    }

    let blacklist = blacklist
        .iter()
        .map(|url| normalize_base_url(url))
        .collect::<Result<HashSet<_>, _>>()?;
    let mut filtered_domains = Vec::with_capacity(domains.len());
    for domain in domains {
        // 黑名单按归一化后的 URL 比较，避免尾斜杠写法影响匹配结果。
        if !blacklist.contains(&normalize_base_url(&domain.url)?) {
            filtered_domains.push(domain);
        }
    }

    Ok(filtered_domains)
}

fn build_targets(
    items: &[DomainItem],
    download_file_path: &str,
) -> Result<Vec<TestTarget>, ConfigError> {
    let mut used_keys = HashSet::new();
    let mut used_hosts = HashSet::new();
    items
        .iter()
        .map(|item| {
            let base_url = normalize_base_url(&item.url)?;
            let host = base_url
                .strip_prefix("https://")
                .or_else(|| base_url.strip_prefix("http://"))
                .unwrap_or(&base_url)
                .to_string();
            let host_key = host.to_ascii_lowercase();
            if !used_hosts.insert(host_key.clone()) {
                return Err(ConfigError::DuplicateDomain(host_key));
            }
            let mut key = slugify(&host);
            if key.is_empty() {
                key = slugify(display_label(item.name.as_deref(), &host).as_str());
            }
            let unique_key = unique_key(key, &mut used_keys);

            Ok(TestTarget {
                key: unique_key,
                label: display_label(item.name.as_deref(), &host),
                host,
                cname: display_cname(item.cname.as_deref()),
                trace_url: format!("{base_url}/cdn-cgi/trace"),
                download_url: format!("{base_url}{download_file_path}"),
            })
        })
        .collect()
}

fn normalize_download_file_path(value: &str) -> Result<String, ConfigError> {
    let path = value.trim();
    // 下载文件路径只描述站内路径，线路域名仍由 domains.url 决定，避免配置成完整 URL 后绕过线路。
    if path.is_empty() || !path.starts_with('/') || path == "/" {
        return Err(ConfigError::InvalidDownloadFilePath(path.to_string()));
    }
    Ok(path.to_string())
}

fn display_label(name: Option<&str>, host: &str) -> String {
    non_empty_string(name).unwrap_or(host).to_string()
}

fn display_cname(cname: Option<&str>) -> String {
    let Some(cname) = non_empty_string(cname) else {
        return "-".to_string();
    };
    let without_scheme = cname
        .strip_prefix("https://")
        .or_else(|| cname.strip_prefix("http://"))
        .unwrap_or(cname);
    let trimmed = without_scheme.trim_end_matches('/').trim();
    if trimmed.is_empty() {
        "-".to_string()
    } else {
        trimmed.to_string()
    }
}

fn non_empty_string(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn normalize_base_url(input: &str) -> Result<String, ConfigError> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        Ok(trimmed.to_string())
    } else {
        Err(ConfigError::InvalidDomainUrl(input.to_string()))
    }
}

fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn unique_key(base: String, used_keys: &mut HashSet<String>) -> String {
    if used_keys.insert(base.clone()) {
        return base;
    }

    for index in 2.. {
        let candidate = format!("{base}-{index}");
        if used_keys.insert(candidate.clone()) {
            return candidate;
        }
    }

    unreachable!("无限递增的后缀最终一定能生成唯一 key")
}

#[cfg(test)]
mod tests {
    use super::{ConfigError, RuntimeConfig};

    #[test]
    fn runtime_config_should_require_external_domain_file_path() {
        let result = RuntimeConfig::from_toml_str(
            r#"
            [speedtest]
            download_file_path = "/speed.bin"

            [domains]
            "#,
        );

        assert!(matches!(result, Err(ConfigError::Toml(_))));
    }

    #[test]
    fn runtime_config_should_reject_missing_external_domain_file() {
        let missing_path = std::env::temp_dir().join(format!(
            "web-speedtest-missing-domains-{}.yaml",
            std::process::id()
        ));
        let toml = format!(
            r#"
            [speedtest]
            download_file_path = "/missing-domain-speed.bin"

            [domains]
            path = '{}'
            "#,
            missing_path.display()
        );

        let result = RuntimeConfig::from_toml_str(&toml);

        assert!(
            matches!(result, Err(ConfigError::MissingDomainFile(path)) if path == missing_path)
        );
    }

    #[test]
    fn runtime_config_should_reject_empty_external_domain_file() {
        let yaml_path = std::env::temp_dir().join(format!(
            "web-speedtest-empty-domains-{}.yaml",
            std::process::id()
        ));
        std::fs::write(&yaml_path, "domains: []").expect("yaml should be writable");
        let toml = format!(
            r#"
            [speedtest]
            download_file_path = "/empty-domain-speed.bin"

            [domains]
            path = '{}'
            "#,
            yaml_path.display()
        );

        let result = RuntimeConfig::from_toml_str(&toml);
        let _ = std::fs::remove_file(&yaml_path);

        assert!(matches!(result, Err(ConfigError::NoDomains)));
    }

    #[test]
    fn runtime_config_should_read_server_and_test_options_from_toml() {
        let yaml_path = std::env::temp_dir().join(format!(
            "web-speedtest-options-domains-{}.yaml",
            std::process::id()
        ));
        std::fs::write(
            &yaml_path,
            r#"
            domains:
              - name: "a1 线路"
                url: "https://a1.example.com"
            "#,
        )
        .expect("yaml should be writable");
        let config = RuntimeConfig::from_toml_str(&format!(
            r#"
            [server]
            host = "0.0.0.0"
            port = 52143

            [speedtest]
            latency_rounds = 7
            latency_timeout_ms = 2200
            download_test_ms = 12000
            download_file_path = "/custom-speed.bin"
            progress_save_ratio = 0.35

            [domains]
            path = '{}'
            "#,
            yaml_path.display()
        ))
        .expect("config should parse");
        let _ = std::fs::remove_file(&yaml_path);

        assert_eq!(config.listen_addr(), "0.0.0.0:52143");
        assert_eq!(config.test.latency_rounds, 7);
        assert_eq!(config.test.download_test_ms, 12000);
        assert_eq!(config.test.progress_save_ratio, 0.35);
        assert_eq!(
            config.targets[0].download_url,
            "https://a1.example.com/custom-speed.bin"
        );
    }

    #[test]
    fn runtime_config_should_accept_legacy_test_section() {
        let yaml_path = std::env::temp_dir().join(format!(
            "web-speedtest-legacy-test-domains-{}.yaml",
            std::process::id()
        ));
        std::fs::write(
            &yaml_path,
            r#"
            domains:
              - name: "a1 线路"
                url: "https://a1.example.com"
            "#,
        )
        .expect("yaml should be writable");

        let config = RuntimeConfig::from_toml_str(&format!(
            r#"
            [test]
            download_file_path = "/legacy-speed.bin"

            [domains]
            path = '{}'
            "#,
            yaml_path.display()
        ))
        .expect("legacy config should parse");
        let _ = std::fs::remove_file(&yaml_path);

        assert_eq!(
            config.targets[0].download_url,
            "https://a1.example.com/legacy-speed.bin"
        );
    }

    #[test]
    fn runtime_config_should_prefer_speedtest_section_over_legacy_test_section() {
        let yaml_path = std::env::temp_dir().join(format!(
            "web-speedtest-preferred-speedtest-domains-{}.yaml",
            std::process::id()
        ));
        std::fs::write(
            &yaml_path,
            r#"
            domains:
              - name: "a1 线路"
                url: "https://a1.example.com"
            "#,
        )
        .expect("yaml should be writable");

        let config = RuntimeConfig::from_toml_str(&format!(
            r#"
            [test]
            download_file_path = "/legacy-speed.bin"

            [speedtest]
            download_file_path = "/new-speed.bin"

            [domains]
            path = '{}'
            "#,
            yaml_path.display()
        ))
        .expect("config should parse");
        let _ = std::fs::remove_file(&yaml_path);

        assert_eq!(
            config.targets[0].download_url,
            "https://a1.example.com/new-speed.bin"
        );
    }

    #[test]
    fn runtime_config_should_require_download_file_path_when_domains_exist() {
        let yaml_path = std::env::temp_dir().join(format!(
            "web-speedtest-missing-download-path-domains-{}.yaml",
            std::process::id()
        ));
        std::fs::write(
            &yaml_path,
            r#"
            domains:
              - name: "a1 线路"
                url: "https://a1.example.com"
            "#,
        )
        .expect("yaml should be writable");

        let result = RuntimeConfig::from_toml_str(&format!(
            r#"
            [speedtest]
            latency_rounds = 2

            [domains]
            path = '{}'
            "#,
            yaml_path.display()
        ));
        let _ = std::fs::remove_file(&yaml_path);

        assert!(matches!(result, Err(ConfigError::Toml(_))));
    }

    #[test]
    fn runtime_config_should_reject_invalid_download_file_path() {
        let yaml_path = std::env::temp_dir().join(format!(
            "web-speedtest-invalid-download-path-domains-{}.yaml",
            std::process::id()
        ));
        std::fs::write(
            &yaml_path,
            r#"
            domains:
              - name: "a1 线路"
                url: "https://a1.example.com"
            "#,
        )
        .expect("yaml should be writable");

        let result = RuntimeConfig::from_toml_str(&format!(
            r#"
            [speedtest]
            download_file_path = "speed.bin"

            [domains]
            path = '{}'
            "#,
            yaml_path.display()
        ));
        let _ = std::fs::remove_file(&yaml_path);

        assert!(
            matches!(result, Err(ConfigError::InvalidDownloadFilePath(path)) if path == "speed.bin")
        );
    }

    #[test]
    fn runtime_config_should_read_history_limits_from_toml() {
        let yaml_path = std::env::temp_dir().join(format!(
            "web-speedtest-history-domains-{}.yaml",
            std::process::id()
        ));
        std::fs::write(
            &yaml_path,
            r#"
            domains:
              - name: "a1 线路"
                url: "https://a1.example.com"
            "#,
        )
        .expect("yaml should be writable");

        let config = RuntimeConfig::from_toml_str(&format!(
            r#"
            [history]
            default_limit = 40
            max_limit = 90

            [speedtest]
            download_file_path = "/history-speed.bin"

            [domains]
            path = '{}'
            "#,
            yaml_path.display()
        ))
        .expect("config should parse");
        let _ = std::fs::remove_file(&yaml_path);

        assert_eq!(config.history.default_limit, 40);
        assert_eq!(config.history.max_limit, 90);
    }

    #[test]
    fn runtime_config_should_read_display_settings_from_toml() {
        let yaml_path = std::env::temp_dir().join(format!(
            "web-speedtest-display-domains-{}.yaml",
            std::process::id()
        ));
        std::fs::write(
            &yaml_path,
            r#"
            domains:
              - name: "a1 线路"
                url: "https://a1.example.com"
            "#,
        )
        .expect("yaml should be writable");

        let config = RuntimeConfig::from_toml_str(&format!(
            r#"
            [display]
            show_domains = false

            [speedtest]
            download_file_path = "/display-speed.bin"

            [domains]
            path = '{}'
            "#,
            yaml_path.display()
        ))
        .expect("config should parse");
        let _ = std::fs::remove_file(&yaml_path);

        assert!(!config.display.show_domains);
    }

    #[test]
    fn runtime_config_should_reject_invalid_history_limits() {
        let yaml_path = std::env::temp_dir().join(format!(
            "web-speedtest-invalid-history-domains-{}.yaml",
            std::process::id()
        ));
        std::fs::write(
            &yaml_path,
            r#"
            domains:
              - name: "a1 线路"
                url: "https://a1.example.com"
            "#,
        )
        .expect("yaml should be writable");

        let result = RuntimeConfig::from_toml_str(&format!(
            r#"
            [history]
            default_limit = 201
            max_limit = 200

            [speedtest]
            download_file_path = "/invalid-history-speed.bin"

            [domains]
            path = '{}'
            "#,
            yaml_path.display()
        ));
        let _ = std::fs::remove_file(&yaml_path);

        assert!(matches!(result, Err(ConfigError::InvalidHistoryLimit)));
    }

    #[test]
    fn runtime_config_should_read_domains_from_yaml_and_apply_overrides() {
        let yaml_path =
            std::env::temp_dir().join(format!("web-speedtest-domains-{}.yaml", std::process::id()));
        std::fs::write(
            &yaml_path,
            r#"
            emby:
              server_url: "https://free.lilyemby.com"
            server:
              port: 52143
            domains:
              - name: "19931110 线路"
                url: "https://line-a.example.com"
              - name: "a1 线路"
                url: "https://line-b.example.com/"
            "#,
        )
        .expect("yaml should be writable");

        let toml = format!(
            r#"
            [speedtest]
            download_file_path = "/domain-speed.bin"

            [domains]
            path = '{}'

            [[domains.overrides]]
            url = "https://line-a.example.com/"
            cname = "line-a.cname.example.com"

            [[domains.overrides]]
            name = "覆盖后的 a1"
            url = "https://line-b.example.com"

            [[domains.overrides]]
            name = "新增线路"
            url = "https://new.example.com/"
            cname = "new.cname.example.com"
            "#,
            yaml_path.display()
        );

        let config = RuntimeConfig::from_toml_str(&toml).expect("config should parse");
        let _ = std::fs::remove_file(&yaml_path);

        assert_eq!(config.targets.len(), 3);
        assert_eq!(config.targets[0].label, "19931110 线路");
        assert_eq!(config.targets[0].cname, "line-a.cname.example.com");
        assert_eq!(
            config.targets[0].trace_url,
            "https://line-a.example.com/cdn-cgi/trace"
        );
        assert_eq!(config.targets[1].label, "覆盖后的 a1");
        assert_eq!(config.targets[1].cname, "-");
        assert_eq!(
            config.targets[1].download_url,
            "https://line-b.example.com/domain-speed.bin"
        );
        assert_eq!(config.targets[2].label, "新增线路");
        assert_eq!(config.targets[2].host, "new.example.com");
        assert_eq!(config.targets[2].cname, "new.cname.example.com");
    }

    #[test]
    fn runtime_config_should_filter_blacklisted_domains_ignoring_trailing_slash() {
        let yaml_path = std::env::temp_dir().join(format!(
            "web-speedtest-blacklist-domains-{}.yaml",
            std::process::id()
        ));
        std::fs::write(
            &yaml_path,
            r#"
            domains:
              - name: "x8 无尾斜杠"
                url: "https://x8.nginxlily.cyou"
              - name: "x9 有尾斜杠"
                url: "https://x9.nginxlily.cyou/"
              - name: "保留线路"
                url: "https://keep.example.com"
            "#,
        )
        .expect("yaml should be writable");

        let config = RuntimeConfig::from_toml_str(&format!(
            r#"
            [speedtest]
            download_file_path = "/blacklist-speed.bin"

            [domains]
            path = '{}'
            blacklist = [
                "https://x8.nginxlily.cyou/",
                "https://x9.nginxlily.cyou"
            ]
            "#,
            yaml_path.display()
        ))
        .expect("config should parse");
        let _ = std::fs::remove_file(&yaml_path);

        assert_eq!(config.targets.len(), 1);
        assert_eq!(config.targets[0].host, "keep.example.com");
    }

    #[test]
    fn runtime_config_should_reject_duplicate_domain_hosts() {
        let yaml_path = std::env::temp_dir().join(format!(
            "web-speedtest-duplicate-domains-{}.yaml",
            std::process::id()
        ));
        std::fs::write(
            &yaml_path,
            r#"
            domains:
              - name: "a1"
                url: "https://dup.example.com"
              - name: "a2"
                url: "https://dup.example.com/"
            "#,
        )
        .expect("yaml should be writable");

        let result = RuntimeConfig::from_toml_str(&format!(
            r#"
            [speedtest]
            download_file_path = "/duplicate-speed.bin"

            [domains]
            path = '{}'
            "#,
            yaml_path.display()
        ));
        let _ = std::fs::remove_file(&yaml_path);

        assert!(
            matches!(result, Err(ConfigError::DuplicateDomain(domain)) if domain == "dup.example.com")
        );
    }

    #[test]
    fn runtime_config_should_reject_missing_config_file() {
        let missing_path = std::env::temp_dir().join(format!(
            "web-speedtest-missing-config-{}.toml",
            std::process::id()
        ));
        let result = RuntimeConfig::load(&missing_path);

        assert!(matches!(result, Err(ConfigError::MissingConfig(path)) if path == missing_path));
    }

    #[test]
    fn runtime_config_should_reject_empty_domain_sources() {
        let yaml_path = std::env::temp_dir().join(format!(
            "web-speedtest-empty-config-{}.yaml",
            std::process::id()
        ));
        std::fs::write(&yaml_path, "domains: []").expect("yaml should be writable");
        let toml = format!(
            r#"
            [speedtest]
            download_file_path = "/empty-source-speed.bin"

            [domains]
            path = '{}'
            "#,
            yaml_path.display()
        );

        let result = RuntimeConfig::from_toml_str(&toml);
        let _ = std::fs::remove_file(&yaml_path);

        assert!(matches!(result, Err(ConfigError::NoDomains)));
    }
}
