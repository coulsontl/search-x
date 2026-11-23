use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;

use crate::cache::QueryCache;
use crate::channels::Channel;
use crate::github_fetch::GithubFetcher;
use crate::logger::SearchLogger;
use crate::model::{
    move_searxng_last, rrf_merge, ChannelError, FetchRequest, FetchResponse, PageResult,
    SearchRequest, SearchResponse, SearchResult,
};

fn fetch_priority(order: &[String], name: &str) -> usize {
    order
        .iter()
        .position(|item| item == name)
        .unwrap_or(order.len())
}

/// 按字符（而非字节）截断，避免切坏中文。
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

/// 空正文的结果对搜索没有价值，进入 RRF 前丢弃。
fn non_empty_results(items: Vec<SearchResult>) -> Vec<SearchResult> {
    items
        .into_iter()
        .filter(|item| !item.content.trim().is_empty())
        .collect()
}

pub struct Router {
    channels: Vec<Arc<dyn Channel>>,
    fallback: Vec<Arc<dyn Channel>>,
    timeouts: HashMap<String, Duration>,
    default_mode: String,
    default_limit: usize,
    balanced_channels: Vec<String>,
    fast_limit: usize,
    fast_channels: Vec<String>,
    fast_timeout_secs: u64,
    search_max_content_chars: usize,
    logger: Option<Arc<SearchLogger>>,
    cache: Option<Arc<QueryCache>>,
    fetch_max_content_chars: usize,
    fetch_min_content_chars: usize,
    fetch_channel_order: Vec<String>,
    github: Option<Arc<GithubFetcher>>,
}

impl Router {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        channels: Vec<Arc<dyn Channel>>,
        fallback: Vec<Arc<dyn Channel>>,
        timeouts: HashMap<String, Duration>,
        default_mode: String,
        default_limit: usize,
        balanced_channels: Vec<String>,
        fast_limit: usize,
        fast_channels: Vec<String>,
        fast_timeout_secs: u64,
        search_max_content_chars: usize,
        logger: Option<Arc<SearchLogger>>,
        cache: Option<Arc<QueryCache>>,
        fetch_max_content_chars: usize,
        fetch_min_content_chars: usize,
        fetch_channel_order: Vec<String>,
        github: Option<Arc<GithubFetcher>>,
    ) -> Self {
        Self {
            channels,
            fallback,
            timeouts,
            default_mode,
            default_limit,
            balanced_channels,
            fast_limit,
            fast_channels,
            fast_timeout_secs,
            search_max_content_chars,
            logger,
            cache,
            fetch_max_content_chars,
            fetch_min_content_chars,
            fetch_channel_order,
            github,
        }
    }

    pub fn default_mode(&self) -> &str {
        &self.default_mode
    }

    pub fn fast_limit(&self) -> usize {
        self.fast_limit
    }

    pub fn log_mcp_search(
        &self,
        queries: &[String],
        mode: &str,
        limit: Option<usize>,
        response_chars: usize,
        truncated: bool,
        omitted_results: usize,
    ) {
        if let Some(logger) = &self.logger {
            logger.log_mcp_search(
                queries,
                mode,
                limit,
                response_chars,
                truncated,
                omitted_results,
            );
        }
    }

    pub fn log_mcp_fetch(
        &self,
        urls: &[String],
        response_chars: usize,
        truncated: bool,
        omitted_pages: usize,
    ) {
        if let Some(logger) = &self.logger {
            logger.log_mcp_fetch(urls, response_chars, truncated, omitted_pages);
        }
    }

    pub fn channel_status(&self) -> Vec<serde_json::Value> {
        let mut status: Vec<serde_json::Value> = self
            .channels
            .iter()
            .chain(self.fallback.iter())
            .map(|channel| {
                serde_json::json!({
                    "channel": channel.name(),
                    "supports_fetch": channel.supports_fetch(),
                    "fallback_only": self.fallback.iter().any(|entry| entry.name() == channel.name()),
                })
            })
            .collect();
        if self.github.is_some() {
            status.push(serde_json::json!({
                "channel": "github",
                "supports_fetch": true,
                "fallback_only": false,
            }));
        }
        status
    }

    pub async fn search(&self, request: SearchRequest) -> Vec<SearchResponse> {
        self.search_with_source(request, "rest").await
    }

    pub async fn search_with_source(
        &self,
        request: SearchRequest,
        source: &str,
    ) -> Vec<SearchResponse> {
        let mode = request
            .mode
            .as_deref()
            .unwrap_or(&self.default_mode)
            .to_string();
        let mut limit = request.limit.unwrap_or(self.default_limit).clamp(1, 100);
        // fast 不走并行渠道：按 fast_channels 串行尝试，第一个有结果的渠道胜出。
        // 调用方显式传 channels 时仍按显式渠道并行，保持原有覆盖语义（含 limit）。
        let fast_serial = mode == "fast" && request.channels.is_none();
        if fast_serial {
            limit = limit.min(self.fast_limit.max(1));
        }
        let explicit_channels = request.channels.is_some();
        let selected = self.select_channels(&mode, request.channels.as_deref());
        let cache_scope = match request.channels.as_deref() {
            Some(channels) => {
                // 渠道顺序不影响结果，排序后作为缓存 key，避免 ["exa","tavily"] 与 ["tavily","exa"] 各存一份
                let mut names: Vec<&str> = channels.iter().map(String::as_str).collect();
                names.sort_unstable();
                names.dedup();
                names.join(",")
            }
            None => mode.clone(),
        };
        let selected_ref = &selected;
        let bypass_cache = request.no_cache.unwrap_or(false);
        let futures = request.queries.iter().map(|query| {
            let cache_key = format!("{query}\u{1f}{cache_scope}\u{1f}{limit}");
            let cached = if bypass_cache {
                None
            } else {
                self.cache.as_ref().and_then(|cache| cache.get(&cache_key))
            };
            async move {
                if let Some(mut response) = cached {
                    response.cached = true;
                    response.elapsed_ms = 0;
                    // 命中缓存没有发生渠道调用，不把首次耗时当成本次耗时。
                    response.channel_elapsed_ms.clear();
                    return response;
                }
                let response = if fast_serial {
                    self.search_fast(query, limit).await
                } else {
                    self.search_one(query, selected_ref, explicit_channels, limit)
                        .await
                };
                // 不缓存空结果或带错误的降级响应，避免瞬时故障污染后续 5 分钟的同查询。
                if let Some(cache) = &self.cache {
                    if !response.results.is_empty() && response.errors.is_empty() {
                        cache.put(cache_key, response.clone());
                    }
                }
                response
            }
        });
        let responses = join_all(futures).await;
        if let Some(logger) = &self.logger {
            // 缓存命中也要记录（轻量行），用于统计命中率。
            for response in &responses {
                logger.log(
                    source,
                    &mode,
                    limit,
                    bypass_cache,
                    request.channels.as_deref(),
                    response,
                );
            }
        }
        responses
    }

    pub async fn fetch(&self, request: FetchRequest) -> FetchResponse {
        self.fetch_with_source(request, "rest").await
    }

    pub async fn fetch_with_source(&self, request: FetchRequest, source: &str) -> FetchResponse {
        let started = Instant::now();
        let mut selected: Vec<Arc<dyn Channel>> = self
            .channels
            .iter()
            .filter(|channel| channel.supports_fetch())
            .filter(|channel| {
                request
                    .channels
                    .as_deref()
                    .is_none_or(|names| names.iter().any(|name| name == channel.name()))
            })
            .cloned()
            .collect();
        // 未指定 channels 时按抓取质量排序；显式指定时尊重调用方顺序。
        match request.channels.as_deref() {
            Some(names) => selected.sort_by_key(|channel| {
                names
                    .iter()
                    .position(|name| name == channel.name())
                    .unwrap_or(usize::MAX)
            }),
            None => selected
                .sort_by_key(|channel| fetch_priority(&self.fetch_channel_order, channel.name())),
        }

        // 多个 URL 并行抓取；单个 URL 内部仍按渠道顺序降级。
        // `channels` 显式指定时，只有包含 "github" 才会启用 GitHub 专项抓取。
        let allow_github = request
            .channels
            .as_deref()
            .is_none_or(|names| names.iter().any(|name| name == "github"));
        let selected_ref = &selected;
        let futures = request
            .urls
            .iter()
            .map(|url| self.fetch_one(url, selected_ref, allow_github));
        let outcomes = join_all(futures).await;

        let mut pages = Vec::new();
        let mut errors = Vec::new();
        for (url, outcome) in request.urls.iter().zip(outcomes) {
            match outcome {
                Ok(mut items) => {
                    for page in items.iter_mut() {
                        page.content = truncate_chars(&page.content, self.fetch_max_content_chars);
                    }
                    pages.extend(items);
                }
                Err(mut channel_errors) => {
                    errors.append(&mut channel_errors);
                    errors.push(ChannelError::new(
                        "router",
                        "fetch_failed",
                        format!("所有渠道都无法抓取 {url}"),
                    ));
                }
            }
        }
        let response = FetchResponse {
            pages,
            errors,
            elapsed_ms: started.elapsed().as_millis() as u64,
        };
        if let Some(logger) = &self.logger {
            logger.log_fetch(source, &response);
        }
        response
    }

    /// 单个 URL：按渠道顺序尝试；内容过短（如 SPA 只抓到 300 字导航文本）时继续尝试下一个，
    /// 最终采用内容最长的结果。内容达到 min_content_chars 即提前停止，避免不必要的付费调用。
    async fn fetch_one(
        &self,
        url: &str,
        channels: &[Arc<dyn Channel>],
        allow_github: bool,
    ) -> Result<Vec<PageResult>, Vec<ChannelError>> {
        let urls = [url.to_string()];
        let mut errors = Vec::new();
        // GitHub issue/PR/release 走专用 GitHub API，内容通常比通用抓取完整；
        // 只有专用抓取失败或为空时才回退到普通渠道。
        if allow_github {
            if let Some(github) = &self.github {
                if github.matches(url) {
                    match github.fetch(url, self.timeout_of("github")).await {
                        Ok(page) if !page.content.trim().is_empty() => return Ok(vec![page]),
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(
                                "github 专项抓取失败（{}），回退普通渠道: {}",
                                error.code,
                                error.message
                            );
                            errors.push(error);
                        }
                    }
                }
            }
        }
        let mut best: Option<Vec<PageResult>> = None;
        let mut best_length = 0;
        for channel in channels {
            let timeout = self.timeout_of(channel.name());
            match channel.fetch(&urls, timeout).await {
                Ok(items) if !items.is_empty() => {
                    let length = items
                        .iter()
                        .map(|page| page.content.chars().count())
                        .max()
                        .unwrap_or(0);
                    if length > best_length {
                        best_length = length;
                        best = Some(items);
                    }
                    if best_length >= self.fetch_min_content_chars {
                        break;
                    }
                }
                Ok(_) => {}
                Err(error) => errors.push(error),
            }
        }
        match best {
            Some(items) => Ok(items),
            None => Err(errors),
        }
    }

    fn select_channels(&self, mode: &str, requested: Option<&[String]>) -> Vec<Arc<dyn Channel>> {
        if let Some(requested) = requested {
            // 显式指定 channels 时允许直接选中 fallback_only 渠道（例如 channels: ["exa", "grok"]）。
            return self
                .channels
                .iter()
                .chain(self.fallback.iter())
                .filter(|channel| requested.iter().any(|name| name == channel.name()))
                .cloned()
                .collect();
        }
        let names: Vec<&str> = match mode {
            "full" => self.channels.iter().map(|channel| channel.name()).collect(),
            // balanced 默认编排；fast 无显式 channels 时走 search_fast 的串行顺序，
            // 这里的列表只用于未知 mode 或默认 balanced 的并行路径。
            _ => self.balanced_channels.iter().map(String::as_str).collect(),
        };
        self.channels
            .iter()
            .filter(|channel| names.contains(&channel.name()))
            .cloned()
            .collect()
    }

    async fn search_one(
        &self,
        query: &str,
        channels: &[Arc<dyn Channel>],
        explicit_channels: bool,
        limit: usize,
    ) -> SearchResponse {
        let started = Instant::now();
        if explicit_channels && channels.is_empty() {
            return SearchResponse {
                query: query.to_string(),
                results: Vec::new(),
                channels: Vec::new(),
                errors: vec![ChannelError::new(
                    "router",
                    "no_channels",
                    "指定的渠道均不可用",
                )],
                elapsed_ms: started.elapsed().as_millis() as u64,
                cached: false,
                channel_elapsed_ms: HashMap::new(),
            };
        }
        let calls = channels.iter().map(|channel| {
            let channel = channel.clone();
            let query = query.to_string();
            let name = channel.name().to_string();
            let timeout = self.timeout_of(&name);
            async move {
                let call_started = Instant::now();
                let outcome = channel.search(&query, limit, timeout).await;
                (name, call_started.elapsed().as_millis() as u64, outcome)
            }
        });
        let outcomes = join_all(calls).await;

        let mut per_channel: Vec<Vec<SearchResult>> = Vec::new();
        let mut errors: Vec<ChannelError> = Vec::new();
        let mut used = Vec::new();
        let mut channel_elapsed_ms = HashMap::new();
        for (name, elapsed_ms, outcome) in outcomes {
            channel_elapsed_ms.insert(name.clone(), elapsed_ms);
            match outcome {
                Ok(items) => {
                    let items = non_empty_results(items);
                    if !items.is_empty() {
                        used.push(name);
                        per_channel.push(items);
                    }
                }
                Err(error) => errors.push(error),
            }
        }

        let mut results = rrf_merge(per_channel);

        if results.is_empty() && !self.fallback.is_empty() {
            for channel in &self.fallback {
                // 如果显式 channels 已经跑过这个 fallback 渠道，不再重复调用。
                if channels.iter().any(|entry| entry.name() == channel.name()) {
                    continue;
                }
                let timeout = self.timeout_of(channel.name());
                let call_started = Instant::now();
                let outcome = channel.search(query, limit, timeout).await;
                channel_elapsed_ms.insert(
                    channel.name().to_string(),
                    call_started.elapsed().as_millis() as u64,
                );
                match outcome {
                    Ok(items) => {
                        let items = non_empty_results(items);
                        if !items.is_empty() {
                            used.push(channel.name().to_string());
                            results = rrf_merge(vec![items]);
                            break;
                        }
                    }
                    Err(error) => errors.push(error),
                }
            }
        }

        let mut results = move_searxng_last(results);
        for item in &mut results {
            item.content = truncate_chars(&item.content, self.search_max_content_chars);
        }

        SearchResponse {
            query: query.to_string(),
            results,
            channels: used,
            errors,
            elapsed_ms: started.elapsed().as_millis() as u64,
            cached: false,
            channel_elapsed_ms,
        }
    }

    /// fast 模式的串行兜底：按 `fast_channels` 顺序尝试，第一个有非空结果的渠道胜出。
    /// 只有报错或 0 条结果才继续下一个渠道；有结果就不再调用后面的渠道。
    async fn search_fast(&self, query: &str, limit: usize) -> SearchResponse {
        let started = Instant::now();
        let mut results = Vec::new();
        let mut errors = Vec::new();
        let mut used = Vec::new();
        let mut channel_elapsed_ms = HashMap::new();

        for name in &self.fast_channels {
            let Some(channel) = self.channels.iter().find(|channel| channel.name() == name) else {
                continue;
            };
            let call_started = Instant::now();
            let timeout = self
                .timeout_of(name)
                .min(Duration::from_secs(self.fast_timeout_secs.max(1)));
            let outcome = channel.search(query, limit, timeout).await;
            channel_elapsed_ms.insert(name.clone(), call_started.elapsed().as_millis() as u64);
            match outcome {
                Ok(items) => {
                    let items = non_empty_results(items);
                    if !items.is_empty() {
                        used.push(name.clone());
                        results = rrf_merge(vec![items]);
                        break;
                    }
                }
                Err(error) => errors.push(error),
            }
        }

        if channel_elapsed_ms.is_empty() {
            errors.push(ChannelError::new(
                "fast",
                "no_channels",
                "fast 没有可用渠道",
            ));
        }

        let mut results = move_searxng_last(results);
        for item in &mut results {
            item.content = truncate_chars(&item.content, self.search_max_content_chars);
        }

        SearchResponse {
            query: query.to_string(),
            results,
            channels: used,
            errors,
            elapsed_ms: started.elapsed().as_millis() as u64,
            cached: false,
            channel_elapsed_ms,
        }
    }

    fn timeout_of(&self, name: &str) -> Duration {
        self.timeouts
            .get(name)
            .copied()
            .unwrap_or(Duration::from_secs(15))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;

    use super::{truncate_chars, Router};
    use crate::cache::QueryCache;
    use crate::channels::Channel;
    use crate::model::{ChannelError, FetchRequest, PageResult, SearchRequest, SearchResult};

    struct MockChannel {
        name: &'static str,
        search_results: Vec<SearchResult>,
        fetch_content: String,
        search_calls: Arc<AtomicUsize>,
        fetch_calls: Arc<AtomicUsize>,
    }

    impl MockChannel {
        fn new(
            name: &'static str,
            fetch_content: &str,
        ) -> (Arc<Self>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let search_calls = Arc::new(AtomicUsize::new(0));
            let fetch_calls = Arc::new(AtomicUsize::new(0));
            let channel = Arc::new(Self {
                name,
                search_results: vec![SearchResult {
                    title: "mock".to_string(),
                    url: format!("https://{name}.example.com/"),
                    content: "mock content".to_string(),
                    published_date: None,
                    channel: name.to_string(),
                }],
                fetch_content: fetch_content.to_string(),
                search_calls: search_calls.clone(),
                fetch_calls: fetch_calls.clone(),
            });
            (channel, search_calls, fetch_calls)
        }

        fn with_results(name: &'static str, search_results: Vec<SearchResult>) -> Arc<Self> {
            Arc::new(Self {
                name,
                search_results,
                fetch_content: String::new(),
                search_calls: Arc::new(AtomicUsize::new(0)),
                fetch_calls: Arc::new(AtomicUsize::new(0)),
            })
        }
    }

    #[async_trait]
    impl Channel for MockChannel {
        fn name(&self) -> &'static str {
            self.name
        }

        fn supports_fetch(&self) -> bool {
            true
        }

        async fn search(
            &self,
            _query: &str,
            _limit: usize,
            _timeout: Duration,
        ) -> Result<Vec<SearchResult>, ChannelError> {
            self.search_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.search_results.clone())
        }

        async fn fetch(
            &self,
            urls: &[String],
            _timeout: Duration,
        ) -> Result<Vec<PageResult>, ChannelError> {
            self.fetch_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![PageResult {
                url: urls[0].clone(),
                title: "mock".to_string(),
                content: self.fetch_content.clone(),
                channel: self.name.to_string(),
            }])
        }
    }

    struct FailingChannel {
        name: &'static str,
        search_calls: Arc<AtomicUsize>,
    }

    impl FailingChannel {
        fn new(name: &'static str) -> (Arc<Self>, Arc<AtomicUsize>) {
            let search_calls = Arc::new(AtomicUsize::new(0));
            let channel = Arc::new(Self {
                name,
                search_calls: search_calls.clone(),
            });
            (channel, search_calls)
        }
    }

    #[async_trait]
    impl Channel for FailingChannel {
        fn name(&self) -> &'static str {
            self.name
        }

        fn supports_fetch(&self) -> bool {
            false
        }

        async fn search(
            &self,
            _query: &str,
            _limit: usize,
            _timeout: Duration,
        ) -> Result<Vec<SearchResult>, ChannelError> {
            self.search_calls.fetch_add(1, Ordering::SeqCst);
            Err(ChannelError::new(self.name, "mock_error", "mock failure"))
        }

        async fn fetch(
            &self,
            _urls: &[String],
            _timeout: Duration,
        ) -> Result<Vec<PageResult>, ChannelError> {
            Err(ChannelError::new(self.name, "mock_error", "mock failure"))
        }
    }

    /// 记录渠道实际收到的超时与条数，用于验证 fast 的独立超时与 limit 语义。
    struct TimeoutRecordingChannel {
        name: &'static str,
        received_timeout: Arc<Mutex<Option<Duration>>>,
        received_limit: Arc<Mutex<Option<usize>>>,
        search_results: Vec<SearchResult>,
    }

    #[async_trait]
    impl Channel for TimeoutRecordingChannel {
        fn name(&self) -> &'static str {
            self.name
        }

        fn supports_fetch(&self) -> bool {
            false
        }

        async fn search(
            &self,
            _query: &str,
            limit: usize,
            timeout: Duration,
        ) -> Result<Vec<SearchResult>, ChannelError> {
            *self.received_timeout.lock().unwrap() = Some(timeout);
            *self.received_limit.lock().unwrap() = Some(limit);
            Ok(self.search_results.clone())
        }

        async fn fetch(
            &self,
            _urls: &[String],
            _timeout: Duration,
        ) -> Result<Vec<PageResult>, ChannelError> {
            Err(ChannelError::new("mock", "mock_error", "mock failure"))
        }
    }

    fn fetch_order(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn build_router(
        channels: Vec<Arc<dyn Channel>>,
        cache: Option<Arc<QueryCache>>,
        max_chars: usize,
        min_chars: usize,
        fetch_channel_order: Vec<String>,
    ) -> Router {
        build_router_with_search_cap(
            channels,
            cache,
            max_chars,
            min_chars,
            fetch_channel_order,
            8000,
        )
    }

    fn build_router_with_search_cap(
        channels: Vec<Arc<dyn Channel>>,
        cache: Option<Arc<QueryCache>>,
        max_chars: usize,
        min_chars: usize,
        fetch_channel_order: Vec<String>,
        search_max_content_chars: usize,
    ) -> Router {
        Router::new(
            channels,
            Vec::new(),
            HashMap::new(),
            "balanced".to_string(),
            10,
            fetch_order(&["exa", "tavily", "workbuddy"]),
            10,
            fetch_order(&["workbuddy", "tavily", "exa"]),
            5,
            search_max_content_chars,
            None,
            cache,
            max_chars,
            min_chars,
            fetch_channel_order,
            None,
        )
    }

    fn search_request() -> SearchRequest {
        SearchRequest {
            queries: vec!["test query".to_string()],
            mode: None,
            channels: None,
            limit: None,
            no_cache: None,
        }
    }

    #[test]
    fn truncate_chars_is_character_safe() {
        assert_eq!(truncate_chars("abcdef", 3), "abc");
        assert_eq!(truncate_chars("中文内容", 2), "中文");
        assert_eq!(truncate_chars("短", 10), "短");
    }

    #[tokio::test]
    async fn search_filters_empty_content_and_truncates() {
        let channel = MockChannel::with_results(
            "exa",
            vec![
                SearchResult {
                    title: "empty".to_string(),
                    url: "https://empty.example.com/".to_string(),
                    content: "   ".to_string(),
                    published_date: None,
                    channel: "exa".to_string(),
                },
                SearchResult {
                    title: "long".to_string(),
                    url: "https://long.example.com/".to_string(),
                    content: "中".repeat(20),
                    published_date: None,
                    channel: "exa".to_string(),
                },
                SearchResult {
                    title: "short".to_string(),
                    url: "https://short.example.com/".to_string(),
                    content: "ok".to_string(),
                    published_date: None,
                    channel: "exa".to_string(),
                },
            ],
        );
        let router =
            build_router_with_search_cap(vec![channel], None, 10000, 500, fetch_order(&["exa"]), 5);

        let response = router.search(search_request()).await;
        let results = &response[0].results;
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|item| !item.content.trim().is_empty()));
        let long = results
            .iter()
            .find(|item| item.url == "https://long.example.com/")
            .unwrap();
        assert_eq!(long.content, "中".repeat(5));
        assert!(!results.iter().any(|item| item.title == "empty"));
    }

    #[tokio::test]
    async fn cache_hit_and_no_cache_bypass() {
        let (channel, search_calls, _) = MockChannel::new("exa", "content");
        let router = build_router(
            vec![channel],
            Some(Arc::new(QueryCache::new(60, 10))),
            10000,
            500,
            fetch_order(&["firecrawl", "tavily", "exa", "tinyfish"]),
        );

        let first = router.search(search_request()).await;
        let second = router.search(search_request()).await;
        assert!(!first[0].cached);
        assert!(
            first[0].channel_elapsed_ms.contains_key("exa"),
            "首次搜索应记录每渠道耗时"
        );
        assert!(second[0].cached);
        assert_eq!(second[0].elapsed_ms, 0);
        assert!(
            second[0].channel_elapsed_ms.is_empty(),
            "缓存命中不应带首次的渠道耗时"
        );
        assert_eq!(search_calls.load(Ordering::SeqCst), 1);

        let forced = router
            .search(SearchRequest {
                no_cache: Some(true),
                ..search_request()
            })
            .await;
        assert!(!forced[0].cached);
        assert_eq!(search_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn fetch_prefers_longer_content_and_truncates() {
        let (short, _, short_fetch_calls) = MockChannel::new("exa", &"短".repeat(100));
        let (long, _, long_fetch_calls) = MockChannel::new("firecrawl", &"长".repeat(5000));
        let router = build_router(
            vec![short, long],
            None,
            2000,
            500,
            fetch_order(&["exa", "firecrawl"]),
        );

        let response = router
            .fetch(FetchRequest {
                urls: vec!["https://example.com/".to_string()],
                channels: None,
            })
            .await;
        assert_eq!(response.pages.len(), 1);
        assert_eq!(response.pages[0].channel, "firecrawl");
        assert_eq!(response.pages[0].content.chars().count(), 2000);
        assert_eq!(short_fetch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(long_fetch_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fetch_uses_configured_channel_order() {
        let (exa, _, exa_fetch_calls) = MockChannel::new("exa", &"e".repeat(1000));
        let (firecrawl, _, firecrawl_fetch_calls) =
            MockChannel::new("firecrawl", &"f".repeat(1000));
        let router = build_router(
            vec![exa, firecrawl],
            None,
            10000,
            500,
            fetch_order(&["firecrawl", "tavily", "exa", "tinyfish"]),
        );

        let response = router
            .fetch(FetchRequest {
                urls: vec!["https://example.com/".to_string()],
                channels: None,
            })
            .await;
        assert_eq!(response.pages[0].channel, "firecrawl");
        assert_eq!(firecrawl_fetch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(exa_fetch_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn fetch_prefers_tavily_and_respects_explicit_channel_order() {
        let (exa, _, exa_fetch_calls) = MockChannel::new("exa", &"e".repeat(1000));
        let (tavily, _, tavily_fetch_calls) = MockChannel::new("tavily", &"t".repeat(1000));
        let router = build_router(
            vec![exa, tavily],
            None,
            10000,
            500,
            fetch_order(&["firecrawl", "tavily", "exa", "tinyfish"]),
        );

        let default_response = router
            .fetch(FetchRequest {
                urls: vec!["https://example.com/".to_string()],
                channels: None,
            })
            .await;
        assert_eq!(default_response.pages[0].channel, "tavily");
        assert_eq!(exa_fetch_calls.load(Ordering::SeqCst), 0);
        assert_eq!(tavily_fetch_calls.load(Ordering::SeqCst), 1);

        let ordered_response = router
            .fetch(FetchRequest {
                urls: vec!["https://example.com/".to_string()],
                channels: Some(vec!["exa".to_string(), "tavily".to_string()]),
            })
            .await;
        assert_eq!(ordered_response.pages[0].channel, "exa");
        assert_eq!(exa_fetch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(tavily_fetch_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fetch_stops_when_content_long_enough() {
        let (good, _, good_fetch_calls) = MockChannel::new("exa", &"好".repeat(1000));
        let (later, _, later_fetch_calls) = MockChannel::new("firecrawl", &"后".repeat(5000));
        let router = build_router(
            vec![good, later],
            None,
            10000,
            500,
            fetch_order(&["exa", "firecrawl"]),
        );

        let response = router
            .fetch(FetchRequest {
                urls: vec!["https://example.com/".to_string()],
                channels: None,
            })
            .await;
        assert_eq!(response.pages[0].channel, "exa");
        assert_eq!(good_fetch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(later_fetch_calls.load(Ordering::SeqCst), 0);
    }

    fn fast_request() -> SearchRequest {
        SearchRequest {
            queries: vec!["test query".to_string()],
            mode: Some("fast".to_string()),
            channels: None,
            limit: None,
            no_cache: Some(true),
        }
    }

    #[tokio::test]
    async fn fast_uses_workbuddy_and_stops_before_fallbacks() {
        let (workbuddy, workbuddy_calls, _) = MockChannel::new("workbuddy", "");
        let (tavily, tavily_calls, _) = MockChannel::new("tavily", "");
        let (exa, exa_calls, _) = MockChannel::new("exa", "");
        let router = build_router(
            vec![workbuddy, tavily, exa],
            None,
            10000,
            500,
            fetch_order(&["firecrawl", "tavily", "exa", "tinyfish"]),
        );

        let response = router.search(fast_request()).await;
        assert_eq!(response[0].channels, vec!["workbuddy"]);
        assert_eq!(workbuddy_calls.load(Ordering::SeqCst), 1);
        assert_eq!(tavily_calls.load(Ordering::SeqCst), 0);
        assert_eq!(exa_calls.load(Ordering::SeqCst), 0);
    }

    fn fast_router_with_timeouts(
        channel: Arc<dyn Channel>,
        timeouts: HashMap<String, Duration>,
    ) -> Router {
        Router::new(
            vec![channel],
            Vec::new(),
            timeouts,
            "balanced".to_string(),
            10,
            fetch_order(&["exa", "tavily", "workbuddy"]),
            10,
            fetch_order(&["workbuddy"]),
            5,
            8000,
            None,
            None,
            10000,
            500,
            Vec::new(),
            None,
        )
    }

    fn recording_channel(
        name: &'static str,
        received_timeout: Arc<Mutex<Option<Duration>>>,
        received_limit: Arc<Mutex<Option<usize>>>,
    ) -> Arc<dyn Channel> {
        Arc::new(TimeoutRecordingChannel {
            name,
            received_timeout,
            received_limit,
            search_results: vec![SearchResult {
                title: "mock".to_string(),
                url: format!("https://{name}.example.com/"),
                content: "mock content".to_string(),
                published_date: None,
                channel: name.to_string(),
            }],
        })
    }

    #[tokio::test]
    async fn fast_caps_channel_timeout_at_fast_timeout() {
        let received = Arc::new(Mutex::new(None));
        // 渠道未配置超时时默认 15 秒，fast 应压到 5 秒。
        let router = fast_router_with_timeouts(
            recording_channel("workbuddy", received.clone(), Arc::new(Mutex::new(None))),
            HashMap::new(),
        );

        let response = router.search(fast_request()).await;
        assert_eq!(response[0].channels, vec!["workbuddy"]);
        assert_eq!(*received.lock().unwrap(), Some(Duration::from_secs(5)));
    }

    #[tokio::test]
    async fn fast_keeps_shorter_channel_timeout() {
        let received = Arc::new(Mutex::new(None));
        // 渠道自身超时 2 秒比 fast 上限 5 秒更短，应保留 2 秒。
        let timeouts = HashMap::from([("workbuddy".to_string(), Duration::from_secs(2))]);
        let router = fast_router_with_timeouts(
            recording_channel("workbuddy", received.clone(), Arc::new(Mutex::new(None))),
            timeouts,
        );

        let response = router.search(fast_request()).await;
        assert_eq!(response[0].channels, vec!["workbuddy"]);
        assert_eq!(*received.lock().unwrap(), Some(Duration::from_secs(2)));
    }

    #[tokio::test]
    async fn fast_caps_limit_only_for_serial_path() {
        let received = Arc::new(Mutex::new(None));
        // 默认 fast（未传 channels）把 limit 压到 fast_limit=10。
        let router = fast_router_with_timeouts(
            recording_channel("workbuddy", Arc::new(Mutex::new(None)), received.clone()),
            HashMap::new(),
        );
        router
            .search(SearchRequest {
                queries: vec!["test query".to_string()],
                mode: Some("fast".to_string()),
                channels: None,
                limit: Some(50),
                no_cache: Some(true),
            })
            .await;
        assert_eq!(*received.lock().unwrap(), Some(10));

        // 显式 channels 覆盖 mode，limit 保持调用方传入的 50。
        let explicit = Arc::new(Mutex::new(None));
        let router = fast_router_with_timeouts(
            recording_channel("workbuddy", Arc::new(Mutex::new(None)), explicit.clone()),
            HashMap::new(),
        );
        router
            .search(SearchRequest {
                queries: vec!["test query".to_string()],
                mode: Some("fast".to_string()),
                channels: Some(vec!["workbuddy".to_string()]),
                limit: Some(50),
                no_cache: Some(true),
            })
            .await;
        assert_eq!(*explicit.lock().unwrap(), Some(50));
    }

    #[tokio::test]
    async fn fast_falls_back_to_tavily_then_stops() {
        let (workbuddy, workbuddy_calls) = FailingChannel::new("workbuddy");
        let (tavily, tavily_calls, _) = MockChannel::new("tavily", "");
        let (exa, exa_calls, _) = MockChannel::new("exa", "");
        let router = build_router(
            vec![workbuddy, tavily, exa],
            None,
            10000,
            500,
            fetch_order(&["firecrawl", "tavily", "exa", "tinyfish"]),
        );

        let response = router.search(fast_request()).await;
        assert_eq!(response[0].channels, vec!["tavily"]);
        assert_eq!(workbuddy_calls.load(Ordering::SeqCst), 1);
        assert_eq!(tavily_calls.load(Ordering::SeqCst), 1);
        assert_eq!(exa_calls.load(Ordering::SeqCst), 0);
        assert!(response[0]
            .errors
            .iter()
            .any(|error| error.channel == "workbuddy"));
    }

    #[tokio::test]
    async fn fast_falls_back_to_exa_when_workbuddy_and_tavily_fail() {
        let (workbuddy, workbuddy_calls) = FailingChannel::new("workbuddy");
        let (tavily, tavily_calls) = FailingChannel::new("tavily");
        let (exa, exa_calls, _) = MockChannel::new("exa", "");
        let router = build_router(
            vec![workbuddy, tavily, exa],
            None,
            10000,
            500,
            fetch_order(&["firecrawl", "tavily", "exa", "tinyfish"]),
        );

        let response = router.search(fast_request()).await;
        assert_eq!(response[0].channels, vec!["exa"]);
        assert_eq!(workbuddy_calls.load(Ordering::SeqCst), 1);
        assert_eq!(tavily_calls.load(Ordering::SeqCst), 1);
        assert_eq!(exa_calls.load(Ordering::SeqCst), 1);
        assert_eq!(response[0].errors.len(), 2);
    }

    #[tokio::test]
    async fn failed_or_empty_responses_are_not_cached() {
        let (failing, failing_calls) = FailingChannel::new("exa");
        let cache = Arc::new(QueryCache::new(60, 10));
        let router = build_router(vec![failing], Some(cache.clone()), 10000, 500, Vec::new());

        let first = router.search(search_request()).await;
        let second = router.search(search_request()).await;
        assert!(first[0].results.is_empty());
        assert!(!second[0].cached);
        assert_eq!(failing_calls.load(Ordering::SeqCst), 2);
        assert_eq!(cache.len(), 0);
    }

    #[tokio::test]
    async fn explicit_channels_include_fallback_only_and_reject_unknown_without_fallback() {
        let (exa, exa_calls, _) = MockChannel::new("exa", "");
        let (grok, grok_calls, _) = MockChannel::new("grok", "");
        let router = Router::new(
            vec![exa],
            vec![grok],
            HashMap::new(),
            "balanced".to_string(),
            10,
            fetch_order(&["exa", "tavily", "workbuddy"]),
            10,
            fetch_order(&["workbuddy", "tavily", "exa"]),
            5,
            8000,
            None,
            None,
            10000,
            500,
            Vec::new(),
            None,
        );

        let both = router
            .search(SearchRequest {
                channels: Some(vec!["exa".to_string(), "grok".to_string()]),
                ..search_request()
            })
            .await;
        assert_eq!(exa_calls.load(Ordering::SeqCst), 1);
        assert_eq!(grok_calls.load(Ordering::SeqCst), 1);
        assert_eq!(both[0].channels, vec!["exa", "grok"]);

        let unknown = router
            .search(SearchRequest {
                channels: Some(vec!["nonexistent".to_string()]),
                ..search_request()
            })
            .await;
        assert_eq!(grok_calls.load(Ordering::SeqCst), 1);
        assert!(unknown[0].results.is_empty());
        assert_eq!(unknown[0].errors[0].code, "no_channels");
    }

    #[tokio::test]
    async fn explicit_fallback_channel_is_not_called_twice_when_failing() {
        let (grok, grok_calls) = FailingChannel::new("grok");
        let router = Router::new(
            Vec::new(),
            vec![grok],
            HashMap::new(),
            "balanced".to_string(),
            10,
            fetch_order(&["exa"]),
            10,
            fetch_order(&["workbuddy"]),
            5,
            8000,
            None,
            None,
            10000,
            500,
            Vec::new(),
            None,
        );

        let response = router
            .search(SearchRequest {
                channels: Some(vec!["grok".to_string()]),
                ..search_request()
            })
            .await;
        assert_eq!(grok_calls.load(Ordering::SeqCst), 1);
        assert_eq!(response[0].errors.len(), 1);
    }
}
