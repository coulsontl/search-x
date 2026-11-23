use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use super::{is_retryable_key_status, status_error, transport_error, Channel};
use crate::keypool::KeyPool;
use crate::model::{ChannelError, PageResult, SearchResult};

pub struct ExaChannel {
    client: reqwest::Client,
    base_url: String,
    keys: KeyPool,
    fetch_max_content_chars: usize,
}

impl ExaChannel {
    pub fn new(
        base_url: String,
        keys: KeyPool,
        client: reqwest::Client,
        fetch_max_content_chars: usize,
    ) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            keys,
            fetch_max_content_chars: fetch_max_content_chars.max(500),
        }
    }
}

#[async_trait]
impl Channel for ExaChannel {
    fn name(&self) -> &'static str {
        "exa"
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
            "numResults": limit.clamp(1, 100),
            "type": "auto",
            "contents": { "text": { "maxCharacters": 3000 } }
        });
        let mut last_error = None;
        for attempt in 0..max_attempts {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Some(key) = self.keys.next() else {
                return Err(last_error
                    .unwrap_or_else(|| ChannelError::new("exa", "no_key", "Exa 没有可用 key")));
            };
            let response = match self
                .client
                .post(format!("{}/search", self.base_url))
                .header("x-api-key", &key)
                .timeout(remaining)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("exa", error));
                }
            };
            let status = response.status();
            let text = match response.text().await {
                Ok(text) => text,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("exa", error));
                }
            };
            if !status.is_success() {
                let code = status.as_u16();
                self.keys.report_failure(&key, Some(code));
                let err = status_error("exa", status, &text);
                if attempt + 1 < max_attempts && is_retryable_key_status(code) {
                    last_error = Some(err);
                    continue;
                }
                return Err(err);
            }
            self.keys.report_success(&key);
            let value: Value = serde_json::from_str(&text).map_err(|_| {
                ChannelError::new("exa", "invalid_response", "Exa 返回了无法解析的 JSON")
            })?;
            return Ok(parse_results(&value));
        }
        Err(last_error.unwrap_or_else(|| ChannelError::new("exa", "timed_out", "渠道请求失败")))
    }

    async fn fetch(
        &self,
        urls: &[String],
        timeout: Duration,
    ) -> Result<Vec<PageResult>, ChannelError> {
        let deadline = Instant::now() + timeout;
        let max_attempts = if self.keys.has_multiple() { 2 } else { 1 };
        let body = json!({
            "urls": urls,
            "text": { "maxCharacters": self.fetch_max_content_chars }
        });
        let mut last_error = None;
        for attempt in 0..max_attempts {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Some(key) = self.keys.next() else {
                return Err(last_error
                    .unwrap_or_else(|| ChannelError::new("exa", "no_key", "Exa 没有可用 key")));
            };
            let response = match self
                .client
                .post(format!("{}/contents", self.base_url))
                .header("x-api-key", &key)
                .timeout(remaining)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("exa", error));
                }
            };
            let status = response.status();
            let text = match response.text().await {
                Ok(text) => text,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("exa", error));
                }
            };
            if !status.is_success() {
                let code = status.as_u16();
                self.keys.report_failure(&key, Some(code));
                let err = status_error("exa", status, &text);
                if attempt + 1 < max_attempts && is_retryable_key_status(code) {
                    last_error = Some(err);
                    continue;
                }
                return Err(err);
            }
            self.keys.report_success(&key);
            let value: Value = serde_json::from_str(&text).map_err(|_| {
                ChannelError::new("exa", "invalid_response", "Exa 返回了无法解析的 JSON")
            })?;
            return Ok(value["results"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .map(|row| PageResult {
                            url: row["url"].as_str().unwrap_or_default().to_string(),
                            title: row["title"].as_str().unwrap_or_default().to_string(),
                            content: row["text"].as_str().unwrap_or_default().to_string(),
                            channel: "exa".to_string(),
                        })
                        .filter(|page| !page.url.is_empty())
                        .collect()
                })
                .unwrap_or_default());
        }
        Err(last_error.unwrap_or_else(|| ChannelError::new("exa", "timed_out", "渠道请求失败")))
    }
}

fn parse_results(value: &Value) -> Vec<SearchResult> {
    value["results"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let url = row["url"].as_str()?.to_string();
                    Some(SearchResult {
                        title: row["title"].as_str().unwrap_or(&url).to_string(),
                        url,
                        content: row["text"].as_str().unwrap_or_default().to_string(),
                        published_date: row["publishedDate"].as_str().map(str::to_string),
                        channel: "exa".to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}
