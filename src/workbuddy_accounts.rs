//! 旁路读取 agent2api 的账号库，解密出 WorkBuddy 账号凭据。
//!
//! agent2api 把账号存成 `enc1:` 信封（AES-256-GCM，密钥为同目录的 `account.key`）。
//! 这里只读打开数据库、按 TTL 缓存账号；token 刷新完全交给 agent2api，
//! 本模块绝不写库、也绝不调用 refresh 接口。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

const ENVELOPE_PREFIX: &str = "enc1:";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const KEY_LEN: usize = 32;
const DEFAULT_ENDPOINT: &str = "https://www.workbuddy.cn";

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

pub struct WorkBuddyAccountStore {
    db_path: PathBuf,
    key_path: PathBuf,
    provider: String,
    ttl: Duration,
    state: Mutex<StoreState>,
}

impl WorkBuddyAccountStore {
    pub fn new(
        db_path: impl Into<PathBuf>,
        key_path: impl Into<PathBuf>,
        provider: impl Into<String>,
        ttl_secs: u64,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            key_path: key_path.into(),
            provider: provider.into(),
            ttl: Duration::from_secs(ttl_secs.max(1)),
            state: Mutex::new(StoreState {
                accounts: Vec::new(),
                loaded_at: None,
                cursor: 0,
            }),
        }
    }

    /// 轮询取一个未过期账号；缓存过期或 `force_reload` 时重新读库。
    pub fn next_account(&self, force_reload: bool) -> Result<Option<WorkBuddyAccount>, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let stale = state
            .loaded_at
            .is_none_or(|loaded_at| loaded_at.elapsed() >= self.ttl);
        if force_reload || stale {
            state.accounts = self.load()?;
            state.loaded_at = Some(Instant::now());
            state.cursor = 0;
        }
        let now_ms = now_millis();
        let count = state.accounts.len();
        for _ in 0..count {
            let index = state.cursor % count;
            state.cursor = (state.cursor + 1) % count;
            let account = &state.accounts[index];
            if !account.is_expired(now_ms) {
                return Ok(Some(account.clone()));
            }
        }
        Ok(None)
    }

    /// 让下一次取号强制重读（遇到 401 时 token 可能刚被 agent2api 刷新过）。
    pub fn invalidate(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.loaded_at = None;
    }

    fn load(&self) -> Result<Vec<WorkBuddyAccount>, String> {
        let key = std::fs::read(&self.key_path)
            .map_err(|error| format!("无法读取账号密钥 {}: {error}", self.key_path.display()))?;
        if key.len() != KEY_LEN {
            return Err(format!("账号密钥长度异常: {} 字节", key.len()));
        }
        let cipher =
            Aes256Gcm::new_from_slice(&key).map_err(|_| "账号密钥不适用于 AES-256".to_string())?;
        let connection =
            Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(
                |error| {
                    format!(
                        "无法打开 agent2api 数据库 {}: {error}",
                        self.db_path.display()
                    )
                },
            )?;
        let _ = connection.busy_timeout(Duration::from_secs(2));
        let mut statement = connection
            .prepare(
                "select data from accounts where provider = ?1 and enabled = 1 \
                 order by priority, added_at",
            )
            .map_err(|error| format!("账号查询失败: {error}"))?;
        let rows = statement
            .query_map([&self.provider], |row| row.get::<_, String>(0))
            .map_err(|error| format!("账号查询失败: {error}"))?;
        let mut accounts = Vec::new();
        for row in rows {
            let envelope = row.map_err(|error| format!("账号读取失败: {error}"))?;
            let secret = decrypt_envelope(&envelope, &cipher)?;
            if let Some(account) = parse_account(&secret) {
                accounts.push(account);
            }
        }
        if accounts.is_empty() {
            return Err(format!(
                "没有找到可用的 WorkBuddy 账号（provider={}）",
                self.provider
            ));
        }
        Ok(accounts)
    }
}

fn decrypt_envelope(envelope: &str, cipher: &Aes256Gcm) -> Result<Value, String> {
    let encoded = envelope
        .trim()
        .strip_prefix(ENVELOPE_PREFIX)
        .ok_or_else(|| "账号数据不是 enc1 信封".to_string())?;
    let blob = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "账号数据不是合法 base64".to_string())?;
    if blob.len() <= NONCE_LEN + TAG_LEN {
        return Err("账号密文长度异常".to_string());
    }
    let nonce = Nonce::from_slice(&blob[..NONCE_LEN]);
    let plaintext = cipher
        .decrypt(nonce, &blob[NONCE_LEN..])
        .map_err(|_| "账号数据解密失败（account.key 是否变更？）".to_string())?;
    serde_json::from_slice(&plaintext).map_err(|_| "账号数据不是合法 JSON".to_string())
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
    use super::{decrypt_envelope, expires_millis, parse_account};
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use base64::Engine;
    use serde_json::json;

    const KEY: [u8; 32] = [9u8; 32];

    fn envelope(secret: &serde_json::Value) -> String {
        let cipher = Aes256Gcm::new_from_slice(&KEY).unwrap();
        let nonce = [3u8; 12];
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                serde_json::to_vec(secret).unwrap().as_slice(),
            )
            .unwrap();
        let mut blob = nonce.to_vec();
        blob.extend(ciphertext);
        format!(
            "enc1:{}",
            base64::engine::general_purpose::STANDARD.encode(blob)
        )
    }

    #[test]
    fn decrypts_and_parses_workbuddy_account() {
        let cipher = Aes256Gcm::new_from_slice(&KEY).unwrap();
        let secret = json!({
            "accessToken": "token-value",
            "uid": "user-1",
            "enterpriseId": "ent-1",
            "domain": "www.workbuddy.cn",
            "endpoint": "https://www.workbuddy.cn",
            "expiresAt": 1_900_000_000_000i64
        });
        let value = decrypt_envelope(&envelope(&secret), &cipher).unwrap();
        let account = parse_account(&value).unwrap();
        assert_eq!(account.access_token, "token-value");
        assert_eq!(account.uid, "user-1");
        assert_eq!(account.enterprise_id, "ent-1");
        assert_eq!(account.endpoint, "https://www.workbuddy.cn");
        assert_eq!(account.expires_at_ms, Some(1_900_000_000_000));
    }

    #[test]
    fn rejects_envelopes_without_token() {
        let cipher = Aes256Gcm::new_from_slice(&KEY).unwrap();
        let value = decrypt_envelope(&envelope(&json!({"uid": "u"})), &cipher).unwrap();
        assert!(parse_account(&value).is_none());
    }

    #[test]
    fn rejects_plain_json() {
        let cipher = Aes256Gcm::new_from_slice(&KEY).unwrap();
        assert!(decrypt_envelope("{\"accessToken\":\"x\"}", &cipher).is_err());
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
}
