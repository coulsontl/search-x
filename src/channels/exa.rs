use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

use super::{status_error, transport_error, Channel};
use crate::keypool::KeyPool;
use crate::model::{ChannelError, PageResult, SearchResult};

pub struct ExaChannel {
    client: reqwest::Client,
    base_url: String,
    keys: KeyPool,
}

impl ExaChannel {
    pub fn new(base_url: String, keys: KeyPool, client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            keys,
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

    async fn search(&self, query: &str, limit: usize, timeout: Duration) -> Result<Vec<SearchResult>, ChannelError> {
        let key = self.keys.next().ok_or_else(|| ChannelError::new("exa", "no_key", "Exa 没有可用 key"))?;
        let body = json!({
            "query": query,
            "numResults": limit.clamp(1, 100),
            "type": "auto",
            "contents": { "text": { "maxCharacters": 3000 } }
        });
        let response = self.client
            .post(format!("{}/search", self.base_url))
            .header("x-api-key", &key)
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                self.keys.report_failure(&key, None);
                transport_error("exa", error)
            })?;
        let status = response.status();
        let text = response.text().await.map_err(|error| transport_error("exa", error))?;
        if !status.is_success() {
            self.keys.report_failure(&key, Some(status.as_u16()));
            return Err(status_error("exa", status, &text));
        }
        self.keys.report_success(&key);
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| ChannelError::new("exa", "invalid_response", "Exa 返回了无法解析的 JSON"))?;
        Ok(parse_results(&value))
    }

    async fn fetch(&self, urls: &[String], timeout: Duration) -> Result<Vec<PageResult>, ChannelError> {
        let key = self.keys.next().ok_or_else(|| ChannelError::new("exa", "no_key", "Exa 没有可用 key"))?;
        let body = json!({ "urls": urls, "text": { "maxCharacters": 5000 } });
        let response = self.client
            .post(format!("{}/contents", self.base_url))
            .header("x-api-key", &key)
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                self.keys.report_failure(&key, None);
                transport_error("exa", error)
            })?;
        let status = response.status();
        let text = response.text().await.map_err(|error| transport_error("exa", error))?;
        if !status.is_success() {
            self.keys.report_failure(&key, Some(status.as_u16()));
            return Err(status_error("exa", status, &text));
        }
        self.keys.report_success(&key);
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| ChannelError::new("exa", "invalid_response", "Exa 返回了无法解析的 JSON"))?;
        Ok(value["results"]
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
            .unwrap_or_default())
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
