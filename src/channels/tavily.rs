use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use super::{is_retryable_key_status, status_error, transport_error, Channel};
use crate::keypool::KeyPool;
use crate::model::{ChannelError, PageResult, SearchResult};

pub struct TavilyChannel {
    client: reqwest::Client,
    base_url: String,
    keys: KeyPool,
    include_raw_content: bool,
}

impl TavilyChannel {
    pub fn new(
        base_url: String,
        keys: KeyPool,
        client: reqwest::Client,
        include_raw_content: bool,
    ) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            keys,
            include_raw_content,
        }
    }
}

#[async_trait]
impl Channel for TavilyChannel {
    fn name(&self) -> &'static str {
        "tavily"
    }

    fn supports_fetch(&self) -> bool {
        true
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
        timeout: Duration,
    ) -> Result<Vec<SearchResult>, ChannelError> {
        let deadline = Instant::now() + timeout;
        let max_attempts = if self.keys.has_multiple() { 2 } else { 1 };
        let body = json!({
            "query": query,
            "search_depth": "basic",
            "max_results": limit.clamp(1, 20),
            "include_raw_content": self.include_raw_content
        });
        let mut last_error = None;
        for attempt in 0..max_attempts {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Some(key) = self.keys.next() else {
                return Err(last_error.unwrap_or_else(|| {
                    ChannelError::new("tavily", "no_key", "Tavily 没有可用 key")
                }));
            };
            let response = match self
                .client
                .post(format!("{}/search", self.base_url))
                .bearer_auth(&key)
                .timeout(remaining)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("tavily", error));
                }
            };
            let status = response.status();
            let text = match response.text().await {
                Ok(text) => text,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("tavily", error));
                }
            };
            if !status.is_success() {
                let code = status.as_u16();
                self.keys.report_failure(&key, Some(code));
                let err = status_error("tavily", status, &text);
                if attempt + 1 < max_attempts && is_retryable_key_status(code) {
                    last_error = Some(err);
                    continue;
                }
                return Err(err);
            }
            self.keys.report_success(&key);
            let value: Value = serde_json::from_str(&text).map_err(|_| {
                ChannelError::new("tavily", "invalid_response", "Tavily 返回了无法解析的 JSON")
            })?;
            return Ok(value["results"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .filter_map(|row| {
                            let url = row["url"].as_str()?.to_string();
                            let content = row["raw_content"]
                                .as_str()
                                .or_else(|| row["content"].as_str())
                                .unwrap_or_default()
                                .to_string();
                            Some(SearchResult {
                                title: row["title"].as_str().unwrap_or(&url).to_string(),
                                url,
                                content,
                                published_date: row["published_date"].as_str().map(str::to_string),
                                channel: "tavily".to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default());
        }
        Err(last_error.unwrap_or_else(|| ChannelError::new("tavily", "timed_out", "渠道请求失败")))
    }

    async fn fetch(
        &self,
        urls: &[String],
        timeout: Duration,
    ) -> Result<Vec<PageResult>, ChannelError> {
        let deadline = Instant::now() + timeout;
        let max_attempts = if self.keys.has_multiple() { 2 } else { 1 };
        let body = json!({ "urls": urls });
        let mut last_error = None;
        for attempt in 0..max_attempts {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Some(key) = self.keys.next() else {
                return Err(last_error.unwrap_or_else(|| {
                    ChannelError::new("tavily", "no_key", "Tavily 没有可用 key")
                }));
            };
            let response = match self
                .client
                .post(format!("{}/extract", self.base_url))
                .bearer_auth(&key)
                .timeout(remaining)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("tavily", error));
                }
            };
            let status = response.status();
            let text = match response.text().await {
                Ok(text) => text,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("tavily", error));
                }
            };
            if !status.is_success() {
                let code = status.as_u16();
                self.keys.report_failure(&key, Some(code));
                let err = status_error("tavily", status, &text);
                if attempt + 1 < max_attempts && is_retryable_key_status(code) {
                    last_error = Some(err);
                    continue;
                }
                return Err(err);
            }
            self.keys.report_success(&key);
            let value: Value = serde_json::from_str(&text).map_err(|_| {
                ChannelError::new("tavily", "invalid_response", "Tavily 返回了无法解析的 JSON")
            })?;
            return Ok(value["results"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .map(|row| PageResult {
                            url: row["url"].as_str().unwrap_or_default().to_string(),
                            title: row["title"].as_str().unwrap_or_default().to_string(),
                            content: row["raw_content"].as_str().unwrap_or_default().to_string(),
                            channel: "tavily".to_string(),
                        })
                        .filter(|page| !page.url.is_empty())
                        .collect()
                })
                .unwrap_or_default());
        }
        Err(last_error.unwrap_or_else(|| ChannelError::new("tavily", "timed_out", "渠道请求失败")))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use axum::{http::HeaderMap, http::StatusCode, routing::post, Json, Router as AxumRouter};
    use serde_json::json;

    use super::TavilyChannel;
    use crate::channels::Channel;
    use crate::config::KeyStrategy;
    use crate::keypool::KeyPool;

    #[tokio::test]
    async fn retries_next_key_transparently_on_432_quota_exceeded() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = AxumRouter::new().route(
            "/search",
            post(move |headers: HeaderMap| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if auth == "Bearer key-exhausted" {
                        (
                            StatusCode::from_u16(432).unwrap(),
                            Json(json!({ "error": "plan limit exceeded" })),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            Json(json!({
                                "results": [{
                                    "title": "Rust",
                                    "url": "https://www.rust-lang.org/",
                                    "content": "Rust language"
                                }]
                            })),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let pool = KeyPool::new(
            vec!["key-exhausted".to_string(), "key-healthy".to_string()],
            KeyStrategy::RoundRobin,
        );
        let channel = TavilyChannel::new(
            format!("http://{addr}"),
            pool,
            reqwest::Client::new(),
            false,
        );

        let results = channel
            .search("rust", 5, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn preserves_upstream_error_when_retry_exhausts_pool() {
        let app = AxumRouter::new().route(
            "/search",
            post(|| async {
                (
                    StatusCode::from_u16(432).unwrap(),
                    Json(json!({ "error": "plan limit exceeded" })),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let pool = KeyPool::new(
            vec!["key-1".to_string(), "key-2".to_string()],
            KeyStrategy::RoundRobin,
        );
        pool.report_failure("key-1", Some(401));
        let channel = TavilyChannel::new(
            format!("http://{addr}"),
            pool,
            reqwest::Client::new(),
            false,
        );

        let err = channel
            .search("rust", 5, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(err.code, "rate_limited");
    }
}
