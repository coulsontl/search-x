use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{status_error, transport_error, Channel};
use crate::model::{ChannelError, SearchResult};
use crate::workbuddy_accounts::{WorkBuddyAccount, WorkBuddyAccountStore};

const WORKBUDDY_USER_AGENT: &str = "WorkBuddy/5.9.3 WorkBuddy/5.9.3 CLI/2.165.0";
const UPSTREAM_MAX_RESULTS: usize = 10;
const MAX_ACCOUNT_ATTEMPTS: usize = 3;
/// 回退 agent2api 所需的最小剩余超时预算，不足则直接按超时返回。
const MIN_FALLBACK_BUDGET: Duration = Duration::from_millis(300);

/// 经 agent2api 的 `/api/web-search/search`；需要把 workbuddy 加入其 selection.search。
pub struct WorkBuddyChannel {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl WorkBuddyChannel {
    pub fn new(base_url: String, api_key: String, client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }
}

#[async_trait]
impl Channel for WorkBuddyChannel {
    fn name(&self) -> &'static str {
        "workbuddy"
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
        timeout: Duration,
    ) -> Result<Vec<SearchResult>, ChannelError> {
        let body = json!({ "queries": [query] });
        let response = self
            .client
            .post(format!("{}/api/web-search/search", self.base_url))
            .bearer_auth(&self.api_key)
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(|error| transport_error("workbuddy", error))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| transport_error("workbuddy", error))?;
        if !status.is_success() {
            return Err(status_error("workbuddy", status, &text));
        }
        let value: Value = serde_json::from_str(&text).map_err(|_| {
            ChannelError::new(
                "workbuddy",
                "invalid_response",
                "agent2api 返回了无法解析的 JSON",
            )
        })?;
        let mut results = Vec::new();
        if let Some(searches) = value["searches"].as_array() {
            for search in searches {
                if let Some(rows) = search["results"].as_array() {
                    for row in rows {
                        if let Some(url) = row["url"].as_str() {
                            results.push(SearchResult {
                                title: row["title"].as_str().unwrap_or(url).to_string(),
                                url: url.to_string(),
                                content: row["content"].as_str().unwrap_or_default().to_string(),
                                published_date: row["publishedDate"].as_str().map(str::to_string),
                                channel: "workbuddy".to_string(),
                            });
                        }
                    }
                }
            }
        }
        results.truncate(limit);
        Ok(results)
    }
}

/// 直连 WorkBuddy 上游：账号凭据来自 agent2api 导出接口，
/// 每次请求轮询一个未过期账号，最多取 10 条；全部失败时回退 agent2api 并记日志。
pub struct UpstreamWorkBuddyChannel {
    client: reqwest::Client,
    accounts: Arc<WorkBuddyAccountStore>,
    max_results: usize,
    fallback: Option<Arc<dyn Channel>>,
}

impl UpstreamWorkBuddyChannel {
    pub fn new(
        client: reqwest::Client,
        accounts: Arc<WorkBuddyAccountStore>,
        max_results: usize,
        fallback: Option<Arc<dyn Channel>>,
    ) -> Self {
        Self {
            client,
            accounts,
            max_results: max_results.clamp(1, UPSTREAM_MAX_RESULTS),
            fallback,
        }
    }

    async fn search_with_account(
        &self,
        account: &WorkBuddyAccount,
        query: &str,
        max_results: usize,
        timeout: Duration,
    ) -> Result<Vec<SearchResult>, ChannelError> {
        let trace = uuid::Uuid::new_v4().simple().to_string();
        let span = trace[..16].to_string();
        let request_id = uuid::Uuid::new_v4().to_string();
        let conversation_id = uuid::Uuid::new_v4().to_string();
        let mut request = self
            .client
            .post(format!(
                "{}/agenttool/v1/search",
                account.endpoint.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {}", account.access_token))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", WORKBUDDY_USER_AGENT)
            .header("X-IDE-Type", "WorkBuddy")
            .header("X-IDE-Name", "WorkBuddy")
            .header("X-IDE-Version", "5.9.3")
            .header("X-Product", "SaaS")
            .header("X-Agent-Intent", "craft")
            .header("X-Agent-Purpose", "conversation")
            .header("X-Agent-Type", "main")
            .header("X-Agent-Runtime", "local-agent")
            .header("X-Private-Data", "true")
            .header("X-Requested-With", "XMLHttpRequest")
            .header("X-Request-ID", request_id.clone())
            .header("X-Trace-ID", trace.clone())
            .header("traceparent", format!("00-{trace}-{span}-01"))
            .header("b3", format!("{trace}-{span}-1"))
            .header("X-B3-TraceId", trace.clone())
            .header("X-B3-SpanId", span)
            .header("X-B3-Sampled", "1")
            .header("X-Conversation-ID", conversation_id)
            .header("X-Conversation-Request-ID", request_id.clone())
            .header("X-Root-Request-ID", request_id.clone())
            .header("X-Conversation-Message-ID", request_id)
            .timeout(timeout);
        if !account.uid.is_empty() {
            request = request.header("X-User-Id", &account.uid);
        }
        if !account.enterprise_id.is_empty() {
            request = request.header("X-Enterprise-Id", &account.enterprise_id);
            request = request.header("X-Tenant-Id", &account.enterprise_id);
        }
        let domain = if account.domain.is_empty() {
            "www.workbuddy.cn"
        } else {
            account.domain.as_str()
        };
        request = request.header("X-Domain", domain);
        if !account.department.is_empty() {
            request = request.header("X-Department-Info", &account.department);
        }

        let response = request
            .json(&json!({ "query": query, "type": "text2text", "max_results": max_results }))
            .send()
            .await
            .map_err(|error| transport_error("workbuddy", error))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| transport_error("workbuddy", error))?;
        if !status.is_success() {
            return Err(status_error("workbuddy", status, &text));
        }
        let value: Value = serde_json::from_str(&text).map_err(|_| {
            ChannelError::new(
                "workbuddy",
                "invalid_response",
                "WorkBuddy 上游返回了无法解析的 JSON",
            )
        })?;
        if value["code"].as_i64() == Some(15001) {
            return Err(ChannelError::new(
                "workbuddy",
                "rate_limited",
                "WorkBuddy 搜索额度已用尽",
            ));
        }
        let rows = value["results"].as_array().ok_or_else(|| {
            ChannelError::new(
                "workbuddy",
                "invalid_response",
                "WorkBuddy 响应缺少 results",
            )
        })?;
        let mut results = Vec::new();
        for row in rows {
            let Some(url) = row["url"].as_str() else {
                continue;
            };
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                continue;
            }
            let published_date = row["publishedDate"]
                .as_str()
                .map(str::to_string)
                .or_else(|| {
                    row["publishedDate"]
                        .as_i64()
                        .and_then(chrono::DateTime::from_timestamp_millis)
                        .map(|value| value.to_rfc3339())
                });
            results.push(SearchResult {
                title: row["title"].as_str().unwrap_or(url).to_string(),
                url: url.to_string(),
                content: row["snippet"].as_str().unwrap_or_default().to_string(),
                published_date,
                channel: "workbuddy".to_string(),
            });
        }
        Ok(results)
    }
}

#[async_trait]
impl Channel for UpstreamWorkBuddyChannel {
    fn name(&self) -> &'static str {
        "workbuddy"
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
        timeout: Duration,
    ) -> Result<Vec<SearchResult>, ChannelError> {
        let deadline = Instant::now() + timeout;
        let max_results = self.max_results.min(limit.max(1));
        let mut last_error: Option<ChannelError> = None;
        let mut invalidated = false;

        for _ in 0..MAX_ACCOUNT_ATTEMPTS {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                last_error.get_or_insert_with(|| {
                    ChannelError::new("workbuddy", "timed_out", "渠道请求失败")
                });
                break;
            }
            let account = match self.accounts.next_account(remaining).await {
                Ok(Some(account)) => account,
                Ok(None) => {
                    last_error.get_or_insert_with(|| {
                        ChannelError::new("workbuddy", "no_account", "没有未过期的 WorkBuddy 账号")
                    });
                    break;
                }
                Err(error) => {
                    // 路径与系统错误只写服务端日志，不回给客户端。
                    tracing::error!("workbuddy 账号来源读取失败: {error}");
                    last_error.get_or_insert_with(|| {
                        ChannelError::new("workbuddy", "store", "WorkBuddy 账号来源不可用")
                    });
                    break;
                }
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                last_error.get_or_insert_with(|| {
                    ChannelError::new("workbuddy", "timed_out", "渠道请求失败")
                });
                break;
            }
            match self
                .search_with_account(&account, query, max_results, remaining)
                .await
            {
                // 上游正常返回（含 0 条结果）时直接结束，避免把同一冷门查询用多个账号重复打 8+ 遍。
                Ok(results) => return Ok(results),
                Err(error) => {
                    let retryable_account_error =
                        matches!(error.code.as_str(), "unauthorized" | "rate_limited");
                    if error.code == "unauthorized" && !invalidated {
                        // token 可能刚被 agent2api 刷新，本次搜索最多强制刷新导出一次。
                        self.accounts.invalidate();
                        invalidated = true;
                    }
                    last_error = Some(error);
                    if !retryable_account_error {
                        break;
                    }
                }
            }
        }

        let last_error = last_error.unwrap_or_else(|| {
            ChannelError::new("workbuddy", "upstream_failed", "WorkBuddy 直连失败")
        });
        let remaining = deadline.saturating_duration_since(Instant::now());
        match &self.fallback {
            Some(fallback) if remaining >= MIN_FALLBACK_BUDGET => {
                tracing::warn!(
                    "workbuddy 直连失败（{}），使用剩余 {}ms 回退 agent2api",
                    last_error.message,
                    remaining.as_millis()
                );
                fallback.search(query, limit, remaining).await
            }
            _ => Err(last_error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use axum::{routing::post, Json, Router as AxumRouter};
    use serde_json::json;

    use super::{UpstreamWorkBuddyChannel, WorkBuddyChannel};
    use crate::channels::Channel;
    use crate::model::{ChannelError, SearchResult};
    use crate::workbuddy_accounts::WorkBuddyAccountStore;

    struct CountingFallback(Arc<AtomicUsize>);

    #[async_trait]
    impl Channel for CountingFallback {
        fn name(&self) -> &'static str {
            "workbuddy"
        }

        async fn search(
            &self,
            _query: &str,
            _limit: usize,
            _timeout: Duration,
        ) -> Result<Vec<SearchResult>, ChannelError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(vec![SearchResult {
                title: "fallback".to_string(),
                url: "https://fallback.example.com/".to_string(),
                content: "from fallback".to_string(),
                published_date: None,
                channel: "workbuddy".to_string(),
            }])
        }
    }

    async fn spawn_mock_server(app: AxumRouter) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn upstream_empty_results_stop_immediately_without_looping_or_fallback() {
        let search_calls = Arc::new(AtomicUsize::new(0));
        let export_calls = Arc::new(AtomicUsize::new(0));
        let search_counter = search_calls.clone();
        let export_counter = export_calls.clone();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let endpoint = base_url.clone();

        let app = AxumRouter::new()
            .route(
                "/api/accounts/export-files",
                post(move || {
                    let export_counter = export_counter.clone();
                    let endpoint = endpoint.clone();
                    async move {
                        export_counter.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "success": true,
                            "data": {
                                "files": [
                                    {"content": {"accessToken": "t1", "endpoint": endpoint, "enabled": true}},
                                    {"content": {"accessToken": "t2", "endpoint": endpoint, "enabled": true}},
                                    {"content": {"accessToken": "t3", "endpoint": endpoint, "enabled": true}}
                                ]
                            }
                        }))
                    }
                }),
            )
            .route(
                "/agenttool/v1/search",
                post(move || {
                    let search_counter = search_counter.clone();
                    async move {
                        search_counter.fetch_add(1, Ordering::SeqCst);
                        Json(json!({ "results": [] }))
                    }
                }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = reqwest::Client::new();
        let store = Arc::new(WorkBuddyAccountStore::new(
            client.clone(),
            base_url,
            "k",
            "workbuddy",
            600,
        ));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let channel = UpstreamWorkBuddyChannel::new(
            client,
            store,
            10,
            Some(Arc::new(CountingFallback(fallback_calls.clone()))),
        );

        let results = channel
            .search("rare query", 10, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(results.is_empty());
        assert_eq!(search_calls.load(Ordering::SeqCst), 1);
        assert_eq!(export_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn upstream_timeout_respects_single_shared_deadline() {
        let search_calls = Arc::new(AtomicUsize::new(0));
        let search_counter = search_calls.clone();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let endpoint = base_url.clone();

        let app = AxumRouter::new()
            .route(
                "/api/accounts/export-files",
                post(move || {
                    let endpoint = endpoint.clone();
                    async move {
                        Json(json!({
                            "success": true,
                            "data": {
                                "files": [
                                    {"content": {"accessToken": "t1", "endpoint": endpoint, "enabled": true}},
                                    {"content": {"accessToken": "t2", "endpoint": endpoint, "enabled": true}},
                                    {"content": {"accessToken": "t3", "endpoint": endpoint, "enabled": true}}
                                ]
                            }
                        }))
                    }
                }),
            )
            .route(
                "/agenttool/v1/search",
                post(move || {
                    let search_counter = search_counter.clone();
                    async move {
                        search_counter.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        Json(json!({ "results": [] }))
                    }
                }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = reqwest::Client::new();
        let store = Arc::new(WorkBuddyAccountStore::new(
            client.clone(),
            base_url,
            "k",
            "workbuddy",
            600,
        ));
        let fallback_calls = Arc::new(AtomicUsize::new(0));
        let channel = UpstreamWorkBuddyChannel::new(
            client,
            store,
            10,
            Some(Arc::new(CountingFallback(fallback_calls.clone()))),
        );

        let started = Instant::now();
        let err = channel
            .search("slow query", 10, Duration::from_millis(350))
            .await
            .unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(err.code, "timed_out");
        assert!(
            elapsed < Duration::from_secs(1),
            "超时预算不应被多账号重试放大，实际耗时 {elapsed:?}"
        );
        assert_eq!(search_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn agent2api_workbuddy_respects_limit_below_five() {
        let app = AxumRouter::new().route(
            "/api/web-search/search",
            post(|| async {
                Json(json!({
                    "searches": [{
                        "results": [
                            {"title": "1", "url": "https://example.com/1", "content": "c1"},
                            {"title": "2", "url": "https://example.com/2", "content": "c2"},
                            {"title": "3", "url": "https://example.com/3", "content": "c3"},
                            {"title": "4", "url": "https://example.com/4", "content": "c4"},
                            {"title": "5", "url": "https://example.com/5", "content": "c5"}
                        ]
                    }]
                }))
            }),
        );
        let base_url = spawn_mock_server(app).await;
        let channel = WorkBuddyChannel::new(base_url, "k".to_string(), reqwest::Client::new());

        let results = channel
            .search("q", 2, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
    }
}
