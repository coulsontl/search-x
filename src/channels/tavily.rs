use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

use super::{status_error, transport_error, Channel};
use crate::keypool::KeyPool;
use crate::model::{ChannelError, PageResult, SearchResult};

pub struct TavilyChannel {
    client: reqwest::Client,
    base_url: String,
    keys: KeyPool,
}

impl TavilyChannel {
    pub fn new(base_url: String, keys: KeyPool, client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            keys,
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
        let key = self
            .keys
            .next()
            .ok_or_else(|| ChannelError::new("tavily", "no_key", "Tavily 没有可用 key"))?;
        let body = json!({
            "query": query,
            "search_depth": "basic",
            "max_results": limit.clamp(1, 20),
            "include_raw_content": true
        });
        let response = self
            .client
            .post(format!("{}/search", self.base_url))
            .bearer_auth(&key)
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                self.keys.report_failure(&key, None);
                transport_error("tavily", error)
            })?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| transport_error("tavily", error))?;
        if !status.is_success() {
            self.keys.report_failure(&key, Some(status.as_u16()));
            return Err(status_error("tavily", status, &text));
        }
        self.keys.report_success(&key);
        let value: Value = serde_json::from_str(&text).map_err(|_| {
            ChannelError::new("tavily", "invalid_response", "Tavily 返回了无法解析的 JSON")
        })?;
        Ok(value["results"]
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
            .unwrap_or_default())
    }

    async fn fetch(
        &self,
        urls: &[String],
        timeout: Duration,
    ) -> Result<Vec<PageResult>, ChannelError> {
        let key = self
            .keys
            .next()
            .ok_or_else(|| ChannelError::new("tavily", "no_key", "Tavily 没有可用 key"))?;
        let body = json!({ "urls": urls });
        let response = self
            .client
            .post(format!("{}/extract", self.base_url))
            .bearer_auth(&key)
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                self.keys.report_failure(&key, None);
                transport_error("tavily", error)
            })?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| transport_error("tavily", error))?;
        if !status.is_success() {
            self.keys.report_failure(&key, Some(status.as_u16()));
            return Err(status_error("tavily", status, &text));
        }
        self.keys.report_success(&key);
        let value: Value = serde_json::from_str(&text).map_err(|_| {
            ChannelError::new("tavily", "invalid_response", "Tavily 返回了无法解析的 JSON")
        })?;
        Ok(value["results"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|row| PageResult {
                        url: row["url"].as_str().unwrap_or_default().to_string(),
                        title: String::new(),
                        content: row["raw_content"].as_str().unwrap_or_default().to_string(),
                        channel: "tavily".to_string(),
                    })
                    .filter(|page| !page.url.is_empty())
                    .collect()
            })
            .unwrap_or_default())
    }
}
