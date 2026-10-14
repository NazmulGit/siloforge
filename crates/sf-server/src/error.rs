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
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
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
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            // Never leak SQL or internals to clients; log the detail instead.
            tracing::error!(error = %e, "internal error");
            "internal error".to_string()
        } else {
            e.to_string()
        };
        Self { status, message }
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
        let body = serde_json::json!({
            "error": self.message,
            "message": self.message,
            "statusCode": self.status.as_u16(),
            "reason": reason,
        });
        (self.status, Json(body)).into_response()
    }
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
