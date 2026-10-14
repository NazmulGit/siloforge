//! API error type. Body shape is v1/Fastify compatible: `{ "error": msg, "message": msg, "statusCode": n }`
//! (the UI reads `message` or `error`; the MCP and plugin read `error`).

use axum::Json;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sf_core::Error;

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    /// Server-side detail of a 5xx (never sent to the client; recorded on the Errors page, G2-16).
    pub detail: Option<String>,
    /// Extra top-level body fields (v2.4 402 `{key, limit, used, plan}`); `error` may be overridden.
    pub extra: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Response extension carrying the detail of a 5xx `ApiError` to [`track_errors`].
#[derive(Clone, Debug)]
pub struct InternalDetail(pub String);

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            detail: None,
            extra: None,
        }
    }
    /// Adds extra top-level fields to the error body.
    pub fn with_extra(mut self, extra: serde_json::Value) -> Self {
        if let serde_json::Value::Object(m) = extra {
            self.extra = Some(m);
        }
        self
    }
    pub fn bad(m: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, m)
    }
    pub fn unauthorized(m: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, m)
    }
    pub fn forbidden(m: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, m)
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, m)
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        let status = match &e {
            Error::BadRequest(_) => StatusCode::BAD_REQUEST,
            Error::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::Upstream(_) => StatusCode::BAD_GATEWAY,
            Error::Cancelled => StatusCode::CONFLICT,
            Error::Db(_) | Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let (message, detail) = if status == StatusCode::INTERNAL_SERVER_ERROR {
            // Never leak SQL or internals to clients; log the detail instead.
            tracing::error!(error = %e, "internal error");
            ("internal error".to_string(), Some(e.to_string()))
        } else {
            (e.to_string(), None)
        };
        Self {
            status,
            message,
            detail,
            extra: None,
        }
    }
}

impl From<axum::extract::rejection::JsonRejection> for ApiError {
    fn from(r: axum::extract::rejection::JsonRejection) -> Self {
        Self::bad(r.body_text())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let reason = self.status.canonical_reason().unwrap_or("Error");
        let mut body = serde_json::json!({
            "error": self.message,
            "message": self.message,
            "statusCode": self.status.as_u16(),
            "reason": reason,
        });
        if let (Some(extra), Some(obj)) = (self.extra, body.as_object_mut()) {
            for (k, v) in extra {
                obj.insert(k, v);
            }
        }
        let mut res = (self.status, Json(body)).into_response();
        if self.status.is_server_error() {
            let d = self.detail.unwrap_or(self.message);
            res.extensions_mut().insert(InternalDetail(d));
        }
        res
    }
}

/// G2-16: records every 5xx produced by a handler in the error tracker (`errors` table, the
/// Errors page) as source `server`, with method + matched route as context (never bodies or
/// headers). Upstream 502s count too: they are what an operator wants to see.
pub async fn track_errors(
    axum::extract::State(st): axum::extract::State<crate::AppState>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().to_string();
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());
    let res = next.run(req).await;
    if let Some(InternalDetail(d)) = res.extensions().get::<InternalDetail>() {
        sf_ops::errors::ErrorTracker::new(st.db.clone()).record_bg(
            &format!("server:{method} {route}"),
            d,
            serde_json::json!({"method": method, "route": route, "status": res.status().as_u16()}),
        );
    }
    res
}

/// Request log line (method, path, status, ms). Never logs bodies or auth headers.
pub async fn request_log(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let t0 = std::time::Instant::now();
    let res = next.run(req).await;
    let ms = t0.elapsed().as_millis();
    let status = res.status().as_u16();
    if path.starts_with("/api/") || path == "/health" || path.starts_with("/mcp") {
        if status >= 500 {
            tracing::warn!(%method, %path, status, ms, "request");
        } else {
            tracing::debug!(%method, %path, status, ms, "request");
        }
    }
    res
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    async fn boom() -> ApiResult<()> {
        Err(Error::Db("no such table: secret_internals".into()).into())
    }

    /// G2-16: a handler's 5xx lands on the Errors page; the client never sees the detail.
    #[tokio::test(flavor = "multi_thread")]
    async fn server_errors_reach_the_error_tracker() {
        let dir = std::env::temp_dir().join(format!("sf-err-test-{}", sf_core::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = sf_core::Config {
            home: dir.clone(),
            ..Default::default()
        };
        cfg.auth.jwt_secret = "test-secret-0123456789abcdef".into();
        let db = sf_store::Db::open(&cfg.db_path()).unwrap();
        let st = crate::AppState::new(cfg, db.clone(), [3u8; 32]);
        let app = crate::router(st, Some(Router::new().route("/boom", get(boom))));
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/boom")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("secret_internals"));
        let mut found = Vec::new();
        for _ in 0..100 {
            found = db
                .read(|c| {
                    let mut s = c
                        .prepare("SELECT source, message FROM errors")
                        .map_err(sf_store::err)?;
                    let rows = s
                        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                        .map_err(sf_store::err)?
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .map_err(sf_store::err)?;
                    Ok(rows)
                })
                .await
                .unwrap();
            if !found.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, "server:GET /boom");
        assert!(found[0].1.contains("secret_internals"));
        db.close();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
