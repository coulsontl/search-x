use std::sync::{Arc, RwLock};

use axum::{
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router as AxumRouter,
};
use serde_json::json;

use crate::config::{AuthConfig, AuthMode};
use crate::mcp;
use crate::model::{FetchRequest, SearchRequest};
use crate::router::Router;
use crate::runtime::{self, Runtime};

struct Inner {
    router: Arc<Router>,
    auth: AuthConfig,
}

pub struct AppState {
    inner: RwLock<Inner>,
    config_path: String,
}

impl AppState {
    pub fn new(runtime: Runtime, config_path: String) -> Self {
        Self {
            inner: RwLock::new(Inner {
                router: runtime.router,
                auth: runtime.auth,
            }),
            config_path,
        }
    }

    fn snapshot(&self) -> (Arc<Router>, AuthConfig) {
        let inner = self.inner.read().expect("state lock poisoned");
        (inner.router.clone(), inner.auth.clone())
    }

    fn replace(&self, runtime: Runtime) {
        let mut inner = self.inner.write().expect("state lock poisoned");
        inner.router = runtime.router;
        inner.auth = runtime.auth;
    }
}

pub fn build(state: Arc<AppState>) -> AxumRouter {
    AxumRouter::new()
        .route("/health", get(health))
        .route("/reload", post(reload))
        .route("/search", post(search))
        .route("/fetch", post(fetch))
        .route("/mcp", post(mcp_endpoint))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state)
}

/// 公开探活：只返回状态，不暴露渠道清单。
async fn health() -> Response {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") })).into_response()
}

/// 重新读取 config.toml 并替换渠道/key/路由/认证配置，无需重启进程。
async fn reload(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let (_, auth) = state.snapshot();
    if let Some(reason) = unauthorized(&auth, &headers) {
        return reason;
    }
    match runtime::build_from_path(&state.config_path) {
        Ok(runtime) => {
            let channels = runtime.router.channel_status();
            state.replace(runtime);
            Json(json!({ "status": "reloaded", "channels": channels })).into_response()
        }
        Err(error) => {
            // 解析错误可能包含配置片段（含 key），只写日志，不回给客户端
            tracing::error!("reload 失败: {error}");
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "配置解析失败，请查看服务日志" })),
            )
                .into_response()
        }
    }
}

async fn search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<SearchRequest>,
) -> Response {
    let (router, auth) = state.snapshot();
    if let Some(reason) = unauthorized(&auth, &headers) {
        return reason;
    }
    if request.queries.is_empty() || request.queries.len() > 10 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "queries 需为 1-10 条" })),
        )
            .into_response();
    }
    if request
        .queries
        .iter()
        .any(|query| query.chars().count() > 4000)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "单条查询不能超过 4000 字符" })),
        )
            .into_response();
    }
    let responses = router.search(request).await;
    Json(responses).into_response()
}

async fn fetch(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<FetchRequest>,
) -> Response {
    let (router, auth) = state.snapshot();
    if let Some(reason) = unauthorized(&auth, &headers) {
        return reason;
    }
    if request.urls.is_empty() || request.urls.len() > 10 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "urls 需为 1-10 个" })),
        )
            .into_response();
    }
    if let Some(bad) = request
        .urls
        .iter()
        .find(|url| !crate::url_guard::is_public_http_url(url))
    {
        tracing::warn!("拒绝非公网 URL: {bad}");
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "仅允许公网 http/https URL" })),
        )
            .into_response();
    }
    let response = router.fetch(request).await;
    Json(response).into_response()
}

async fn mcp_endpoint(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (router, auth) = state.snapshot();
    if let Some(reason) = unauthorized(&auth, &headers) {
        return reason;
    }
    Json(mcp::handle(&router, body).await).into_response()
}

fn unauthorized(auth: &AuthConfig, headers: &HeaderMap) -> Option<Response> {
    if !auth.enabled {
        return None;
    }
    let provided = extract_key(headers, auth);
    let allowed = provided.as_deref().is_some_and(|key| {
        !key.is_empty()
            && auth.keys.iter().any(|candidate| {
                !candidate.is_empty() && constant_time_eq(candidate.as_bytes(), key.as_bytes())
            })
    });
    if allowed {
        None
    } else {
        Some(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "invalid api key" })),
            )
                .into_response(),
        )
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn extract_key(headers: &HeaderMap, auth: &AuthConfig) -> Option<String> {
    let bearer = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.trim().split_once(' ')?;
            if scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty() {
                Some(token.trim().to_string())
            } else {
                None
            }
        });
    let x_api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    match auth.mode {
        AuthMode::Bearer => bearer,
        AuthMode::XApiKey => x_api_key,
        AuthMode::Both => bearer.or(x_api_key),
        AuthMode::Custom => auth
            .custom_header
            .as_deref()
            .and_then(|name| headers.get(name))
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::{constant_time_eq, extract_key};
    use crate::config::{AuthConfig, AuthMode};
    use axum::http::{HeaderMap, HeaderValue};

    fn auth_config(mode: AuthMode) -> AuthConfig {
        AuthConfig {
            enabled: true,
            keys: Vec::new(),
            mode,
            custom_header: None,
        }
    }

    #[test]
    fn constant_time_eq_matches_only_identical_bytes() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret2"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn extract_key_respects_mode() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer token-a"));
        headers.insert("x-api-key", HeaderValue::from_static("token-b"));

        assert_eq!(
            extract_key(&headers, &auth_config(AuthMode::Bearer)).as_deref(),
            Some("token-a")
        );
        assert_eq!(
            extract_key(&headers, &auth_config(AuthMode::XApiKey)).as_deref(),
            Some("token-b")
        );
        assert_eq!(
            extract_key(&headers, &auth_config(AuthMode::Both)).as_deref(),
            Some("token-a")
        );
    }

    #[test]
    fn custom_mode_requires_custom_header() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer token-a"));
        let mut config = auth_config(AuthMode::Custom);
        config.custom_header = Some("x-search-hub-key".to_string());
        assert_eq!(extract_key(&headers, &config), None);
        headers.insert("x-search-hub-key", HeaderValue::from_static("token-c"));
        assert_eq!(extract_key(&headers, &config).as_deref(), Some("token-c"));
    }
}
