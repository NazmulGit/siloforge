//! MCP server configuration. Env var names are identical to the v1 Node server so the Hermes
//! `config.yaml` entries keep working; prefer `SILOFORGE_TOKEN` (a revocable `sfk_...` API token)
//! over the legacy email/password pair (fixes R1: plaintext admin password in agent configs).
//!
//! v2.4: `SILOFORGE_WORKSPACE` (optional, a workspace id such as `default` or `ws-<id>`) is sent as
//! `X-SiloForge-Workspace` on every REST call, so list routes are scoped to that workspace and
//! creates land in it. Unset = no header = the server's default (pre-2.4 behaviour).

use std::fmt;
use std::time::Duration;

/// Default REST API base URL (the v1 port, kept by v2).
pub const DEFAULT_API_URL: &str = "http://127.0.0.1:8600";
/// v1 default login email.
pub const DEFAULT_EMAIL: &str = "admin@siloforge.local";

/// How the MCP server reaches and authenticates to the SiloForge REST API.
#[derive(Clone)]
pub struct McpConfig {
    /// REST API base URL (`SILOFORGE_API_URL`), no trailing slash.
    pub api_url: String,
    /// `SILOFORGE_TOKEN`: a v2 API token (`sfk_...`) or a JWT. Preferred.
    pub token: Option<String>,
    /// `SILOFORGE_EMAIL` (legacy login fallback).
    pub email: String,
    /// `SILOFORGE_PASSWORD` (legacy login fallback; replace with a token).
    pub password: Option<String>,
    /// Per-request HTTP timeout towards the REST API.
    pub request_timeout: Duration,
    /// How long a resolved `site` (domain -> id) stays cached.
    pub site_cache_ttl: Duration,
    /// Max characters of tool output text before it is truncated with a note.
    pub max_output_chars: usize,
    /// Lower bound for job polling intervals (tests lower it; agents cannot go below it).
    pub min_poll: Duration,
    /// Extra `Host` header values the HTTP transport accepts (empty = loopback only).
    pub http_allowed_hosts: Vec<String>,
    /// How long an effective MCP tool policy (`GET /api/v2/mcp/policy/effective`) stays cached
    /// per identity before it is fetched again.
    pub policy_ttl: Duration,
    /// `SILOFORGE_WORKSPACE`: workspace id sent as `X-SiloForge-Workspace` on every REST call.
    /// Only `[A-Za-z0-9_-]{1,128}` is accepted; anything else is ignored with a warning.
    pub workspace: Option<String>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            api_url: DEFAULT_API_URL.to_string(),
            token: None,
            email: DEFAULT_EMAIL.to_string(),
            password: None,
            request_timeout: Duration::from_secs(120),
            site_cache_ttl: Duration::from_secs(120),
            max_output_chars: 80_000,
            min_poll: Duration::from_secs(2),
            http_allowed_hosts: Vec::new(),
            policy_ttl: Duration::from_secs(60),
            workspace: None,
        }
    }
}

impl McpConfig {
    /// Reads `SILOFORGE_API_URL`, `SILOFORGE_TOKEN`, `SILOFORGE_EMAIL`, `SILOFORGE_PASSWORD`,
    /// `SILOFORGE_WORKSPACE`.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Same as [`McpConfig::from_env`] with an injectable lookup (tests).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let nonempty = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let mut cfg = Self::default();
        if let Some(url) = nonempty("SILOFORGE_API_URL") {
            cfg.api_url = url;
        }
        cfg.api_url = cfg.api_url.trim_end_matches('/').to_string();
        // A1: older MCP panels advertised `<origin>/api`; the client appends `/api/v1/...` itself.
        if let Some(origin) = cfg.api_url.strip_suffix("/api") {
            cfg.api_url = origin.to_string();
        }
        cfg.token = nonempty("SILOFORGE_TOKEN");
        if let Some(email) = nonempty("SILOFORGE_EMAIL") {
            cfg.email = email;
        }
        cfg.password = nonempty("SILOFORGE_PASSWORD");
        cfg.workspace = nonempty("SILOFORGE_WORKSPACE").and_then(|w| {
            if is_workspace_id(&w) {
                Some(w)
            } else {
                tracing::warn!(
                    "SILOFORGE_WORKSPACE ignored: a workspace id may only contain A-Z, a-z, 0-9, `_` and `-` (max 128)"
                );
                None
            }
        });
        cfg
    }

    /// `"token"`, `"password"` or `"none"` (never the secret itself).
    pub fn auth_mode(&self) -> &'static str {
        if self.token.is_some() {
            "token"
        } else if self.password.is_some() {
            "password"
        } else {
            "none"
        }
    }
}

/// `[A-Za-z0-9_-]{1,128}`: safe as an HTTP header value and as a path segment.
pub fn is_workspace_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl fmt::Debug for McpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpConfig")
            .field("api_url", &self.api_url)
            .field("auth", &self.auth_mode())
            .field("email", &self.email)
            .field("token", &self.token.as_ref().map(|_| "***"))
            .field("password", &self.password.as_ref().map(|_| "***"))
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A1: an api_url copied from the old MCP panel (`.../api`) still works.
    #[test]
    fn api_url_with_api_suffix_is_normalised() {
        let cfg = McpConfig::from_lookup(|k| {
            (k == "SILOFORGE_API_URL").then(|| "http://127.0.0.1:8600/api/".to_string())
        });
        assert_eq!(cfg.api_url, "http://127.0.0.1:8600");
    }

    #[test]
    fn env_names_match_v1_and_debug_hides_secrets() {
        let cfg = McpConfig::from_lookup(|k| match k {
            "SILOFORGE_API_URL" => Some("http://127.0.0.1:8610/".into()),
            "SILOFORGE_PASSWORD" => Some("hunter2".into()),
            "SILOFORGE_TOKEN" => Some("  ".into()),
            _ => None,
        });
        assert_eq!(cfg.api_url, "http://127.0.0.1:8610");
        assert_eq!(cfg.email, DEFAULT_EMAIL);
        assert_eq!(cfg.token, None);
        assert_eq!(cfg.auth_mode(), "password");
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("hunter2"));
        assert_eq!(cfg.workspace, None);
    }

    #[test]
    fn workspace_env_is_validated() {
        let with = |v: &'static str| {
            McpConfig::from_lookup(move |k| (k == "SILOFORGE_WORKSPACE").then(|| v.to_string()))
                .workspace
        };
        assert_eq!(with(" ws-abc_1 ").as_deref(), Some("ws-abc_1"));
        assert_eq!(with("default").as_deref(), Some("default"));
        for bad in [
            "",
            "  ",
            "a b",
            "x\r\nX-Evil: 1",
            "../x",
            "日本",
            "w".repeat(129).as_str(),
        ] {
            let bad = bad.to_string();
            let got = McpConfig::from_lookup(|k| (k == "SILOFORGE_WORKSPACE").then(|| bad.clone()))
                .workspace;
            assert_eq!(got, None, "{bad:?}");
        }
    }
}
