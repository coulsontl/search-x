use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::cache::QueryCache;
use crate::channels::exa::ExaChannel;
use crate::channels::firecrawl::FirecrawlChannel;
use crate::channels::grok::GrokChannel;
use crate::channels::searxng::SearxngChannel;
use crate::channels::tavily::TavilyChannel;
use crate::channels::tinyfish::TinyFishChannel;
use crate::channels::workbuddy::{UpstreamWorkBuddyChannel, WorkBuddyChannel};
use crate::channels::Channel;
use crate::config::{AuthConfig, Config, ProxyConfig, WorkBuddyMode};
use crate::github_fetch::GithubFetcher;
use crate::keypool::KeyPool;
use crate::logger::SearchLogger;
use crate::router::Router;
use crate::workbuddy_accounts::WorkBuddyAccountStore;

pub struct Runtime {
    pub router: Arc<Router>,
    pub auth: AuthConfig,
}

const GROK_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// 所有渠道共用一个出站客户端；grok 额外带浏览器 UA（Cloudflare 会拦非浏览器 UA）。
fn build_http_client(
    proxy: Option<&ProxyConfig>,
    user_agent: Option<&str>,
) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder();
    if let Some(user_agent) = user_agent {
        builder = builder.user_agent(user_agent);
    }
    if let Some(proxy) = proxy {
        let mut http_proxy =
            reqwest::Proxy::all(&proxy.url).map_err(|error| format!("proxy.url 无效: {error}"))?;
        if !proxy.no_proxy.trim().is_empty() {
            http_proxy = http_proxy.no_proxy(reqwest::NoProxy::from_string(&proxy.no_proxy));
        }
        builder = builder.proxy(http_proxy);
        tracing::info!("出站代理: {}（no_proxy: {}）", proxy.url, proxy.no_proxy);
    }
    builder
        .build()
        .map_err(|error| format!("构建 HTTP 客户端失败: {error}"))
}

pub fn build(config: &Config) -> Result<Runtime, String> {
    let shared_client = build_http_client(config.proxy.as_ref(), None)?;
    let grok_client = build_http_client(config.proxy.as_ref(), Some(GROK_USER_AGENT))?;
    let mut adapters: Vec<Arc<dyn Channel>> = Vec::new();
    let mut fallback: Vec<Arc<dyn Channel>> = Vec::new();
    let mut timeouts: HashMap<String, Duration> = HashMap::new();

    if let Some(cfg) = &config.channels.searxng {
        if cfg.enabled {
            adapters.push(Arc::new(SearxngChannel::new(
                cfg.base_url.clone(),
                shared_client.clone(),
            )));
            timeouts.insert("searxng".into(), Duration::from_secs(cfg.timeout_secs));
        }
    }

    if let Some(cfg) = &config.channels.exa {
        if cfg.enabled && !cfg.keys.is_empty() {
            adapters.push(Arc::new(ExaChannel::new(
                cfg.base_url.clone(),
                KeyPool::new(cfg.keys.clone(), cfg.strategy),
                shared_client.clone(),
            )));
            timeouts.insert("exa".into(), Duration::from_secs(cfg.timeout_secs));
        }
    }

    if let Some(cfg) = &config.channels.tavily {
        if cfg.enabled && !cfg.keys.is_empty() {
            adapters.push(Arc::new(TavilyChannel::new(
                cfg.base_url.clone(),
                KeyPool::new(cfg.keys.clone(), cfg.strategy),
                shared_client.clone(),
            )));
            timeouts.insert("tavily".into(), Duration::from_secs(cfg.timeout_secs));
        }
    }

    if let Some(cfg) = &config.channels.tinyfish {
        if cfg.enabled && !cfg.keys.is_empty() {
            adapters.push(Arc::new(TinyFishChannel::new(
                cfg.search_url.clone(),
                cfg.fetch_url.clone(),
                KeyPool::new(cfg.keys.clone(), cfg.strategy),
                shared_client.clone(),
            )));
            timeouts.insert("tinyfish".into(), Duration::from_secs(cfg.timeout_secs));
        }
    }

    if let Some(cfg) = &config.channels.firecrawl {
        if cfg.enabled && !cfg.keys.is_empty() {
            adapters.push(Arc::new(FirecrawlChannel::new(
                cfg.base_url.clone(),
                KeyPool::new(cfg.keys.clone(), cfg.strategy),
                shared_client.clone(),
            )));
            timeouts.insert("firecrawl".into(), Duration::from_secs(cfg.timeout_secs));
        }
    }

    if let Some(cfg) = &config.channels.workbuddy {
        if cfg.enabled {
            // agent2api 通道同时是 export 模式的回退。
            let agent2api: Option<Arc<dyn Channel>> = if cfg.base_url.trim().is_empty() {
                None
            } else {
                Some(Arc::new(WorkBuddyChannel::new(
                    cfg.base_url.clone(),
                    cfg.api_key.clone(),
                    shared_client.clone(),
                )))
            };
            match cfg.mode {
                WorkBuddyMode::Agent2Api => match agent2api {
                    Some(channel) => adapters.push(channel),
                    None => {
                        tracing::warn!("workbuddy 处于 agent2api 模式但 base_url 为空，渠道未启用")
                    }
                },
                WorkBuddyMode::Export => {
                    if cfg.base_url.trim().is_empty() {
                        tracing::warn!("workbuddy export 模式缺少 base_url，渠道未启用");
                    } else {
                        let accounts = Arc::new(WorkBuddyAccountStore::new(
                            shared_client.clone(),
                            cfg.base_url.clone(),
                            cfg.api_key.clone(),
                            cfg.provider.clone(),
                            cfg.account_ttl_secs,
                        ));
                        if agent2api.is_none() {
                            tracing::warn!("workbuddy export 模式没有配置 agent2api 回退通道");
                        }
                        tracing::info!(
                            "workbuddy export 模式: provider={} max_results={} api={}/api/accounts/export-files",
                            cfg.provider,
                            cfg.max_results.clamp(1, 10),
                            cfg.base_url.trim_end_matches('/')
                        );
                        adapters.push(Arc::new(UpstreamWorkBuddyChannel::new(
                            shared_client.clone(),
                            accounts,
                            cfg.max_results,
                            agent2api,
                        )));
                    }
                }
            }
            timeouts.insert("workbuddy".into(), Duration::from_secs(cfg.timeout_secs));
        }
    }

    if let Some(cfg) = &config.channels.grok {
        if cfg.enabled && !cfg.keys.is_empty() {
            let channel: Arc<dyn Channel> = Arc::new(GrokChannel::new(
                cfg.base_url.clone(),
                cfg.model.clone(),
                KeyPool::new(cfg.keys.clone(), cfg.strategy),
                grok_client.clone(),
            ));
            timeouts.insert("grok".into(), Duration::from_secs(cfg.timeout_secs));
            if cfg.fallback_only {
                fallback.push(channel);
            } else {
                adapters.push(channel);
            }
        }
    }

    if adapters.is_empty() && fallback.is_empty() {
        return Err("没有启用任何渠道，请检查配置文件".to_string());
    }
    if config.auth.enabled && config.auth.keys.is_empty() {
        tracing::warn!("认证已启用但没有任何 API key，所有请求都会被拒绝");
    }
    if !config.auth.enabled {
        tracing::warn!("认证未启用，所有接口对外公开");
    }

    let cache = if config.cache.enabled {
        tracing::info!(
            "查询缓存已启用: ttl={}s max_entries={}",
            config.cache.ttl_secs,
            config.cache.max_entries
        );
        Some(Arc::new(QueryCache::new(
            config.cache.ttl_secs,
            config.cache.max_entries,
        )))
    } else {
        None
    };

    let logger = if config.log.enabled {
        Some(Arc::new(SearchLogger::new(
            std::path::PathBuf::from(&config.log.path),
            config.log.max_size_mb,
            config.log.include_results,
            config.log.content_chars,
        )))
    } else {
        None
    };

    let github_token = if config.fetch.github_token.trim().is_empty() {
        std::env::var("GITHUB_TOKEN")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    } else {
        Some(config.fetch.github_token.trim().to_string())
    };
    tracing::info!(
        "github 专项抓取: token={}",
        if github_token.is_some() {
            "set"
        } else {
            "unset"
        }
    );
    let github = Some(Arc::new(GithubFetcher::new(
        shared_client.clone(),
        github_token,
    )));

    Ok(Runtime {
        router: Arc::new(Router::new(
            adapters,
            fallback,
            timeouts,
            config.router.default_mode.clone(),
            config.router.default_limit,
            config.search.max_content_chars,
            logger,
            cache,
            config.fetch.max_content_chars,
            config.fetch.min_content_chars,
            config.fetch.channel_order.clone(),
            github,
        )),
        auth: config.auth.clone(),
    })
}

pub fn build_from_path(path: &str) -> Result<Runtime, String> {
    let mut config = Config::load(path)?;
    import_grok_key(&mut config);
    build(&config)
}

/// 配置里 grok 没有 key 时，尝试从 Claude Code 的 MCP 配置读取 X-Grok-Api-Key。
pub fn import_grok_key(config: &mut Config) {
    let Some(grok) = config.channels.grok.as_mut() else {
        return;
    };
    if !grok.keys.is_empty() {
        return;
    }
    let path = std::env::var("CLAUDE_CONFIG_PATH").unwrap_or_else(|_| {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default();
        format!("{home}/.claude.json")
    });
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    if let Some(key) = value["mcpServers"]["grok-search-rs"]["headers"]["X-Grok-Api-Key"].as_str() {
        grok.keys.push(key.to_string());
        tracing::info!("grok key 已从 {path} 导入");
    }
}
