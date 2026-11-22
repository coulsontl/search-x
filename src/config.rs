use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub router: RouterConfig,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub fetch: FetchConfig,
    #[serde(default)]
    pub proxy: Option<ProxyConfig>,
    #[serde(default)]
    pub channels: ChannelsConfig,
}

/// 查询结果缓存：相同 query + 模式 + limit 在 TTL 内直接返回，省渠道调用费用。
#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    #[serde(default = "default_cache_enabled")]
    pub enabled: bool,
    #[serde(default = "default_cache_ttl_secs")]
    pub ttl_secs: u64,
    #[serde(default = "default_cache_max_entries")]
    pub max_entries: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: default_cache_enabled(),
            ttl_secs: default_cache_ttl_secs(),
            max_entries: default_cache_max_entries(),
        }
    }
}

fn default_cache_enabled() -> bool {
    true
}

fn default_cache_ttl_secs() -> u64 {
    300
}

fn default_cache_max_entries() -> usize {
    500
}

/// 抓取行为：单页正文返回上限，以及"内容过短时继续尝试下一个渠道"的阈值。
#[derive(Debug, Clone, Deserialize)]
pub struct FetchConfig {
    #[serde(default = "default_fetch_max_content_chars")]
    pub max_content_chars: usize,
    #[serde(default = "default_fetch_min_content_chars")]
    pub min_content_chars: usize,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            max_content_chars: default_fetch_max_content_chars(),
            min_content_chars: default_fetch_min_content_chars(),
        }
    }
}

fn default_fetch_max_content_chars() -> usize {
    10000
}

fn default_fetch_min_content_chars() -> usize {
    500
}

/// 所有渠道的出站 HTTP 代理；no_proxy 里的主机（内网自建服务）保持直连。
#[derive(Debug, Clone, Deserialize)]
pub struct ProxyConfig {
    pub url: String,
    #[serde(default = "default_no_proxy")]
    pub no_proxy: String,
}

fn default_no_proxy() -> String {
    "localhost,127.0.0.1,::1,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct LogConfig {
    #[serde(default = "default_log_enabled")]
    pub enabled: bool,
    #[serde(default = "default_log_path")]
    pub path: String,
    #[serde(default = "default_log_max_mb")]
    pub max_size_mb: u64,
    /// 每条结果正文写入日志的截断长度（0 = 不记录正文）
    #[serde(default = "default_log_content_chars")]
    pub content_chars: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            enabled: default_log_enabled(),
            path: default_log_path(),
            max_size_mb: default_log_max_mb(),
            content_chars: default_log_content_chars(),
        }
    }
}

fn default_log_enabled() -> bool {
    true
}

fn default_log_path() -> String {
    "logs/search.jsonl".to_string()
}

fn default_log_max_mb() -> u64 {
    100
}

fn default_log_content_chars() -> usize {
    200
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    /// 默认开启：配置缺失时宁可拒绝，也不意外对外开放
    #[serde(default = "default_auth_enabled")]
    pub enabled: bool,
    /// 允许访问的 API key；空 = 认证开启但没有可用 key（全部拒绝）
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub mode: AuthMode,
    /// mode = custom 时读取的 header 名
    #[serde(default)]
    pub custom_header: Option<String>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: default_auth_enabled(),
            keys: Vec::new(),
            mode: AuthMode::default(),
            custom_header: None,
        }
    }
}

fn default_auth_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    #[default]
    Bearer,
    XApiKey,
    Both,
    Custom,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouterConfig {
    #[serde(default = "default_mode")]
    pub default_mode: String,
    #[serde(default = "default_limit")]
    pub default_limit: usize,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            default_mode: default_mode(),
            default_limit: default_limit(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ChannelsConfig {
    pub searxng: Option<SearxngConfig>,
    pub exa: Option<ChannelConfig>,
    pub tavily: Option<ChannelConfig>,
    pub tinyfish: Option<TinyFishConfig>,
    pub firecrawl: Option<ChannelConfig>,
    pub grok: Option<GrokConfig>,
    pub workbuddy: Option<WorkBuddyConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearxngConfig {
    pub base_url: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelConfig {
    pub base_url: String,
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub strategy: KeyStrategy,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TinyFishConfig {
    pub search_url: String,
    pub fetch_url: String,
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub strategy: KeyStrategy,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GrokConfig {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub strategy: KeyStrategy,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub fallback_only: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkBuddyConfig {
    pub base_url: String,
    pub api_key: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyStrategy {
    #[default]
    RoundRobin,
    Random,
    Failover,
}

fn default_bind() -> String {
    "0.0.0.0:3088".to_string()
}

fn default_mode() -> String {
    "balanced".to_string()
}

fn default_limit() -> usize {
    20
}

fn default_timeout() -> u64 {
    15
}

fn default_enabled() -> bool {
    true
}

impl Config {
    pub fn load(path: &str) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|error| format!("无法读取配置文件 {path}: {error}"))?;
        toml::from_str(&raw).map_err(|error| format!("配置文件解析失败: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn defaults_are_secure_and_sane() {
        let config: Config = toml::from_str(
            r#"
[server]
bind = "0.0.0.0:3088"
"#,
        )
        .unwrap();
        assert!(config.auth.enabled, "认证必须默认开启");
        assert!(config.auth.keys.is_empty(), "没有 key 时全部拒绝");
        assert_eq!(config.router.default_mode, "balanced");
        assert_eq!(config.router.default_limit, 20);
        assert!(config.cache.enabled);
        assert_eq!(config.cache.ttl_secs, 300);
        assert_eq!(config.fetch.max_content_chars, 10000);
        assert_eq!(config.fetch.min_content_chars, 500);
        assert!(config.proxy.is_none());
    }

    #[test]
    fn proxy_defaults_bypass_private_ranges() {
        let config: Config = toml::from_str(
            r#"
[server]
bind = "127.0.0.1:3088"

[proxy]
url = "http://proxy.example.com:7890"
"#,
        )
        .unwrap();
        let proxy = config.proxy.unwrap();
        assert!(proxy.no_proxy.contains("10.0.0.0/8"));
        assert!(proxy.no_proxy.contains("192.168.0.0/16"));
        assert!(proxy.no_proxy.contains("localhost"));
    }
}
