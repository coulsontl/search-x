use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::KeyStrategy;

struct KeyState {
    key: String,
    cooldown_until: Option<Instant>,
    disabled: bool,
    failures: u32,
}

pub struct KeyPool {
    keys: Mutex<Vec<KeyState>>,
    strategy: KeyStrategy,
    cursor: AtomicUsize,
}

impl KeyPool {
    pub fn new(keys: Vec<String>, strategy: KeyStrategy) -> Self {
        let keys = keys
            .into_iter()
            .map(|key| KeyState {
                key,
                cooldown_until: None,
                disabled: false,
                failures: 0,
            })
            .collect();
        Self {
            keys: Mutex::new(keys),
            strategy,
            cursor: AtomicUsize::new(0),
        }
    }

    /// 取一个当前可用的 key；全部冷却/禁用时返回 None。
    pub fn next(&self) -> Option<String> {
        let keys = self.keys.lock().ok()?;
        if keys.is_empty() {
            return None;
        }
        let now = Instant::now();
        let available: Vec<usize> = keys
            .iter()
            .enumerate()
            .filter(|(_, state)| {
                !state.disabled && state.cooldown_until.is_none_or(|until| until <= now)
            })
            .map(|(index, _)| index)
            .collect();
        if available.is_empty() {
            return None;
        }
        let index = match self.strategy {
            KeyStrategy::Failover => available[0],
            KeyStrategy::Random => {
                let seed = now.elapsed().subsec_nanos() as usize;
                available[seed % available.len()]
            }
            KeyStrategy::RoundRobin => {
                let cursor = self.cursor.fetch_add(1, Ordering::Relaxed);
                available[cursor % available.len()]
            }
        };
        Some(keys[index].key.clone())
    }

    pub fn report_success(&self, key: &str) {
        if let Ok(mut keys) = self.keys.lock() {
            if let Some(state) = keys.iter_mut().find(|state| state.key == key) {
                state.failures = 0;
                state.cooldown_until = None;
            }
        }
    }

    /// 401/403 禁用；429 冷却 60s；5xx/超时累计失败，5 次后冷却 5 分钟。
    pub fn report_failure(&self, key: &str, status: Option<u16>) {
        if let Ok(mut keys) = self.keys.lock() {
            if let Some(state) = keys.iter_mut().find(|state| state.key == key) {
                match status {
                    Some(401) | Some(403) => state.disabled = true,
                    Some(429) => {
                        state.cooldown_until = Some(Instant::now() + Duration::from_secs(60))
                    }
                    _ => {
                        state.failures += 1;
                        if state.failures >= 5 {
                            state.cooldown_until = Some(Instant::now() + Duration::from_secs(300));
                            state.failures = 0;
                        }
                    }
                }
            }
        }
    }
}
