//! MCP tool policy enforcement (contract §3). The server computes the final list of tools the
//! calling credential may use (`GET /api/v2/mcp/policy/effective`); this module caches it per
//! identity for `McpConfig::policy_ttl` and answers "may this caller see/call tool X?".
//!
//! Failure handling:
//! * `404` (a server older than v2.1) or an unreachable API with no earlier answer: fail open,
//!   i.e. every tool except the default-disabled ones (`siloforge_api_request`), logged once.
//! * any other failure (5xx, 401, timeout, malformed body): keep the last known policy for that
//!   identity; without one, fail open the same way.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::Value;

use crate::client::Api;
use crate::tools;

/// Policy endpoint (any user / API token).
pub const EFFECTIVE_PATH: &str = "/api/v2/mcp/policy/effective";
/// Usage telemetry endpoint.
pub const USAGE_PATH: &str = "/api/v2/mcp/usage";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Distinct identities (HTTP bearer hashes) kept at once.
pub const MAX_POLICY_IDENTITIES: usize = 256;

/// Where an effective policy came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The server answered `GET /api/v2/mcp/policy/effective`.
    Server,
    /// The server has no policy endpoint (404, pre-2.1): default tool set, no usage reports.
    Legacy,
    /// The policy could not be fetched (unreachable/error) and none was known: default tool set.
    Unavailable,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Server => "server",
            Source::Legacy => "fail-open (server has no MCP policy endpoint)",
            Source::Unavailable => "fail-open (policy endpoint unavailable)",
        }
    }
}

/// The tools one caller may use.
#[derive(Debug)]
pub struct Effective {
    pub source: Source,
    allowed: HashSet<&'static str>,
    /// The caller's scope as reported by the server (`read`/`write`/`admin`).
    pub scope: Option<String>,
}

impl Effective {
    /// Every catalogue tool that is enabled by default.
    pub fn fail_open(source: Source) -> Self {
        Self {
            source,
            allowed: tools::catalogue_ref()
                .iter()
                .filter(|t| t.default_enabled)
                .map(|t| t.name)
                .collect(),
            scope: None,
        }
    }

    /// Parses the server answer. `None` when the body is not a policy object.
    pub fn from_server(v: &Value, http: bool) -> Option<Self> {
        let list = v.get("allowed")?.as_array()?;
        let wanted: HashSet<&str> = list.iter().filter_map(Value::as_str).collect();
        let http_off = http && v.get("http_enabled").and_then(Value::as_bool) == Some(false);
        let allowed = if http_off {
            HashSet::new()
        } else {
            tools::catalogue_ref()
                .iter()
                .map(|t| t.name)
                .filter(|n| wanted.contains(n))
                .collect()
        };
        Some(Self {
            source: Source::Server,
            allowed,
            scope: v
                .get("scope")
                .and_then(Value::as_str)
                .map(|s| crate::output::truncate_str(s, 32).to_string()),
        })
    }

    pub fn allows(&self, name: &str) -> bool {
        self.allowed.contains(name)
    }

    pub fn allowed_count(&self) -> usize {
        self.allowed.len()
    }

    /// Usage reports go only to servers that have the MCP endpoints.
    pub fn reports_usage(&self) -> bool {
        self.source != Source::Legacy
    }
}

struct Slot {
    fetched: Instant,
    eff: Arc<Effective>,
}

/// Per-identity cache of effective policies (bounded).
#[derive(Default)]
pub struct PolicyCache {
    entries: Mutex<HashMap<String, Slot>>,
    warned_legacy: AtomicBool,
    warned_unavailable: AtomicBool,
}

impl PolicyCache {
    fn fresh(&self, key: &str, ttl: Duration) -> Option<Arc<Effective>> {
        self.entries
            .lock()
            .get(key)
            .filter(|s| s.fetched.elapsed() < ttl)
            .map(|s| s.eff.clone())
    }

    fn last(&self, key: &str) -> Option<Arc<Effective>> {
        self.entries.lock().get(key).map(|s| s.eff.clone())
    }

    fn put(&self, key: String, eff: Arc<Effective>) {
        let mut map = self.entries.lock();
        if map.len() >= MAX_POLICY_IDENTITIES
            && !map.contains_key(&key)
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, s)| s.fetched)
                .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
        map.insert(
            key,
            Slot {
                fetched: Instant::now(),
                eff,
            },
        );
    }

    /// Number of cached identities (tests).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// The effective policy for `api`'s identity, fetched when missing or older than the TTL.
    pub async fn effective(&self, api: &Api, http: bool) -> Arc<Effective> {
        // Bearer tokens are never kept as map keys: only their hash.
        let key = format!(
            "{}:{}",
            if http { "h" } else { "s" },
            sf_core::crypto::sha256_hex(api.auth.identity().as_bytes())
        );
        let ttl = api.cfg().policy_ttl;
        if let Some(eff) = self.fresh(&key, ttl) {
            return eff;
        }
        let fetched = api.get_status(EFFECTIVE_PATH, FETCH_TIMEOUT).await;
        let eff = match fetched {
            Ok((200, Some(v))) => match Effective::from_server(&v, http) {
                Some(e) => Arc::new(e),
                None => self.on_failure(&key, "malformed policy response"),
            },
            Ok((404, _)) => {
                if !self.warned_legacy.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "SiloForge API has no MCP policy endpoint (pre-2.1 server): all default-enabled tools are allowed"
                    );
                }
                self.last(&key)
                    .filter(|e| e.source == Source::Server)
                    .unwrap_or_else(|| Arc::new(Effective::fail_open(Source::Legacy)))
            }
            Ok((status, _)) => self.on_failure(&key, &format!("HTTP {status}")),
            Err(e) => self.on_failure(&key, &e.0),
        };
        self.put(key, eff.clone());
        eff
    }

    fn on_failure(&self, key: &str, why: &str) -> Arc<Effective> {
        if let Some(prev) = self.last(key) {
            tracing::debug!(reason = %crate::output::truncate_str(why, 200), "mcp policy refresh failed; keeping the last known policy");
            return prev;
        }
        if !self.warned_unavailable.swap(true, Ordering::Relaxed) {
            tracing::warn!(reason = %crate::output::truncate_str(why, 200), "mcp policy unavailable: failing open to the default-enabled tools");
        }
        Arc::new(Effective::fail_open(Source::Unavailable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fail_open_excludes_default_disabled() {
        let e = Effective::fail_open(Source::Legacy);
        assert!(e.allows("siloforge_list_sites"));
        assert!(!e.allows("siloforge_api_request"));
        assert!(!e.reports_usage());
    }

    #[test]
    fn server_policy_parsing() {
        let v = json!({"http_enabled": false, "scope": "read",
            "allowed": ["siloforge_list_sites", "not_a_tool", 5]});
        let e = Effective::from_server(&v, false)
            .unwrap_or_else(|| Effective::fail_open(Source::Unavailable));
        assert_eq!(e.source, Source::Server);
        assert!(e.allows("siloforge_list_sites"));
        assert!(!e.allows("not_a_tool"));
        assert_eq!(e.allowed_count(), 1);
        assert_eq!(e.scope.as_deref(), Some("read"));
        // http_enabled=false hides everything over HTTP.
        let h = Effective::from_server(&v, true)
            .unwrap_or_else(|| Effective::fail_open(Source::Unavailable));
        assert_eq!(h.allowed_count(), 0);
        assert!(Effective::from_server(&json!({"x": 1}), false).is_none());
    }

    #[test]
    fn cache_is_bounded() {
        let c = PolicyCache::default();
        for i in 0..(MAX_POLICY_IDENTITIES + 10) {
            c.put(
                format!("k{i}"),
                Arc::new(Effective::fail_open(Source::Legacy)),
            );
        }
        assert_eq!(c.len(), MAX_POLICY_IDENTITIES);
    }
}
