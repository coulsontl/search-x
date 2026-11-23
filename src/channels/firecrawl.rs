use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use super::{is_retryable_key_status, status_error, transport_error, Channel};
use crate::keypool::KeyPool;
use crate::model::{ChannelError, PageResult, SearchResult};

pub struct FirecrawlChannel {
    client: reqwest::Client,
    base_url: String,
    keys: KeyPool,
    scrape_content: bool,
}

impl FirecrawlChannel {
    pub fn new(
        base_url: String,
        keys: KeyPool,
        client: reqwest::Client,
        scrape_content: bool,
    ) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            keys,
            scrape_content,
        }
    }
}

#[async_trait]
impl Channel for FirecrawlChannel {
    fn name(&self) -> &'static str {
        "firecrawl"
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
        let mut body = json!({
            "query": query,
            "limit": limit.clamp(1, 100),
            "sources": ["web"]
        });
        if self.scrape_content {
            body["scrapeOptions"] = json!({ "formats": ["markdown"], "onlyMainContent": true });
        }
        let mut last_error = None;
        for attempt in 0..max_attempts {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Some(key) = self.keys.next() else {
                return Err(last_error.unwrap_or_else(|| {
                    ChannelError::new("firecrawl", "no_key", "Firecrawl 没有可用 key")
                }));
            };
            let response = match self
                .client
                .post(format!("{}/v2/search", self.base_url))
                .bearer_auth(&key)
                .timeout(remaining)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("firecrawl", error));
                }
            };
            let status = response.status();
            let text = match response.text().await {
                Ok(text) => text,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("firecrawl", error));
                }
            };
            if !status.is_success() {
                let code = status.as_u16();
                // 403 常表示目标站点不支持或套餐限制，不代表 key 失效，避免误禁用或无意义换 key。
                if code != 403 {
                    self.keys.report_failure(&key, Some(code));
                }
                let err = status_error("firecrawl", status, &text);
                if attempt + 1 < max_attempts && code != 403 && is_retryable_key_status(code) {
                    last_error = Some(err);
                    continue;
                }
                return Err(err);
            }
            self.keys.report_success(&key);
            let value: Value = serde_json::from_str(&text).map_err(|_| {
                ChannelError::new(
                    "firecrawl",
                    "invalid_response",
                    "Firecrawl 返回了无法解析的 JSON",
                )
            })?;
            let rows = value["data"]["web"]
                .as_array()
                .or_else(|| value["data"].as_array());
            return Ok(rows
                .map(|rows| {
                    rows.iter()
                        .filter_map(|row| {
                            let url = row["url"].as_str()?.to_string();
                            let content = row["markdown"]
                                .as_str()
                                .or_else(|| row["description"].as_str())
                                .unwrap_or_default()
                                .to_string();
                            Some(SearchResult {
                                title: row["title"].as_str().unwrap_or(&url).to_string(),
                                url,
                                content,
                                published_date: row["publishedDate"].as_str().map(str::to_string),
                                channel: "firecrawl".to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default());
        }
        Err(last_error
            .unwrap_or_else(|| ChannelError::new("firecrawl", "timed_out", "渠道请求失败")))
    }

    async fn fetch(
        &self,
        urls: &[String],
        timeout: Duration,
    ) -> Result<Vec<PageResult>, ChannelError> {
        let deadline = Instant::now() + timeout;
        let max_attempts = if self.keys.has_multiple() { 2 } else { 1 };
        let mut pages = Vec::new();
        for url in urls {
            let body = json!({ "url": url, "formats": ["markdown"], "onlyMainContent": true });
            let mut last_error = None;
            let mut fetched = None;
            for attempt in 0..max_attempts {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let Some(key) = self.keys.next() else {
                    return Err(last_error.unwrap_or_else(|| {
                        ChannelError::new("firecrawl", "no_key", "Firecrawl 没有可用 key")
                    }));
                };
                let response = match self
                    .client
                    .post(format!("{}/v2/scrape", self.base_url))
                    .bearer_auth(&key)
                    .timeout(remaining)
                    .json(&body)
                    .send()
                    .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        self.keys.report_failure(&key, None);
                        return Err(transport_error("firecrawl", error));
                    }
                };
                let status = response.status();
                let text = match response.text().await {
                    Ok(text) => text,
                    Err(error) => {
                        self.keys.report_failure(&key, None);
                        return Err(transport_error("firecrawl", error));
                    }
                };
                if !status.is_success() {
                    let code = status.as_u16();
                    // 403 常表示目标站点不支持或套餐限制，不代表 key 失效，避免误禁用。
                    if code != 403 {
                        self.keys.report_failure(&key, Some(code));
                    }
                    let err = status_error("firecrawl", status, &text);
                    if attempt + 1 < max_attempts && code != 403 && is_retryable_key_status(code) {
                        last_error = Some(err);
                        continue;
                    }
                    return Err(err);
                }
                self.keys.report_success(&key);
                let value: Value = serde_json::from_str(&text).map_err(|_| {
                    ChannelError::new(
                        "firecrawl",
                        "invalid_response",
                        "Firecrawl 返回了无法解析的 JSON",
                    )
                })?;
                fetched = Some(PageResult {
                    url: url.clone(),
                    title: value["data"]["metadata"]["title"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    content: value["data"]["markdown"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    channel: "firecrawl".to_string(),
                });
                break;
            }
            match fetched {
                Some(page) => pages.push(page),
                None => {
                    return Err(last_error.unwrap_or_else(|| {
                        ChannelError::new("firecrawl", "timed_out", "渠道请求失败")
                    }))
                }
            }
        }
        Ok(pages)
    }
}
