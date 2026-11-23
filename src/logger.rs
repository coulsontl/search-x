use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::json;

use crate::model::{FetchResponse, SearchResponse};

pub struct SearchLogger {
    path: PathBuf,
    max_bytes: u64,
    include_results: bool,
    content_chars: usize,
    lock: Mutex<()>,
}

impl SearchLogger {
    pub fn new(
        path: PathBuf,
        max_size_mb: u64,
        include_results: bool,
        content_chars: usize,
    ) -> Self {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        Self {
            path,
            max_bytes: max_size_mb.saturating_mul(1024 * 1024),
            include_results,
            content_chars,
            lock: Mutex::new(()),
        }
    }

    /// 追加一条搜索 JSONL 记录；超过上限时先把当前文件轮转为 `.1`。
    /// 默认只写查询与统计字段，`include_results = true` 时才附结果明细。
    pub fn log(
        &self,
        source: &str,
        mode: &str,
        limit: usize,
        no_cache: bool,
        requested_channels: Option<&[String]>,
        response: &SearchResponse,
    ) {
        let Ok(_guard) = self.lock.lock() else { return };
        self.rotate_if_needed();
        let mut record = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "kind": "search",
            "source": source,
            "query": response.query,
            "mode": mode,
            "limit": limit,
            "no_cache": no_cache,
            "elapsed_ms": response.elapsed_ms,
            "channels": response.channels,
            "channel_elapsed_ms": response.channel_elapsed_ms,
            "cached": response.cached,
            "result_count": response.results.len(),
            "result_content_chars": response
                .results
                .iter()
                .map(|item| item.content.chars().count())
                .collect::<Vec<_>>(),
            "errors": response.errors,
        });
        if let Some(channels) = requested_channels {
            record["requested_channels"] = json!(channels);
        }
        if self.include_results && !response.cached {
            record["results"] = json!(response
                .results
                .iter()
                .map(|item| {
                    json!({
                        "title": item.title,
                        "url": item.url,
                        "channel": item.channel,
                        "published_date": item.published_date,
                        "content": truncate(&item.content, self.content_chars),
                    })
                })
                .collect::<Vec<_>>());
        }
        self.append(&record);
    }

    /// 抓取日志只记录 URL、渠道与正文长度，不写正文内容。
    pub fn log_fetch(&self, source: &str, response: &FetchResponse) {
        let Ok(_guard) = self.lock.lock() else { return };
        self.rotate_if_needed();
        let record = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "kind": "fetch",
            "source": source,
            "elapsed_ms": response.elapsed_ms,
            "page_count": response.pages.len(),
            "pages": response
                .pages
                .iter()
                .map(|page| json!({
                    "url": page.url,
                    "channel": page.channel,
                    "chars": page.content.chars().count(),
                }))
                .collect::<Vec<_>>(),
            "errors": response.errors,
        });
        self.append(&record);
    }

    /// MCP 层的结果预算日志：记录最终响应字符数与是否发生截断/丢弃。
    pub fn log_mcp_search(
        &self,
        queries: &[String],
        mode: &str,
        limit: Option<usize>,
        response_chars: usize,
        truncated: bool,
        omitted_results: usize,
    ) {
        let Ok(_guard) = self.lock.lock() else { return };
        self.rotate_if_needed();
        let record = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "kind": "mcp",
            "source": "mcp",
            "tool": "web_search",
            "queries": queries,
            "query_count": queries.len(),
            "mode": mode,
            "limit": limit,
            "response_chars": response_chars,
            "truncated": truncated,
            "omitted_results": omitted_results,
        });
        self.append(&record);
    }

    pub fn log_mcp_fetch(
        &self,
        urls: &[String],
        response_chars: usize,
        truncated: bool,
        omitted_pages: usize,
    ) {
        let Ok(_guard) = self.lock.lock() else { return };
        self.rotate_if_needed();
        let record = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "kind": "mcp",
            "source": "mcp",
            "tool": "web_fetch",
            "urls": urls,
            "url_count": urls.len(),
            "response_chars": response_chars,
            "truncated": truncated,
            "omitted_pages": omitted_pages,
        });
        self.append(&record);
    }

    fn append(&self, record: &serde_json::Value) {
        let Ok(line) = serde_json::to_string(record) else {
            return;
        };
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{line}");
        }
    }

    fn rotate_if_needed(&self) {
        let Ok(meta) = fs::metadata(&self.path) else {
            return;
        };
        if meta.len() < self.max_bytes {
            return;
        }
        let backup = self.path.with_extension("jsonl.1");
        let _ = fs::remove_file(&backup);
        let _ = fs::rename(&self.path, backup);
    }
}

fn truncate(value: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    value.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::SearchLogger;
    use crate::model::{FetchResponse, PageResult, SearchResponse, SearchResult};
    use serde_json::Value;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;

    fn temp_log_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("search-x-{tag}-{}.jsonl", uuid::Uuid::new_v4()))
    }

    fn sample_search_response() -> SearchResponse {
        SearchResponse {
            query: "test query".to_string(),
            results: vec![SearchResult {
                title: "title".to_string(),
                url: "https://example.com/".to_string(),
                content: "1234567890".to_string(),
                published_date: None,
                channel: "tavily".to_string(),
            }],
            channels: vec!["tavily".to_string()],
            errors: Vec::new(),
            elapsed_ms: 12,
            cached: false,
            channel_elapsed_ms: HashMap::from([("tavily".to_string(), 10)]),
        }
    }

    #[test]
    fn search_log_omits_results_by_default_and_truncates_when_enabled() {
        let path = temp_log_path("search-metadata");
        let requested = vec!["tavily".to_string()];
        SearchLogger::new(path.clone(), 1, false, 200).log(
            "rest",
            "balanced",
            20,
            false,
            Some(&requested),
            &sample_search_response(),
        );
        let line = fs::read_to_string(&path).unwrap();
        let record: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(record["kind"], "search");
        assert_eq!(record["source"], "rest");
        assert_eq!(record["limit"], 20);
        assert_eq!(record["no_cache"], false);
        assert_eq!(record["requested_channels"][0], "tavily");
        assert_eq!(record["result_count"], 1);
        assert_eq!(record["result_content_chars"][0], 10);
        assert!(record.get("results").is_none());
        fs::remove_file(&path).unwrap();

        let detailed_path = temp_log_path("search-detailed");
        SearchLogger::new(detailed_path.clone(), 1, true, 5).log(
            "rest",
            "balanced",
            20,
            false,
            None,
            &sample_search_response(),
        );
        let line = fs::read_to_string(&detailed_path).unwrap();
        let record: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(record["results"][0]["content"], "12345");
        assert!(record.get("requested_channels").is_none());
        fs::remove_file(&detailed_path).unwrap();
    }

    #[test]
    fn fetch_log_keeps_only_page_metadata() {
        let path = temp_log_path("fetch-metadata");
        let response = FetchResponse {
            pages: vec![PageResult {
                url: "https://example.com/".to_string(),
                title: "title".to_string(),
                content: "abcdef".to_string(),
                channel: "tavily".to_string(),
            }],
            errors: Vec::new(),
            elapsed_ms: 12,
        };
        SearchLogger::new(path.clone(), 1, false, 200).log_fetch("rest", &response);
        let line = fs::read_to_string(&path).unwrap();
        let record: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(record["kind"], "fetch");
        assert_eq!(record["source"], "rest");
        assert_eq!(record["page_count"], 1);
        assert_eq!(record["pages"][0]["channel"], "tavily");
        assert_eq!(record["pages"][0]["chars"], 6);
        assert!(record["pages"][0].get("content").is_none());
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn mcp_search_log_records_budget_fields() {
        let path = temp_log_path("mcp-search");
        SearchLogger::new(path.clone(), 1, false, 200).log_mcp_search(
            &["query".to_string()],
            "balanced",
            Some(20),
            48_582,
            true,
            35,
        );
        let line = fs::read_to_string(&path).unwrap();
        let record: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(record["kind"], "mcp");
        assert_eq!(record["source"], "mcp");
        assert_eq!(record["tool"], "web_search");
        assert_eq!(record["query_count"], 1);
        assert_eq!(record["response_chars"], 48_582);
        assert_eq!(record["truncated"], true);
        assert_eq!(record["omitted_results"], 35);
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn mcp_fetch_log_records_budget_fields() {
        let path = temp_log_path("mcp-fetch");
        SearchLogger::new(path.clone(), 1, false, 200).log_mcp_fetch(
            &["https://example.com/".to_string()],
            12_345,
            true,
            1,
        );
        let line = fs::read_to_string(&path).unwrap();
        let record: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(record["kind"], "mcp");
        assert_eq!(record["tool"], "web_fetch");
        assert_eq!(record["url_count"], 1);
        assert_eq!(record["response_chars"], 12_345);
        assert_eq!(record["truncated"], true);
        assert_eq!(record["omitted_pages"], 1);
        fs::remove_file(&path).unwrap();
    }
}
