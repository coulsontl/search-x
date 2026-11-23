use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    rng_state: AtomicU64,
}

impl KeyPool {
    pub fn new(keys: Vec<String>, strategy: KeyStrategy) -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15)
            ^ 0x517c_c1b7_2722_0a95;
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
            rng_state: AtomicU64::new(seed.max(1)),
        }
    }

    pub fn has_multiple(&self) -> bool {
        self.keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
            > 1
    }

    /// 取一个当前可用的 key；全部冷却/禁用时返回 None。
    pub fn next(&self) -> Option<String> {
        let keys = self
            .keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
                let sample = self.next_random() as usize;
                available[sample % available.len()]
            }
            KeyStrategy::RoundRobin => {
                let cursor = self.cursor.fetch_add(1, Ordering::Relaxed);
                available[cursor % available.len()]
            }
        };
        Some(keys[index].key.clone())
    }

    pub fn report_success(&self, key: &str) {
        let mut keys = self
            .keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(state) = keys.iter_mut().find(|state| state.key == key) {
            state.failures = 0;
            state.cooldown_until = None;
        }
    }

    /// 401/403 禁用；429 冷却 60s；432（如 Tavily 月额度耗尽）冷却 1 小时；5xx/超时累计 5 次后冷却 5 分钟。
    pub fn report_failure(&self, key: &str, status: Option<u16>) {
        let mut keys = self
            .keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(state) = keys.iter_mut().find(|state| state.key == key) {
            match status {
                Some(401) | Some(403) => state.disabled = true,
                Some(429) => {
                    state.cooldown_until = Some(Instant::now() + Duration::from_secs(60));
                }
                Some(432) => {
                    tracing::warn!("key 触发 HTTP 432（额度耗尽），冷却 3600 秒");
                    state.cooldown_until = Some(Instant::now() + Duration::from_secs(3600));
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

    /// 轻量无依赖伪随机（SplitMix64 步进），避免 Windows/高频调用下 Instant 精度不足导致偏斜。
    fn next_random(&self) -> u64 {
        let mut x = self
            .rng_state
            .fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed)
            .wrapping_add(0x9e37_79b9_7f4a_7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::KeyPool;
    use crate::config::KeyStrategy;

    #[test]
    fn round_robin_and_failover_work() {
        let rr = KeyPool::new(vec!["k1".into(), "k2".into()], KeyStrategy::RoundRobin);
        assert!(rr.has_multiple());
        assert_eq!(rr.next().as_deref(), Some("k1"));
        assert_eq!(rr.next().as_deref(), Some("k2"));
        assert_eq!(rr.next().as_deref(), Some("k1"));

        let fo = KeyPool::new(vec!["k1".into(), "k2".into()], KeyStrategy::Failover);
        assert_eq!(fo.next().as_deref(), Some("k1"));
        fo.report_failure("k1", Some(401));
        assert_eq!(fo.next().as_deref(), Some("k2"));
    }

    #[test]
    fn random_strategy_spreads_across_keys() {
        let pool = KeyPool::new(
            vec!["k1".into(), "k2".into(), "k3".into()],
            KeyStrategy::Random,
        );
        let seen: HashSet<String> = (0..60).filter_map(|_| pool.next()).collect();
        assert_eq!(seen.len(), 3, "random 策略应均匀命中全部可用 key");
    }

    #[test]
    fn quota_432_cools_down_key_immediately() {
        let pool = KeyPool::new(vec!["k1".into(), "k2".into()], KeyStrategy::Failover);
        pool.report_failure("k1", Some(432));
        assert_eq!(pool.next().as_deref(), Some("k2"));
        pool.report_failure("k2", Some(429));
        assert!(pool.next().is_none());
    }
}
