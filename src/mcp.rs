use serde_json::{json, Value};

use crate::model::{FetchRequest, SearchRequest};
use crate::router::Router;

const PROTOCOL_VERSION: &str = "2025-03-26";

pub async fn handle(router: &Router, body: Value) -> Value {
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
        "tools/list" => ok(id, json!({ "tools": tools() })),
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
            call_tool(router, name, arguments, id).await
        }
        "ping" => ok(id, json!({})),
        _ => error(id, -32601, format!("unknown method: {method}")),
    }
}

fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "web_search",
            "description": "Search the web through multiple configured channels in parallel, merging and de-duplicating sources. Modes: fast (free SearXNG only), balanced (default: SearXNG + Exa + Tavily + WorkBuddy), full (all channels). Pass 'channels' to override the mode and use only the listed providers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "queries": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 10 },
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

async fn call_tool(router: &Router, name: &str, arguments: Value, id: Value) -> Value {
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
            if queries.is_empty() || queries.len() > 10 {
                return tool_error(id, "queries 需为 1-10 条");
            }
            let request = SearchRequest {
                queries,
                mode: arguments
                    .get("mode")
                    .and_then(Value::as_str)
                    .map(str::to_string),
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
                limit: arguments
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize),
                no_cache: arguments.get("no_cache").and_then(Value::as_bool),
            };
            let responses = router.search(request).await;
            tool_result(id, serde_json::to_value(responses).unwrap_or(Value::Null))
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
            if let Some(bad) = urls
                .iter()
                .find(|url| !crate::url_guard::is_public_http_url(url))
            {
                return tool_error(id, format!("仅允许公网 http/https URL: {bad}"));
            }
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
            let response = router.fetch(request).await;
            tool_result(id, serde_json::to_value(response).unwrap_or(Value::Null))
        }
        _ => tool_error(id, format!("unknown tool: {name}")),
    }
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

    use super::handle;
    use crate::router::Router;

    fn test_router() -> Router {
        Router::new(
            Vec::new(),
            Vec::new(),
            HashMap::new(),
            "balanced".to_string(),
            10,
            None,
            None,
            10000,
            500,
        )
    }

    #[tokio::test]
    async fn notifications_get_no_response() {
        let result = handle(
            &test_router(),
            serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
        assert!(result.is_null());
    }

    #[tokio::test]
    async fn unknown_method_returns_jsonrpc_error() {
        let result = handle(
            &test_router(),
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "no/such"}),
        )
        .await;
        assert_eq!(result["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn initialize_reports_server_name() {
        let result = handle(
            &test_router(),
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}),
        )
        .await;
        assert_eq!(result["result"]["serverInfo"]["name"], "search-x");
    }
}
