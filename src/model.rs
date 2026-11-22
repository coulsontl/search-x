use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_date: Option<String>,
    pub channel: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageResult {
    pub url: String,
    pub title: String,
    pub content: String,
    pub channel: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelError {
    pub channel: String,
    pub code: String,
    pub message: String,
}

impl ChannelError {
    pub fn new(
        channel: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            channel: channel.into(),
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub results: Vec<SearchResult>,
    pub channels: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ChannelError>,
    pub elapsed_ms: u64,
    /// true = 本次响应来自查询缓存
    #[serde(default, skip_serializing_if = "is_false")]
    pub cached: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize)]
pub struct FetchResponse {
    pub pages: Vec<PageResult>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ChannelError>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub queries: Vec<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub channels: Option<Vec<String>>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// true = 跳过缓存读取强制重新搜索（结果仍写入缓存）
    #[serde(default)]
    pub no_cache: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct FetchRequest {
    pub urls: Vec<String>,
    #[serde(default)]
    pub channels: Option<Vec<String>>,
}

/// 明确属于跟踪/分享来源的参数；`from`、`source` 这类可能承载业务语义的参数不排除。
const TRACKING_PARAMS: &[&str] = &[
    "spm",
    "scm",
    "spm_id_from",
    "fbclid",
    "gclid",
    "msclkid",
    "yclid",
    "igshid",
    "mc_cid",
    "mc_eid",
    "_hsenc",
    "_hsmi",
    "ref_src",
    "ref_url",
    "share_token",
    "share_source",
    "vd_source",
    "from_source",
    "isappinstalled",
    "mpshare",
    "chksm",
];

/// 去掉跟踪参数与 fragment，保留原始路径大小写。
pub fn clean_url(raw: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(raw) else {
        return raw.to_string();
    };
    let filtered: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(key, _)| {
            let lowered = key.to_ascii_lowercase();
            !lowered.starts_with("utm_") && !TRACKING_PARAMS.contains(&lowered.as_str())
        })
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    if filtered.is_empty() {
        parsed.set_query(None);
    } else {
        parsed.query_pairs_mut().clear().extend_pairs(filtered);
    }
    parsed.set_fragment(None);
    parsed.to_string()
}

/// Reciprocal Rank Fusion 常数（业界默认 60）。
const RRF_K: f64 = 60.0;

/// 按渠道分组做 RRF 融合：每个渠道内部的顺序即排名，
/// 同一 URL 的分数跨渠道累加并合并最长正文/标题、首个发布日期。
pub fn rrf_merge(per_channel: Vec<Vec<SearchResult>>) -> Vec<SearchResult> {
    let mut first_seen_order: Vec<String> = Vec::new();
    let mut scores: HashMap<String, f64> = HashMap::new();
    let mut merged: HashMap<String, SearchResult> = HashMap::new();

    for items in per_channel {
        let mut seen_in_channel = std::collections::HashSet::new();
        for (index, mut item) in items.into_iter().enumerate() {
            item.url = clean_url(&item.url);
            let key = normalize_url(&item.url);
            let rank = index + 1;
            if seen_in_channel.insert(key.clone()) {
                *scores.entry(key.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank as f64);
            }
            match merged.get_mut(&key) {
                Some(existing) => merge_item(existing, item),
                None => {
                    first_seen_order.push(key.clone());
                    merged.insert(key, item);
                }
            }
        }
    }

    let mut results: Vec<SearchResult> = first_seen_order
        .into_iter()
        .filter_map(|key| merged.remove(&key))
        .collect();
    // 稳定排序：同分时保持首次出现顺序
    results.sort_by(|left, right| {
        let left_score = scores
            .get(&normalize_url(&left.url))
            .copied()
            .unwrap_or(0.0);
        let right_score = scores
            .get(&normalize_url(&right.url))
            .copied()
            .unwrap_or(0.0);
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

/// 合并同一 URL 的多来源结果：正文/标题取更长，发布日期取首个非空。
fn merge_item(existing: &mut SearchResult, item: SearchResult) {
    if item.content.chars().count() > existing.content.chars().count() {
        existing.content = item.content;
    }
    if item.title.chars().count() > existing.title.chars().count() {
        existing.title = item.title;
    }
    if existing.published_date.is_none() {
        existing.published_date = item.published_date;
    }
}

/// SearXNG 只返回短摘要，去重合并后统一排到最后；其余渠道保持首次出现顺序。
pub fn move_searxng_last(results: Vec<SearchResult>) -> Vec<SearchResult> {
    let (mut primary, searxng): (Vec<SearchResult>, Vec<SearchResult>) = results
        .into_iter()
        .partition(|item| item.channel != "searxng");
    primary.extend(searxng);
    primary
}

fn normalize_url(raw: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(raw) else {
        return raw.trim_end_matches('/').to_ascii_lowercase();
    };
    parsed.set_fragment(None);
    let host = parsed.host_str().unwrap_or_default();
    let host = host.strip_prefix("www.").unwrap_or(host);
    let port = parsed
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    let path = parsed.path().trim_end_matches('/');
    match parsed.query() {
        Some(query) if !query.is_empty() => format!("{host}{port}{path}?{query}"),
        _ => format!("{host}{port}{path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{clean_url, move_searxng_last, rrf_merge, SearchResult};

    fn result(title: &str, url: &str, content: &str, channel: &str) -> SearchResult {
        SearchResult {
            title: title.to_string(),
            url: url.to_string(),
            content: content.to_string(),
            published_date: None,
            channel: channel.to_string(),
        }
    }

    #[test]
    fn strips_tracking_params_and_fragment() {
        assert_eq!(
            clean_url("https://example.com/a?utm_source=x&fbclid=1&id=7#part"),
            "https://example.com/a?id=7"
        );
        assert_eq!(
            clean_url("https://example.com/a?spm=abc"),
            "https://example.com/a"
        );
    }

    #[test]
    fn merges_same_page_across_url_variants() {
        let results = vec![
            result(
                "Short title",
                "https://www.example.com/post/?utm_source=a",
                "短摘要",
                "searxng",
            ),
            result(
                "Longer and better title",
                "http://example.com/post#section",
                "这是一段更长的正文内容，应该被保留下来。",
                "exa",
            ),
        ];
        let merged = rrf_merge(vec![results]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].channel, "searxng");
        assert_eq!(merged[0].title, "Longer and better title");
        assert_eq!(
            merged[0].content,
            "这是一段更长的正文内容，应该被保留下来。"
        );
    }

    #[test]
    fn rrf_fusion_ranks_across_channels() {
        let channel_a = vec![
            result("X", "https://x.example.com/", "x content", "exa"),
            result("Y", "https://y.example.com/", "y content", "exa"),
            result("Z", "https://z.example.com/", "z", "exa"),
        ];
        let channel_b = vec![
            result(
                "Z",
                "https://z.example.com/",
                "z content much longer",
                "tavily",
            ),
            result("X", "https://x.example.com/", "x", "tavily"),
            result("W", "https://w.example.com/", "w", "tavily"),
        ];
        let fused = rrf_merge(vec![channel_a, channel_b]);
        let urls: Vec<&str> = fused.iter().map(|item| item.url.as_str()).collect();
        // 分数：x=1/61+1/62 > z=1/63+1/61 > y=1/62 == w=1/62
        assert_eq!(
            urls,
            [
                "https://x.example.com/",
                "https://z.example.com/",
                "https://y.example.com/",
                "https://w.example.com/",
            ]
        );
        let z = fused
            .iter()
            .find(|item| item.url == "https://z.example.com/")
            .unwrap();
        assert_eq!(z.content, "z content much longer");
        assert_eq!(z.channel, "exa");
    }

    #[test]
    fn moves_searxng_results_to_the_end() {
        let results = vec![
            result("S1", "https://s1.example.com/", "", "searxng"),
            result("E", "https://e.example.com/", "", "exa"),
            result("S2", "https://s2.example.com/", "", "searxng"),
            result("T", "https://t.example.com/", "", "tavily"),
        ];
        let ordered = move_searxng_last(rrf_merge(vec![results]));
        let channels: Vec<&str> = ordered.iter().map(|item| item.channel.as_str()).collect();
        assert_eq!(channels, ["exa", "tavily", "searxng", "searxng"]);
    }

    #[test]
    fn keeps_port_distinct() {
        let results = vec![
            result("A", "https://example.com:8080/foo", "", "exa"),
            result("B", "https://example.com/foo", "", "tavily"),
        ];
        assert_eq!(rrf_merge(vec![results]).len(), 2);
    }

    #[test]
    fn keeps_path_case_sensitive_but_ignores_scheme() {
        let results = vec![
            result("A", "https://example.com/Wiki/Foo", "", "exa"),
            result("B", "http://example.com/wiki/foo", "", "tavily"),
            result("C", "https://example.com/Wiki/Foo/", "", "tavily"),
        ];
        let deduped = rrf_merge(vec![results]);
        assert_eq!(deduped.len(), 2);
    }
}
