//! One error type shared by the library crates. HTTP mapping lives in `sf-server`.

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    /// Caller sent something invalid (HTTP 400).
    #[error("{0}")]
    BadRequest(String),
    /// Missing/invalid credentials (HTTP 401).
    #[error("{0}")]
    Unauthorized(String),
    /// Authenticated but not allowed (HTTP 403).
    #[error("{0}")]
    Forbidden(String),
    /// Not found, or owned by someone else (HTTP 404; never reveal foreign rows).
    #[error("{0}")]
    NotFound(String),
    /// Unique-constraint style conflicts (HTTP 409).
    #[error("{0}")]
    Conflict(String),
    /// Upstream service failed (WordPress, model provider, network) (HTTP 502).
    #[error("{0}")]
    Upstream(String),
    /// Work was cancelled (job cancel, shutdown).
    #[error("cancelled")]
    Cancelled,
    /// Storage failure.
    #[error("database: {0}")]
    Db(String),
    /// Anything else (HTTP 500).
    #[error("{0}")]
    Internal(String),
}

impl Error {
    pub fn bad(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }
    pub fn internal(msg: impl std::fmt::Display) -> Self {
        Self::Internal(msg.to_string())
    }
    pub fn upstream(msg: impl std::fmt::Display) -> Self {
        Self::Upstream(msg.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::BadRequest(format!("invalid json: {e}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Internal(format!("io: {e}"))
    }
}
