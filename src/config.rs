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
#[derive(Clone, Deserialize)]
pub struct FetchConfig {
    #[serde(default = "default_fetch_max_content_chars")]
    pub max_content_chars: usize,
    #[serde(default = "default_fetch_min_content_chars")]
    pub min_content_chars: usize,
    /// 默认抓取渠道顺序；未列出的渠道排在后面。请求显式传 channels 时以请求顺序为准。
    #[serde(default = "default_fetch_channel_order")]
    pub channel_order: Vec<String>,
    /// GitHub issue/PR/release 专项抓取使用的 token；为空时回退环境变量 `GITHUB_TOKEN`
    #[serde(default)]
    pub github_token: String,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            max_content_chars: default_fetch_max_content_chars(),
            min_content_chars: default_fetch_min_content_chars(),
            channel_order: default_fetch_channel_order(),
            github_token: String::new(),
        }
    }
}

/// 手写 Debug：只报告 token 是否配置，避免 `{:?}` 把凭据打进日志。
impl std::fmt::Debug for FetchConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let github_token = if self.github_token.is_empty() {
            "unset"
        } else {
            "set"
        };
        formatter
            .debug_struct("FetchConfig")
            .field("max_content_chars", &self.max_content_chars)
            .field("min_content_chars", &self.min_content_chars)
            .field("channel_order", &self.channel_order)
            .field("github_token", &github_token)
            .finish()
    }
}

fn default_fetch_max_content_chars() -> usize {
    10000
}

fn default_fetch_min_content_chars() -> usize {
    500
}

fn default_fetch_channel_order() -> Vec<String> {
    ["firecrawl", "tavily", "exa", "tinyfish"]
        .iter()
        .map(|name| name.to_string())
        .collect()
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
    /// 是否把搜索结果明细写入日志；默认关闭，只记查询与统计字段
    #[serde(default = "default_log_include_results")]
    pub include_results: bool,
    /// `include_results = true` 时每条结果正文的截断长度（0 = 不记录正文）
    #[serde(default = "default_log_content_chars")]
    pub content_chars: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            enabled: default_log_enabled(),
            path: default_log_path(),
            max_size_mb: default_log_max_mb(),
            include_results: default_log_include_results(),
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

fn default_log_include_results() -> bool {
    false
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
    /// agent2api = 走 agent2api 搜索接口；export = 从 agent2api 导出接口取账号后直连上游并回退 agent2api
    #[serde(default)]
    pub mode: WorkBuddyMode,
    /// agent2api 地址与 key；两种模式都用作回退，export 模式还用于调导出接口
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_workbuddy_provider")]
    pub provider: String,
    /// export 模式：上游单次条数（服务端硬上限 10）
    #[serde(default = "default_workbuddy_max_results")]
    pub max_results: usize,
    /// export 模式：账号缓存秒数，过期后重新调导出接口
    #[serde(default = "default_workbuddy_account_ttl_secs")]
    pub account_ttl_secs: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
pub enum WorkBuddyMode {
    #[default]
    #[serde(rename = "agent2api")]
    Agent2Api,
    #[serde(rename = "export")]
    Export,
}

fn default_workbuddy_provider() -> String {
    "workbuddy".to_string()
}

fn default_workbuddy_max_results() -> usize {
    10
}

fn default_workbuddy_account_ttl_secs() -> u64 {
    600
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
        assert_eq!(
            config.fetch.channel_order,
            ["firecrawl", "tavily", "exa", "tinyfish"]
        );
        assert!(config.fetch.github_token.is_empty());
        assert!(!config.log.include_results);
        assert!(config.proxy.is_none());
    }

    #[test]
    fn fetch_debug_redacts_github_token() {
        let config: Config = toml::from_str(
            r#"
[server]
bind = "127.0.0.1:3088"

[fetch]
github_token = "ghp_secret_value"
"#,
        )
        .unwrap();
        let debug = format!("{:?}", config.fetch);
        assert!(debug.contains("github_token: \"set\""));
        assert!(!debug.contains("ghp_secret_value"));
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
