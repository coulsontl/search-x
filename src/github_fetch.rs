//! GitHub issue / PR / release 专项抓取。
//!
//! 只匹配固定形态的 github.com URL，API 主机固定为 `api.github.com`，
//! 不接受调用方指定其它主机。token 可选：未配置时匿名访问（约 60 次/小时），
//! 配置后提高限额并支持私有仓库；token 只用于 Authorization 头，不写日志。

use std::time::Duration;

use serde_json::Value;
use url::Url;

use crate::channels::{status_error, transport_error};
use crate::model::{ChannelError, PageResult};

const GITHUB_API: &str = "https://api.github.com";
const USER_AGENT: &str = concat!("search-x/", env!("CARGO_PKG_VERSION"));
const MAX_COMMENTS: usize = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
enum GithubTarget {
    Issue {
        owner: String,
        repo: String,
        number: u64,
        is_pr: bool,
    },
    Release {
        owner: String,
        repo: String,
        tag: Option<String>,
    },
}

pub struct GithubFetcher {
    client: reqwest::Client,
    token: Option<String>,
}

impl GithubFetcher {
    pub fn new(client: reqwest::Client, token: Option<String>) -> Self {
        Self {
            client,
            token: token
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
        }
    }

    pub fn matches(&self, url: &str) -> bool {
        parse_target(url).is_some()
    }

    pub async fn fetch(&self, url: &str, timeout: Duration) -> Result<PageResult, ChannelError> {
        let target = parse_target(url).ok_or_else(|| {
            ChannelError::new("github", "unsupported", "不是 GitHub issue/PR/release 链接")
        })?;
        match target {
            GithubTarget::Issue {
                owner,
                repo,
                number,
                is_pr,
            } => {
                let issue = self
                    .fetch_issue_or_pr(&owner, &repo, number, is_pr, timeout)
                    .await?;
                let title = if issue.title.is_empty() {
                    url.to_string()
                } else {
                    issue.title.clone()
                };
                Ok(PageResult {
                    url: url.to_string(),
                    title,
                    content: render_issue(&issue),
                    channel: "github".to_string(),
                })
            }
            GithubTarget::Release { owner, repo, tag } => {
                let release = self
                    .fetch_release(&owner, &repo, tag.as_deref(), timeout)
                    .await?;
                let title = if release.name.trim().is_empty() {
                    release.tag.clone()
                } else {
                    release.name.clone()
                };
                Ok(PageResult {
                    url: url.to_string(),
                    title,
                    content: render_release(&release),
                    channel: "github".to_string(),
                })
            }
        }
    }

    async fn fetch_issue_or_pr(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        is_pr: bool,
        timeout: Duration,
    ) -> Result<GithubIssue, ChannelError> {
        let comments_url = format!(
            "{GITHUB_API}/repos/{owner}/{repo}/issues/{number}/comments?per_page={}",
            MAX_COMMENTS + 1
        );
        if is_pr {
            let main_url = format!("{GITHUB_API}/repos/{owner}/{repo}/pulls/{number}");
            let review_comments_url = format!(
                "{GITHUB_API}/repos/{owner}/{repo}/pulls/{number}/comments?per_page={}",
                MAX_COMMENTS + 1
            );
            let reviews_url = format!(
                "{GITHUB_API}/repos/{owner}/{repo}/pulls/{number}/reviews?per_page={}",
                MAX_COMMENTS + 1
            );
            let (main, conversation, review_comments, reviews) = tokio::join!(
                self.get_json(&main_url, timeout, "pull"),
                self.get_json(&comments_url, timeout, "comments"),
                self.get_json(&review_comments_url, timeout, "review comments"),
                self.get_json(&reviews_url, timeout, "reviews"),
            );
            let main = main?;
            let mut comments = match conversation {
                Ok(value) => parse_comments(&value),
                Err(error) => {
                    tracing::warn!("github PR 评论拉取失败，降级为空: {}", error.message);
                    Vec::new()
                }
            };
            // 评审相关接口是补充信息，失败时降级为空，不影响主贴内容。
            if let Ok(value) = review_comments {
                comments.extend(parse_comments(&value));
            }
            if let Ok(value) = reviews {
                comments.extend(parse_review_bodies(&value));
            }
            comments.sort_by(|left, right| left.created_at.cmp(&right.created_at));
            Ok(issue_from_json(&main, comments, true))
        } else {
            let main_url = format!("{GITHUB_API}/repos/{owner}/{repo}/issues/{number}");
            let (main, conversation) = tokio::join!(
                self.get_json(&main_url, timeout, "issue"),
                self.get_json(&comments_url, timeout, "comments"),
            );
            let main = main?;
            let comments = match conversation {
                Ok(value) => parse_comments(&value),
                Err(error) => {
                    tracing::warn!("github issue 评论拉取失败，降级为空: {}", error.message);
                    Vec::new()
                }
            };
            Ok(issue_from_json(&main, comments, false))
        }
    }

    async fn fetch_release(
        &self,
        owner: &str,
        repo: &str,
        tag: Option<&str>,
        timeout: Duration,
    ) -> Result<GithubRelease, ChannelError> {
        let url = match tag {
            // 标签里的 `/` 在 API 路径中必须保持为单个编码段。
            Some(tag) => format!(
                "{GITHUB_API}/repos/{owner}/{repo}/releases/tags/{}",
                tag.replace('/', "%2F")
            ),
            None => format!("{GITHUB_API}/repos/{owner}/{repo}/releases/latest"),
        };
        let value = self.get_json(&url, timeout, "release").await?;
        parse_release(&value).ok_or_else(|| {
            ChannelError::new(
                "github",
                "invalid_response",
                "GitHub release 响应缺少 tag_name",
            )
        })
    }

    async fn get_json(
        &self,
        url: &str,
        timeout: Duration,
        label: &str,
    ) -> Result<Value, ChannelError> {
        let mut request = self
            .client
            .get(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .timeout(timeout);
        if let Some(token) = &self.token {
            request = request.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = request
            .send()
            .await
            .map_err(|error| transport_error("github", error))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| transport_error("github", error))?;
        if !status.is_success() {
            return Err(status_error("github", status, &text));
        }
        serde_json::from_str(&text).map_err(|_| {
            ChannelError::new(
                "github",
                "invalid_response",
                format!("GitHub {label} 返回了无法解析的 JSON"),
            )
        })
    }
}

struct GithubIssue {
    title: String,
    state: String,
    merged: Option<bool>,
    author: String,
    body: String,
    labels: Vec<String>,
    comments: Vec<GithubComment>,
    total_issue_comments: Option<usize>,
    is_pr: bool,
}

struct GithubComment {
    author: String,
    body: String,
    created_at: String,
}

struct GithubRelease {
    tag: String,
    name: String,
    author: String,
    published_at: String,
    prerelease: bool,
    body: String,
}

fn parse_target(raw: &str) -> Option<GithubTarget> {
    let url = Url::parse(raw).ok()?;
    if url.scheme() != "https" || !url.host_str()?.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() < 4 {
        return None;
    }
    let (owner, repo) = (segments[0], segments[1]);
    if !valid_identifier(owner) || !valid_identifier(repo) {
        return None;
    }
    match (segments[2], segments.len()) {
        ("issues", 4) => Some(GithubTarget::Issue {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: parse_number(segments[3])?,
            is_pr: false,
        }),
        ("pull", 4) => Some(GithubTarget::Issue {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: parse_number(segments[3])?,
            is_pr: true,
        }),
        ("releases", 4) if segments[3] == "latest" => Some(GithubTarget::Release {
            owner: owner.to_string(),
            repo: repo.to_string(),
            tag: None,
        }),
        ("releases", len)
            if len >= 5
                && segments[3] == "tag"
                && segments[4..].iter().all(|segment| valid_tag(segment)) =>
        {
            let tag = segments[4..].join("/");
            if tag.len() <= 200 {
                Some(GithubTarget::Release {
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                    tag: Some(tag),
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

fn parse_number(value: &str) -> Option<u64> {
    value.parse::<u64>().ok().filter(|number| *number > 0)
}

/// GitHub 用户名/仓库名的保守字符集；同时拒绝 `.` / `..`，避免路径穿越。
fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn valid_tag(value: &str) -> bool {
    let lowered = value.to_ascii_lowercase();
    !value.is_empty()
        && value.len() <= 200
        && !value.contains("..")
        && !lowered.contains("%2e")
        && !lowered.contains("%0a")
        && !lowered.contains("%0d")
        && !lowered.contains("%5c")
        && !value
            .chars()
            .any(|character| character.is_control() || matches!(character, '?' | '#'))
}

fn issue_from_json(value: &Value, comments: Vec<GithubComment>, is_pr: bool) -> GithubIssue {
    GithubIssue {
        title: str_field(value, "title"),
        state: str_field(value, "state"),
        merged: value.get("merged").and_then(Value::as_bool),
        author: nested_login(value, "user"),
        body: str_field(value, "body"),
        labels: value
            .get("labels")
            .and_then(Value::as_array)
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|label| label.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        comments,
        total_issue_comments: value
            .get("comments")
            .and_then(Value::as_u64)
            .map(|count| count as usize),
        is_pr,
    }
}

fn parse_comments(value: &Value) -> Vec<GithubComment> {
    value
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| GithubComment {
                    author: nested_login(row, "user"),
                    body: str_field(row, "body"),
                    created_at: str_field(row, "created_at"),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// PR review 摘要用 `submitted_at`，且空 body 的 APPROVE 没有信息量，直接丢弃。
fn parse_review_bodies(value: &Value) -> Vec<GithubComment> {
    value
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| GithubComment {
                    author: nested_login(row, "user"),
                    body: str_field(row, "body"),
                    created_at: str_field(row, "submitted_at"),
                })
                .filter(|comment| !comment.body.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn parse_release(value: &Value) -> Option<GithubRelease> {
    let tag = str_field(value, "tag_name");
    if tag.is_empty() {
        return None;
    }
    Some(GithubRelease {
        tag,
        name: str_field(value, "name"),
        author: nested_login(value, "author"),
        published_at: str_field(value, "published_at"),
        prerelease: value
            .get("prerelease")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        body: str_field(value, "body"),
    })
}

fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn nested_login(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(|user| user.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn render_issue(raw: &GithubIssue) -> String {
    let mut out = format!("# {}\n\n", raw.title);
    let merged_suffix = if raw.is_pr {
        match raw.merged {
            Some(true) => " (merged)",
            _ if raw.state == "closed" => " (closed, not merged)",
            _ => "",
        }
    } else {
        ""
    };
    out.push_str(&format!("**State:** {}{}\n", raw.state, merged_suffix));
    out.push_str(&format!("**Author:** {}\n", raw.author));
    if !raw.labels.is_empty() {
        out.push_str(&format!("**Labels:** {}\n", raw.labels.join(", ")));
    }
    out.push_str(&format!("\n{}\n\n## Comments\n\n", raw.body));
    for comment in raw.comments.iter().take(MAX_COMMENTS) {
        out.push_str(&format!(
            "### Comment by {} ({})\n\n{}\n\n",
            comment.author, comment.created_at, comment.body
        ));
    }
    if raw.comments.len() > MAX_COMMENTS {
        let extra = raw
            .total_issue_comments
            .filter(|total| *total > MAX_COMMENTS)
            .map(|total| total - MAX_COMMENTS);
        match extra {
            Some(remaining) if !raw.is_pr => {
                out.push_str(&format!("_还有 {remaining} 条评论_\n"));
            }
            _ => {
                out.push_str(&format!("_仅展示前 {MAX_COMMENTS} 条评论，其余已折叠_\n"));
            }
        }
    }
    out
}

fn render_release(raw: &GithubRelease) -> String {
    let title = if raw.name.trim().is_empty() {
        &raw.tag
    } else {
        &raw.name
    };
    let mut out = format!("# {title}\n\n");
    let prerelease = if raw.prerelease { " (prerelease)" } else { "" };
    out.push_str(&format!("**Tag:** {}{}\n", raw.tag, prerelease));
    if !raw.published_at.is_empty() {
        out.push_str(&format!("**Published:** {}\n", raw.published_at));
    }
    out.push_str(&format!("**Author:** {}\n", raw.author));
    out.push_str(&format!("\n{}\n", raw.body));
    out
}

#[cfg(test)]
mod tests {
    use super::{
        parse_comments, parse_release, parse_review_bodies, parse_target, render_issue,
        GithubComment, GithubIssue, GithubTarget,
    };
    use serde_json::json;

    #[test]
    fn matches_issue_pr_and_release_urls() {
        assert_eq!(
            parse_target("https://github.com/rust-lang/rust/issues/123"),
            Some(GithubTarget::Issue {
                owner: "rust-lang".to_string(),
                repo: "rust".to_string(),
                number: 123,
                is_pr: false,
            })
        );
        assert_eq!(
            parse_target("https://github.com/rust-lang/rust/pull/456/"),
            Some(GithubTarget::Issue {
                owner: "rust-lang".to_string(),
                repo: "rust".to_string(),
                number: 456,
                is_pr: true,
            })
        );
        assert_eq!(
            parse_target("https://github.com/rust-lang/rust/releases/tag/v1.80.0"),
            Some(GithubTarget::Release {
                owner: "rust-lang".to_string(),
                repo: "rust".to_string(),
                tag: Some("v1.80.0".to_string()),
            })
        );
        assert_eq!(
            parse_target("https://github.com/acme/mono/releases/tag/packages/core/v1.2.3"),
            Some(GithubTarget::Release {
                owner: "acme".to_string(),
                repo: "mono".to_string(),
                tag: Some("packages/core/v1.2.3".to_string()),
            })
        );
        assert_eq!(
            parse_target("https://github.com/rust-lang/rust/releases/latest"),
            Some(GithubTarget::Release {
                owner: "rust-lang".to_string(),
                repo: "rust".to_string(),
                tag: None,
            })
        );
    }

    #[test]
    fn rejects_other_github_urls() {
        for url in [
            "https://github.com/rust-lang/rust",
            "https://github.com/rust-lang/rust/issues",
            "https://github.com/rust-lang/rust/issues/abc",
            "https://github.com/rust-lang/rust/issues/0",
            "https://github.com/rust-lang/rust/pull/456/files",
            "https://github.com/rust-lang/rust/releases",
            "https://github.com/rust-lang/rust/releases/download/v1.0.0/x.tar.gz",
            "https://github.com/rust-lang/rust/releases/tag/%2e%2e%2fmain",
            "https://gitlab.com/rust-lang/rust/issues/123",
            "http://github.com/rust-lang/rust/issues/123",
            "https://github.com/../rust/issues/123",
        ] {
            assert!(parse_target(url).is_none(), "should reject {url}");
        }
    }

    #[test]
    fn parses_comments_and_pr_reviews() {
        let comments = parse_comments(&json!([
            {
                "user": { "login": "alice" },
                "body": "looks good",
                "created_at": "2024-01-01T00:00:00Z"
            }
        ]));
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "alice");
        assert_eq!(comments[0].body, "looks good");

        let reviews = parse_review_bodies(&json!([
            {
                "user": { "login": "bob" },
                "body": "needs changes",
                "submitted_at": "2024-01-02T00:00:00Z"
            },
            {
                "user": { "login": "carol" },
                "body": "   ",
                "submitted_at": "2024-01-03T00:00:00Z"
            }
        ]));
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].author, "bob");
        assert_eq!(reviews[0].created_at, "2024-01-02T00:00:00Z");
    }

    #[test]
    fn parses_release_and_requires_tag() {
        let release = parse_release(&json!({
            "tag_name": "v1.2.3",
            "name": "Release 1.2.3",
            "author": { "login": "alice" },
            "published_at": "2024-01-01T00:00:00Z",
            "prerelease": false,
            "body": "notes"
        }))
        .unwrap();
        assert_eq!(release.tag, "v1.2.3");
        assert_eq!(release.name, "Release 1.2.3");
        assert_eq!(release.author, "alice");
        assert!(!release.prerelease);

        assert!(parse_release(&json!({"name": "no tag"})).is_none());
    }

    #[test]
    fn renders_issue_and_folds_long_comment_lists() {
        let comments = (0..31)
            .map(|index| GithubComment {
                author: format!("user-{index}"),
                body: format!("comment-{index}"),
                created_at: "2024-01-01T00:00:00Z".to_string(),
            })
            .collect();
        let issue = GithubIssue {
            title: "Bug".to_string(),
            state: "closed".to_string(),
            merged: Some(true),
            author: "alice".to_string(),
            body: "body".to_string(),
            labels: vec!["bug".to_string()],
            comments,
            total_issue_comments: Some(120),
            is_pr: true,
        };
        let rendered = render_issue(&issue);
        assert!(rendered.contains("**State:** closed (merged)"));
        assert!(rendered.contains("comment-29"));
        assert!(!rendered.contains("comment-30"));
        assert!(rendered.contains("仅展示前 30 条评论"));

        let non_pr = GithubIssue {
            is_pr: false,
            ..issue
        };
        assert!(render_issue(&non_pr).contains("还有 90 条评论"));
    }
}
