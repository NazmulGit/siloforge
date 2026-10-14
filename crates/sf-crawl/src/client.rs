//! Polite, SSRF-guarded HTTP client shared by every crawler in this crate.
//!
//! * rustls, gzip + brotli, total + connect timeouts, user agent, no system proxy.
//! * Body size cap applied to the *decompressed* stream (zip-bomb safe).
//! * Per-host concurrency limit + politeness delay; adaptive backoff on 429/503 honouring
//!   `Retry-After` (the whole host pauses, not just the one request).
//! * Redirects are followed manually so the SSRF policy runs on every hop; `Authorization`
//!   is dropped on cross-origin hops.
//! * Host aliases are applied in the DNS resolver, so the `Host` header keeps the original name.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use url::Url;

use crate::error::{CrawlError, Result};
use crate::ssrf::{GuardedResolver, ResolveFailure, SsrfPolicy};

/// Default user agent.
pub const DEFAULT_USER_AGENT: &str = "SiloForge/2.0 (+internal-linking)";

/// Client configuration. Build via [`HttpClient::builder`] or [`HttpConfig::from_crawl_cfg`].
#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub user_agent: String,
    /// Total time budget per request attempt (headers + body).
    pub timeout: Duration,
    pub connect_timeout: Duration,
    /// Default body cap (bytes, after decompression).
    pub max_body_bytes: usize,
    /// Concurrent requests per `host:port`.
    pub per_host_concurrency: usize,
    /// Minimum gap between request starts on one host.
    pub politeness_delay: Duration,
    pub max_redirects: usize,
    /// Retries on 429/503 (0 = none).
    pub max_retries: u32,
    /// First backoff when no `Retry-After` is given; doubles per attempt.
    pub base_backoff: Duration,
    /// Cap for any single wait (also caps `Retry-After`).
    pub max_backoff: Duration,
    /// Disables address filtering for every destination. Dev/test only: the `[crawl]` config
    /// never sets this (see `private_site_hosts`).
    pub allow_private: bool,
    /// Hosts allowed to resolve to private addresses (names, IP literals or URLs).
    pub allow_hosts: Vec<String>,
    /// The job's configured site hosts (domain / wp_url). Host aliases apply only to these
    /// (and to `allow_hosts`), never to arbitrary link or redirect targets (SSRF H-02).
    pub site_hosts: Vec<String>,
    /// When true, `site_hosts` may resolve to private addresses (`crawl.allow_private_hosts`).
    pub private_site_hosts: bool,
    /// Host rewrites applied at connect time, e.g. `site01.example -> 127.0.0.1`.
    /// An alias only rewrites the address; the target is still subject to the IP check.
    pub host_aliases: BTreeMap<String, String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            user_agent: DEFAULT_USER_AGENT.into(),
            timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            max_body_bytes: 8 * 1024 * 1024,
            per_host_concurrency: 4,
            politeness_delay: Duration::from_millis(100),
            max_redirects: 10,
            max_retries: 2,
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(60),
            allow_private: false,
            allow_hosts: Vec::new(),
            site_hosts: Vec::new(),
            private_site_hosts: false,
            host_aliases: BTreeMap::new(),
        }
    }
}

impl HttpConfig {
    /// Map the `[crawl]` section of `siloforge.toml`.
    pub fn from_crawl_cfg(c: &sf_core::config::CrawlCfg) -> Self {
        let d = Self::default();
        Self {
            user_agent: if c.user_agent.trim().is_empty() {
                d.user_agent.clone()
            } else {
                c.user_agent.clone()
            },
            timeout: if c.timeout_ms == 0 {
                d.timeout
            } else {
                Duration::from_millis(c.timeout_ms)
            },
            per_host_concurrency: c.concurrency.clamp(1, 64),
            // The global flag only lets the configured site hosts be private; it no longer
            // disables the guard for links, redirects or sitemap children (SSRF H-02).
            allow_private: false,
            private_site_hosts: c.allow_private_hosts,
            host_aliases: c.host_aliases.clone(),
            ..d
        }
    }
}

/// Host name of a host-or-URL string, normalized.
fn host_of(h: &str) -> String {
    let host = Url::parse(h)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| h.to_string());
    crate::ssrf::normalize_host(&host)
}

/// Aliases restricted to the configured site hosts and explicitly allowed hosts (SSRF H-02):
/// an alias such as `site01.example -> 127.0.0.1` is applied only when the job's own site
/// lives on that name, never to a third-party link or redirect target.
pub(crate) fn effective_aliases(cfg: &HttpConfig) -> BTreeMap<String, String> {
    let known: HashSet<String> = cfg
        .site_hosts
        .iter()
        .chain(cfg.allow_hosts.iter())
        .map(|h| host_of(h))
        .filter(|h| !h.is_empty())
        .collect();
    cfg.host_aliases
        .iter()
        .filter(|(k, _)| known.contains(&crate::ssrf::normalize_host(k)))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Builder for [`HttpClient`].
#[derive(Debug, Clone, Default)]
pub struct HttpClientBuilder {
    cfg: HttpConfig,
}

macro_rules! setter {
    ($(#[$m:meta])* $name:ident : $ty:ty) => {
        $(#[$m])*
        pub fn $name(mut self, v: $ty) -> Self {
            self.cfg.$name = v;
            self
        }
    };
}

impl HttpClientBuilder {
    /// Start from an existing config.
    pub fn from_config(cfg: HttpConfig) -> Self {
        Self { cfg }
    }
    setter!(user_agent: String);
    setter!(timeout: Duration);
    setter!(connect_timeout: Duration);
    setter!(max_body_bytes: usize);
    setter!(per_host_concurrency: usize);
    setter!(politeness_delay: Duration);
    setter!(max_redirects: usize);
    setter!(max_retries: u32);
    setter!(base_backoff: Duration);
    setter!(max_backoff: Duration);
    setter!(allow_private: bool);
    setter!(host_aliases: BTreeMap<String, String>);

    /// Allow `host` (name, IP literal or URL) to resolve to private addresses.
    pub fn allow_host(mut self, host: impl Into<String>) -> Self {
        self.cfg.allow_hosts.push(host.into());
        self
    }

    /// Mark `host` (name, IP literal or URL) as one of the job's configured site hosts.
    pub fn site_host(mut self, host: impl Into<String>) -> Self {
        self.cfg.site_hosts.push(host.into());
        self
    }

    /// Add one host alias.
    pub fn alias(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.cfg.host_aliases.insert(from.into(), to.into());
        self
    }

    /// Build the client.
    pub fn build(self) -> Result<HttpClient> {
        HttpClient::from_http_config(self.cfg)
    }
}

/// One hop of a redirect chain (`link_health.redirect_chain` element shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RedirectHop {
    pub url: String,
    pub status: u16,
    pub order: usize,
}

/// A request for [`HttpClient::fetch`].
#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Option<Bytes>,
    basic_auth: Option<(String, String)>,
    pub follow_redirects: bool,
    /// Overrides the client's body cap.
    pub max_body_bytes: Option<usize>,
    /// Truncate oversize bodies (sets `truncated`) instead of failing with `BodyTooLarge`.
    pub truncate_body: bool,
    /// Retry 429/503 per the client's policy.
    pub retry: bool,
    /// Overrides the client's per-attempt timeout.
    pub timeout: Option<Duration>,
    /// Skip reading the body (HEAD-like use).
    pub discard_body: bool,
}

impl FetchRequest {
    /// New request with redirects followed, retries on, body cap from the client.
    pub fn new(method: Method, url: Url) -> Self {
        Self {
            method,
            url,
            headers: HeaderMap::new(),
            body: None,
            basic_auth: None,
            follow_redirects: true,
            max_body_bytes: None,
            truncate_body: false,
            retry: true,
            timeout: None,
            discard_body: false,
        }
    }
    /// GET.
    pub fn get(url: Url) -> Self {
        Self::new(Method::GET, url)
    }
    /// HEAD (body discarded).
    pub fn head(url: Url) -> Self {
        let mut r = Self::new(Method::HEAD, url);
        r.discard_body = true;
        r
    }
    /// Add a header; invalid names/values are a `Config` error.
    pub fn header(mut self, name: &str, value: &str) -> Result<Self> {
        let n = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| CrawlError::Config(format!("header name {name}: {e}")))?;
        let v = HeaderValue::from_str(value)
            .map_err(|e| CrawlError::Config(format!("header value for {name}: {e}")))?;
        self.headers.insert(n, v);
        Ok(self)
    }
    /// JSON body (sets content-type).
    pub fn json<T: Serialize + ?Sized>(mut self, body: &T) -> Result<Self> {
        let b = serde_json::to_vec(body).map_err(|e| CrawlError::Decode(e.to_string()))?;
        self.body = Some(Bytes::from(b));
        self.headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        Ok(self)
    }
    /// HTTP Basic auth (WP application passwords). Dropped on cross-origin redirects.
    pub fn basic_auth(mut self, user: impl Into<String>, password: impl Into<String>) -> Self {
        self.basic_auth = Some((user.into(), password.into()));
        self
    }
    /// Do not follow redirects (the 3xx response is returned).
    pub fn no_redirects(mut self) -> Self {
        self.follow_redirects = false;
        self
    }
    /// Body cap override.
    pub fn max_body(mut self, n: usize) -> Self {
        self.max_body_bytes = Some(n);
        self
    }
    /// Truncate instead of erroring on oversize bodies.
    pub fn truncate(mut self) -> Self {
        self.truncate_body = true;
        self
    }
    /// Disable 429/503 retries for this request.
    pub fn no_retry(mut self) -> Self {
        self.retry = false;
        self
    }
    /// Per-attempt timeout override.
    pub fn with_timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }
}

/// Response of [`HttpClient::fetch`]. Non-2xx statuses are returned, not errors; use
/// [`FetchResponse::error_for_status`] to convert.
#[derive(Debug, Clone)]
pub struct FetchResponse {
    /// Final URL after redirects.
    pub url: Url,
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub truncated: bool,
    /// Every response seen, in order (single element when there was no redirect).
    pub chain: Vec<RedirectHop>,
}

impl FetchResponse {
    /// 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
    /// Header as str.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    /// Body as (lossy) UTF-8.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    /// Deserialize the body as JSON.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body)
            .map_err(|e| CrawlError::Decode(format!("{}: {e}", self.url)))
    }
    /// Parsed `Retry-After`.
    pub fn retry_after(&self) -> Option<Duration> {
        self.header("retry-after").and_then(parse_retry_after)
    }
    /// Err(`CrawlError::Http`) unless 2xx.
    pub fn error_for_status(self) -> Result<Self> {
        if self.is_success() {
            Ok(self)
        } else {
            Err(CrawlError::Http {
                status: self.status,
                url: self.url.to_string(),
                retry_after: self.retry_after(),
            })
        }
    }
}

/// `Retry-After`: delta-seconds or HTTP-date.
pub fn parse_retry_after(v: &str) -> Option<Duration> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(v).ok()?;
    Some(
        at.duration_since(std::time::SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

#[derive(Debug)]
struct Gate {
    next_at: Instant,
    paused_until: Instant,
    delay: Duration,
}

#[derive(Debug)]
struct HostState {
    sem: Arc<Semaphore>,
    gate: tokio::sync::Mutex<Gate>,
}

/// Shared, cheaply clonable HTTP client.
#[derive(Debug, Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
    cfg: Arc<HttpConfig>,
    policy: Arc<SsrfPolicy>,
    hosts: Arc<Mutex<HashMap<String, Arc<HostState>>>>,
}

impl HttpClient {
    /// Start a builder with defaults.
    pub fn builder() -> HttpClientBuilder {
        HttpClientBuilder::default()
    }

    /// Build from the `[crawl]` config section plus extra allowlisted site hosts.
    pub fn from_crawl_cfg<I, S>(c: &sf_core::config::CrawlCfg, site_hosts: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut cfg = HttpConfig::from_crawl_cfg(c);
        cfg.site_hosts
            .extend(site_hosts.into_iter().map(Into::into));
        Self::from_http_config(cfg)
    }

    /// Build from a full config.
    pub fn from_http_config(mut cfg: HttpConfig) -> Result<Self> {
        cfg.per_host_concurrency = cfg.per_host_concurrency.max(1);
        let mut allow: Vec<String> = cfg.allow_hosts.clone();
        if cfg.private_site_hosts {
            allow.extend(cfg.site_hosts.iter().cloned());
        }
        let policy = Arc::new(SsrfPolicy::new(cfg.allow_private, allow));
        let aliases = effective_aliases(&cfg);
        let resolver = Arc::new(GuardedResolver::new(policy.clone(), &aliases));
        let inner = reqwest::Client::builder()
            .use_rustls_tls()
            .user_agent(cfg.user_agent.clone())
            .timeout(cfg.timeout)
            .connect_timeout(cfg.connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .gzip(true)
            .brotli(true)
            .pool_max_idle_per_host(cfg.per_host_concurrency)
            .dns_resolver(resolver)
            .build()
            .map_err(|e| CrawlError::Config(format!("http client: {e}")))?;
        Ok(Self {
            inner,
            cfg: Arc::new(cfg),
            policy,
            hosts: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// The effective configuration.
    pub fn config(&self) -> &HttpConfig {
        &self.cfg
    }

    /// The SSRF policy in force.
    pub fn policy(&self) -> &SsrfPolicy {
        &self.policy
    }

    /// Convenience: GET with defaults, error on non-2xx.
    pub async fn get(&self, url: &Url) -> Result<FetchResponse> {
        self.fetch(FetchRequest::get(url.clone()))
            .await?
            .error_for_status()
    }

    /// Execute a request: SSRF check per hop, manual redirects, host throttle, 429/503 retries,
    /// capped body read. Non-2xx statuses are returned as responses.
    pub async fn fetch(&self, req: FetchRequest) -> Result<FetchResponse> {
        let start_url = req.url.clone();
        let mut current = req.url.clone();
        let mut method = req.method.clone();
        let mut body = req.body.clone();
        let mut auth = req.basic_auth.clone();
        let mut chain: Vec<RedirectHop> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(redirect_key(&current));

        loop {
            self.policy.check_url_static(&current)?;
            let (resp, permit) = self
                .send_with_retry(&req, &method, &current, body.as_ref(), auth.as_ref())
                .await?;
            let status = resp.status().as_u16();
            chain.push(RedirectHop {
                url: current.to_string(),
                status,
                order: chain.len(),
            });

            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            if req.follow_redirects
                && matches!(status, 301 | 302 | 303 | 307 | 308)
                && let Some(loc) = location.filter(|l| !l.trim().is_empty())
            {
                drop(resp);
                drop(permit);
                if chain.len() > self.cfg.max_redirects {
                    return Err(CrawlError::TooManyRedirects {
                        url: start_url.to_string(),
                        hops: chain.len(),
                    });
                }
                let next = current
                    .join(loc.trim())
                    .map_err(|e| CrawlError::InvalidUrl(format!("Location {loc}: {e}")))?;
                if !seen.insert(redirect_key(&next)) {
                    return Err(CrawlError::RedirectLoop(next.to_string()));
                }
                if status == 303 || (matches!(status, 301 | 302) && method == Method::POST) {
                    method = Method::GET;
                    body = None;
                }
                if next.origin() != current.origin() {
                    auth = None;
                }
                current = next;
                continue;
            }

            let headers = resp.headers().clone();
            let cap = req.max_body_bytes.unwrap_or(self.cfg.max_body_bytes);
            let (bytes, truncated) = if req.discard_body || method == Method::HEAD {
                (Bytes::new(), false)
            } else {
                read_capped(resp, cap, req.truncate_body, &current).await?
            };
            drop(permit);
            return Ok(FetchResponse {
                url: current,
                status,
                headers,
                body: bytes,
                truncated,
                chain,
            });
        }
    }

    async fn send_with_retry(
        &self,
        req: &FetchRequest,
        method: &Method,
        url: &Url,
        body: Option<&Bytes>,
        auth: Option<&(String, String)>,
    ) -> Result<(reqwest::Response, OwnedSemaphorePermit)> {
        let host = self.host_state(url);
        let mut attempt: u32 = 0;
        loop {
            let permit = self.acquire(&host).await?;
            let mut rb = self
                .inner
                .request(method.clone(), url.clone())
                .headers(req.headers.clone());
            if let Some(t) = req.timeout {
                rb = rb.timeout(t);
            }
            if let Some(b) = body {
                rb = rb.body(b.clone());
            }
            if let Some((u, p)) = auth {
                rb = rb.basic_auth(u, Some(p));
            }
            let resp = rb.send().await.map_err(|e| map_reqwest(e, url))?;
            let status = resp.status().as_u16();
            if matches!(status, 429 | 503) {
                let ra = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_retry_after);
                let backoff = self
                    .cfg
                    .base_backoff
                    .saturating_mul(2u32.saturating_pow(attempt));
                let wait = ra.unwrap_or(backoff).min(self.cfg.max_backoff);
                self.penalize(&host, wait).await;
                if req.retry && attempt < self.cfg.max_retries {
                    tracing::debug!(%url, status, ?wait, attempt, "throttled; backing off");
                    drop(resp);
                    drop(permit);
                    attempt += 1;
                    continue;
                }
            } else if (200..400).contains(&status) {
                self.relax(&host).await;
            }
            return Ok((resp, permit));
        }
    }

    fn host_state(&self, url: &Url) -> Arc<HostState> {
        let key = format!(
            "{}:{}",
            url.host_str().unwrap_or_default().to_ascii_lowercase(),
            url.port_or_known_default().unwrap_or(0)
        );
        let mut map = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(key)
            .or_insert_with(|| {
                let now = Instant::now();
                Arc::new(HostState {
                    sem: Arc::new(Semaphore::new(self.cfg.per_host_concurrency)),
                    gate: tokio::sync::Mutex::new(Gate {
                        next_at: now,
                        paused_until: now,
                        delay: self.cfg.politeness_delay,
                    }),
                })
            })
            .clone()
    }

    async fn acquire(&self, host: &HostState) -> Result<OwnedSemaphorePermit> {
        let permit = host
            .sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| CrawlError::Other(format!("host limiter closed: {e}")))?;
        let wait_until = {
            let mut g = host.gate.lock().await;
            let now = Instant::now();
            let at = g.next_at.max(g.paused_until).max(now);
            g.next_at = at + g.delay;
            at
        };
        tokio::time::sleep_until(wait_until).await;
        Ok(permit)
    }

    /// Pause the host for `wait` and widen its politeness delay (adaptive backoff).
    async fn penalize(&self, host: &HostState, wait: Duration) {
        let mut g = host.gate.lock().await;
        let until = Instant::now() + wait;
        if until > g.paused_until {
            g.paused_until = until;
        }
        let widened = g.delay.saturating_mul(2).max(Duration::from_millis(250));
        g.delay = widened.min(Duration::from_secs(10));
    }

    /// Decay the politeness delay back towards the configured base after a success.
    async fn relax(&self, host: &HostState) {
        let base = self.cfg.politeness_delay;
        let mut g = host.gate.lock().await;
        if g.delay > base {
            g.delay = (g.delay / 2).max(base);
        }
    }
}

/// Loop-detection key: fragment dropped, host lowercased, default port removed (url does both).
fn redirect_key(u: &Url) -> String {
    let mut u = u.clone();
    u.set_fragment(None);
    u.to_string()
}

async fn read_capped(
    mut resp: reqwest::Response,
    cap: usize,
    truncate: bool,
    url: &Url,
) -> Result<(Bytes, bool)> {
    if !truncate
        && let Some(len) = resp.content_length()
        && len > cap as u64
    {
        return Err(CrawlError::BodyTooLarge {
            url: url.to_string(),
            limit: cap,
        });
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| map_reqwest(e, url))? {
        let room = cap.saturating_sub(buf.len());
        if chunk.len() > room {
            if truncate {
                buf.extend_from_slice(chunk.get(..room).unwrap_or_default());
                return Ok((Bytes::from(buf), true));
            }
            return Err(CrawlError::BodyTooLarge {
                url: url.to_string(),
                limit: cap,
            });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok((Bytes::from(buf), false))
}

/// Flatten an error and its sources into one string.
fn chain_message(e: &(dyn std::error::Error + 'static)) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    let mut depth = 0;
    while let Some(c) = cur {
        let m = c.to_string();
        if !s.contains(&m) {
            s.push_str(": ");
            s.push_str(&m);
        }
        cur = c.source();
        depth += 1;
        if depth > 16 {
            break;
        }
    }
    s
}

fn map_reqwest(e: reqwest::Error, url: &Url) -> CrawlError {
    // Resolver failures (SSRF block / DNS) arrive wrapped in a connect error.
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(&e);
    let mut depth = 0;
    while let Some(c) = cur {
        if let Some(rf) = c.downcast_ref::<ResolveFailure>() {
            return resolve_to_error(rf, url);
        }
        cur = c.source();
        depth += 1;
        if depth > 16 {
            break;
        }
    }
    let msg = chain_message(&e);
    if msg.contains("sf-ssrf-blocked") {
        return CrawlError::Blocked {
            url: url.to_string(),
            reason: msg,
        };
    }
    if msg.contains("sf-dns-failed") {
        return CrawlError::Dns {
            host: url.host_str().unwrap_or_default().to_string(),
            message: msg,
        };
    }
    if e.is_timeout() {
        CrawlError::Timeout(format!("{url}: {msg}"))
    } else if e.is_connect() || e.is_body() || e.is_request() {
        CrawlError::Connect(format!("{url}: {msg}"))
    } else if e.is_decode() {
        CrawlError::Decode(format!("{url}: {msg}"))
    } else {
        CrawlError::Other(format!("{url}: {msg}"))
    }
}

fn resolve_to_error(rf: &ResolveFailure, url: &Url) -> CrawlError {
    if rf.blocked {
        CrawlError::Blocked {
            url: url.to_string(),
            reason: rf.message.clone(),
        }
    } else {
        CrawlError::Dns {
            host: rf.host.clone(),
            message: rf.message.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_forms() {
        assert_eq!(parse_retry_after("3"), Some(Duration::from_secs(3)));
        assert_eq!(parse_retry_after(" "), None);
        assert_eq!(parse_retry_after("soon"), None);
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn error_retry_classes() {
        let http = |s| CrawlError::Http {
            status: s,
            url: String::new(),
            retry_after: None,
        };
        assert!(http(500).is_retryable());
        assert!(http(429).is_retryable());
        assert!(!http(404).is_retryable());
        assert!(!http(401).is_retryable());
        assert!(http(403).is_auth());
        assert!(CrawlError::Timeout(String::new()).is_retryable());
        assert!(!CrawlError::Decode(String::new()).is_retryable());
    }
}
