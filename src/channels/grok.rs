use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use super::{is_retryable_key_status, status_error, transport_error, Channel};
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

    async fn search(
        &self,
        query: &str,
        _limit: usize,
        timeout: Duration,
    ) -> Result<Vec<SearchResult>, ChannelError> {
        let deadline = Instant::now() + timeout;
        let max_attempts = if self.keys.has_multiple() { 2 } else { 1 };
        let body = json!({
            "model": self.model,
            "input": [{ "role": "user", "content": [{ "type": "input_text", "text": query }] }],
            "tools": [{ "type": "web_search" }]
        });
        let mut last_error = None;
        for attempt in 0..max_attempts {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Some(key) = self.keys.next() else {
                return Err(last_error
                    .unwrap_or_else(|| ChannelError::new("grok", "no_key", "Grok 没有可用 key")));
            };
            let response = match self
                .client
                .post(format!("{}/responses", self.base_url))
                .bearer_auth(&key)
                .timeout(remaining)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("grok", error));
                }
            };
            let status = response.status();
            let text = match response.text().await {
                Ok(text) => text,
                Err(error) => {
                    self.keys.report_failure(&key, None);
                    return Err(transport_error("grok", error));
                }
            };
            if !status.is_success() {
                let code = status.as_u16();
                self.keys.report_failure(&key, Some(code));
                let err = status_error("grok", status, &text);
                if attempt + 1 < max_attempts && is_retryable_key_status(code) {
                    last_error = Some(err);
                    continue;
                }
                return Err(err);
            }
            self.keys.report_success(&key);
            let value: Value = serde_json::from_str(&text).map_err(|_| {
                ChannelError::new("grok", "invalid_response", "Grok 返回了无法解析的 JSON")
            })?;
            return Ok(parse_sources(&value));
        }
        Err(last_error.unwrap_or_else(|| ChannelError::new("grok", "timed_out", "渠道请求失败")))
    }
}

/// 从 Responses 的 output 里收集最终综合答案与 web_search_call.sources。
/// 综合答案始终作为首条保留；每条 citation 保证非空 content，避免被空正文过滤器误删。
fn parse_sources(value: &Value) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let Some(output) = value["output"].as_array() else {
        return results;
    };

    let answer_text: String = output
        .iter()
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if !answer_text.is_empty() {
        results.push(SearchResult {
            title: "Grok 综合答案".to_string(),
            url: "grok://answer".to_string(),
            content: answer_text,
            published_date: None,
            channel: "grok".to_string(),
        });
    }

    for item in output {
        if item["type"].as_str() == Some("web_search_call") {
            if let Some(sources) = item["action"]["sources"].as_array() {
                for source in sources {
                    let Some(url) = source["url"].as_str().filter(|u| !u.trim().is_empty()) else {
                        continue;
                    };
                    let title = source["title"]
                        .as_str()
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .unwrap_or(url)
                        .to_string();
                    let snippet = source["snippet"]
                        .as_str()
                        .or_else(|| source["description"].as_str())
                        .or_else(|| source["content"].as_str())
                        .or_else(|| source["text"].as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| title.clone());
                    results.push(SearchResult {
                        title,
                        url: url.to_string(),
                        content: snippet,
                        published_date: source["published_date"]
                            .as_str()
                            .or_else(|| source["publishedDate"].as_str())
                            .map(str::to_string),
                        channel: "grok".to_string(),
                    });
                }
            }
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::parse_sources;

    #[test]
    fn keeps_synthesized_answer_and_non_empty_source_content() {
        let payload = json!({
            "output": [
                {
                    "type": "web_search_call",
                    "action": {
                        "sources": [
                            { "title": "Rust Official", "url": "https://www.rust-lang.org/" },
                            {
                                "title": "Tokio Docs",
                                "url": "https://tokio.rs/",
                                "snippet": "Async runtime for Rust"
                            }
                        ]
                    }
                },
                {
                    "type": "message",
                    "content": [{ "type": "output_text", "text": "Rust is a systems language." }]
                }
            ]
        });

        let results = parse_sources(&payload);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].url, "grok://answer");
        assert_eq!(results[0].content, "Rust is a systems language.");
        assert_eq!(results[1].url, "https://www.rust-lang.org/");
        assert_eq!(results[1].content, "Rust Official");
        assert_eq!(results[2].content, "Async runtime for Rust");
        assert!(results.iter().all(|item| !item.content.trim().is_empty()));
    }

    #[test]
    fn handles_answer_without_sources() {
        let payload = json!({
            "output": [
                {
                    "type": "message",
                    "content": [{ "type": "output_text", "text": "Direct answer" }]
                }
            ]
        });
        let results = parse_sources(&payload);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "grok://answer");
        assert_eq!(results[0].content, "Direct answer");
    }
}
