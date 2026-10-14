//! The embedding chain: ordered rungs, circuit breaker, per-batch deadline, streaming batches.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use sf_core::config::EmbedCfg;
use sf_core::{Error, Result};

use crate::budget::CpuBudget;
use crate::remote::RemoteEmbedder;
use crate::selftest::{SelfTest, reference_text};
use crate::{Embedder, EmbedderKind, check_output};

#[derive(Debug, Clone)]
pub struct ChainOptions {
    /// Texts per backend call (and per `embed_batched` progress step).
    pub batch_size: usize,
    /// Wall-clock budget for one batch across all rungs (no endless fallback loops).
    pub deadline: Duration,
    /// Consecutive failures that open the circuit for a rung.
    pub fail_threshold: u32,
    /// First cooldown; doubles on every re-trip up to `max_cooldown`.
    pub cooldown: Duration,
    pub max_cooldown: Duration,
}

impl Default for ChainOptions {
    fn default() -> Self {
        Self {
            batch_size: 64,
            deadline: Duration::from_secs(120),
            fail_threshold: 3,
            cooldown: Duration::from_secs(30),
            max_cooldown: Duration::from_secs(600),
        }
    }
}

impl ChainOptions {
    pub fn from_config(cfg: &EmbedCfg) -> Self {
        let attempts = u64::from(cfg.retries).saturating_add(1);
        let deadline_ms = cfg
            .timeout_ms
            .max(1_000)
            .saturating_mul(attempts)
            .saturating_mul(2)
            .max(30_000);
        Self {
            batch_size: cfg.batch_size.clamp(1, 1024),
            deadline: Duration::from_millis(deadline_ms),
            ..Self::default()
        }
    }
}

/// Snapshot of one rung for `/api/v2/embed/status` and `siloforge doctor`.
#[derive(Debug, Clone, Serialize)]
pub struct RungStatus {
    pub id: String,
    pub kind: EmbedderKind,
    pub device: String,
    /// Configured and compiled in (has a key, feature enabled). Cooldown does not change this.
    pub available: bool,
    pub last_error: Option<String>,
    pub ok_count: u64,
    pub fail_count: u64,
    pub consecutive_failures: u32,
    /// RFC 3339 time until which the rung is skipped (circuit open).
    pub cooldown_until: Option<String>,
}

/// One finished batch, handed to the `embed_batched` callback so callers can persist it at once.
#[derive(Debug)]
pub struct BatchDone {
    /// Index of `vectors[0]` in the caller's input.
    pub offset: usize,
    pub vectors: Vec<Vec<f32>>,
    /// Rung that produced this batch.
    pub rung: String,
    /// Texts done so far (including this batch) and total.
    pub done: usize,
    pub total: usize,
}

#[derive(Default)]
struct RungState {
    ok: u64,
    fail: u64,
    consecutive: u32,
    trips: u32,
    last_error: Option<String>,
    cooldown_until: Option<(Instant, SystemTime)>,
}

struct Rung {
    id: String,
    kind: EmbedderKind,
    embedder: Option<Arc<dyn Embedder>>,
    st: Mutex<RungState>,
}

/// Everything the rungs are built from. A change in any of these rebuilds the rungs on
/// [`EmbedChain::reconfigure`]; batch size and deadline are applied without a rebuild. Holds the
/// remote keys, so it has no `Debug`.
#[derive(Clone, PartialEq)]
struct BuildKey {
    chain: Vec<String>,
    mode: String,
    gpu: bool,
    share_bits: u32,
    remote_url: String,
    remote_model: String,
    remote_key: String,
    remote_key2: String,
    timeout_ms: u64,
    retries: u32,
    model_dir: String,
    onnx_runtime_dir: String,
    max_chars: usize,
    home: PathBuf,
    budget: CpuBudget,
}

impl BuildKey {
    fn new(cfg: &EmbedCfg, home: &Path, budget: CpuBudget) -> Self {
        Self {
            chain: cfg.chain.clone(),
            mode: normalize_mode(&cfg.mode).0,
            gpu: cfg.gpu,
            share_bits: cfg.hybrid_gpu_share.to_bits(),
            remote_url: cfg.remote_url.clone(),
            remote_model: cfg.remote_model.clone(),
            remote_key: cfg.remote_key.clone(),
            remote_key2: cfg.remote_key2.clone(),
            timeout_ms: cfg.timeout_ms,
            retries: cfg.retries,
            model_dir: cfg.model_dir.clone(),
            onnx_runtime_dir: cfg.onnx_runtime_dir.clone(),
            max_chars: cfg.max_chars,
            home: home.to_path_buf(),
            budget,
        }
    }
}

/// The embedding chain. Shared as `Arc<EmbedChain>`; [`EmbedChain::reconfigure`] swaps the rungs
/// and options at runtime. Every batch works on a snapshot, so a running batch is unaffected.
pub struct EmbedChain {
    rungs: RwLock<Arc<Vec<Rung>>>,
    opts: RwLock<ChainOptions>,
    /// What the rungs were built from (None for [`EmbedChain::new`]).
    key: Mutex<Option<BuildKey>>,
    mode: RwLock<String>,
    /// Rung id that served the last successful batch.
    last: Mutex<Option<String>>,
}

fn kind_of(name: &str) -> EmbedderKind {
    match name {
        "onnx" => EmbedderKind::Onnx,
        "local" | "candle" => EmbedderKind::Candle,
        _ => EmbedderKind::Remote,
    }
}

/// `embed.mode` lower-cased and checked. Unknown values become `auto` (second value false).
fn normalize_mode(raw: &str) -> (String, bool) {
    let m = raw.trim().to_ascii_lowercase();
    match m.as_str() {
        "" => ("auto".into(), true),
        "auto" | "gpu" | "cpu" | "hybrid" | "remote" => (m, true),
        _ => ("auto".into(), false),
    }
}

/// How the onnx rung is built for a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnnxPlan {
    /// One session; DirectML first when `gpu`.
    Single { gpu: bool },
    /// DirectML and CPU sessions sharing each batch.
    Hybrid,
}

fn onnx_plan(mode: &str, cfg_gpu: bool) -> OnnxPlan {
    match mode {
        "gpu" => OnnxPlan::Single { gpu: true },
        "cpu" => OnnxPlan::Single { gpu: false },
        "hybrid" => OnnxPlan::Hybrid,
        // auto and remote keep the configured `embed.gpu`
        _ => OnnxPlan::Single { gpu: cfg_gpu },
    }
}

/// Rung names from `embed.chain` (aliases folded, duplicates dropped), reordered for `mode`.
fn rung_names(chain: &[String], mode: &str) -> Vec<String> {
    let raw: Vec<String> = if chain.is_empty() {
        vec!["onnx".into(), "local".into()]
    } else {
        chain.to_vec()
    };
    let mut names: Vec<String> = Vec::new();
    for r in raw {
        let name = match r.trim().to_ascii_lowercase().as_str() {
            "openrouter" | "openrouter1" | "remote" => "remote".to_string(),
            "openrouter2" | "remote2" => "remote2".to_string(),
            "candle" | "local" => "local".to_string(),
            other => other.to_string(),
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if mode == "remote" {
        // stable partition: remote rungs first, the rest keep their order
        let (mut front, rest): (Vec<String>, Vec<String>) = names
            .into_iter()
            .partition(|n| n == "remote" || n == "remote2");
        front.extend(rest);
        names = front;
    }
    names
}

fn resolve_threads(cpu_threads: usize) -> usize {
    if cpu_threads == 0 {
        std::thread::available_parallelism()
            .map(|n| (n.get() / 2).max(1))
            .unwrap_or(4)
    } else {
        cpu_threads
    }
}

/// Build the rungs for `cfg` (never fails; broken rungs are kept as unavailable). Returns the
/// rungs and the normalised mode.
fn build_rungs(cfg: &EmbedCfg, home: &Path, budget: CpuBudget) -> (Vec<Rung>, String) {
    let (mode, known) = normalize_mode(&cfg.mode);
    if !known {
        tracing::warn!(mode = %cfg.mode, "unknown embed.mode, using auto");
    }
    let timeout = Duration::from_millis(cfg.timeout_ms.max(1_000));
    let mut rungs = Vec::new();
    for name in rung_names(&cfg.chain, &mode) {
        let built: std::result::Result<Arc<dyn Embedder>, String> = match name.as_str() {
            "remote" | "remote2" => {
                let (key, field) = if name == "remote" {
                    (&cfg.remote_key, "remote_key")
                } else {
                    (&cfg.remote_key2, "remote_key2")
                };
                if key.trim().is_empty() {
                    Err(format!("no API key (embed.{field})"))
                } else {
                    RemoteEmbedder::new(
                        name.clone(),
                        &cfg.remote_url,
                        &cfg.remote_model,
                        key.trim(),
                        timeout,
                        cfg.retries,
                        cfg.max_chars,
                    )
                    .map(|e| Arc::new(e) as Arc<dyn Embedder>)
                    .map_err(|e| e.to_string())
                }
            }
            "onnx" => onnx_rung(cfg, home, budget, onnx_plan(&mode, cfg.gpu)),
            "local" => candle_rung(cfg, home, budget),
            other => Err(format!("unknown embed rung `{other}`")),
        };
        let (embedder, err) = match built {
            Ok(e) => (Some(e), None),
            Err(e) => (None, Some(e)),
        };
        rungs.push(Rung {
            kind: kind_of(&name),
            id: name,
            embedder,
            st: Mutex::new(RungState {
                last_error: err,
                ..RungState::default()
            }),
        });
    }
    (rungs, mode)
}

impl EmbedChain {
    fn from_parts(
        rungs: Vec<Rung>,
        opts: ChainOptions,
        key: Option<BuildKey>,
        mode: String,
    ) -> Self {
        Self {
            rungs: RwLock::new(Arc::new(rungs)),
            opts: RwLock::new(opts),
            key: Mutex::new(key),
            mode: RwLock::new(mode),
            last: Mutex::new(None),
        }
    }

    /// Chain from explicit backends (tests, custom setups). `mode()` reports `custom`.
    pub fn new(embedders: Vec<Arc<dyn Embedder>>, opts: ChainOptions) -> Self {
        let rungs = embedders
            .into_iter()
            .map(|e| Rung {
                id: e.id().to_string(),
                kind: e.kind(),
                embedder: Some(e),
                st: Mutex::new(RungState::default()),
            })
            .collect();
        Self::from_parts(rungs, opts, None, "custom".into())
    }

    /// Chain from config. Never fails: rungs that cannot run (no key, feature not compiled, unknown
    /// name) are kept as unavailable so `status()` can explain them. `cpu_threads` is the CPU budget
    /// for local inference (physical cores; 0 = half the logical CPUs).
    pub fn from_config(cfg: &EmbedCfg, home: &Path, cpu_threads: usize) -> Self {
        Self::from_config_budget(cfg, home, cpu_threads, false)
    }

    /// [`Self::from_config`] plus `resources.low_priority_jobs`: local rungs then run their
    /// inference threads below normal priority without busy-wait spinning (R11).
    ///
    /// `embed.mode`: `auto` (onnx follows `embed.gpu`), `gpu` (DirectML forced on), `cpu`
    /// (DirectML off), `hybrid` (the onnx rung runs DirectML and CPU sessions together),
    /// `remote` (remote rungs moved to the front). Unknown modes act as `auto`.
    pub fn from_config_budget(
        cfg: &EmbedCfg,
        home: &Path,
        cpu_threads: usize,
        low_priority: bool,
    ) -> Self {
        let budget = CpuBudget::new(resolve_threads(cpu_threads), low_priority);
        let (rungs, mode) = build_rungs(cfg, home, budget);
        Self::from_parts(
            rungs,
            ChainOptions::from_config(cfg),
            Some(BuildKey::new(cfg, home, budget)),
            mode,
        )
    }

    /// Apply new settings at runtime. Batch size and deadline always change; the rungs are
    /// rebuilt only when something they are built from changed (chain, mode, gpu,
    /// hybrid_gpu_share, remote url/model/keys/timeout/retries, model_dir, onnx_runtime_dir,
    /// max_chars, home, budget). Batches already running finish on the old rungs. Rebuilt rungs
    /// keep the ok/fail counters of the rung with the same id; their circuit breaker is reset.
    /// `budget.threads` is used as given (resolve `cpu_threads = 0` before calling).
    pub fn reconfigure(&self, cfg: &EmbedCfg, home: &Path, budget: CpuBudget) {
        let fresh = ChainOptions::from_config(cfg);
        {
            let mut o = self.opts.write();
            o.batch_size = fresh.batch_size;
            o.deadline = fresh.deadline;
        }
        let key = BuildKey::new(cfg, home, budget);
        let mut current = self.key.lock();
        if current.as_ref() == Some(&key) {
            return;
        }
        let (rungs, mode) = build_rungs(cfg, home, budget);
        let old = self.rungs.read().clone();
        for r in &rungs {
            if let Some(prev) = old.iter().find(|o| o.id == r.id) {
                let p = prev.st.lock();
                let mut st = r.st.lock();
                st.ok = p.ok;
                st.fail = p.fail;
                if r.embedder.is_some() {
                    st.last_error.clone_from(&p.last_error);
                }
            }
        }
        let ids: Vec<&str> = rungs.iter().map(|r| r.id.as_str()).collect();
        tracing::info!(mode = %mode, rungs = ?ids, threads = budget.threads, low_priority = budget.low_priority, "embed chain rebuilt");
        *self.rungs.write() = Arc::new(rungs);
        *self.mode.write() = mode;
        *self.last.lock() = None;
        *current = Some(key);
    }

    /// Current options (a copy; [`Self::reconfigure`] may change them).
    pub fn options(&self) -> ChainOptions {
        self.opts.read().clone()
    }

    /// Normalised `embed.mode` (`auto`, `gpu`, `cpu`, `hybrid`, `remote`; `custom` for
    /// [`Self::new`]).
    pub fn mode(&self) -> String {
        self.mode.read().clone()
    }

    fn snapshot(&self) -> Arc<Vec<Rung>> {
        self.rungs.read().clone()
    }

    /// Device of the rung that served the last batch; else of the first available rung (not
    /// cooling down, if any); else `none`.
    pub fn active_device(&self) -> String {
        let rungs = self.snapshot();
        let last = self.last.lock().clone();
        if let Some(id) = last
            && let Some(e) = rungs
                .iter()
                .find(|r| r.id == id)
                .and_then(|r| r.embedder.as_ref())
        {
            return e.device();
        }
        let now = Instant::now();
        let healthy = rungs.iter().find(|r| {
            r.embedder.is_some()
                && r.st
                    .lock()
                    .cooldown_until
                    .is_none_or(|(until, _)| until <= now)
        });
        healthy
            .or_else(|| rungs.iter().find(|r| r.embedder.is_some()))
            .and_then(|r| r.embedder.as_ref())
            .map_or_else(|| "none".to_string(), |e| e.device())
    }

    /// True when at least one rung is configured and compiled in.
    pub fn has_available(&self) -> bool {
        self.snapshot().iter().any(|r| r.embedder.is_some())
    }

    pub fn status(&self) -> Vec<RungStatus> {
        let now = Instant::now();
        self.snapshot()
            .iter()
            .map(|r| {
                let st = r.st.lock();
                let cooldown_until = st.cooldown_until.and_then(|(i, wall)| {
                    (i > now).then(|| chrono::DateTime::<chrono::Utc>::from(wall).to_rfc3339())
                });
                RungStatus {
                    id: r.id.clone(),
                    kind: r.kind,
                    device: r
                        .embedder
                        .as_ref()
                        .map_or_else(|| "unavailable".to_string(), |e| e.device()),
                    available: r.embedder.is_some(),
                    last_error: st.last_error.clone(),
                    ok_count: st.ok,
                    fail_count: st.fail,
                    consecutive_failures: st.consecutive,
                    cooldown_until,
                }
            })
            .collect()
    }

    fn batch_size(&self) -> usize {
        self.opts.read().batch_size.max(1)
    }

    /// Embed everything (internally in `batch_size` chunks). Vectors come back in input order.
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed_cap(texts, crate::MAX_TOKENS).await
    }

    /// Embed in `batch_size` chunks and hand each finished chunk to `on_batch` before starting the
    /// next one, so a late failure never loses earlier work. Stops at the first error (from the
    /// chain or from the callback). Returns the number of texts embedded.
    pub async fn embed_batched<F, Fut>(&self, texts: &[String], on_batch: F) -> Result<usize>
    where
        F: FnMut(BatchDone) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        self.embed_batched_cap(texts, crate::MAX_TOKENS, on_batch)
            .await
    }

    /// Embed the reference paragraph through the chain (first healthy rung) and compare it with the
    /// stored production vector.
    pub async fn self_test(&self) -> Result<SelfTest> {
        let t = Instant::now();
        let (v, rung) = self.embed_batch(&[reference_text().to_string()]).await?;
        let device = self
            .snapshot()
            .iter()
            .find(|r| r.id == rung)
            .and_then(|r| r.embedder.as_ref())
            .map(|e| e.device())
            .unwrap_or_default();
        let first = v
            .first()
            .ok_or_else(|| Error::internal("self-test: no vector"))?;
        Ok(crate::selftest::finish(rung, device, first, t))
    }

    /// [`EmbedChain::embed_batched`] with a word-piece cap ([`crate::MAX_TOKENS_LONG`] reproduces
    /// vectors made by v1's local rung). Rungs that cannot change their cap ignore it.
    pub async fn embed_batched_cap<F, Fut>(
        &self,
        texts: &[String],
        max_tokens: usize,
        mut on_batch: F,
    ) -> Result<usize>
    where
        F: FnMut(BatchDone) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let total = texts.len();
        let mut done = 0usize;
        for chunk in texts.chunks(self.batch_size()) {
            let (vectors, rung) = self.embed_batch_cap(chunk, max_tokens).await?;
            let offset = done;
            done += chunk.len();
            on_batch(BatchDone {
                offset,
                vectors,
                rung,
                done,
                total,
            })
            .await?;
        }
        Ok(done)
    }

    /// [`EmbedChain::embed`] with a word-piece cap.
    pub async fn embed_cap(&self, texts: &[String], max_tokens: usize) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(self.batch_size()) {
            let (v, _) = self.embed_batch_cap(chunk, max_tokens).await?;
            out.extend(v);
        }
        Ok(out)
    }

    /// One batch through the rungs, in order, within the deadline. Returns vectors and the rung id.
    pub async fn embed_batch(&self, texts: &[String]) -> Result<(Vec<Vec<f32>>, String)> {
        self.embed_batch_cap(texts, crate::MAX_TOKENS).await
    }

    /// [`EmbedChain::embed_batch`] with a word-piece cap.
    pub async fn embed_batch_cap(
        &self,
        texts: &[String],
        max_tokens: usize,
    ) -> Result<(Vec<Vec<f32>>, String)> {
        if texts.is_empty() {
            return Ok((Vec::new(), String::new()));
        }
        // Snapshot: a reconfigure during this batch does not affect it.
        let rungs = self.snapshot();
        let opts = self.options();
        let start = Instant::now();
        let deadline = start + opts.deadline;
        let mut errors: Vec<String> = Vec::new();
        for r in rungs.iter() {
            let Some(e) = r.embedder.as_ref() else {
                continue;
            };
            let now = Instant::now();
            if let Some((until, _)) = r.st.lock().cooldown_until
                && until > now
            {
                errors.push(format!(
                    "{}: cooling down for {}s",
                    r.id,
                    until.saturating_duration_since(now).as_secs()
                ));
                continue;
            }
            let remaining = deadline.saturating_duration_since(now);
            if remaining.is_zero() {
                errors.push("deadline reached".into());
                break;
            }
            let call = if max_tokens == crate::MAX_TOKENS {
                e.embed(texts)
            } else {
                e.embed_with_cap(texts, max_tokens)
            };
            match tokio::time::timeout(remaining, call).await {
                Ok(Ok(v)) => match check_output(texts.len(), v) {
                    Ok(v) => {
                        record_ok(r);
                        *self.last.lock() = Some(r.id.clone());
                        return Ok((v, r.id.clone()));
                    }
                    Err(err) => {
                        let msg = format!("invalid output: {err}");
                        record_fail(&opts, r, &msg);
                        errors.push(format!("{}: {msg}", r.id));
                    }
                },
                Ok(Err(err)) => {
                    let msg = err.to_string();
                    record_fail(&opts, r, &msg);
                    errors.push(format!("{}: {msg}", r.id));
                }
                Err(_) => {
                    let msg = format!("timed out (batch deadline {}s)", opts.deadline.as_secs());
                    record_fail(&opts, r, &msg);
                    errors.push(format!("{}: {msg}", r.id));
                    break;
                }
            }
        }
        if errors.is_empty() {
            errors
                .push("no embedding rung is available (check embed.chain / keys / runtime)".into());
        }
        Err(Error::upstream(format!(
            "[embed] all rungs failed: {}",
            errors.join("; ")
        )))
    }
}

fn record_ok(r: &Rung) {
    let mut st = r.st.lock();
    st.ok = st.ok.saturating_add(1);
    st.consecutive = 0;
    st.trips = 0;
    st.cooldown_until = None;
}

fn record_fail(opts: &ChainOptions, r: &Rung, msg: &str) {
    let mut st = r.st.lock();
    st.fail = st.fail.saturating_add(1);
    st.consecutive = st.consecutive.saturating_add(1);
    st.last_error = Some(msg.chars().take(500).collect());
    tracing::warn!(rung = %r.id, error = %msg, "embed rung failed");
    if st.consecutive >= opts.fail_threshold.max(1) {
        let factor = 1u32 << st.trips.min(16);
        let cd = opts.cooldown.saturating_mul(factor).min(opts.max_cooldown);
        st.trips = st.trips.saturating_add(1);
        st.cooldown_until = Some((Instant::now() + cd, SystemTime::now() + cd));
        tracing::warn!(rung = %r.id, secs = cd.as_secs(), "embed rung in cooldown");
    }
}

#[cfg(feature = "onnx")]
fn onnx_rung(
    cfg: &EmbedCfg,
    home: &Path,
    budget: CpuBudget,
    plan: OnnxPlan,
) -> std::result::Result<Arc<dyn Embedder>, String> {
    let mut opts = crate::onnx::OnnxOptions::from_config_budget(cfg, home, budget);
    Ok(match plan {
        OnnxPlan::Single { gpu } => {
            opts.gpu = gpu;
            Arc::new(crate::onnx::OnnxEmbedder::new(opts))
        }
        OnnxPlan::Hybrid => Arc::new(crate::hybrid::HybridEmbedder::new(
            opts,
            cfg.hybrid_gpu_share,
        )),
    })
}

#[cfg(not(feature = "onnx"))]
fn onnx_rung(
    _cfg: &EmbedCfg,
    _home: &Path,
    _budget: CpuBudget,
    _plan: OnnxPlan,
) -> std::result::Result<Arc<dyn Embedder>, String> {
    Err("built without the `onnx` feature".into())
}

#[cfg(feature = "candle")]
fn candle_rung(
    cfg: &EmbedCfg,
    home: &Path,
    budget: CpuBudget,
) -> std::result::Result<Arc<dyn Embedder>, String> {
    Ok(Arc::new(
        crate::candle::CandleEmbedder::from_config(cfg, home).with_budget(budget),
    ))
}

#[cfg(not(feature = "candle"))]
fn candle_rung(
    _cfg: &EmbedCfg,
    _home: &Path,
    _budget: CpuBudget,
) -> std::result::Result<Arc<dyn Embedder>, String> {
    Err("built without the `candle` feature".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EmbedFuture;
    use sf_core::EMBED_DIM;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Test rung: fails the first `fail_first` calls, optionally sleeps, tags vectors with `tag`.
    struct Fake {
        id: &'static str,
        tag: usize,
        fail_first: usize,
        always_fail: bool,
        sleep: Duration,
        calls: AtomicUsize,
    }

    impl Fake {
        fn new(id: &'static str, tag: usize) -> Self {
            Self {
                id,
                tag,
                fail_first: 0,
                always_fail: false,
                sleep: Duration::ZERO,
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl Embedder for Fake {
        fn id(&self) -> &str {
            self.id
        }
        fn kind(&self) -> EmbedderKind {
            EmbedderKind::Remote
        }
        fn device(&self) -> String {
            format!("dev-{}", self.id)
        }
        fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a> {
            Box::pin(async move {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if !self.sleep.is_zero() {
                    tokio::time::sleep(self.sleep).await;
                }
                if self.always_fail || n < self.fail_first {
                    return Err(Error::upstream(format!("{} down", self.id)));
                }
                Ok(texts
                    .iter()
                    .enumerate()
                    .map(|(i, _)| {
                        let mut v = vec![0.0f32; EMBED_DIM];
                        v[self.tag] = 1.0;
                        v[100 + i % 200] = 0.5;
                        v
                    })
                    .collect())
            })
        }
    }

    fn opts() -> ChainOptions {
        ChainOptions {
            batch_size: 4,
            deadline: Duration::from_secs(5),
            fail_threshold: 2,
            cooldown: Duration::from_millis(300),
            max_cooldown: Duration::from_secs(5),
        }
    }

    fn texts(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("t{i}")).collect()
    }

    #[tokio::test]
    async fn falls_back_in_order_and_trips_breaker() {
        let a = Arc::new(Fake {
            always_fail: true,
            ..Fake::new("a", 1)
        });
        let b = Arc::new(Fake::new("b", 2));
        let chain = EmbedChain::new(vec![a.clone(), b.clone()], opts());
        for _ in 0..4 {
            let (v, rung) = chain.embed_batch(&texts(3)).await.unwrap();
            assert_eq!(rung, "b");
            assert!(v[0][2] > 0.5);
        }
        // threshold 2: a was tried twice, then skipped while cooling down
        assert_eq!(a.calls.load(Ordering::SeqCst), 2);
        assert_eq!(b.calls.load(Ordering::SeqCst), 4);
        let st = chain.status();
        assert_eq!(st[0].fail_count, 2);
        assert!(st[0].cooldown_until.is_some());
        assert!(
            st[0]
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("a down"))
        );
        assert_eq!(st[1].ok_count, 4);
        // after the cooldown a is tried again (half-open), fails, re-trips with a longer cooldown
        tokio::time::sleep(Duration::from_millis(350)).await;
        chain.embed_batch(&texts(1)).await.unwrap();
        assert_eq!(a.calls.load(Ordering::SeqCst), 3);
        chain.embed_batch(&texts(1)).await.unwrap();
        assert_eq!(a.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn recovers_after_transient_failure() {
        let a = Arc::new(Fake {
            fail_first: 1,
            ..Fake::new("a", 1)
        });
        let b = Arc::new(Fake::new("b", 2));
        let chain = EmbedChain::new(vec![a.clone(), b], opts());
        assert_eq!(chain.embed_batch(&texts(1)).await.unwrap().1, "b");
        assert_eq!(chain.embed_batch(&texts(1)).await.unwrap().1, "a");
        assert_eq!(chain.status()[0].consecutive_failures, 0);
    }

    #[tokio::test]
    async fn deadline_bounds_a_hanging_rung() {
        let slow = Arc::new(Fake {
            sleep: Duration::from_secs(30),
            ..Fake::new("slow", 1)
        });
        let b = Arc::new(Fake::new("b", 2));
        let chain = EmbedChain::new(
            vec![slow, b.clone()],
            ChainOptions {
                deadline: Duration::from_millis(200),
                ..opts()
            },
        );
        let t = Instant::now();
        let e = chain.embed_batch(&texts(2)).await.unwrap_err();
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(e.to_string().contains("timed out"), "{e}");
        assert_eq!(b.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn all_failing_gives_one_clear_error() {
        let a = Arc::new(Fake {
            always_fail: true,
            ..Fake::new("a", 1)
        });
        let chain = EmbedChain::new(vec![a], opts());
        let e = chain.embed(&texts(2)).await.unwrap_err().to_string();
        assert!(
            e.contains("all rungs failed") && e.contains("a down"),
            "{e}"
        );
        let empty = EmbedChain::new(Vec::new(), opts());
        assert!(empty.embed(&texts(1)).await.is_err());
        assert!(empty.embed(&[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn batched_streams_and_keeps_earlier_work() {
        // succeeds for 2 batches, then fails every call
        struct Flaky(AtomicUsize);
        impl Embedder for Flaky {
            fn id(&self) -> &str {
                "flaky"
            }
            fn kind(&self) -> EmbedderKind {
                EmbedderKind::Remote
            }
            fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a> {
                Box::pin(async move {
                    if self.0.fetch_add(1, Ordering::SeqCst) >= 2 {
                        return Err(Error::upstream("boom"));
                    }
                    Ok(texts.iter().map(|_| vec![1.0f32; EMBED_DIM]).collect())
                })
            }
        }
        let chain = EmbedChain::new(vec![Arc::new(Flaky(AtomicUsize::new(0)))], opts());
        let mut saved = Vec::new();
        let r = chain
            .embed_batched(&texts(10), |b| {
                saved.push((b.offset, b.vectors.len(), b.done, b.total, b.rung));
                async { Ok(()) }
            })
            .await;
        assert!(r.is_err());
        assert_eq!(
            saved,
            vec![
                (0, 4, 4, 10, "flaky".to_string()),
                (4, 4, 8, 10, "flaky".to_string())
            ]
        );

        // callback error stops the run
        let chain = EmbedChain::new(vec![Arc::new(Fake::new("ok", 1))], opts());
        let mut n = 0;
        let r = chain
            .embed_batched(&texts(10), |_| {
                n += 1;
                async { Err(Error::Internal("db full".into())) }
            })
            .await;
        assert!(r.is_err());
        assert_eq!(n, 1);
        let total = chain.embed_batched(&texts(9), |_| async { Ok(()) }).await;
        assert_eq!(total.unwrap(), 9);
    }

    #[tokio::test]
    async fn invalid_backend_output_counts_as_failure() {
        struct Bad;
        impl Embedder for Bad {
            fn id(&self) -> &str {
                "bad"
            }
            fn kind(&self) -> EmbedderKind {
                EmbedderKind::Remote
            }
            fn embed<'a>(&'a self, _t: &'a [String]) -> EmbedFuture<'a> {
                Box::pin(async { Ok(vec![vec![f32::NAN; EMBED_DIM]]) })
            }
        }
        let chain = EmbedChain::new(vec![Arc::new(Bad), Arc::new(Fake::new("b", 2))], opts());
        assert_eq!(chain.embed_batch(&texts(1)).await.unwrap().1, "b");
        assert_eq!(chain.status()[0].fail_count, 1);
    }

    #[test]
    fn from_config_marks_unavailable_rungs() {
        let cfg = EmbedCfg {
            chain: vec![
                "openrouter".into(),
                "remote2".into(),
                "remote".into(),
                "bogus".into(),
                "onnx".into(),
                "local".into(),
            ],
            remote_key: "k".into(),
            ..EmbedCfg::default()
        };
        let chain = EmbedChain::from_config(&cfg, Path::new("D:/tmp/sf-none"), 2);
        let st = chain.status();
        let ids: Vec<&str> = st.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["remote", "remote2", "bogus", "onnx", "local"]);
        assert!(st[0].available);
        assert!(!st[1].available && st[1].last_error.as_deref().unwrap_or("").contains("key"));
        assert!(!st[2].available);
        assert_eq!(st[3].available, cfg!(feature = "onnx"));
        assert_eq!(st[4].available, cfg!(feature = "candle"));
        assert!(chain.has_available());
        // the remote key never shows up in status
        let json = serde_json::to_string(&st).unwrap();
        assert!(!json.contains("\"k\""));
        assert!(ChainOptions::from_config(&cfg).deadline >= Duration::from_secs(30));
    }

    fn cfg_with(mode: &str, chain: &[&str]) -> EmbedCfg {
        EmbedCfg {
            chain: chain.iter().map(|s| (*s).to_string()).collect(),
            remote_key: "k".into(),
            remote_key2: "k2".into(),
            mode: mode.into(),
            ..EmbedCfg::default()
        }
    }

    fn ids(chain: &EmbedChain) -> Vec<String> {
        chain.status().into_iter().map(|s| s.id).collect()
    }

    #[test]
    fn mode_remote_moves_remote_rungs_first() {
        let home = Path::new("D:/tmp/sf-none");
        let c = EmbedChain::from_config(
            &cfg_with("remote", &["onnx", "remote2", "local", "openrouter"]),
            home,
            2,
        );
        assert_eq!(ids(&c), ["remote2", "remote", "onnx", "local"]);
        assert_eq!(c.mode(), "remote");
        for m in ["auto", "gpu", "cpu", "hybrid"] {
            let c = EmbedChain::from_config(
                &cfg_with(m, &["onnx", "remote2", "local", "openrouter"]),
                home,
                2,
            );
            assert_eq!(ids(&c), ["onnx", "remote2", "local", "remote"], "{m}");
            assert_eq!(c.mode(), m);
        }
        // unknown mode -> auto; mixed case is accepted
        let c = EmbedChain::from_config(&cfg_with("turbo", &["onnx"]), home, 2);
        assert_eq!(c.mode(), "auto");
        let c = EmbedChain::from_config(&cfg_with(" Hybrid ", &["onnx"]), home, 2);
        assert_eq!(c.mode(), "hybrid");
        assert_eq!(
            EmbedChain::new(Vec::new(), ChainOptions::default()).mode(),
            "custom"
        );
    }

    #[test]
    fn mode_picks_the_onnx_device_plan() {
        assert_eq!(onnx_plan("auto", true), OnnxPlan::Single { gpu: true });
        assert_eq!(onnx_plan("auto", false), OnnxPlan::Single { gpu: false });
        assert_eq!(onnx_plan("remote", false), OnnxPlan::Single { gpu: false });
        assert_eq!(onnx_plan("gpu", false), OnnxPlan::Single { gpu: true });
        assert_eq!(onnx_plan("cpu", true), OnnxPlan::Single { gpu: false });
        assert_eq!(onnx_plan("hybrid", false), OnnxPlan::Hybrid);
        assert_eq!(normalize_mode(""), ("auto".to_string(), true));
        assert_eq!(normalize_mode("GPU"), ("gpu".to_string(), true));
        assert_eq!(normalize_mode("x"), ("auto".to_string(), false));
    }

    #[test]
    fn reconfigure_updates_options_and_rebuilds_only_when_needed() {
        let home = Path::new("D:/tmp/sf-none");
        let budget = CpuBudget::new(2, false);
        let mut cfg = cfg_with("auto", &["onnx", "remote"]);
        let c = EmbedChain::from_config_budget(&cfg, home, 2, false);
        assert_eq!(c.options().batch_size, 64);
        let before = c.snapshot();

        // batch size only: no rebuild
        cfg.batch_size = 16;
        c.reconfigure(&cfg, home, budget);
        assert_eq!(c.options().batch_size, 16);
        assert!(Arc::ptr_eq(&before, &c.snapshot()));

        // mode change: rebuilt, new order
        cfg.mode = "remote".into();
        c.reconfigure(&cfg, home, budget);
        assert!(!Arc::ptr_eq(&before, &c.snapshot()));
        assert_eq!(ids(&c), ["remote", "onnx"]);
        assert_eq!(c.mode(), "remote");
        let after_mode = c.snapshot();

        // same settings again: nothing rebuilt
        c.reconfigure(&cfg, home, budget);
        assert!(Arc::ptr_eq(&after_mode, &c.snapshot()));

        // budget, share and key changes rebuild too
        c.reconfigure(&cfg, home, CpuBudget::new(3, true));
        let s1 = c.snapshot();
        assert!(!Arc::ptr_eq(&after_mode, &s1));
        cfg.hybrid_gpu_share = 0.6;
        c.reconfigure(&cfg, home, CpuBudget::new(3, true));
        let s2 = c.snapshot();
        assert!(!Arc::ptr_eq(&s1, &s2));
        cfg.remote_key = String::new();
        c.reconfigure(&cfg, home, CpuBudget::new(3, true));
        let st = c.status();
        assert!(!st[0].available);
        assert!(st[0].last_error.as_deref().unwrap_or("").contains("key"));
    }

    #[tokio::test]
    async fn reconfigure_keeps_counters_and_resets_breaker() {
        let a = Arc::new(Fake {
            always_fail: true,
            ..Fake::new("bogus", 1)
        });
        let chain = EmbedChain::new(vec![a], opts());
        for _ in 0..3 {
            let _ = chain.embed_batch(&texts(1)).await;
        }
        assert_eq!(chain.status()[0].fail_count, 2);
        // rebuild from config: rung `bogus` keeps its counters but is now unavailable
        let cfg = cfg_with("auto", &["bogus"]);
        chain.reconfigure(&cfg, Path::new("D:/tmp/sf-none"), CpuBudget::new(1, false));
        let st = chain.status();
        assert_eq!(st[0].fail_count, 2);
        assert_eq!(st[0].consecutive_failures, 0);
        assert!(st[0].cooldown_until.is_none());
        assert!(!st[0].available);
        assert_eq!(chain.active_device(), "none");
    }

    #[tokio::test]
    async fn running_batch_is_unaffected_by_reconfigure() {
        let slow = Arc::new(Fake {
            sleep: Duration::from_millis(300),
            ..Fake::new("slow", 1)
        });
        let chain = Arc::new(EmbedChain::new(vec![slow], opts()));
        let c2 = chain.clone();
        let running = tokio::spawn(async move { c2.embed_batch(&texts(3)).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let cfg = EmbedCfg {
            batch_size: 7,
            ..cfg_with("cpu", &["bogus"])
        };
        chain.reconfigure(&cfg, Path::new("D:/tmp/sf-none"), CpuBudget::new(1, false));
        let (v, rung) = running.await.unwrap().unwrap();
        assert_eq!((v.len(), rung.as_str()), (3, "slow"));
        // new batches see the new rungs and options
        assert!(chain.embed_batch(&texts(1)).await.is_err());
        assert_eq!(chain.options().batch_size, 7);
        assert_eq!(chain.mode(), "cpu");
    }

    #[tokio::test]
    async fn active_device_follows_the_serving_rung() {
        let a = Arc::new(Fake {
            always_fail: true,
            ..Fake::new("a", 1)
        });
        let b = Arc::new(Fake::new("b", 2));
        let chain = EmbedChain::new(vec![a, b], opts());
        // nothing served yet: first available rung
        assert_eq!(chain.active_device(), "dev-a");
        chain.embed_batch(&texts(1)).await.unwrap();
        assert_eq!(chain.active_device(), "dev-b");
        assert_eq!(chain.last.lock().as_deref(), Some("b"));
        let empty = EmbedChain::new(Vec::new(), opts());
        assert_eq!(empty.active_device(), "none");
    }
}
