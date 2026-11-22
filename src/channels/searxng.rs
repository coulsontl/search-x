use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;

use super::{status_error, transport_error, Channel};
use crate::model::{ChannelError, SearchResult};

/// 免费渠道：本机/内网自建 SearXNG，JSON API 无需 key。
pub struct SearxngChannel {
    client: reqwest::Client,
    base_url: String,
}

impl SearxngChannel {
    pub fn new(base_url: String, client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

#[async_trait]
impl Channel for SearxngChannel {
    fn name(&self) -> &'static str {
        "searxng"
    }

    async fn search(&self, query: &str, limit: usize, timeout: Duration) -> Result<Vec<SearchResult>, ChannelError> {
        let response = self.client
            .get(format!("{}/search", self.base_url))
            .query(&[("q", query), ("format", "json")])
            .timeout(timeout)
            .send()
            .await
            .map_err(|error| transport_error("searxng", error))?;
        let status = response.status();
        let text = response.text().await.map_err(|error| transport_error("searxng", error))?;
        if !status.is_success() {
            return Err(status_error("searxng", status, &text));
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| ChannelError::new("searxng", "invalid_response", "SearXNG 返回了无法解析的 JSON"))?;
        Ok(value["results"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .take(limit)
                    .filter_map(|row| {
                        let url = row["url"].as_str()?.to_string();
                        Some(SearchResult {
                            title: row["title"].as_str().unwrap_or(&url).to_string(),
                            url,
                            content: row["content"].as_str().unwrap_or_default().to_string(),
                            published_date: row["publishedDate"].as_str().map(str::to_string),
                            channel: "searxng".to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}
