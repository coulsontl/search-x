//! 从 agent2api 的导出接口读取 WorkBuddy 账号。
//!
//! 导出接口直接返回明文字段；本模块只按 TTL 缓存账号，token 刷新完全交给
//! agent2api，本模块绝不写库、也绝不调用 refresh 接口。

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const DEFAULT_ENDPOINT: &str = "https://www.workbuddy.cn";
const EXPORT_TIMEOUT_SECS: u64 = 15;
/// 缓存里的账号全部过期时，距上次加载超过这个间隔才强制重取，避免打爆账号来源。
const MIN_EXPIRED_RELOAD_INTERVAL: Duration = Duration::from_secs(30);

// 不实现 Debug：结构体持有 accessToken，避免未来 `{:?}` 把凭据打进日志。
#[derive(Clone)]
pub struct WorkBuddyAccount {
    pub access_token: String,
    pub uid: String,
    pub enterprise_id: String,
    pub department: String,
    pub domain: String,
    pub endpoint: String,
    pub expires_at_ms: Option<i64>,
}

impl WorkBuddyAccount {
    /// 提前 60 秒视为过期，避免请求发出时刚好失效。
    pub fn is_expired(&self, now_ms: i64) -> bool {
        self.expires_at_ms
            .is_some_and(|expires| expires <= now_ms + 60_000)
    }
}

struct StoreState {
    accounts: Vec<WorkBuddyAccount>,
    loaded_at: Option<Instant>,
    cursor: usize,
}

fn next_cached_account(state: &mut StoreState) -> Option<WorkBuddyAccount> {
    let count = state.accounts.len();
    if count == 0 {
        return None;
    }
    let now_ms = now_millis();
    for _ in 0..count {
        let index = state.cursor % count;
        state.cursor = (state.cursor + 1) % count;
        let account = &state.accounts[index];
        if !account.is_expired(now_ms) {
            return Some(account.clone());
        }
    }
    None
}

/// 从 agent2api 的账号导出接口取明文凭据（`POST /api/accounts/export-files`），
/// 只保留 accessToken；刷新仍完全由 agent2api 负责。
pub struct WorkBuddyAccountStore {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    provider: String,
    ttl: Duration,
    state: Mutex<StoreState>,
    /// 合并并发刷新，避免冷启动或 TTL 到期时多 query 同时打爆导出接口。
    reload_lock: tokio::sync::Mutex<()>,
}

impl WorkBuddyAccountStore {
    pub fn new(
        client: reqwest::Client,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        provider: impl Into<String>,
        ttl_secs: u64,
    ) -> Self {
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            provider: provider.into(),
            ttl: Duration::from_secs(ttl_secs.max(1)),
            state: Mutex::new(StoreState {
                accounts: Vec::new(),
                loaded_at: None,
                cursor: 0,
            }),
            reload_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// 轮询取一个未过期账号；TTL 到期时重新调导出接口。
    /// 如果缓存里所有账号都已过期，距上次加载超过 30 秒会立刻重取一次，
    /// 以便及时拿到 agent2api 刷新后的 token。
    pub async fn next_account(
        &self,
        timeout: Duration,
    ) -> Result<Option<WorkBuddyAccount>, String> {
        let (stale, since_load) = self.freshness();
        if stale {
            self.reload_coalesced(timeout, false).await?;
        }
        let account = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            next_cached_account(&mut state)
        };
        if account.is_none()
            && !stale
            && since_load.is_some_and(|elapsed| elapsed >= MIN_EXPIRED_RELOAD_INTERVAL)
        {
            self.reload_coalesced(timeout, true).await?;
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            return Ok(next_cached_account(&mut state));
        }
        Ok(account)
    }

    fn freshness(&self) -> (bool, Option<Duration>) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (
            state
                .loaded_at
                .is_none_or(|loaded_at| loaded_at.elapsed() >= self.ttl),
            state.loaded_at.map(|loaded_at| loaded_at.elapsed()),
        )
    }

    async fn reload_coalesced(
        &self,
        timeout: Duration,
        force_if_expired_interval: bool,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let _guard = tokio::time::timeout(timeout, self.reload_lock.lock())
            .await
            .map_err(|_| "等待 WorkBuddy 账号刷新超时".to_string())?;
        // 拿到锁后二次检查：可能已有前一个并发请求完成了刷新。
        let (stale, since_load) = self.freshness();
        let should_reload = stale
            || (force_if_expired_interval
                && since_load.is_some_and(|elapsed| elapsed >= MIN_EXPIRED_RELOAD_INTERVAL));
        if !should_reload {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("WorkBuddy 账号刷新超时".to_string());
        }
        let accounts = self.load(remaining).await?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let next_cursor = if accounts.is_empty() {
            0
        } else {
            state.cursor % accounts.len()
        };
        state.accounts = accounts;
        state.loaded_at = Some(Instant::now());
        state.cursor = next_cursor;
        Ok(())
    }

    /// 让下一次取号强制重调导出接口（遇到 401 时 token 可能刚被 agent2api 刷新过）。
    pub fn invalidate(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.loaded_at = None;
    }

    async fn load(&self, timeout: Duration) -> Result<Vec<WorkBuddyAccount>, String> {
        if self.base_url.is_empty() {
            return Err("agent2api 地址为空".to_string());
        }
        let effective_timeout = timeout.min(Duration::from_secs(EXPORT_TIMEOUT_SECS));
        let response = self
            .client
            .post(format!("{}/api/accounts/export-files", self.base_url))
            .bearer_auth(&self.api_key)
            .timeout(effective_timeout)
            .json(&json!({ "provider": self.provider }))
            .send()
            .await
            .map_err(|error| format!("agent2api 导出接口请求失败: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 | 403 => "agent2api 导出接口认证失败（api_key 需要管理权限）".to_string(),
                _ => format!("agent2api 导出接口返回 HTTP {status}"),
            });
        }
        let value: Value = response
            .json()
            .await
            .map_err(|_| "agent2api 导出接口返回了非 JSON 响应".to_string())?;
        parse_export_response(&value, &self.provider)
    }
}

fn parse_export_response(value: &Value, provider: &str) -> Result<Vec<WorkBuddyAccount>, String> {
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("agent2api 导出接口返回失败".to_string());
    }
    let files = value
        .get("data")
        .and_then(|data| data.get("files"))
        .and_then(Value::as_array)
        .ok_or_else(|| "agent2api 导出接口缺少 files".to_string())?;
    let mut accounts = Vec::new();
    for file in files {
        let Some(content) = file.get("content") else {
            continue;
        };
        let secret = match content {
            Value::String(text) => match serde_json::from_str::<Value>(text) {
                Ok(value) => value,
                Err(_) => continue,
            },
            other => other.clone(),
        };
        if secret.get("enabled").is_some_and(is_disabled) {
            continue;
        }
        if secret
            .get("provider")
            .and_then(Value::as_str)
            .is_some_and(|content_provider| {
                !content_provider.is_empty() && content_provider != provider
            })
        {
            continue;
        }
        if let Some(account) = parse_account(&secret) {
            accounts.push(account);
        }
    }
    if accounts.is_empty() {
        return Err(format!(
            "agent2api 导出接口没有可用的 WorkBuddy 账号（provider={provider}）"
        ));
    }
    Ok(accounts)
}

fn is_disabled(value: &Value) -> bool {
    match value {
        Value::Bool(enabled) => !enabled,
        Value::Number(number) => number.as_i64() == Some(0),
        _ => false,
    }
}

fn parse_account(secret: &Value) -> Option<WorkBuddyAccount> {
    let access_token = text_field(secret, "accessToken");
    if access_token.is_empty() {
        return None;
    }
    let endpoint = text_field(secret, "endpoint");
    Some(WorkBuddyAccount {
        access_token,
        uid: text_field(secret, "uid"),
        enterprise_id: text_field(secret, "enterpriseId"),
        department: text_field(secret, "departmentFullName"),
        domain: text_field(secret, "domain"),
        endpoint: if endpoint.is_empty() {
            DEFAULT_ENDPOINT.to_string()
        } else {
            endpoint
        },
        expires_at_ms: secret.get("expiresAt").and_then(expires_millis),
    })
}

fn text_field(secret: &Value, key: &str) -> String {
    secret
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// `expiresAt` 兼容毫秒/秒时间戳与 RFC3339 字符串。
fn expires_millis(value: &Value) -> Option<i64> {
    let millis = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => {
            let text = text.trim();
            if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text) {
                return Some(parsed.timestamp_millis());
            }
            text.parse::<f64>().ok()?
        }
        _ => return None,
    };
    Some(if millis < 1e11 {
        (millis * 1000.0) as i64
    } else {
        millis as i64
    })
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{expires_millis, parse_account, parse_export_response};
    use serde_json::json;

    #[test]
    fn parses_workbuddy_account() {
        let secret = json!({
            "accessToken": "token-value",
            "uid": "user-1",
            "enterpriseId": "ent-1",
            "domain": "www.workbuddy.cn",
            "endpoint": "https://www.workbuddy.cn",
            "expiresAt": 1_900_000_000_000i64
        });
        let account = parse_account(&secret).unwrap();
        assert_eq!(account.access_token, "token-value");
        assert_eq!(account.uid, "user-1");
        assert_eq!(account.enterprise_id, "ent-1");
        assert_eq!(account.endpoint, "https://www.workbuddy.cn");
        assert_eq!(account.expires_at_ms, Some(1_900_000_000_000));
        assert!(parse_account(&json!({"uid": "u"})).is_none());
    }

    #[test]
    fn expiry_accepts_seconds_and_rfc3339() {
        assert_eq!(
            expires_millis(&json!(1_900_000_000)),
            Some(1_900_000_000_000)
        );
        assert_eq!(
            expires_millis(&json!(1_900_000_000_000u64)),
            Some(1_900_000_000_000)
        );
        assert_eq!(
            expires_millis(&json!("2026-11-22T12:00:00Z")),
            Some(1_795_348_800_000)
        );
        assert_eq!(expires_millis(&json!("not-a-date")), None);
    }

    #[test]
    fn parses_export_response_and_skips_disabled() {
        let response = json!({
            "success": true,
            "data": {
                "files": [
                    {"content": {
                        "accessToken": "token-a",
                        "uid": "user-1",
                        "enterpriseId": "ent-1",
                        "domain": "www.codebuddy.cn",
                        "endpoint": "https://copilot.tencent.com",
                        "expiresAt": 1_900_000_000_000i64,
                        "enabled": true
                    }},
                    {"content": "{\"accessToken\":\"token-b\",\"enabled\":1}"},
                    {"content": {"accessToken": "token-c", "enabled": false}},
                    {"content": {"uid": "no-token"}}
                ]
            }
        });
        let accounts = parse_export_response(&response, "workbuddy").unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].access_token, "token-a");
        assert_eq!(accounts[0].endpoint, "https://copilot.tencent.com");
        assert_eq!(accounts[1].access_token, "token-b");
    }

    #[test]
    fn rejects_export_response_without_accounts() {
        let empty = json!({"success": true, "data": {"files": []}});
        assert!(parse_export_response(&empty, "workbuddy").is_err());
        let failed = json!({"success": false});
        assert!(parse_export_response(&failed, "workbuddy").is_err());
    }

    #[tokio::test]
    async fn coalesces_concurrent_cold_start_reloads() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        use axum::{routing::post, Json, Router as AxumRouter};
        use futures::future::join_all;

        let export_calls = Arc::new(AtomicUsize::new(0));
        let counter = export_calls.clone();
        let app = AxumRouter::new().route(
            "/api/accounts/export-files",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    Json(json!({
                        "success": true,
                        "data": {
                            "files": [
                                {"content": {"accessToken": "token-a", "enabled": true}},
                                {"content": {"accessToken": "token-b", "enabled": true}}
                            ]
                        }
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let store = Arc::new(super::WorkBuddyAccountStore::new(
            reqwest::Client::new(),
            format!("http://{addr}"),
            "k",
            "workbuddy",
            600,
        ));
        let tasks = (0..5).map(|_| {
            let store = store.clone();
            async move { store.next_account(Duration::from_secs(5)).await.unwrap() }
        });
        let accounts = join_all(tasks).await;
        assert!(accounts.iter().all(Option::is_some));
        assert_eq!(export_calls.load(Ordering::SeqCst), 1);
    }
}
