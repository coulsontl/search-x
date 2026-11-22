use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::json;

use crate::model::SearchResponse;

pub struct SearchLogger {
    path: PathBuf,
    max_bytes: u64,
    content_chars: usize,
    lock: Mutex<()>,
}

impl SearchLogger {
    pub fn new(path: PathBuf, max_size_mb: u64, content_chars: usize) -> Self {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        Self {
            path,
            max_bytes: max_size_mb.saturating_mul(1024 * 1024),
            content_chars,
            lock: Mutex::new(()),
        }
    }

    /// 追加一条 JSONL 记录；超过上限时先把当前文件轮转为 `.1`。
    pub fn log(&self, mode: &str, response: &SearchResponse) {
        let Ok(_guard) = self.lock.lock() else { return };
        self.rotate_if_needed();
        let record = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "query": response.query,
            "mode": mode,
            "elapsed_ms": response.elapsed_ms,
            "channels": response.channels,
            "result_count": response.results.len(),
            "results": response.results.iter().map(|item| json!({
                "title": item.title,
                "url": item.url,
                "channel": item.channel,
                "published_date": item.published_date,
                "content": truncate(&item.content, self.content_chars),
            })).collect::<Vec<_>>(),
            "errors": response.errors,
        });
        let Ok(line) = serde_json::to_string(&record) else { return };
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(file, "{line}");
        }
    }

    fn rotate_if_needed(&self) {
        let Ok(meta) = fs::metadata(&self.path) else { return };
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
