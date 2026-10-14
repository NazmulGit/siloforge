//! Shared job context (`JobContext`), the per-job handle (`Job`), the job error type and the
//! per-site vector cache.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use sf_ai::Ai;
use sf_core::types::{JobKind, blob_to_vec};
use sf_core::{Config, EMBED_DIM, Error, LiveConfig};
use sf_crawl::{CrawlError, HttpClient, HttpConfig, LinkCheckOptions, VerifyOptions};
use sf_embed::EmbedChain;
use sf_engine::nlp::SiteTerms;
use sf_engine::vector::VectorIndex;
use sf_ops::errors::ErrorTracker;
use sf_ops::monitor::Monitor;
use sf_ops::notify::Notifier;
use sf_store::{Db, err};

use crate::queue::{ClaimedJob, JobQueue};

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// A pipeline failure: the error plus whether a later retry could succeed.
#[derive(Debug)]
pub struct JobError {
    pub error: Error,
    pub retryable: bool,
}

pub type JobResult<T> = std::result::Result<T, JobError>;

impl JobError {
    pub fn fatal(e: impl Into<Error>) -> Self {
        Self {
            error: e.into(),
            retryable: false,
        }
    }
    pub fn retry(e: impl Into<Error>) -> Self {
        Self {
            error: e.into(),
            retryable: true,
        }
    }
    pub fn cancelled() -> Self {
        Self::fatal(Error::Cancelled)
    }
    pub fn msg(m: impl std::fmt::Display) -> Self {
        Self::fatal(Error::internal(m))
    }
    pub fn is_cancelled(&self) -> bool {
        matches!(self.error, Error::Cancelled)
    }
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl From<Error> for JobError {
    fn from(e: Error) -> Self {
        Self::fatal(e)
    }
}

impl From<CrawlError> for JobError {
    fn from(e: CrawlError) -> Self {
        let retryable = e.is_retryable();
        let error = match e {
            CrawlError::Cancelled => Error::Cancelled,
            other => Error::Upstream(other.to_string()),
        };
        Self { error, retryable }
    }
}

impl From<sf_engine::pool::ComputeError> for JobError {
    fn from(e: sf_engine::pool::ComputeError) -> Self {
        Self::msg(e)
    }
}

// ---------------------------------------------------------------------------------------------
// JobContext
// ---------------------------------------------------------------------------------------------

/// Everything a pipeline needs. Build once at startup, share as `Arc<JobContext>`.
///
/// ```ignore
/// let mut ctx = JobContext::new(cfg.clone(), db.clone(), queue.clone(), embed, secret_key);
/// ctx.ai = Some(Arc::new(ai));
/// ctx.notifier = Some(notifier.clone());
/// ctx.monitor = Some(monitor.clone());
/// let ctx = Arc::new(ctx);
/// let runner = Runner::start(ctx.clone());
/// let scheduler = Scheduler::start(ctx.clone());
/// ```
pub struct JobContext {
    /// Configuration at boot. Values that may change live are read from [`JobContext::live`].
    pub cfg: Arc<Config>,
    /// Current configuration (v2.1 settings): runner limits, defer thresholds, crawl/WP client
    /// settings, WordPress defaults, vector cache budget and memory cap are read from here.
    pub live: LiveConfig,
    pub db: Db,
    pub queue: JobQueue,
    /// Base HTTP config for crawler/writer clients (site hosts are added per job).
    pub http: HttpConfig,
    pub embed: Arc<EmbedChain>,
    /// LLM access for the rephrase/context insert modules (None = native only).
    pub ai: Option<Arc<Ai>>,
    pub notifier: Option<Notifier>,
    pub errors: ErrorTracker,
    /// Key for `sites.secrets_enc` (`sf_core::crypto::derive_secret_key(cfg.encryption_master())`).
    pub secret_key: Arc<[u8; 32]>,
    /// Machine load source for deferring heavy jobs (None = never defer).
    pub monitor: Option<Monitor>,
    pub vectors: VectorCache,
    /// Per-site term statistics for anchors (IDF + proper nouns), idle-evicted like `vectors`.
    pub terms: TermsCache,
    /// Phrase text -> embedding, for semantic anchor scoring (idle-evicted, bounded).
    pub phrases: PhraseCache,
    /// Live verification after injection.
    pub verify: VerifyOptions,
    /// Link-health checker options.
    pub link_check: LinkCheckOptions,
    /// Unified event log (§9): job started/done/failed/cancelled rows (None = not recorded).
    pub events: Option<sf_ops::eventlog::EventLog>,
}

impl JobContext {
    pub fn new(
        cfg: Arc<Config>,
        db: Db,
        queue: JobQueue,
        embed: Arc<EmbedChain>,
        secret_key: Arc<[u8; 32]>,
    ) -> Self {
        let budget = cfg
            .resources
            .vector_cache_mb
            .max(16)
            .saturating_mul(1024 * 1024);
        let vectors = VectorCache::new(db.clone(), budget);
        let terms = TermsCache::default();
        let phrases = PhraseCache::default();
        spawn_idle_sweeper(&vectors, &terms, &phrases, CACHE_IDLE_TTL);
        Self {
            live: LiveConfig::from_arc(cfg.clone()),
            http: HttpConfig::from_crawl_cfg(&cfg.crawl),
            errors: ErrorTracker::new(db.clone()),
            vectors,
            terms,
            phrases,
            cfg,
            db,
            queue,
            embed,
            ai: None,
            notifier: None,
            secret_key,
            monitor: None,
            verify: VerifyOptions::default(),
            link_check: LinkCheckOptions::default(),
            events: None,
        }
    }

    /// Base HTTP config for new clients: [`JobContext::http`] with the live `[crawl]` values laid
    /// over it once settings were changed at runtime (so tests that tune `http` keep their values).
    pub fn http_config(&self) -> HttpConfig {
        let mut cfg = self.http.clone();
        if self.live.generation() > 0 {
            let live = HttpConfig::from_crawl_cfg(&self.live.get().crawl);
            cfg.user_agent = live.user_agent;
            cfg.timeout = live.timeout;
            cfg.per_host_concurrency = live.per_host_concurrency;
            cfg.private_site_hosts = live.private_site_hosts;
            cfg.host_aliases = live.host_aliases;
        }
        cfg
    }

    /// Like [`JobContext::http_config`] with the `[wordpress] request_timeout_ms` timeout, for
    /// WordPress REST writers.
    pub fn wp_http_config(&self) -> HttpConfig {
        let mut cfg = self.http_config();
        if self.live.generation() > 0 {
            let ms = self.live.get().wordpress.request_timeout_ms;
            if ms > 0 {
                cfg.timeout = Duration::from_millis(ms.clamp(1_000, 600_000));
            }
        }
        cfg
    }

    /// Applies the live resource settings: vector cache budget (`vector_cache_mb`, quartered
    /// while the monitor reports memory pressure, which also drops idle caches at once) and the
    /// live compute-thread limit (`cpu_threads`). Run every 5 s by [`JobContext::start_resource_watch`].
    pub fn apply_resources(&self) {
        let cfg = self.live.get();
        let mut budget = cfg
            .resources
            .vector_cache_mb
            .max(16)
            .saturating_mul(1024 * 1024);
        let pressure = self.monitor.as_ref().is_some_and(Monitor::memory_pressure);
        if pressure {
            budget /= 4;
        }
        let before = self.vectors.cached_bytes();
        self.vectors.set_budget(budget);
        if pressure {
            let short = Duration::from_secs(30);
            let n = self.vectors.evict_idle(short) + self.terms.evict_idle(short);
            let np = evict_idle_phrases(&self.phrases.inner, short);
            let after = self.vectors.cached_bytes();
            if n + np > 0 || after < before {
                tracing::info!(
                    entries = n,
                    phrases = np,
                    freed_mb = before.saturating_sub(after) / (1024 * 1024),
                    "memory cap: caches shrunk"
                );
            }
        }
        if self.live.generation() > 0 {
            sf_engine::pool::set_live_limit(cfg.resources.cpu_threads);
        }
    }

    /// Spawns the loop that calls [`JobContext::apply_resources`] every 5 s (ends when the
    /// context is dropped).
    pub fn start_resource_watch(self: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak = Arc::downgrade(self);
        handle.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let Some(ctx) = weak.upgrade() else { break };
                ctx.apply_resources();
            }
        });
    }

    /// HTTP client allowed to reach `hosts` (site hosts / URLs) even when they resolve privately.
    pub fn http_client(&self, hosts: &[String]) -> JobResult<HttpClient> {
        let mut cfg = self.http_config();
        cfg.allow_hosts.extend(hosts.iter().cloned());
        HttpClient::from_http_config(cfg).map_err(JobError::from)
    }
}

// ---------------------------------------------------------------------------------------------
// Job handle
// ---------------------------------------------------------------------------------------------

/// One running job as seen by its pipeline.
pub struct Job {
    pub id: String,
    pub kind: JobKind,
    pub site_id: Option<String>,
    pub payload: Value,
    pub attempts: i64,
    queue: JobQueue,
    cancel: Arc<AtomicBool>,
    last_db_check: Mutex<Instant>,
    last_progress: Mutex<Option<(String, i64)>>,
}

impl Job {
    pub fn new(claimed: ClaimedJob, queue: JobQueue) -> Self {
        Self {
            id: claimed.id,
            kind: claimed.kind,
            site_id: claimed.site_id,
            payload: claimed.payload,
            attempts: claimed.attempts,
            queue,
            cancel: Arc::new(AtomicBool::new(false)),
            last_db_check: Mutex::new(Instant::now()),
            last_progress: Mutex::new(None),
        }
    }

    /// The job's site id (error when the job has none).
    pub fn site(&self) -> JobResult<&str> {
        self.site_id
            .as_deref()
            .ok_or_else(|| JobError::fatal(Error::bad(format!("{} job needs a site", self.kind))))
    }

    pub fn queue(&self) -> &JobQueue {
        &self.queue
    }

    /// Shared flag the runner sets on cancel / timeout.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    pub fn cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Cooperative cancellation point: returns `Err(cancelled)` once the job was cancelled
    /// (flag from the runner, or `status='cancelled'` in the DB, polled at most once a second).
    pub async fn checkpoint(&self) -> JobResult<()> {
        if self.cancel_requested() {
            return Err(JobError::cancelled());
        }
        let due = {
            let mut last = self.last_db_check.lock();
            if last.elapsed() >= Duration::from_secs(1) {
                *last = Instant::now();
                true
            } else {
                false
            }
        };
        if due && self.queue.is_cancelled(&self.id).await.unwrap_or(false) {
            self.cancel.store(true, Ordering::Relaxed);
            return Err(JobError::cancelled());
        }
        Ok(())
    }

    /// `stats.progress = {phase, pct}` (written only when phase or integer pct changes).
    pub async fn progress(&self, phase: &str, pct: f64) {
        let pct = if pct.is_finite() {
            pct.clamp(0.0, 100.0).round() as i64
        } else {
            0
        };
        {
            let mut last = self.last_progress.lock();
            if last.as_ref().is_some_and(|(p, v)| p == phase && *v == pct) {
                return;
            }
            *last = Some((phase.to_string(), pct));
        }
        self.merge_stats(json!({ "progress": { "phase": phase, "pct": pct } }))
            .await;
    }

    /// Shallow-merge into `stats` (best effort; failures are logged, never fatal).
    pub async fn merge_stats(&self, partial: Value) {
        if let Err(e) = self.queue.merge_stats(&self.id, partial).await {
            tracing::warn!(job = %self.id, error = %e, "merge_stats failed");
        }
    }

    /// A `job_logs` line (best effort).
    pub async fn log(&self, level: &str, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::debug!(job = %self.id, kind = %self.kind, "{msg}");
        if let Err(e) = self.queue.log(&self.id, level, msg, None).await {
            tracing::warn!(job = %self.id, error = %e, "job log failed");
        }
    }

    pub async fn info(&self, msg: impl Into<String>) {
        self.log("info", msg).await;
    }

    pub async fn warn(&self, msg: impl Into<String>) {
        self.log("warn", msg).await;
    }

    /// String field of the payload, accepting `snake_case` and `camelCase` spellings.
    pub fn payload_str(&self, snake: &str, camel: &str) -> Option<String> {
        self.payload
            .get(snake)
            .or_else(|| self.payload.get(camel))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    }

    pub fn payload_bool(&self, snake: &str, camel: &str) -> bool {
        self.payload
            .get(snake)
            .or_else(|| self.payload.get(camel))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// String array field (e.g. `edge_ids` / `edgeIds`), capped at `max` entries.
    pub fn payload_ids(&self, snake: &str, camel: &str, max: usize) -> Vec<String> {
        self.payload
            .get(snake)
            .or_else(|| self.payload.get(camel))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .take(max)
                    .collect()
            })
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------------
// Vector cache
// ---------------------------------------------------------------------------------------------

/// All embeddings of one site: post vectors plus paragraph vectors grouped by post.
pub struct SiteVectors {
    /// Post id → normalised post embedding.
    pub posts: VectorIndex,
    /// Paragraph id → normalised paragraph embedding (rows ordered by post id, idx).
    pub paras: VectorIndex,
    /// Per paragraph row: (post id, paragraphs.idx).
    pub para_meta: Vec<(Arc<str>, i64)>,
    /// Post id → paragraph rows (ascending idx).
    pub by_post: HashMap<Arc<str>, Vec<usize>>,
}

impl SiteVectors {
    pub fn memory_bytes(&self) -> usize {
        self.posts
            .memory_bytes()
            .saturating_add(self.paras.memory_bytes())
            .saturating_add(self.para_meta.len().saturating_mul(48))
            .saturating_add(self.by_post.len().saturating_mul(64))
    }

    /// Paragraph rows of a post (empty when it has none embedded).
    pub fn paragraphs_of(&self, post_id: &str) -> &[usize] {
        self.by_post.get(post_id).map(Vec::as_slice).unwrap_or(&[])
    }
}

struct CacheEntry {
    site: String,
    data: Arc<SiteVectors>,
    bytes: usize,
    used: u64,
    last: Instant,
}

/// Caches unused this long are dropped so idle RAM falls back to the baseline (perf open item:
/// the site30 vectors kept about 270 MB resident after a plan).
pub const CACHE_IDLE_TTL: Duration = Duration::from_secs(600);
/// How often the idle sweeper looks.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// Per-site `SiteVectors`, LRU-evicted to stay within `resources.vector_cache_mb`, and dropped
/// after [`CACHE_IDLE_TTL`] without use.
pub struct VectorCache {
    db: Db,
    /// Bytes; live-adjustable ([`VectorCache::set_budget`]).
    budget: AtomicUsize,
    clock: AtomicU64,
    entries: Arc<Mutex<Vec<CacheEntry>>>,
}

impl VectorCache {
    pub fn new(db: Db, budget_bytes: usize) -> Self {
        Self {
            db,
            budget: AtomicUsize::new(budget_bytes),
            clock: AtomicU64::new(0),
            entries: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Drop entries unused for `ttl` (running jobs keep their own `Arc`). Returns how many.
    pub fn evict_idle(&self, ttl: Duration) -> usize {
        evict_idle_vectors(&self.entries, ttl)
    }

    /// Change the byte budget; entries beyond it are evicted least recently used first.
    pub fn set_budget(&self, bytes: usize) {
        self.budget.store(bytes, Ordering::Relaxed);
        let mut entries = self.entries.lock();
        evict_over_budget(&mut entries, bytes, None);
    }

    pub fn budget(&self) -> usize {
        self.budget.load(Ordering::Relaxed)
    }

    /// Drop a site's vectors (call after embeddings change).
    pub fn invalidate(&self, site_id: &str) {
        self.entries.lock().retain(|e| e.site != site_id);
    }

    pub fn cached_bytes(&self) -> usize {
        self.entries.lock().iter().map(|e| e.bytes).sum()
    }

    /// Cached vectors of a site, loading them from the DB on a miss.
    pub async fn get(&self, site_id: &str) -> JobResult<Arc<SiteVectors>> {
        let tick = self.clock.fetch_add(1, Ordering::Relaxed);
        {
            let mut entries = self.entries.lock();
            if let Some(e) = entries.iter_mut().find(|e| e.site == site_id) {
                e.used = tick;
                e.last = Instant::now();
                return Ok(e.data.clone());
            }
        }
        let site = site_id.to_string();
        let data = Arc::new(self.db.read(move |c| load_site_vectors(c, &site)).await?);
        let bytes = data.memory_bytes();
        let mut entries = self.entries.lock();
        entries.retain(|e| e.site != site_id);
        entries.push(CacheEntry {
            site: site_id.to_string(),
            data: data.clone(),
            bytes,
            used: tick,
            last: Instant::now(),
        });
        // Evict least recently used entries (never the one just loaded) until within budget.
        evict_over_budget(&mut entries, self.budget(), Some(site_id));
        Ok(data)
    }
}

/// LRU eviction down to `budget` bytes; `keep` is never evicted, and the last entry stays.
fn evict_over_budget(entries: &mut Vec<CacheEntry>, budget: usize, keep: Option<&str>) {
    loop {
        let total: usize = entries.iter().map(|e| e.bytes).sum();
        if total <= budget || entries.len() <= 1 {
            break;
        }
        let victim = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| keep != Some(e.site.as_str()))
            .min_by_key(|(_, e)| e.used)
            .map(|(i, _)| i);
        match victim {
            Some(i) => {
                entries.remove(i);
            }
            None => break,
        }
    }
}

fn evict_idle_vectors(entries: &Mutex<Vec<CacheEntry>>, ttl: Duration) -> usize {
    let mut e = entries.lock();
    let before = e.len();
    e.retain(|x| x.last.elapsed() < ttl);
    before - e.len()
}

struct TermsEntry {
    fingerprint: String,
    data: Arc<SiteTerms>,
    last: Instant,
}

/// Per-site [`SiteTerms`], keyed by a content fingerprint, idle-evicted.
#[derive(Default)]
pub struct TermsCache {
    entries: Arc<Mutex<HashMap<String, TermsEntry>>>,
}

impl TermsCache {
    /// Cached statistics when the fingerprint still matches.
    pub fn get(&self, site_id: &str, fingerprint: &str) -> Option<Arc<SiteTerms>> {
        let mut m = self.entries.lock();
        let e = m.get_mut(site_id)?;
        if e.fingerprint != fingerprint {
            return None;
        }
        e.last = Instant::now();
        Some(e.data.clone())
    }

    pub fn put(&self, site_id: &str, fingerprint: String, data: Arc<SiteTerms>) {
        self.entries.lock().insert(
            site_id.to_string(),
            TermsEntry {
                fingerprint,
                data,
                last: Instant::now(),
            },
        );
    }

    pub fn evict_idle(&self, ttl: Duration) -> usize {
        evict_idle_terms(&self.entries, ttl)
    }

    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn evict_idle_terms(entries: &Mutex<HashMap<String, TermsEntry>>, ttl: Duration) -> usize {
    let mut m = entries.lock();
    let before = m.len();
    m.retain(|_, e| e.last.elapsed() < ttl);
    before - m.len()
}

/// Max phrase embeddings kept (about 1.6 KB each).
pub const PHRASE_CACHE_MAX: usize = 50_000;

struct PhraseInner {
    map: HashMap<String, Arc<[f32]>>,
    last: Instant,
}

impl Default for PhraseInner {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            last: Instant::now(),
        }
    }
}

/// Phrase text (lowercase) -> unit embedding. Cleared when full or idle.
#[derive(Default)]
pub struct PhraseCache {
    inner: Arc<Mutex<PhraseInner>>,
}

impl PhraseCache {
    pub fn get(&self, phrase: &str) -> Option<Arc<[f32]>> {
        let mut g = self.inner.lock();
        g.last = Instant::now();
        g.map.get(phrase).cloned()
    }

    pub fn insert(&self, phrase: String, v: Vec<f32>) {
        let mut g = self.inner.lock();
        if g.map.len() >= PHRASE_CACHE_MAX {
            g.map.clear();
        }
        g.last = Instant::now();
        g.map.insert(phrase, Arc::from(v));
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn evict_idle(&self, ttl: Duration) -> usize {
        evict_idle_phrases(&self.inner, ttl)
    }
}

fn evict_idle_phrases(inner: &Mutex<PhraseInner>, ttl: Duration) -> usize {
    let mut g = inner.lock();
    if g.last.elapsed() < ttl {
        return 0;
    }
    let n = g.map.len();
    g.map = HashMap::new();
    n
}

type SweepRefs = (
    Weak<Mutex<Vec<CacheEntry>>>,
    Weak<Mutex<HashMap<String, TermsEntry>>>,
    Weak<Mutex<PhraseInner>>,
);

/// Background task (when a tokio runtime is running) that drops idle cache entries every
/// [`SWEEP_EVERY`]. Holds only weak references: it ends when the context is dropped.
fn spawn_idle_sweeper(v: &VectorCache, t: &TermsCache, p: &PhraseCache, ttl: Duration) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let (wv, wt, wp): SweepRefs = (
        Arc::downgrade(&v.entries),
        Arc::downgrade(&t.entries),
        Arc::downgrade(&p.inner),
    );
    handle.spawn(async move {
        loop {
            tokio::time::sleep(SWEEP_EVERY).await;
            let (Some(v), Some(t), Some(p)) = (wv.upgrade(), wt.upgrade(), wp.upgrade()) else {
                break;
            };
            let n = evict_idle_vectors(&v, ttl) + evict_idle_terms(&t, ttl);
            let np = evict_idle_phrases(&p, ttl);
            if n + np > 0 {
                tracing::info!(entries = n, phrases = np, "idle caches dropped");
            }
        }
    });
}

fn load_site_vectors(c: &rusqlite::Connection, site: &str) -> sf_core::Result<SiteVectors> {
    // Exact capacities: a pushed-to Vec of 112k x 384 f32 otherwise ends at 2^26 floats (+95 MB).
    let (n_posts, n_paras): (i64, i64) = c
        .query_row(
            "SELECT (SELECT count(*) FROM posts WHERE site_id=?1 AND embedding IS NOT NULL),
                    (SELECT count(*) FROM paragraphs pa JOIN posts p ON p.id = pa.post_id
                      WHERE p.site_id=?1 AND pa.embedding IS NOT NULL)",
            [site],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(err)?;
    let cap = |n: i64| usize::try_from(n).unwrap_or(0);
    let mut posts = VectorIndex::with_capacity(EMBED_DIM, cap(n_posts))
        .map_err(|e| Error::internal(e.to_string()))?;
    let mut stmt = c
        .prepare(
            "SELECT id, embedding FROM posts WHERE site_id=?1 AND embedding IS NOT NULL ORDER BY id",
        )
        .map_err(err)?;
    let mut rows = stmt.query([site]).map_err(err)?;
    while let Some(r) = rows.next().map_err(err)? {
        let id: String = r.get(0).map_err(err)?;
        let blob: Vec<u8> = r.get(1).map_err(err)?;
        if let Some(v) = blob_to_vec(&blob).filter(|v| v.len() == EMBED_DIM) {
            let _ = posts.insert(&id, &v);
        }
    }
    drop(rows);
    drop(stmt);

    let mut paras = VectorIndex::with_capacity(EMBED_DIM, cap(n_paras))
        .map_err(|e| Error::internal(e.to_string()))?;
    let mut para_meta: Vec<(Arc<str>, i64)> = Vec::with_capacity(cap(n_paras));
    let mut by_post: HashMap<Arc<str>, Vec<usize>> = HashMap::new();
    let mut stmt = c
        .prepare(
            "SELECT pa.id, pa.post_id, pa.idx, pa.embedding FROM paragraphs pa JOIN posts p ON p.id = pa.post_id
             WHERE p.site_id=?1 AND pa.embedding IS NOT NULL ORDER BY pa.post_id, pa.idx",
        )
        .map_err(err)?;
    let mut rows = stmt.query([site]).map_err(err)?;
    let mut current: Option<Arc<str>> = None;
    while let Some(r) = rows.next().map_err(err)? {
        let id: String = r.get(0).map_err(err)?;
        let post_id: String = r.get(1).map_err(err)?;
        let idx: i64 = r.get(2).map_err(err)?;
        let blob: Vec<u8> = r.get(3).map_err(err)?;
        let Some(v) = blob_to_vec(&blob).filter(|v| v.len() == EMBED_DIM) else {
            continue;
        };
        let Ok(row) = paras.insert(&id, &v) else {
            continue;
        };
        let pid: Arc<str> = match &current {
            Some(p) if **p == *post_id => p.clone(),
            _ => {
                let p: Arc<str> = Arc::from(post_id.as_str());
                current = Some(p.clone());
                p
            }
        };
        if row == para_meta.len() {
            para_meta.push((pid.clone(), idx));
            by_post.entry(pid).or_default().push(row);
        }
    }
    posts.shrink_to_fit();
    paras.shrink_to_fit();
    para_meta.shrink_to_fit();
    Ok(SiteVectors {
        posts,
        paras,
        para_meta,
        by_post,
    })
}

/// Run CPU-heavy work on the shared rayon pool and await it without blocking a tokio worker.
pub async fn compute<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> JobResult<R> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    sf_engine::pool::spawn_with_callback(f, move |r| {
        let _ = tx.send(r);
    });
    match rx.await {
        Ok(r) => r.map_err(JobError::from),
        Err(_) => Err(JobError::msg("compute task dropped")),
    }
}
