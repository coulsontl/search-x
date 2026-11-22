use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::model::SearchResponse;

struct CacheEntry {
    response: SearchResponse,
    expires_at: Instant,
}

/// 进程内查询缓存：TTL + 条目上限，过期或超额时淘汰最旧条目。
pub struct QueryCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
    ttl: Duration,
    max_entries: usize,
}

impl QueryCache {
    pub fn new(ttl_secs: u64, max_entries: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_secs),
            max_entries: max_entries.max(1),
        }
    }

    pub fn get(&self, key: &str) -> Option<SearchResponse> {
        let mut entries = self.entries.lock().expect("cache lock poisoned");
        match entries.get(key) {
            Some(entry) if entry.expires_at > Instant::now() => Some(entry.response.clone()),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    pub fn put(&self, key: String, response: SearchResponse) {
        let mut entries = self.entries.lock().expect("cache lock poisoned");
        let now = Instant::now();
        entries.retain(|_, entry| entry.expires_at > now);
        if entries.len() >= self.max_entries {
            if let Some(oldest_key) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone())
            {
                entries.remove(&oldest_key);
            }
        }
        entries.insert(
            key,
            CacheEntry {
                response,
                expires_at: now + self.ttl,
            },
        );
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.lock().expect("cache lock poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::QueryCache;
    use crate::model::SearchResponse;

    fn response(query: &str) -> SearchResponse {
        SearchResponse {
            query: query.to_string(),
            results: Vec::new(),
            channels: Vec::new(),
            errors: Vec::new(),
            elapsed_ms: 1,
            cached: false,
        }
    }

    #[test]
    fn stores_and_expires_entries() {
        let expired = QueryCache::new(0, 10);
        expired.put("k".to_string(), response("q"));
        assert!(expired.get("k").is_none());

        let cache = QueryCache::new(60, 10);
        cache.put("k".to_string(), response("q"));
        assert!(cache.get("k").is_some());
    }

    #[test]
    fn evicts_when_full() {
        let cache = QueryCache::new(60, 2);
        cache.put("a".to_string(), response("a"));
        cache.put("b".to_string(), response("b"));
        cache.put("c".to_string(), response("c"));
        assert_eq!(cache.len(), 2);
    }
}
