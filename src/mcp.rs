use serde_json::{json, Value};

use crate::config::McpConfig;
use crate::model::{FetchRequest, SearchRequest};
use crate::router::Router;

const PROTOCOL_VERSION: &str = "2025-03-26";

pub async fn handle(router: &Router, config: &McpConfig, body: Value) -> Value {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let method = body
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // 通知类消息（如 notifications/initialized）按 MCP 规范不需要响应。
    if method.starts_with("notifications/") {
        return Value::Null;
    }
    match method {
        "initialize" => ok(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "search-x", "version": env!("CARGO_PKG_VERSION") }
            }),
        ),
        "tools/list" => ok(id, json!({ "tools": tools(config.max_queries) })),
        "tools/call" => {
            let params = body.get("params").cloned().unwrap_or(Value::Null);
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            call_tool(router, config, name, arguments, id).await
        }
        "ping" => ok(id, json!({})),
        _ => error(id, -32601, format!("unknown method: {method}")),
    }
}

fn tools(max_queries: usize) -> Vec<Value> {
    vec![
        json!({
            "name": "web_search",
            "description": "Search the web through configured channels. Modes: fast (serial fallback: WorkBuddy -> Tavily -> Exa, 10 results each), balanced (default: Exa + Tavily + WorkBuddy in parallel), full (all enabled channels). Pass 'channels' to override the mode and use only the listed providers. Results are snippet-sized; use web_fetch for full page content.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "queries": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": max_queries },
                    "mode": { "type": "string", "enum": ["fast", "balanced", "full"] },
                    "channels": { "type": "array", "items": { "type": "string" } },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
                    "no_cache": { "type": "boolean", "description": "Skip the query cache and force a fresh search." }
                },
                "required": ["queries"]
            }
        }),
        json!({
            "name": "web_fetch",
            "description": "Fetch and extract readable content from 1-10 public http/https URLs using the first configured fetch-capable channel.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "urls": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 },
                    "channels": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["urls"]
            }
        }),
    ]
}

async fn call_tool(
    router: &Router,
    config: &McpConfig,
    name: &str,
    arguments: Value,
    id: Value,
) -> Value {
    match name {
        "web_search" => {
            let queries = arguments
                .get("queries")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if queries.is_empty() || queries.len() > config.max_queries {
                return tool_error(id, format!("queries 需为 1-{} 条", config.max_queries));
            }
            // 与 REST /search 保持同样的长度上限，避免超长 query 打到上游。
            if queries.iter().any(|query| query.chars().count() > 4000) {
                return tool_error(id, "单条查询不能超过 4000 字符");
            }
            let requested_mode = arguments
                .get("mode")
                .and_then(Value::as_str)
                .map(str::to_string);
            let effective_mode = requested_mode
                .clone()
                .unwrap_or_else(|| router.default_mode().to_string());
            let requested_limit = arguments
                .get("limit")
                .and_then(Value::as_u64)
                .map(|value| value as usize);
            let effective_limit = requested_limit.unwrap_or(config.default_limit);
            let queries_for_log = queries.clone();
            let request = SearchRequest {
                queries,
                mode: requested_mode,
                channels: arguments
                    .get("channels")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    }),
                limit: Some(effective_limit),
                no_cache: arguments.get("no_cache").and_then(Value::as_bool),
            };
            let responses = router.search_with_source(request, "mcp").await;
            let payload = serde_json::to_value(responses).unwrap_or(Value::Null);
            let payload =
                cap_search_payload(payload, config.max_response_chars, config.max_content_chars);
            let response_chars = serialized_chars(&payload);
            let (truncated, omitted_results) = search_cap_stats(&payload);
            router.log_mcp_search(
                &queries_for_log,
                &effective_mode,
                Some(effective_limit),
                response_chars,
                truncated,
                omitted_results,
            );
            if response_chars > config.max_response_chars {
                return tool_error(
                    id,
                    "搜索结果过大，已超过 MCP 输出预算；请减少 queries 或 limit",
                );
            }
            tool_result(id, payload)
        }
        "web_fetch" => {
            let urls = arguments
                .get("urls")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if urls.is_empty() || urls.len() > 10 {
                return tool_error(id, "urls 需为 1-10 个");
            }
            for url in &urls {
                if !crate::url_guard::is_public_http_url_resolved(url).await {
                    return tool_error(id, "仅允许公网 http/https URL");
                }
            }
            let urls_for_log = urls.clone();
            let request = FetchRequest {
                urls,
                channels: arguments
                    .get("channels")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    }),
            };
            let response = router.fetch_with_source(request, "mcp").await;
            let payload = serde_json::to_value(response).unwrap_or(Value::Null);
            let payload = cap_fetch_payload(
                payload,
                config.max_response_chars,
                config.max_fetch_content_chars,
            );
            let response_chars = serialized_chars(&payload);
            let (truncated, omitted_pages) = fetch_cap_stats(&payload);
            router.log_mcp_fetch(&urls_for_log, response_chars, truncated, omitted_pages);
            if response_chars > config.max_response_chars {
                return tool_error(id, "抓取结果过大，已超过 MCP 输出预算；请减少 urls");
            }
            tool_result(id, payload)
        }
        _ => tool_error(id, format!("unknown tool: {name}")),
    }
}

/// 按 Unicode 字符截断一个 JSON 字符串字段；返回是否真的发生了截断。
fn truncate_field(value: &mut Value, field: &str, max_chars: usize) -> bool {
    let Some(text) = value.get(field).and_then(Value::as_str) else {
        return false;
    };
    if text.chars().count() <= max_chars {
        return false;
    }
    value[field] = Value::String(text.chars().take(max_chars).collect());
    true
}

fn serialized_chars(value: &Value) -> usize {
    serde_json::to_string(value)
        .map(|text| text.chars().count())
        .unwrap_or(0)
}

/// MCP 结果必须一次性放进客户端上下文：先压每条正文，再从排名最低的结果开始丢，
/// 直到整个响应不超过 `max_chars`。
fn cap_search_payload(mut payload: Value, max_chars: usize, max_content_chars: usize) -> Value {
    if let Some(responses) = payload.as_array_mut() {
        for response in responses.iter_mut() {
            let mut content_truncated = false;
            if let Some(results) = response.get_mut("results").and_then(Value::as_array_mut) {
                for result in results {
                    content_truncated |= truncate_field(result, "content", max_content_chars);
                }
            }
            if content_truncated {
                response["truncated"] = Value::Bool(true);
            }
        }
    }

    while serialized_chars(&payload) > max_chars {
        let target = payload.as_array().and_then(|responses| {
            responses
                .iter()
                .enumerate()
                .filter_map(|(index, response)| {
                    response
                        .get("results")
                        .and_then(Value::as_array)
                        .map(|results| (index, results.len()))
                })
                .max_by_key(|(_, result_count)| *result_count)
                .map(|(index, _)| index)
        });
        let Some(index) = target else {
            break;
        };
        let Some(results) = payload[index]
            .get_mut("results")
            .and_then(Value::as_array_mut)
        else {
            break;
        };
        if results.pop().is_none() {
            break;
        }
        let omitted = payload[index]
            .get("omitted_results")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        payload[index]["truncated"] = Value::Bool(true);
        payload[index]["omitted_results"] = json!(omitted + 1);
    }
    payload
}

/// web_fetch 没有排名概念：按 URL 顺序保留，超出预算时从尾部丢页面。
fn cap_fetch_payload(mut payload: Value, max_chars: usize, max_content_chars: usize) -> Value {
    let mut content_truncated = false;
    if let Some(pages) = payload.get_mut("pages").and_then(Value::as_array_mut) {
        for page in pages {
            content_truncated |= truncate_field(page, "content", max_content_chars);
        }
    }
    if content_truncated {
        payload["truncated"] = Value::Bool(true);
    }

    let mut omitted_pages = 0usize;
    while serialized_chars(&payload) > max_chars {
        let Some(pages) = payload.get_mut("pages").and_then(Value::as_array_mut) else {
            break;
        };
        if pages.pop().is_none() {
            break;
        }
        omitted_pages += 1;
    }
    if omitted_pages > 0 {
        payload["truncated"] = Value::Bool(true);
        payload["omitted_pages"] = json!(omitted_pages);
    }
    payload
}

fn search_cap_stats(payload: &Value) -> (bool, usize) {
    let mut truncated = false;
    let mut omitted_results = 0usize;
    if let Some(responses) = payload.as_array() {
        for response in responses {
            truncated |= response
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            omitted_results += response
                .get("omitted_results")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
        }
    }
    (truncated, omitted_results)
}

fn fetch_cap_stats(payload: &Value) -> (bool, usize) {
    (
        payload
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        payload
            .get("omitted_pages")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
    )
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_result(id: Value, payload: Value) -> Value {
    ok(
        id,
        json!({
            "content": [{ "type": "text", "text": serde_json::to_string(&payload).unwrap_or_default() }],
            "isError": false
        }),
    )
}

fn tool_error(id: Value, message: impl Into<String>) -> Value {
    ok(
        id,
        json!({
            "content": [{ "type": "text", "text": message.into() }],
            "isError": true
        }),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::{json, Value};

    use super::{cap_fetch_payload, cap_search_payload, handle, serialized_chars};
    use crate::config::McpConfig;
    use crate::router::Router;

    fn test_router() -> Router {
        Router::new(
            Vec::new(),
            Vec::new(),
            HashMap::new(),
            "balanced".to_string(),
            10,
            10,
            vec![
                "workbuddy".to_string(),
                "tavily".to_string(),
                "exa".to_string(),
            ],
            5,
            8000,
            None,
            None,
            10000,
            500,
            vec![
                "firecrawl".to_string(),
                "tavily".to_string(),
                "exa".to_string(),
                "tinyfish".to_string(),
            ],
            None,
        )
    }

    fn test_config() -> McpConfig {
        McpConfig::default()
    }

    #[tokio::test]
    async fn notifications_get_no_response() {
        let result = handle(
            &test_router(),
            &test_config(),
            serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
        assert!(result.is_null());
    }

    #[tokio::test]
    async fn unknown_method_returns_jsonrpc_error() {
        let result = handle(
            &test_router(),
            &test_config(),
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "no/such"}),
        )
        .await;
        assert_eq!(result["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn initialize_reports_server_name() {
        let result = handle(
            &test_router(),
            &test_config(),
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}),
        )
        .await;
        assert_eq!(result["result"]["serverInfo"]["name"], "search-x");
    }

    #[test]
    fn search_payload_respects_total_and_content_limits() {
        let results: Vec<Value> = (0..20)
            .map(|index| {
                json!({
                    "title": format!("title {index}"),
                    "url": format!("https://example.com/{index}"),
                    "content": "x".repeat(8000),
                    "channel": "test"
                })
            })
            .collect();
        let payload = json!([{
            "query": "q",
            "results": results,
            "channels": ["test"],
            "elapsed_ms": 1
        }]);

        let capped = cap_search_payload(payload, 10_000, 1500);

        assert!(serialized_chars(&capped) <= 10_000);
        assert_eq!(capped[0]["truncated"], true);
        assert!(capped[0]["results"].as_array().unwrap().len() < 20);
        assert!(
            capped[0]["results"][0]["content"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= 1500
        );
    }

    #[test]
    fn fetch_payload_respects_total_and_content_limits() {
        let pages: Vec<Value> = (0..3)
            .map(|index| {
                json!({
                    "url": format!("https://example.com/{index}"),
                    "title": format!("title {index}"),
                    "content": "y".repeat(10000),
                    "channel": "test"
                })
            })
            .collect();
        let payload = json!({ "pages": pages, "elapsed_ms": 1 });

        let capped = cap_fetch_payload(payload, 5000, 4000);

        assert!(serialized_chars(&capped) <= 5000);
        assert_eq!(capped["truncated"], true);
        assert!(capped["omitted_pages"].as_u64().unwrap() >= 1);
        assert!(
            capped["pages"][0]["content"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= 4000
        );
    }
}
