//! Crawl error type with retry classification for the job runner.

use std::time::Duration;

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, CrawlError>;

/// Every failure the crawler, writer and checker can surface.
///
/// Use [`CrawlError::is_retryable`] to decide whether a job should be retried and
/// [`CrawlError::kind`] for a stable machine-readable label (logs, `job_runs.error`).
#[derive(Debug, thiserror::Error)]
pub enum CrawlError {
    /// The URL could not be parsed or uses an unsupported scheme.
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    /// The SSRF guard refused the target (private/reserved address, or policy).
    #[error("blocked by ssrf guard: {url}: {reason}")]
    Blocked { url: String, reason: String },
    /// DNS resolution failed or returned no records.
    #[error("dns lookup failed for {host}: {message}")]
    Dns { host: String, message: String },
    /// TCP/TLS connection failure (refused, reset, certificate...).
    #[error("connection error: {0}")]
    Connect(String),
    /// Request or body read exceeded its time budget.
    #[error("timeout: {0}")]
    Timeout(String),
    /// More redirects than allowed.
    #[error("too many redirects ({hops}) starting at {url}")]
    TooManyRedirects { url: String, hops: usize },
    /// A redirect chain revisited a URL.
    #[error("redirect loop at {0}")]
    RedirectLoop(String),
    /// Non-success HTTP status. `retry_after` is parsed from `Retry-After` when present.
    #[error("http {status} from {url}")]
    Http {
        status: u16,
        url: String,
        retry_after: Option<Duration>,
    },
    /// The response body exceeded the configured cap.
    #[error("response body exceeds {limit} bytes: {url}")]
    BodyTooLarge { url: String, limit: usize },
    /// The response could not be decoded (malformed JSON/XML, wrong shape).
    #[error("decode error: {0}")]
    Decode(String),
    /// Optimistic concurrency failure: the post changed since the caller read it.
    #[error("post {post_id} was modified since read (expected {expected}, now {actual})")]
    Conflict {
        post_id: u64,
        expected: String,
        actual: String,
    },
    /// The consumer of a streaming crawl went away.
    #[error("crawl cancelled: receiver dropped")]
    Cancelled,
    /// Bad configuration (e.g. invalid header value, client build failure).
    #[error("config error: {0}")]
    Config(String),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

impl CrawlError {
    /// True when a retry later could plausibly succeed: timeouts, connection and DNS
    /// failures (resolvers fail transiently), 408/425/429 and 5xx. Never for 4xx auth/not-found,
    /// SSRF blocks, conflicts or decode errors.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Timeout(_) | Self::Connect(_) | Self::Dns { .. } => true,
            Self::Http { status, .. } => matches!(*status, 408 | 425 | 429) || *status >= 500,
            _ => false,
        }
    }

    /// Stable short label.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidUrl(_) => "invalid_url",
            Self::Blocked { .. } => "blocked",
            Self::Dns { .. } => "dns",
            Self::Connect(_) => "connection",
            Self::Timeout(_) => "timeout",
            Self::TooManyRedirects { .. } => "hop_cap",
            Self::RedirectLoop(_) => "loop",
            Self::Http { .. } => "http",
            Self::BodyTooLarge { .. } => "body_too_large",
            Self::Decode(_) => "decode",
            Self::Conflict { .. } => "conflict",
            Self::Cancelled => "cancelled",
            Self::Config(_) => "config",
            Self::Other(_) => "other",
        }
    }

    /// HTTP status, when the error carries one.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// True for 401/403 (bad or missing WP credentials, or bot protection).
    pub fn is_auth(&self) -> bool {
        matches!(self.status(), Some(401 | 403))
    }
}
