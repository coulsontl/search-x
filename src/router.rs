use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;

use crate::cache::QueryCache;
use crate::channels::Channel;
use crate::logger::SearchLogger;
use crate::model::{
    move_searxng_last, rrf_merge, ChannelError, FetchRequest, FetchResponse, PageResult,
    SearchRequest, SearchResponse, SearchResult,
};

/// 按字符（而非字节）截断，避免切坏中文。
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

pub struct Router {
    channels: Vec<Arc<dyn Channel>>,
    fallback: Vec<Arc<dyn Channel>>,
    timeouts: HashMap<String, Duration>,
    default_mode: String,
    default_limit: usize,
    logger: Option<Arc<SearchLogger>>,
    cache: Option<Arc<QueryCache>>,
    fetch_max_content_chars: usize,
    fetch_min_content_chars: usize,
}

impl Router {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        channels: Vec<Arc<dyn Channel>>,
        fallback: Vec<Arc<dyn Channel>>,
        timeouts: HashMap<String, Duration>,
        default_mode: String,
        default_limit: usize,
        logger: Option<Arc<SearchLogger>>,
        cache: Option<Arc<QueryCache>>,
        fetch_max_content_chars: usize,
        fetch_min_content_chars: usize,
    ) -> Self {
        Self {
            channels,
            fallback,
            timeouts,
            default_mode,
            default_limit,
            logger,
            cache,
            fetch_max_content_chars,
            fetch_min_content_chars,
        }
    }

    pub fn channel_status(&self) -> Vec<serde_json::Value> {
        self.channels
            .iter()
            .chain(self.fallback.iter())
            .map(|channel| {
                serde_json::json!({
                    "channel": channel.name(),
                    "supports_fetch": channel.supports_fetch(),
                    "fallback_only": self.fallback.iter().any(|entry| entry.name() == channel.name()),
                })
            })
            .collect()
    }

    pub async fn search(&self, request: SearchRequest) -> Vec<SearchResponse> {
        let mode = request
            .mode
            .as_deref()
            .unwrap_or(&self.default_mode)
            .to_string();
        let limit = request.limit.unwrap_or(self.default_limit).clamp(1, 100);
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
                let response = self.search_one(query, selected_ref, limit).await;
                if let Some(cache) = &self.cache {
                    cache.put(cache_key, response.clone());
                }
                response
            }
        });
        let responses = join_all(futures).await;
        if let Some(logger) = &self.logger {
            // 缓存命中也要记录（轻量行），用于统计命中率。
            for response in &responses {
                logger.log(&mode, response);
            }
        }
        responses
    }

    pub async fn fetch(&self, request: FetchRequest) -> FetchResponse {
        let started = Instant::now();
        let selected: Vec<Arc<dyn Channel>> = self
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

        // 多个 URL 并行抓取；单个 URL 内部仍按渠道顺序降级。
        let selected_ref = &selected;
        let futures = request
            .urls
            .iter()
            .map(|url| self.fetch_one(url, selected_ref));
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
        FetchResponse {
            pages,
            errors,
            elapsed_ms: started.elapsed().as_millis() as u64,
        }
    }

    /// 单个 URL：按渠道顺序尝试；内容过短（如 SPA 只抓到 300 字导航文本）时继续尝试下一个，
    /// 最终采用内容最长的结果。内容达到 min_content_chars 即提前停止，避免不必要的付费调用。
    async fn fetch_one(
        &self,
        url: &str,
        channels: &[Arc<dyn Channel>],
    ) -> Result<Vec<PageResult>, Vec<ChannelError>> {
        let urls = [url.to_string()];
        let mut errors = Vec::new();
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
        let names: Vec<&str> = match mode {
            "fast" => vec!["searxng"],
            "full" => self.channels.iter().map(|channel| channel.name()).collect(),
            // 默认编排：四个国内可用渠道并发，WorkBuddy 走 agent2api（国内版）
            _ => vec!["searxng", "exa", "tavily", "workbuddy"],
        };
        self.channels
            .iter()
            .filter(|channel| {
                if let Some(requested) = requested {
                    requested.iter().any(|name| name == channel.name())
                } else {
                    names.contains(&channel.name())
                }
            })
            .cloned()
            .collect()
    }

    async fn search_one(
        &self,
        query: &str,
        channels: &[Arc<dyn Channel>],
        limit: usize,
    ) -> SearchResponse {
        let started = Instant::now();
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
                Ok(items) if !items.is_empty() => {
                    used.push(name);
                    per_channel.push(items);
                }
                Ok(_) => {}
                Err(error) => errors.push(error),
            }
        }

        let mut results = rrf_merge(per_channel);

        if results.is_empty() && !self.fallback.is_empty() {
            for channel in &self.fallback {
                let timeout = self.timeout_of(channel.name());
                let call_started = Instant::now();
                let outcome = channel.search(query, limit, timeout).await;
                channel_elapsed_ms.insert(
                    channel.name().to_string(),
                    call_started.elapsed().as_millis() as u64,
                );
                match outcome {
                    Ok(items) if !items.is_empty() => {
                        used.push(channel.name().to_string());
                        results = rrf_merge(vec![items]);
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => errors.push(error),
                }
            }
        }

        SearchResponse {
            query: query.to_string(),
            results: move_searxng_last(results),
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
    use std::sync::Arc;
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

    fn build_router(
        channels: Vec<Arc<dyn Channel>>,
        cache: Option<Arc<QueryCache>>,
        max_chars: usize,
        min_chars: usize,
    ) -> Router {
        Router::new(
            channels,
            Vec::new(),
            HashMap::new(),
            "balanced".to_string(),
            10,
            None,
            cache,
            max_chars,
            min_chars,
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
    async fn cache_hit_and_no_cache_bypass() {
        let (channel, search_calls, _) = MockChannel::new("exa", "content");
        let router = build_router(
            vec![channel],
            Some(Arc::new(QueryCache::new(60, 10))),
            10000,
            500,
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
        let router = build_router(vec![short, long], None, 2000, 500);

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
    async fn fetch_stops_when_content_long_enough() {
        let (good, _, good_fetch_calls) = MockChannel::new("exa", &"好".repeat(1000));
        let (later, _, later_fetch_calls) = MockChannel::new("firecrawl", &"后".repeat(5000));
        let router = build_router(vec![good, later], None, 10000, 500);

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
}
