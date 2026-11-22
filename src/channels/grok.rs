use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

use super::{status_error, transport_error, Channel};
use crate::keypool::KeyPool;
use crate::model::{ChannelError, SearchResult};

/// 直连 OpenAI Responses 兼容端点（grok2api / AxonHub），仅作兜底。
pub struct GrokChannel {
    client: reqwest::Client,
    base_url: String,
    model: String,
    keys: KeyPool,
}

impl GrokChannel {
    pub fn new(base_url: String, model: String, keys: KeyPool, client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            keys,
        }
    }
}

#[async_trait]
impl Channel for GrokChannel {
    fn name(&self) -> &'static str {
        "grok"
    }

    async fn search(&self, query: &str, _limit: usize, timeout: Duration) -> Result<Vec<SearchResult>, ChannelError> {
        let key = self.keys.next().ok_or_else(|| ChannelError::new("grok", "no_key", "Grok 没有可用 key"))?;
        let body = json!({
            "model": self.model,
            "input": [{ "role": "user", "content": [{ "type": "input_text", "text": query }] }],
            "tools": [{ "type": "web_search" }]
        });
        let response = self.client
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&key)
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                self.keys.report_failure(&key, None);
                transport_error("grok", error)
            })?;
        let status = response.status();
        let text = response.text().await.map_err(|error| transport_error("grok", error))?;
        if !status.is_success() {
            self.keys.report_failure(&key, Some(status.as_u16()));
            return Err(status_error("grok", status, &text));
        }
        self.keys.report_success(&key);
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| ChannelError::new("grok", "invalid_response", "Grok 返回了无法解析的 JSON"))?;
        Ok(parse_sources(&value))
    }
}

/// 从 Responses 的 output 里收集 web_search_call.sources 与最终文本。
fn parse_sources(value: &Value) -> Vec<SearchResult> {
    let mut results = Vec::new();
    if let Some(output) = value["output"].as_array() {
        for item in output {
            if item["type"].as_str() == Some("web_search_call") {
                if let Some(sources) = item["action"]["sources"].as_array() {
                    for source in sources {
                        if let Some(url) = source["url"].as_str() {
                            results.push(SearchResult {
                                title: source["title"].as_str().unwrap_or(url).to_string(),
                                url: url.to_string(),
                                content: String::new(),
                                published_date: None,
                                channel: "grok".to_string(),
                            });
                        }
                    }
                }
            }
        }
        if results.is_empty() {
            let text: String = output
                .iter()
                .filter_map(|item| item["content"].as_array())
                .flatten()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                results.push(SearchResult {
                    title: "Grok 综合答案".to_string(),
                    url: "grok://answer".to_string(),
                    content: text,
                    published_date: None,
                    channel: "grok".to_string(),
                });
            }
        }
    }
    results
}
