use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use super::{status_error, transport_error, Channel};
use crate::model::{ChannelError, SearchResult};
use crate::workbuddy_accounts::{WorkBuddyAccount, WorkBuddyAccountStore};

const WORKBUDDY_USER_AGENT: &str = "WorkBuddy/5.9.3 WorkBuddy/5.9.3 CLI/2.165.0";
const UPSTREAM_MAX_RESULTS: usize = 10;
const MAX_ACCOUNT_ATTEMPTS: usize = 8;

/// 经 `.3` 的 agent2api `/api/web-search/search`；需要把 workbuddy 加入其 selection.search。
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

/// 直连 WorkBuddy 上游：账号凭据旁路取自 agent2api 的账号库，
/// 每次请求轮询一个未过期账号，最多取 10 条；全部失败时回退 agent2api 并记日志。
pub struct DirectWorkBuddyChannel {
    client: reqwest::Client,
    accounts: Arc<WorkBuddyAccountStore>,
    max_results: usize,
    fallback: Option<Arc<dyn Channel>>,
}

impl DirectWorkBuddyChannel {
    pub fn new(
        client: reqwest::Client,
        accounts: Arc<WorkBuddyAccountStore>,
        max_results: usize,
        fallback: Option<Arc<dyn Channel>>,
    ) -> Self {
        Self {
            client,
            accounts,
            max_results: max_results.clamp(1, UPSTREAM_MAX_RESULTS),
            fallback,
        }
    }

    async fn search_with_account(
        &self,
        account: &WorkBuddyAccount,
        query: &str,
        max_results: usize,
        timeout: Duration,
    ) -> Result<Vec<SearchResult>, ChannelError> {
        let trace = uuid::Uuid::new_v4().simple().to_string();
        let span = trace[..16].to_string();
        let request_id = uuid::Uuid::new_v4().to_string();
        let conversation_id = uuid::Uuid::new_v4().to_string();
        let mut request = self
            .client
            .post(format!(
                "{}/agenttool/v1/search",
                account.endpoint.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {}", account.access_token))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", WORKBUDDY_USER_AGENT)
            .header("X-IDE-Type", "WorkBuddy")
            .header("X-IDE-Name", "WorkBuddy")
            .header("X-IDE-Version", "5.9.3")
            .header("X-Product", "SaaS")
            .header("X-Agent-Intent", "craft")
            .header("X-Agent-Purpose", "conversation")
            .header("X-Agent-Type", "main")
            .header("X-Agent-Runtime", "local-agent")
            .header("X-Private-Data", "true")
            .header("X-Requested-With", "XMLHttpRequest")
            .header("X-Request-ID", request_id.clone())
            .header("X-Trace-ID", trace.clone())
            .header("traceparent", format!("00-{trace}-{span}-01"))
            .header("b3", format!("{trace}-{span}-1"))
            .header("X-B3-TraceId", trace.clone())
            .header("X-B3-SpanId", span)
            .header("X-B3-Sampled", "1")
            .header("X-Conversation-ID", conversation_id)
            .header("X-Conversation-Request-ID", request_id.clone())
            .header("X-Root-Request-ID", request_id.clone())
            .header("X-Conversation-Message-ID", request_id)
            .timeout(timeout);
        if !account.uid.is_empty() {
            request = request.header("X-User-Id", &account.uid);
        }
        if !account.enterprise_id.is_empty() {
            request = request.header("X-Enterprise-Id", &account.enterprise_id);
            request = request.header("X-Tenant-Id", &account.enterprise_id);
        }
        let domain = if account.domain.is_empty() {
            "www.workbuddy.cn"
        } else {
            account.domain.as_str()
        };
        request = request.header("X-Domain", domain);
        if !account.department.is_empty() {
            request = request.header("X-Department-Info", &account.department);
        }

        let response = request
            .json(&json!({ "query": query, "type": "text2text", "max_results": max_results }))
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
                "WorkBuddy 上游返回了无法解析的 JSON",
            )
        })?;
        if value["code"].as_i64() == Some(15001) {
            return Err(ChannelError::new(
                "workbuddy",
                "rate_limited",
                "WorkBuddy 搜索额度已用尽",
            ));
        }
        let rows = value["results"].as_array().ok_or_else(|| {
            ChannelError::new(
                "workbuddy",
                "invalid_response",
                "WorkBuddy 响应缺少 results",
            )
        })?;
        let mut results = Vec::new();
        for row in rows {
            let Some(url) = row["url"].as_str() else {
                continue;
            };
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                continue;
            }
            let published_date = row["publishedDate"]
                .as_str()
                .map(str::to_string)
                .or_else(|| {
                    row["publishedDate"]
                        .as_i64()
                        .and_then(chrono::DateTime::from_timestamp_millis)
                        .map(|value| value.to_rfc3339())
                });
            results.push(SearchResult {
                title: row["title"].as_str().unwrap_or(url).to_string(),
                url: url.to_string(),
                content: row["snippet"].as_str().unwrap_or_default().to_string(),
                published_date,
                channel: "workbuddy".to_string(),
            });
        }
        Ok(results)
    }
}

#[async_trait]
impl Channel for DirectWorkBuddyChannel {
    fn name(&self) -> &'static str {
        "workbuddy"
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
        timeout: Duration,
    ) -> Result<Vec<SearchResult>, ChannelError> {
        let max_results = self.max_results.min(limit.max(1));
        let mut last_error: Option<ChannelError> = None;
        for _ in 0..MAX_ACCOUNT_ATTEMPTS {
            let account = match self.accounts.next_account(false) {
                Ok(Some(account)) => account,
                Ok(None) => {
                    last_error.get_or_insert_with(|| {
                        ChannelError::new("workbuddy", "no_account", "没有未过期的 WorkBuddy 账号")
                    });
                    break;
                }
                Err(error) => {
                    // 路径与系统错误只写服务端日志，不回给客户端。
                    tracing::error!("workbuddy 账号库读取失败: {error}");
                    last_error.get_or_insert_with(|| {
                        ChannelError::new("workbuddy", "store", "WorkBuddy 账号库不可用")
                    });
                    break;
                }
            };
            match self
                .search_with_account(&account, query, max_results, timeout)
                .await
            {
                Ok(results) if !results.is_empty() => return Ok(results),
                Ok(_) => {}
                Err(error) => {
                    if error.code == "unauthorized" {
                        // token 可能刚被 agent2api 刷新，下一次取号会重读库。
                        self.accounts.invalidate();
                    }
                    last_error = Some(error);
                }
            }
        }

        let reason = last_error
            .map(|error| error.message)
            .unwrap_or_else(|| "WorkBuddy 直连没有返回结果".to_string());
        match &self.fallback {
            Some(fallback) => {
                tracing::warn!("workbuddy 直连失败（{reason}），回退 agent2api");
                fallback.search(query, limit, timeout).await
            }
            None => Err(ChannelError::new(
                "workbuddy",
                "direct_failed",
                format!("WorkBuddy 直连失败：{reason}"),
            )),
        }
    }
}
