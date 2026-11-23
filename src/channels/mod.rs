pub mod exa;
pub mod firecrawl;
pub mod grok;
pub mod searxng;
pub mod tavily;
pub mod tinyfish;
pub mod workbuddy;

use async_trait::async_trait;
use std::time::Duration;

use crate::model::{ChannelError, PageResult, SearchResult};

#[async_trait]
pub trait Channel: Send + Sync {
    fn name(&self) -> &'static str;
    fn supports_fetch(&self) -> bool {
        false
    }
    async fn search(
        &self,
        query: &str,
        limit: usize,
        timeout: Duration,
    ) -> Result<Vec<SearchResult>, ChannelError>;
    async fn fetch(
        &self,
        _urls: &[String],
        _timeout: Duration,
    ) -> Result<Vec<PageResult>, ChannelError> {
        Err(ChannelError::new(
            self.name(),
            "unsupported",
            "该渠道不支持抓取",
        ))
    }
}

/// 把 reqwest 错误压成分类型错误，避免把 URL/凭据带进返回。
pub(crate) fn transport_error(channel: &str, error: reqwest::Error) -> ChannelError {
    let code = if error.is_timeout() {
        "timed_out"
    } else {
        "transport"
    };
    ChannelError::new(channel, code, "渠道请求失败")
}

pub(crate) fn status_error(channel: &str, status: reqwest::StatusCode, body: &str) -> ChannelError {
    let code = match status.as_u16() {
        401 | 403 => "unauthorized",
        429 | 432 => "rate_limited",
        _ => "upstream",
    };
    let snippet: String = body.chars().take(200).collect();
    ChannelError::new(channel, code, format!("upstream {status}: {snippet}"))
}

/// 遇到凭据/限流/额度类状态码时，多 key 池可在单次请求内换下一把 key 重试一次。
pub(crate) fn is_retryable_key_status(status: u16) -> bool {
    matches!(status, 401 | 403 | 429 | 432)
}
