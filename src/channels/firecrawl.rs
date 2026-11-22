use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

use super::{status_error, transport_error, Channel};
use crate::keypool::KeyPool;
use crate::model::{ChannelError, PageResult, SearchResult};

pub struct FirecrawlChannel {
    client: reqwest::Client,
    base_url: String,
    keys: KeyPool,
}

impl FirecrawlChannel {
    pub fn new(base_url: String, keys: KeyPool, client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            keys,
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

    async fn search(&self, query: &str, limit: usize, timeout: Duration) -> Result<Vec<SearchResult>, ChannelError> {
        let key = self.keys.next().ok_or_else(|| ChannelError::new("firecrawl", "no_key", "Firecrawl 没有可用 key"))?;
        let body = json!({
            "query": query,
            "limit": limit.clamp(1, 100),
            "sources": ["web"],
            "scrapeOptions": { "formats": ["markdown"], "onlyMainContent": true }
        });
        let response = self.client
            .post(format!("{}/v2/search", self.base_url))
            .bearer_auth(&key)
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                self.keys.report_failure(&key, None);
                transport_error("firecrawl", error)
            })?;
        let status = response.status();
        let text = response.text().await.map_err(|error| transport_error("firecrawl", error))?;
        if !status.is_success() {
            self.keys.report_failure(&key, Some(status.as_u16()));
            return Err(status_error("firecrawl", status, &text));
        }
        self.keys.report_success(&key);
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| ChannelError::new("firecrawl", "invalid_response", "Firecrawl 返回了无法解析的 JSON"))?;
        let rows = value["data"]["web"].as_array().or_else(|| value["data"].as_array());
        Ok(rows
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| {
                        let url = row["url"].as_str()?.to_string();
                        let content = row["markdown"].as_str()
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
            .unwrap_or_default())
    }

    async fn fetch(&self, urls: &[String], timeout: Duration) -> Result<Vec<PageResult>, ChannelError> {
        let key = self.keys.next().ok_or_else(|| ChannelError::new("firecrawl", "no_key", "Firecrawl 没有可用 key"))?;
        let mut pages = Vec::new();
        for url in urls {
            let body = json!({ "url": url, "formats": ["markdown"], "onlyMainContent": true });
            let response = self.client
                .post(format!("{}/v2/scrape", self.base_url))
                .bearer_auth(&key)
                .timeout(timeout)
                .json(&body)
                .send()
                .await
                .map_err(|error| {
                    self.keys.report_failure(&key, None);
                    transport_error("firecrawl", error)
                })?;
            let status = response.status();
            let text = response.text().await.map_err(|error| transport_error("firecrawl", error))?;
            if !status.is_success() {
                self.keys.report_failure(&key, Some(status.as_u16()));
                return Err(status_error("firecrawl", status, &text));
            }
            self.keys.report_success(&key);
            let value: Value = serde_json::from_str(&text)
                .map_err(|_| ChannelError::new("firecrawl", "invalid_response", "Firecrawl 返回了无法解析的 JSON"))?;
            pages.push(PageResult {
                url: url.clone(),
                title: value["data"]["metadata"]["title"].as_str().unwrap_or_default().to_string(),
                content: value["data"]["markdown"].as_str().unwrap_or_default().to_string(),
                channel: "firecrawl".to_string(),
            });
        }
        Ok(pages)
    }
}
