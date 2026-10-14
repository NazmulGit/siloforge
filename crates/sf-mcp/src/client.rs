//! Thin SiloForge REST client used by the tools. Auth, ownership and validation stay in the API.
//!
//! Auth sources:
//! * `Static` token (`SILOFORGE_TOKEN`, an `sfk_...` API token or a JWT): sent as-is, no re-login.
//! * `Login` (legacy `SILOFORGE_EMAIL`/`SILOFORGE_PASSWORD`): `POST /api/v1/auth/login`, cached,
//!   re-login once on a 401.
//! * `Forwarded`: HTTP transport; the MCP client's own bearer token is passed through, so the
//!   caller's permissions apply. Never replaced by a built-in identity.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

use crate::config::McpConfig;
use crate::output::{strip_secrets, truncate_str};

/// A failed API call or a client-side problem, rendered as the v1 error text.
#[derive(Debug, Clone)]
pub struct ToolError(pub String);

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl ToolError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

pub type ToolResult<T> = Result<T, ToolError>;

/// A request body: JSON (the norm) or raw text with its content type (CSV import).
#[derive(Debug, Clone)]
pub enum ReqBody {
    Json(Value),
    Text(String, &'static str),
}

/// Header that marks REST calls made by the MCP server (unified log, contract §9).
pub const VIA_HEADER: &str = "x-siloforge-via";
/// Header naming the MCP tool a REST call was made for.
pub const TOOL_HEADER: &str = "x-siloforge-tool";

/// A successful API response.
#[derive(Debug, Clone)]
pub enum ApiBody {
    Json(Value),
    Text(String),
}

impl ApiBody {
    pub fn into_json(self) -> Value {
        match self {
            ApiBody::Json(v) => v,
            ApiBody::Text(t) => Value::String(t),
        }
    }
}

#[derive(Clone)]
pub(crate) enum Auth {
    Static(String),
    Login {
        email: String,
        password: Option<String>,
        cached: Arc<Mutex<Option<String>>>,
    },
    Forwarded(String),
}

impl Auth {
    /// Key that scopes the site cache to one identity (different callers own different sites).
    pub(crate) fn identity(&self) -> String {
        match self {
            Auth::Static(t) | Auth::Forwarded(t) => format!("t:{t}"),
            Auth::Login { email, .. } => format!("l:{email}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SiteRef {
    pub id: String,
    pub domain: String,
}

struct CacheEntry {
    fetched: Instant,
    sites: Vec<SiteRef>,
}

/// Site list cache keyed by identity. Bounded so a stream of distinct HTTP callers cannot grow it.
#[derive(Default)]
pub(crate) struct SiteCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
}

const MAX_CACHE_IDENTITIES: usize = 64;

impl SiteCache {
    fn get(&self, identity: &str, ttl: Duration) -> Option<Vec<SiteRef>> {
        let map = self.entries.lock();
        map.get(identity)
            .filter(|e| e.fetched.elapsed() < ttl)
            .map(|e| e.sites.clone())
    }

    fn put(&self, identity: String, sites: Vec<SiteRef>) {
        let mut map = self.entries.lock();
        if map.len() >= MAX_CACHE_IDENTITIES
            && !map.contains_key(&identity)
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, e)| e.fetched)
                .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
        map.insert(
            identity,
            CacheEntry {
                fetched: Instant::now(),
                sites,
            },
        );
    }

    fn invalidate(&self, identity: &str) {
        self.entries.lock().remove(identity);
    }
}

/// Shared state: HTTP client, config and caches.
pub(crate) struct Core {
    pub cfg: McpConfig,
    pub http: reqwest::Client,
    pub sites: SiteCache,
    /// The process-wide auth for stdio mode (holds the cached login JWT).
    pub stdio_auth: Auth,
    /// Counts `GET /api/v1/sites` calls (tests assert the cache works).
    pub site_list_calls: std::sync::atomic::AtomicUsize,
    /// Effective MCP tool policy per identity.
    pub policy: crate::policy::PolicyCache,
    /// Bounds the fire-and-forget `POST /api/v2/mcp/usage` calls in flight.
    pub usage_slots: Arc<tokio::sync::Semaphore>,
}

/// Max usage reports in flight at once; extra reports are dropped (telemetry never blocks tools).
pub const MAX_USAGE_IN_FLIGHT: usize = 16;

impl Core {
    pub fn new(cfg: McpConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(cfg.request_timeout)
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("siloforge-mcp/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let stdio_auth = match &cfg.token {
            Some(t) => Auth::Static(t.clone()),
            None => Auth::Login {
                email: cfg.email.clone(),
                password: cfg.password.clone(),
                cached: Arc::new(Mutex::new(None)),
            },
        };
        Ok(Self {
            cfg,
            http,
            sites: SiteCache::default(),
            stdio_auth,
            site_list_calls: std::sync::atomic::AtomicUsize::new(0),
            policy: crate::policy::PolicyCache::default(),
            usage_slots: Arc::new(tokio::sync::Semaphore::new(MAX_USAGE_IN_FLIGHT)),
        })
    }
}

/// A per-call API handle: shared core + the identity to act as.
#[derive(Clone)]
pub struct Api {
    pub(crate) core: Arc<Core>,
    pub(crate) auth: Auth,
    /// The tool this handle runs for: sent as `X-SiloForge-Tool` (with `X-SiloForge-Via: mcp`).
    pub(crate) tool: Option<&'static str>,
    /// The caller's effective tool policy (capabilities output), when known.
    pub(crate) policy: Option<Arc<crate::policy::Effective>>,
}

const MAX_ERROR_BODY: usize = 300;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_POLICY_BYTES: usize = 4 * 1024 * 1024;

impl Api {
    pub(crate) fn cfg(&self) -> &McpConfig {
        &self.core.cfg
    }

    async fn login(&self) -> ToolResult<String> {
        let Auth::Login {
            email,
            password,
            cached,
        } = &self.auth
        else {
            return Err(ToolError::new(
                "internal: login called without password auth",
            ));
        };
        let Some(password) = password else {
            return Err(ToolError::new(
                "SILOFORGE_TOKEN (preferred) or SILOFORGE_PASSWORD is not set — cannot authenticate to the SiloForge API.",
            ));
        };
        let url = format!("{}/api/v1/auth/login", self.core.cfg.api_url);
        let res = self
            .core
            .http
            .post(&url)
            .json(&json!({ "email": email, "password": password }))
            .send()
            .await
            .map_err(|e| {
                ToolError::new(format!(
                    "SiloForge API unreachable at {}: {e}",
                    self.core.cfg.api_url
                ))
            })?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ToolError::new(format!(
                "SILOFORGE login failed ({}): {}",
                status.as_u16(),
                truncate_str(&text, 200)
            )));
        }
        let token = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("token").and_then(Value::as_str).map(str::to_string))
            .ok_or_else(|| ToolError::new("SILOFORGE login response had no token"))?;
        *cached.lock() = Some(token.clone());
        Ok(token)
    }

    async fn bearer(&self) -> ToolResult<String> {
        match &self.auth {
            Auth::Static(t) | Auth::Forwarded(t) => Ok(t.clone()),
            Auth::Login { cached, .. } => {
                let existing = cached.lock().clone();
                match existing {
                    Some(t) => Ok(t),
                    None => self.login().await,
                }
            }
        }
    }

    async fn send_once(
        &self,
        method: &Method,
        path: &str,
        body: Option<&ReqBody>,
        token: &str,
        timeout: Option<Duration>,
    ) -> ToolResult<reqwest::Response> {
        let url = format!("{}{}", self.core.cfg.api_url, path);
        let mut req = self
            .core
            .http
            .request(method.clone(), &url)
            .bearer_auth(token);
        if let Some(tool) = self.tool {
            req = req.header(VIA_HEADER, "mcp").header(TOOL_HEADER, tool);
        }
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        match body {
            Some(ReqBody::Json(b)) => req = req.json(b),
            Some(ReqBody::Text(t, ct)) => {
                req = req
                    .header(reqwest::header::CONTENT_TYPE, *ct)
                    .body(t.clone());
            }
            None => {}
        }
        req.send().await.map_err(|e| {
            ToolError::new(format!(
                "SiloForge API unreachable at {}: {e}",
                self.core.cfg.api_url
            ))
        })
    }

    /// Sends with the current identity; legacy login auth re-logs in once on a 401.
    async fn send_raw(
        &self,
        method: &Method,
        path: &str,
        body: Option<&ReqBody>,
        timeout: Option<Duration>,
    ) -> ToolResult<reqwest::Response> {
        let token = self.bearer().await?;
        let res = self.send_once(method, path, body, &token, timeout).await?;
        if res.status() == StatusCode::UNAUTHORIZED && matches!(self.auth, Auth::Login { .. }) {
            let fresh = self.login().await?;
            return self.send_once(method, path, body, &fresh, timeout).await;
        }
        Ok(res)
    }

    /// `GET path` that reports the HTTP status instead of failing on non-2xx (policy fetch).
    /// `Err` only when the API is unreachable or authentication itself failed.
    pub(crate) async fn get_status(
        &self,
        path: &str,
        timeout: Duration,
    ) -> ToolResult<(u16, Option<Value>)> {
        let res = self
            .send_raw(&Method::GET, path, None, Some(timeout))
            .await?;
        let status = res.status().as_u16();
        let bytes = res
            .bytes()
            .await
            .map_err(|e| ToolError::new(format!("SiloForge API read error: {e}")))?;
        let bytes = bytes
            .get(..bytes.len().min(MAX_POLICY_BYTES))
            .unwrap_or_default();
        Ok((status, serde_json::from_slice::<Value>(bytes).ok()))
    }

    /// Fire-and-forget POST (usage telemetry): the response body is ignored.
    pub(crate) async fn post_quiet(
        &self,
        path: &str,
        body: Value,
        timeout: Duration,
    ) -> ToolResult<u16> {
        let res = self
            .send_raw(
                &Method::POST,
                path,
                Some(&ReqBody::Json(body)),
                Some(timeout),
            )
            .await?;
        Ok(res.status().as_u16())
    }

    /// Calls the API. `path` includes the query string. JSON bodies are secret-stripped.
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> ToolResult<ApiBody> {
        self.call_body(method, path, body.map(ReqBody::Json)).await
    }

    /// [`Api::call`] with any request body kind.
    pub async fn call_body(
        &self,
        method: Method,
        path: &str,
        body: Option<ReqBody>,
    ) -> ToolResult<ApiBody> {
        let res = self.send_raw(&method, path, body.as_ref(), None).await?;
        let status = res.status();
        let is_json = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("application/json"));
        let bytes = res
            .bytes()
            .await
            .map_err(|e| ToolError::new(format!("SiloForge API read error: {e}")))?;
        let bytes = bytes
            .get(..bytes.len().min(MAX_RESPONSE_BYTES))
            .unwrap_or_default();
        let parsed = if is_json {
            serde_json::from_slice::<Value>(bytes).ok()
        } else {
            None
        };
        if !status.is_success() {
            let msg = match &parsed {
                Some(v) => v
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        let mut v = v.clone();
                        strip_secrets(&mut v);
                        truncate_str(&v.to_string(), MAX_ERROR_BODY).to_string()
                    }),
                None => truncate_str(&String::from_utf8_lossy(bytes), MAX_ERROR_BODY).to_string(),
            };
            let path_only = path.split('?').next().unwrap_or(path);
            return Err(ToolError::new(format!(
                "SILOFORGE {method} {path_only} → {}: {msg}",
                status.as_u16()
            )));
        }
        match parsed {
            Some(mut v) => {
                strip_secrets(&mut v);
                Ok(ApiBody::Json(v))
            }
            None if is_json => Err(ToolError::new(format!(
                "SILOFORGE {method} {path}: invalid JSON response"
            ))),
            None => Ok(ApiBody::Text(String::from_utf8_lossy(bytes).into_owned())),
        }
    }

    pub async fn get(&self, path: &str) -> ToolResult<Value> {
        self.call(Method::GET, path, None)
            .await
            .map(ApiBody::into_json)
    }

    pub async fn post(&self, path: &str, body: Value) -> ToolResult<Value> {
        self.call(Method::POST, path, Some(body))
            .await
            .map(ApiBody::into_json)
    }

    /// Lists the caller's sites (id + domain only; nothing else is kept).
    async fn fetch_sites(&self) -> ToolResult<Vec<SiteRef>> {
        self.core
            .site_list_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let v = self.get("/api/v1/sites").await?;
        let list = v.as_array().cloned().unwrap_or_default();
        let sites: Vec<SiteRef> = list
            .iter()
            .filter_map(|s| {
                Some(SiteRef {
                    id: s.get("id")?.as_str()?.to_string(),
                    domain: s
                        .get("domain")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                })
            })
            .collect();
        self.core.sites.put(self.auth.identity(), sites.clone());
        Ok(sites)
    }

    /// Forgets the cached site list (after add_site).
    pub fn invalidate_sites(&self) {
        self.core.sites.invalidate(&self.auth.identity());
    }

    /// Resolves a site id or domain to an id. A UUID-shaped value is used directly (the API
    /// enforces ownership with a 404); domains go through a per-identity TTL cache (R14).
    pub async fn resolve_site(&self, site: &str) -> ToolResult<String> {
        let site = site.trim();
        if site.is_empty() {
            return Err(ToolError::new("site is required (a site id or domain)"));
        }
        if looks_like_uuid(site) {
            return Ok(site.to_ascii_lowercase());
        }
        let want = normalize_domain(site);
        let ttl = self.core.cfg.site_cache_ttl;
        if let Some(cached) = self.core.sites.get(&self.auth.identity(), ttl)
            && let Some(id) = find_site(&cached, site, &want)
        {
            return Ok(id);
        }
        let fresh = self.fetch_sites().await?;
        find_site(&fresh, site, &want).ok_or_else(|| {
            ToolError::new(format!(
                "site not found: \"{}\" (use siloforge_list_sites to see ids/domains)",
                truncate_str(site, 200)
            ))
        })
    }
}

fn find_site(sites: &[SiteRef], raw: &str, want: &str) -> Option<String> {
    let exact = sites
        .iter()
        .find(|s| s.id == raw || normalize_domain(&s.domain) == want);
    let found = exact.or_else(|| {
        let alt = want
            .strip_prefix("www.")
            .map(str::to_string)
            .unwrap_or_else(|| format!("www.{want}"));
        sites.iter().find(|s| normalize_domain(&s.domain) == alt)
    });
    found.map(|s| s.id.clone())
}

/// `https://WWW.Example.com/path` -> `www.example.com`.
pub fn normalize_domain(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let host = s.split(['/', '?', '#']).next().unwrap_or(s);
    host.trim_end_matches('.').to_ascii_lowercase()
}

pub fn looks_like_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_normalization() {
        assert_eq!(
            normalize_domain("https://WWW.Example.com/a?b"),
            "www.example.com"
        );
        assert_eq!(normalize_domain("example.com."), "example.com");
        assert!(looks_like_uuid("123e4567-e89b-12d3-a456-426614174000"));
        assert!(!looks_like_uuid("example.com"));
        let sites = vec![SiteRef {
            id: "s1".into(),
            domain: "www.a.com".into(),
        }];
        assert_eq!(find_site(&sites, "a.com", "a.com").as_deref(), Some("s1"));
        assert_eq!(find_site(&sites, "s1", "s1").as_deref(), Some("s1"));
        assert_eq!(find_site(&sites, "b.com", "b.com"), None);
    }
}
