use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

use super::{status_error, transport_error, Channel};
use crate::model::{ChannelError, SearchResult};

/// 走 `.3` 的 agent2api `/api/web-search/search`；需要把 workbuddy 加入其 selection.search。
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
        results.truncate(limit.max(5));
        Ok(results)
    }
}
