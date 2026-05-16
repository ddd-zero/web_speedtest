use std::{collections::HashSet, path::PathBuf};

#[derive(Debug, Clone, serde::Serialize)]
pub struct TestTarget {
    pub key: String,
    pub label: String,
    pub host: String,
    pub trace_url: String,
    pub download_url: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RuntimeConfig {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub test: TestSettings,
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TestSettings {
    pub latency_rounds: usize,
    pub latency_timeout_ms: u64,
    pub download_test_ms: u64,
    pub progress_save_ratio: f64,
}

#[derive(Debug, Default, serde::Deserialize)]
struct FileConfig {
    #[serde(default)]
    server: ServerConfig,
    #[serde(default)]
    database: DatabaseConfig,
    #[serde(default)]
    test: TestSettings,
    #[serde(default)]
    domains: DomainConfig,
}

#[derive(Debug, Default, serde::Deserialize)]
struct DomainConfig {
    external_yaml_path: Option<PathBuf>,
    #[serde(default)]
    items: Vec<DomainItem>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DomainItem {
    pub name: String,
    pub url: String,
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
    #[error("TOML 配置解析失败: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("YAML 线路配置解析失败: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("配置中没有可用线路")]
    NoDomains,
    #[error("线路 URL 无效: {0}")]
    InvalidDomainUrl(String),
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
        let domain_items = load_domain_items(&file_config.domains)?;
        let targets = build_targets(&domain_items)?;
        if targets.is_empty() {
            return Err(ConfigError::NoDomains);
        }

        Ok(Self {
            server: file_config.server,
            database: file_config.database,
            test: file_config.test.normalized(),
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

impl Default for TestSettings {
    fn default() -> Self {
        Self {
            latency_rounds: 5,
            latency_timeout_ms: 3500,
            download_test_ms: 8500,
            progress_save_ratio: 0.5,
        }
    }
}

impl TestSettings {
    fn normalized(mut self) -> Self {
        self.latency_rounds = self.latency_rounds.clamp(1, 20);
        self.latency_timeout_ms = self.latency_timeout_ms.clamp(300, 30_000);
        self.download_test_ms = self.download_test_ms.clamp(1_000, 120_000);
        self.progress_save_ratio = self.progress_save_ratio.clamp(0.05, 0.95);
        self
    }
}

fn load_domain_items(config: &DomainConfig) -> Result<Vec<DomainItem>, ConfigError> {
    if let Some(path) = &config.external_yaml_path
        && path.is_file()
    {
        let external =
            serde_yaml::from_str::<ExternalDomainsFile>(&std::fs::read_to_string(path)?)?;
        if !external.domains.is_empty() {
            return Ok(external.domains);
        }
    }

    Ok(config.items.clone())
}

fn build_targets(items: &[DomainItem]) -> Result<Vec<TestTarget>, ConfigError> {
    let mut used_keys = HashSet::new();
    items
        .iter()
        .map(|item| {
            let base_url = normalize_base_url(&item.url)?;
            let host = base_url
                .strip_prefix("https://")
                .or_else(|| base_url.strip_prefix("http://"))
                .unwrap_or(&base_url)
                .to_string();
            let mut key = slugify(&host);
            if key.is_empty() {
                key = slugify(&item.name);
            }
            let unique_key = unique_key(key, &mut used_keys);

            Ok(TestTarget {
                key: unique_key,
                label: item.name.clone(),
                host,
                trace_url: format!("{base_url}/cdn-cgi/trace"),
                download_url: format!("{base_url}/200mb.test"),
            })
        })
        .collect()
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
    fn runtime_config_should_read_server_and_test_options_from_toml() {
        let config = RuntimeConfig::from_toml_str(
            r#"
            [server]
            host = "0.0.0.0"
            port = 52143

            [test]
            latency_rounds = 7
            latency_timeout_ms = 2200
            download_test_ms = 12000
            progress_save_ratio = 0.35

            [[domains.items]]
            name = "a1 线路"
            url = "https://a1.example.com"
            "#,
        )
        .expect("config should parse");

        assert_eq!(config.listen_addr(), "0.0.0.0:52143");
        assert_eq!(config.test.latency_rounds, 7);
        assert_eq!(config.test.download_test_ms, 12000);
        assert_eq!(config.test.progress_save_ratio, 0.35);
    }

    #[test]
    fn runtime_config_should_read_domains_from_external_yaml() {
        let yaml_path =
            std::env::temp_dir().join(format!("web-speed-domains-{}.yaml", std::process::id()));
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
            [domains]
            external_yaml_path = '{}'

            [[domains.items]]
            name = "fallback"
            url = "https://fallback.example.com"
            "#,
            yaml_path.display()
        );

        let config = RuntimeConfig::from_toml_str(&toml).expect("config should parse");
        let _ = std::fs::remove_file(&yaml_path);

        assert_eq!(config.targets.len(), 2);
        assert_eq!(config.targets[0].label, "19931110 线路");
        assert_eq!(
            config.targets[0].trace_url,
            "https://line-a.example.com/cdn-cgi/trace"
        );
        assert_eq!(
            config.targets[1].download_url,
            "https://line-b.example.com/200mb.test"
        );
    }

    #[test]
    fn runtime_config_should_reject_missing_config_file() {
        let missing_path = std::env::temp_dir().join(format!(
            "web-speed-missing-config-{}.toml",
            std::process::id()
        ));
        let result = RuntimeConfig::load(&missing_path);

        assert!(matches!(result, Err(ConfigError::MissingConfig(path)) if path == missing_path));
    }

    #[test]
    fn runtime_config_should_reject_empty_domain_sources() {
        let result = RuntimeConfig::from_toml_str(
            r#"
            [server]
            host = "127.0.0.1"
            port = 3000
            "#,
        );

        assert!(matches!(result, Err(ConfigError::NoDomains)));
    }
}
