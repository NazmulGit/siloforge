//! SiloForge shared foundation: configuration, error type, ids/time helpers, domain enums and the
//! v1-compatible crypto (HS256 JWT, bcrypt, AES-256-GCM `v1:` secret blobs).

pub mod config;
pub mod crypto;
pub mod error;
pub mod live;
pub mod net;
pub mod secrets;
pub mod settings_file;
pub mod types;

pub use config::Config;
pub use error::{Error, Result};
pub use live::LiveConfig;

/// Application version (workspace version), reported by `/health`, backups and the CLI.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Embedding width of all-MiniLM-L6-v2. Every stored vector has this many f32 values.
pub const EMBED_DIM: usize = 384;

/// New id: a UUID v7 (time-ordered) as a lowercase hyphenated string. Same text format as v1's v4
/// ids, but inserts land at the end of SQLite's indexes instead of at random pages (much faster bulk writes).
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Current UTC time as RFC 3339 with millisecond precision (`2026-10-13T10:00:00.000Z`).
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Current UNIX time in seconds.
pub fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}
