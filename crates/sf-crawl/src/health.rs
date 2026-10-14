//! Link-health checker (spec 16 / v1 `health/*`).
//!
//! * [`trace`]: hop-by-hop redirect tracer. SSRF policy on every hop (via the client), HEAD
//!   first with GET fallback, chain capture `(url, status, order)`, loop / hop-cap / deadline
//!   detection, temp-redirect flag, failure taxonomy of v1 migration 011.
//! * [`check_links`]: dedupes targets by normalized URL, checks each once with bounded
//!   concurrency + a sliding-window rate cap that halves on 429/503 or timeout pressure,
//!   re-checks timeouts once after the queue drains, then fans results back to one row per
//!   `(source_post_id, href, anchor)` shaped like the `link_health` table.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use futures::StreamExt;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use url::Url;

use crate::client::{FetchRequest, HttpClient, RedirectHop};
use crate::error::CrawlError;
use crate::html::{ExtractedLink, normalize_target_url};

/// Why a check did not come back clean (v1 migration 011, exact strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    None,
    NotFound,
    Gone,
    Loop,
    HopCap,
    ServerError,
    Blocked,
    RateLimited,
    Timeout,
    Dns,
    Connection,
    Other,
}

impl FailureKind {
    /// DB string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::NotFound => "not_found",
            Self::Gone => "gone",
            Self::Loop => "loop",
            Self::HopCap => "hop_cap",
            Self::ServerError => "server_error",
            Self::Blocked => "blocked",
            Self::RateLimited => "rate_limited",
            Self::Timeout => "timeout",
            Self::Dns => "dns",
            Self::Connection => "connection",
            Self::Other => "other",
        }
    }

    /// Dead link: not_found, gone, dns, loop.
    pub fn is_broken(self) -> bool {
        matches!(self, Self::NotFound | Self::Gone | Self::Dns | Self::Loop)
    }

    /// Inconclusive ("couldn't check"): timeout, rate_limited, connection, blocked, hop_cap.
    pub fn is_unverified(self) -> bool {
        matches!(
            self,
            Self::Timeout | Self::RateLimited | Self::Connection | Self::Blocked | Self::HopCap
        )
    }

    /// Classify a terminal HTTP status.
    pub fn from_status(status: u16) -> Self {
        match status {
            404 => Self::NotFound,
            410 => Self::Gone,
            401 | 403 => Self::Blocked,
            429 => Self::RateLimited,
            s if s >= 500 => Self::ServerError,
            s if s >= 400 => Self::Other,
            _ => Self::None,
        }
    }

    /// Classify a transport-level error. An SSRF policy block is our own policy, not a dead
    /// link, so it maps to `other` (v1 semantics).
    pub fn from_error(e: &CrawlError) -> Self {
        match e {
            CrawlError::Timeout(_) => Self::Timeout,
            CrawlError::Dns { .. } => Self::Dns,
            CrawlError::Connect(_) => Self::Connection,
            CrawlError::TooManyRedirects { .. } => Self::HopCap,
            CrawlError::RedirectLoop(_) => Self::Loop,
            CrawlError::Http { status, .. } => Self::from_status(*status),
            _ => Self::Other,
        }
    }
}

const GET_FALLBACK: [u16; 11] = [400, 401, 403, 405, 406, 409, 429, 500, 501, 502, 503];

/// Tracer options.
#[derive(Debug, Clone)]
pub struct TraceOptions {
    pub max_hops: usize,
    /// Whole-chain budget.
    pub deadline: Duration,
    /// GET from the first hop (internal targets; enables body capture).
    pub prefer_get: bool,
    /// Keep a final 200 text/html body (breadcrumb detection).
    pub capture_body: bool,
    pub max_body_bytes: usize,
}

impl Default for TraceOptions {
    fn default() -> Self {
        Self {
            max_hops: 10,
            deadline: Duration::from_secs(60),
            prefer_get: false,
            capture_body: false,
            max_body_bytes: 2_000_000,
        }
    }
}

/// Outcome of tracing one URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceResult {
    pub requested_url: String,
    pub resolved_url: Option<String>,
    pub status_code: Option<u16>,
    pub ok: bool,
    pub is_broken: bool,
    pub failure_kind: FailureKind,
    pub is_unverified: bool,
    pub chain: Vec<RedirectHop>,
    pub hops: usize,
    pub has_loop: bool,
    pub has_temp_redirect: bool,
    pub error: Option<String>,
    #[serde(skip)]
    pub body: Option<String>,
    pub content_type: Option<String>,
    #[serde(skip)]
    pub retry_after: Option<Duration>,
}

impl TraceResult {
    fn new(url: &str) -> Self {
        Self {
            requested_url: url.to_string(),
            resolved_url: None,
            status_code: None,
            ok: false,
            is_broken: false,
            failure_kind: FailureKind::None,
            is_unverified: false,
            chain: Vec::new(),
            hops: 0,
            has_loop: false,
            has_temp_redirect: false,
            error: None,
            body: None,
            content_type: None,
            retry_after: None,
        }
    }

    fn fail(mut self, msg: String, kind: FailureKind) -> Self {
        self.error = Some(msg);
        self.failure_kind = kind;
        self.is_broken = kind.is_broken();
        self.is_unverified = kind.is_unverified();
        self.ok = false;
        self.hops = self.chain.len().saturating_sub(1);
        self
    }
}

/// Trace `url` hop by hop. Never returns an error: every problem is a classified result.
pub async fn trace(client: &HttpClient, url: &str, opts: &TraceOptions) -> TraceResult {
    let mut res = TraceResult::new(url);
    let started = Instant::now();
    let mut seen: HashSet<String> = HashSet::new();
    let mut current = url.to_string();
    let mut order: usize = 0;
    loop {
        if order > opts.max_hops {
            let msg = format!("redirect chain exceeded {} hops", opts.max_hops);
            return res.fail(msg, FailureKind::HopCap);
        }
        let remaining = opts.deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            let msg = format!("redirect chain exceeded {:?} deadline", opts.deadline);
            return res.fail(msg, FailureKind::Timeout);
        }
        let Some(key) = normalize_target_url(&current) else {
            return res.fail(
                format!("unsupported or malformed url: {current}"),
                FailureKind::Other,
            );
        };
        if !seen.insert(key) {
            res.has_loop = true;
            return res.fail(format!("redirect loop at {current}"), FailureKind::Loop);
        }
        let Ok(u) = Url::parse(&current) else {
            return res.fail(format!("malformed url: {current}"), FailureKind::Other);
        };
        let timeout = remaining.min(client.config().timeout);
        let first = if opts.prefer_get {
            Method::GET
        } else {
            Method::HEAD
        };
        let mut resp = match hop(client, &u, first.clone(), timeout, opts).await {
            Ok(r) => r,
            Err(e) => {
                let kind = FailureKind::from_error(&e);
                return res.fail(e.to_string(), kind);
            }
        };
        if first == Method::HEAD && GET_FALLBACK.contains(&resp.status) {
            let remaining = opts.deadline.saturating_sub(started.elapsed());
            let timeout = remaining
                .min(client.config().timeout)
                .max(Duration::from_millis(1));
            resp = match hop(client, &u, Method::GET, timeout, opts).await {
                Ok(r) => r,
                Err(e) => {
                    let kind = FailureKind::from_error(&e);
                    return res.fail(e.to_string(), kind);
                }
            };
        }
        res.chain.push(RedirectHop {
            url: current.clone(),
            status: resp.status,
            order,
        });
        res.status_code = Some(resp.status);
        res.resolved_url = Some(current.clone());
        if matches!(resp.status, 302 | 307) {
            res.has_temp_redirect = true;
        }
        let location = resp.header("location").map(str::trim).unwrap_or_default();
        if matches!(resp.status, 301 | 302 | 303 | 307 | 308) && !location.is_empty() {
            match u.join(location) {
                Ok(next) => {
                    current = next.to_string();
                    order += 1;
                    continue;
                }
                Err(_) => {
                    let msg = format!("unparseable Location header: {location}");
                    return res.fail(msg, FailureKind::Other);
                }
            }
        }
        res.content_type = resp.header("content-type").map(str::to_string);
        res.retry_after = resp.retry_after();
        res.ok = (200..400).contains(&resp.status);
        res.failure_kind = FailureKind::from_status(resp.status);
        res.is_broken = res.failure_kind.is_broken();
        res.is_unverified = res.failure_kind.is_unverified();
        res.hops = res.chain.len().saturating_sub(1);
        let is_html = res
            .content_type
            .as_deref()
            .is_some_and(|c| c.to_ascii_lowercase().contains("text/html"));
        if opts.capture_body && resp.status == 200 && is_html {
            res.body = Some(resp.text());
        }
        return res;
    }
}

async fn hop(
    client: &HttpClient,
    url: &Url,
    method: Method,
    timeout: Duration,
    opts: &TraceOptions,
) -> Result<crate::client::FetchResponse, CrawlError> {
    let mut req = FetchRequest::new(method.clone(), url.clone())
        .no_redirects()
        // 429/503 are recorded, not waited out here: the client still pauses the host per
        // Retry-After and check_links halves its rate, so one URL cannot stall the run.
        .no_retry()
        .with_timeout(timeout.max(Duration::from_millis(1)))
        .max_body(opts.max_body_bytes)
        .truncate()
        .header("accept", "text/html,application/xhtml+xml,*/*;q=0.8")?;
    req.discard_body = method == Method::HEAD || !opts.capture_body;
    client.fetch(req).await
}

/// Sliding-window request pacer whose cap can be halved mid-run.
#[derive(Debug)]
struct Throttle {
    cap: usize,
    interval: Duration,
    starts: VecDeque<Instant>,
    paused_until: Option<Instant>,
}

impl Throttle {
    fn penalize(&mut self) -> bool {
        let next = (self.cap / 2).max(1);
        let changed = next != self.cap;
        self.cap = next;
        changed
    }
    fn backoff(&mut self, d: Duration) {
        let until = Instant::now() + d;
        if self.paused_until.is_none_or(|p| until > p) {
            self.paused_until = Some(until);
        }
    }
    /// Reserve a slot, or say how long to wait.
    fn try_acquire(&mut self) -> Option<Duration> {
        let now = Instant::now();
        if let Some(p) = self.paused_until
            && now < p
        {
            return Some(p - now);
        }
        if self.interval.is_zero() {
            return None;
        }
        while self
            .starts
            .front()
            .is_some_and(|s| now.duration_since(*s) >= self.interval)
        {
            self.starts.pop_front();
        }
        if self.starts.len() < self.cap {
            self.starts.push_back(now);
            return None;
        }
        let oldest = self.starts.front().copied().unwrap_or(now);
        Some(self.interval.saturating_sub(now.duration_since(oldest)) + Duration::from_millis(1))
    }
}

/// Sustained-timeout detector: 3-streak, or >10% of the last 50 with >= 10 samples.
#[derive(Debug, Default)]
struct TimeoutPressure {
    streak: usize,
    window: VecDeque<bool>,
}

impl TimeoutPressure {
    fn record(&mut self, is_timeout: bool) -> bool {
        self.streak = if is_timeout { self.streak + 1 } else { 0 };
        self.window.push_back(is_timeout);
        if self.window.len() > 50 {
            self.window.pop_front();
        }
        let n = self.window.len();
        let hits = self.window.iter().filter(|t| **t).count();
        let trigger = self.streak >= 3 || (n >= 10 && hits * 10 > n);
        if trigger {
            self.streak = 0;
            self.window.clear();
        }
        trigger
    }
}

/// Options for [`check_links`] (v1 defaults).
#[derive(Debug, Clone)]
pub struct LinkCheckOptions {
    pub concurrency: usize,
    /// Requests started per `interval` across the run (halved on backpressure, floor 1).
    pub interval_cap: usize,
    pub interval: Duration,
    /// Stop checking new distinct URLs past this count (extra links become unchecked rows).
    pub max_urls: usize,
    /// Only check internal links (external rows are still emitted, unchecked).
    pub internal_only: bool,
    /// Re-check timed-out URLs once, sequentially, after the queue drains.
    pub retry_timeouts: bool,
    pub trace: TraceOptions,
}

impl Default for LinkCheckOptions {
    fn default() -> Self {
        Self {
            concurrency: 2,
            interval_cap: 4,
            interval: Duration::from_secs(1),
            max_urls: 20_000,
            internal_only: false,
            retry_timeouts: true,
            trace: TraceOptions::default(),
        }
    }
}

/// A link to check, with the post it appears in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkCheckInput {
    pub source_post_id: Option<String>,
    pub link: ExtractedLink,
}

/// One `link_health` row (minus `id`, `site_id`, `target_post_id`, `checked_at`, which the
/// caller owns). `redirect_chain` serializes to the JSON stored in the column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkHealthRow {
    pub source_post_id: Option<String>,
    pub href: String,
    pub requested_url: String,
    pub resolved_url: Option<String>,
    pub scope: String,
    pub status_code: Option<u16>,
    pub ok: bool,
    pub is_broken: bool,
    pub redirect_hops: usize,
    pub redirect_chain: Vec<RedirectHop>,
    pub has_redirect_loop: bool,
    pub has_temp_redirect: bool,
    pub rel: String,
    pub is_nofollow: bool,
    pub is_sponsored: bool,
    pub is_ugc: bool,
    pub anchor: String,
    pub placement: String,
    pub error: Option<String>,
    pub failure_kind: FailureKind,
    pub is_unverified: bool,
    /// Not a column: true when the target was not checked (internal_only / max_urls).
    pub skipped: bool,
}

impl LinkHealthRow {
    /// `redirect_chain` column value.
    pub fn redirect_chain_json(&self) -> String {
        serde_json::to_string(&self.redirect_chain).unwrap_or_else(|_| "[]".into())
    }
}

/// Run statistics (v1 `LinkHealthRunStats`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkHealthStats {
    pub checked: usize,
    pub distinct_urls: usize,
    pub broken: usize,
    pub unverified: usize,
    pub blocked: usize,
    pub server_error: usize,
    pub timeouts: usize,
    pub timeouts_recovered: usize,
    pub nofollow: usize,
    pub redirected: usize,
    pub chains_over_two: usize,
    pub loops: usize,
    pub temp_redirects: usize,
    pub skipped: usize,
}

/// Rows + stats + per-URL results (keyed by normalized URL; bodies dropped unless captured).
#[derive(Debug, Clone, Default)]
pub struct LinkHealthReport {
    pub rows: Vec<LinkHealthRow>,
    pub stats: LinkHealthStats,
    pub results: HashMap<String, TraceResult>,
}

/// Check every link once per distinct target and return `link_health` rows.
/// `on_progress(checked, total)` is called after each distinct URL.
pub async fn check_links(
    client: &HttpClient,
    inputs: &[LinkCheckInput],
    opts: &LinkCheckOptions,
    on_progress: Option<&(dyn Fn(usize, usize) + Sync)>,
) -> LinkHealthReport {
    // 1. distinct targets (normalized URL -> (url, internal)), insertion-ordered for determinism
    let mut order: Vec<String> = Vec::new();
    let mut targets: HashMap<String, (String, bool)> = HashMap::new();
    let mut keys: Vec<Option<String>> = Vec::with_capacity(inputs.len());
    for inp in inputs {
        let internal = inp.link.scope == "internal";
        if opts.internal_only && !internal {
            keys.push(None);
            continue;
        }
        let Some(key) = normalize_target_url(&inp.link.requested_url) else {
            keys.push(None);
            continue;
        };
        if let Some(t) = targets.get_mut(&key) {
            t.1 |= internal;
            keys.push(Some(key));
            continue;
        }
        if targets.len() >= opts.max_urls {
            keys.push(None);
            continue;
        }
        targets.insert(key.clone(), (inp.link.requested_url.clone(), internal));
        order.push(key.clone());
        keys.push(Some(key));
    }

    // 2. check each once
    let throttle = tokio::sync::Mutex::new(Throttle {
        cap: opts.interval_cap.max(1),
        interval: opts.interval,
        starts: VecDeque::new(),
        paused_until: None,
    });
    let pressure = Mutex::new(TimeoutPressure::default());
    let total = order.len();
    let done = std::sync::atomic::AtomicUsize::new(0);

    let check = |url: String, internal: bool| {
        let throttle = &throttle;
        let pressure = &pressure;
        async move {
            loop {
                let wait = throttle.lock().await.try_acquire();
                match wait {
                    Some(d) => tokio::time::sleep(d).await,
                    None => break,
                }
            }
            let mut topts = opts.trace.clone();
            topts.prefer_get |= internal && topts.capture_body;
            let r = trace(client, &url, &topts).await;
            let is_timeout = r.failure_kind == FailureKind::Timeout;
            let pressured = pressure
                .lock()
                .map(|mut p| p.record(is_timeout))
                .unwrap_or(false);
            let mut t = throttle.lock().await;
            if pressured && t.penalize() {
                tracing::info!(cap = t.cap, "link health: sustained timeouts, slowing down");
            }
            if r.status_code.is_some_and(|s| s == 429 || s == 503) && t.penalize() {
                tracing::info!(cap = t.cap, "link health: throttled, slowing down");
            }
            if let Some(ra) = r.retry_after.filter(|d| !d.is_zero()) {
                t.backoff(ra.min(Duration::from_secs(120)));
            }
            r
        }
    };

    let mut results: HashMap<String, TraceResult> = HashMap::with_capacity(total);
    {
        let mut stream = futures::stream::iter(order.iter().cloned())
            .map(|key| {
                let (url, internal) = targets.get(&key).cloned().unwrap_or_default();
                let fut = check(url, internal);
                async move { (key, fut.await) }
            })
            .buffer_unordered(opts.concurrency.max(1));
        while let Some((key, r)) = stream.next().await {
            results.insert(key, r);
            let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if let Some(cb) = on_progress {
                cb(n, total);
            }
        }
    }

    // 3. post-drain retry of timeouts
    let mut stats = LinkHealthStats {
        distinct_urls: total,
        ..Default::default()
    };
    if opts.retry_timeouts {
        let timed_out: Vec<String> = order
            .iter()
            .filter(|k| {
                results
                    .get(*k)
                    .is_some_and(|r| r.failure_kind == FailureKind::Timeout)
            })
            .cloned()
            .collect();
        for key in timed_out {
            let (url, internal) = targets.get(&key).cloned().unwrap_or_default();
            let r = check(url, internal).await;
            if r.failure_kind != FailureKind::Timeout {
                stats.timeouts_recovered += 1;
            }
            results.insert(key, r);
        }
    }

    // 4. rows, deduped on (post, href, anchor)
    let mut seen: HashSet<(Option<String>, String, String)> = HashSet::new();
    let mut rows = Vec::new();
    for (inp, key) in inputs.iter().zip(keys.iter()) {
        let l = &inp.link;
        if !seen.insert((inp.source_post_id.clone(), l.href.clone(), l.anchor.clone())) {
            continue;
        }
        let res = key.as_ref().and_then(|k| results.get(k));
        stats.checked += 1;
        if l.is_nofollow {
            stats.nofollow += 1;
        }
        if let Some(r) = res {
            stats.broken += usize::from(r.is_broken);
            stats.unverified += usize::from(r.is_unverified);
            stats.blocked += usize::from(r.failure_kind == FailureKind::Blocked);
            stats.server_error += usize::from(r.failure_kind == FailureKind::ServerError);
            stats.timeouts += usize::from(r.failure_kind == FailureKind::Timeout);
            stats.redirected += usize::from(r.hops > 0);
            stats.chains_over_two += usize::from(r.hops > 2);
            stats.loops += usize::from(r.has_loop);
            stats.temp_redirects += usize::from(r.has_temp_redirect);
        } else {
            stats.skipped += 1;
        }
        rows.push(LinkHealthRow {
            source_post_id: inp.source_post_id.clone(),
            href: l.href.clone(),
            requested_url: l.requested_url.clone(),
            resolved_url: res.and_then(|r| r.resolved_url.clone()),
            scope: l.scope.clone(),
            status_code: res.and_then(|r| r.status_code),
            ok: res.is_some_and(|r| r.ok),
            is_broken: res.is_some_and(|r| r.is_broken),
            redirect_hops: res.map_or(0, |r| r.hops),
            redirect_chain: res.map(|r| r.chain.clone()).unwrap_or_default(),
            has_redirect_loop: res.is_some_and(|r| r.has_loop),
            has_temp_redirect: res.is_some_and(|r| r.has_temp_redirect),
            rel: l.rel.clone(),
            is_nofollow: l.is_nofollow,
            is_sponsored: l.is_sponsored,
            is_ugc: l.is_ugc,
            anchor: l.anchor.clone(),
            placement: l.placement.clone(),
            error: res.and_then(|r| r.error.clone()),
            failure_kind: res.map_or(FailureKind::None, |r| r.failure_kind),
            is_unverified: res.is_some_and(|r| r.is_unverified),
            skipped: res.is_none(),
        });
    }
    LinkHealthReport {
        rows,
        stats,
        results,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taxonomy_strings_and_sets() {
        let all = [
            (FailureKind::None, "none"),
            (FailureKind::NotFound, "not_found"),
            (FailureKind::Gone, "gone"),
            (FailureKind::Loop, "loop"),
            (FailureKind::HopCap, "hop_cap"),
            (FailureKind::ServerError, "server_error"),
            (FailureKind::Blocked, "blocked"),
            (FailureKind::RateLimited, "rate_limited"),
            (FailureKind::Timeout, "timeout"),
            (FailureKind::Dns, "dns"),
            (FailureKind::Connection, "connection"),
            (FailureKind::Other, "other"),
        ];
        for (k, s) in all {
            assert_eq!(k.as_str(), s);
            assert_eq!(serde_json::to_string(&k).unwrap(), format!("\"{s}\""));
            assert!(!(k.is_broken() && k.is_unverified()));
        }
        assert_eq!(FailureKind::from_status(404), FailureKind::NotFound);
        assert_eq!(FailureKind::from_status(410), FailureKind::Gone);
        assert_eq!(FailureKind::from_status(403), FailureKind::Blocked);
        assert_eq!(FailureKind::from_status(429), FailureKind::RateLimited);
        assert_eq!(FailureKind::from_status(502), FailureKind::ServerError);
        assert_eq!(FailureKind::from_status(405), FailureKind::Other);
        assert_eq!(FailureKind::from_status(200), FailureKind::None);
        assert!(FailureKind::Dns.is_broken() && FailureKind::Loop.is_broken());
        assert!(FailureKind::HopCap.is_unverified() && FailureKind::Blocked.is_unverified());
        assert!(!FailureKind::ServerError.is_broken() && !FailureKind::ServerError.is_unverified());
    }

    #[test]
    fn pressure_triggers() {
        let mut p = TimeoutPressure::default();
        assert!(!p.record(true));
        assert!(!p.record(true));
        assert!(p.record(true));
        let mut p = TimeoutPressure::default();
        let mut fired = false;
        for i in 0..20 {
            fired |= p.record(i % 5 == 0);
        }
        assert!(fired);
    }
}
