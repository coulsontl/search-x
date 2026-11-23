use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use super::{is_retryable_key_status, status_error, transport_error, Channel};
use crate::keypool::KeyPool;
use crate::model::{ChannelError, PageResult, SearchResult};

pub struct TinyFishChannel {
    client: reqwest::Client,
    search_url: String,
    fetch_url: String,
    keys: KeyPool,
}

impl TinyFishChannel {
    pub fn new(
        search_url: String,
        fetch_url: String,
        keys: KeyPool,
        client: reqwest::Client,
    ) -> Self {
        Self {
            client,
            search_url,
            fetch_url,
            keys,
        }
    }
}

#[async_trait]
impl Channel for TinyFishChannel {
    fn name(&self) -> &'static str {
        "tinyfish"
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
        let mut last_error = None;
        for attempt in 0..max_attempts {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Some(key) = self.keys.next() else {
                return Err(last_error.unwrap_or_else(|| {
                    ChannelError::new("tinyfish", "no_key", "TinyFish 没有可用 key")
                }));
            };
            let response = match self
                .client
                .get(&self.search_url)
                .query(&[("query", query)])
                .header("X-API-Key", &key)
                .timeout(remaining)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("tinyfish", error));
                }
            };
            let status = response.status();
            let text = match response.text().await {
                Ok(text) => text,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("tinyfish", error));
                }
            };
            if !status.is_success() {
                let code = status.as_u16();
                self.keys.report_failure(&key, Some(code));
                let err = status_error("tinyfish", status, &text);
                if attempt + 1 < max_attempts && is_retryable_key_status(code) {
                    last_error = Some(err);
                    continue;
                }
                return Err(err);
            }
            self.keys.report_success(&key);
            let value: Value = serde_json::from_str(&text).map_err(|_| {
                ChannelError::new(
                    "tinyfish",
                    "invalid_response",
                    "TinyFish 返回了无法解析的 JSON",
                )
            })?;
            return Ok(value["results"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .take(limit)
                        .filter_map(|row| {
                            let url = row["url"].as_str()?.to_string();
                            Some(SearchResult {
                                title: row["title"].as_str().unwrap_or(&url).to_string(),
                                url,
                                content: row["snippet"].as_str().unwrap_or_default().to_string(),
                                published_date: row["published_date"].as_str().map(str::to_string),
                                channel: "tinyfish".to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default());
        }
        Err(last_error
            .unwrap_or_else(|| ChannelError::new("tinyfish", "timed_out", "渠道请求失败")))
    }

    async fn fetch(
        &self,
        urls: &[String],
        timeout: Duration,
    ) -> Result<Vec<PageResult>, ChannelError> {
        let deadline = Instant::now() + timeout;
        let max_attempts = if self.keys.has_multiple() { 2 } else { 1 };
        let body = json!({ "urls": urls, "format": "markdown" });
        let mut last_error = None;
        for attempt in 0..max_attempts {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Some(key) = self.keys.next() else {
                return Err(last_error.unwrap_or_else(|| {
                    ChannelError::new("tinyfish", "no_key", "TinyFish 没有可用 key")
                }));
            };
            let response = match self
                .client
                .post(&self.fetch_url)
                .header("X-API-Key", &key)
                .timeout(remaining)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("tinyfish", error));
                }
            };
            let status = response.status();
            let text = match response.text().await {
                Ok(text) => text,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("tinyfish", error));
                }
            };
            if !status.is_success() {
                let code = status.as_u16();
                self.keys.report_failure(&key, Some(code));
                let err = status_error("tinyfish", status, &text);
                if attempt + 1 < max_attempts && is_retryable_key_status(code) {
                    last_error = Some(err);
                    continue;
                }
                return Err(err);
            }
            self.keys.report_success(&key);
            let value: Value = serde_json::from_str(&text).map_err(|_| {
                ChannelError::new(
                    "tinyfish",
                    "invalid_response",
                    "TinyFish 返回了无法解析的 JSON",
                )
            })?;
            let rows = value["results"].as_array().or_else(|| value.as_array());
            return Ok(rows
                .map(|rows| {
                    rows.iter()
                        .map(|row| PageResult {
                            url: row["url"]
                                .as_str()
                                .or_else(|| row["final_url"].as_str())
                                .unwrap_or_default()
                                .to_string(),
                            title: row["title"].as_str().unwrap_or_default().to_string(),
                            content: row["text"].as_str().unwrap_or_default().to_string(),
                            channel: "tinyfish".to_string(),
                        })
                        .filter(|page| !page.url.is_empty())
                        .collect()
                })
                .unwrap_or_default());
        }
        Err(last_error
            .unwrap_or_else(|| ChannelError::new("tinyfish", "timed_out", "渠道请求失败")))
    }
}
