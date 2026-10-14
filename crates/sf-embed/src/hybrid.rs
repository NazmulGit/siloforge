//! Hybrid onnx rung (`embed.mode = "hybrid"`): a DirectML session and a CPU session work on the
//! same batch at the same time.
//!
//! Each batch is tokenised once, sorted by length and cut into chunks of [`CPU_BATCH`]. Both
//! workers pull from that one queue ([`HybridJob`]): the GPU takes up to [`GPU_BATCH`] sequences
//! from the long end, the CPU single chunks from the short end, capped at about
//! `1 - hybrid_gpu_share` of the items while the GPU is alive. Each worker runs on its own tokio
//! blocking thread (never a tokio worker), below normal priority with `low_priority_jobs`.
//!
//! When the DirectML session cannot be created, or a GPU run fails, the rung continues on the CPU
//! alone for the rest of its life (and the other way round). A rebuild (`EmbedChain::reconfigure`)
//! tries both again.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ort::session::Session;
use parking_lot::{Mutex, RwLock};
use sf_core::config::EmbedCfg;
use sf_core::{Error, Result};

use crate::batching::{HybridJob, HybridPlan, HybridSplit, Side, clamp_share};
use crate::budget::{CpuBudget, PriorityGuard};
use crate::onnx::{
    CPU_BATCH, GPU_BATCH, GPU_PAD_TO, OnnxOptions, has_token_type, hidden, prepare, session,
};
use crate::tok::Tok;
use crate::{EmbedFuture, Embedder, EmbedderKind, check_output};

/// Intra-op threads of the DirectML session (it only runs the few ops DirectML leaves on the
/// CPU; the CPU session gets the real budget).
const GPU_SESSION_THREADS: usize = 1;

struct Sess {
    session: Mutex<Session>,
    has_tt: bool,
    /// Still usable (false after a failed run).
    ok: AtomicBool,
}

struct Loaded {
    tok: Tok,
    gpu: Option<Sess>,
    cpu: Option<Sess>,
}

impl Loaded {
    fn usable(s: &Option<Sess>) -> bool {
        s.as_ref().is_some_and(|s| s.ok.load(Ordering::Relaxed))
    }

    fn device(&self) -> String {
        match (Self::usable(&self.gpu), Self::usable(&self.cpu)) {
            (true, true) => "hybrid (directml+cpu)".into(),
            (false, true) => "cpu (gpu unavailable)".into(),
            (true, false) => "directml (cpu unavailable)".into(),
            (false, false) => "unavailable".into(),
        }
    }
}

pub struct HybridEmbedder {
    opts: OnnxOptions,
    share: f32,
    state: Arc<Mutex<Option<Arc<Loaded>>>>,
    device: Arc<RwLock<String>>,
    split: Arc<Mutex<HybridSplit>>,
    /// Try DirectML at load (tests turn it off to exercise the CPU-only path without a GPU).
    try_gpu: bool,
}

impl HybridEmbedder {
    /// `opts.gpu` is ignored (hybrid always tries DirectML). `share` = `embed.hybrid_gpu_share`.
    pub fn new(opts: OnnxOptions, share: f32) -> Self {
        Self {
            opts,
            share: clamp_share(share) as f32,
            state: Arc::new(Mutex::new(None)),
            device: Arc::new(RwLock::new("not loaded".into())),
            split: Arc::new(Mutex::new(HybridSplit::default())),
            try_gpu: true,
        }
    }

    pub fn from_config_budget(cfg: &EmbedCfg, home: &Path, budget: CpuBudget) -> Self {
        Self::new(
            OnnxOptions::from_config_budget(cfg, home, budget),
            cfg.hybrid_gpu_share,
        )
    }

    /// GPU share in use (clamped to 0.1..0.9).
    pub fn share(&self) -> f32 {
        self.share
    }

    /// How the last batch was split between the GPU and the CPU worker.
    pub fn last_split(&self) -> HybridSplit {
        *self.split.lock()
    }

    /// Load the runtime and both sessions now (normally lazy). Returns the device string.
    pub async fn load(&self) -> Result<String> {
        let state = self.state.clone();
        let opts = self.opts.clone();
        let device = self.device.clone();
        let try_gpu = self.try_gpu;
        tokio::task::spawn_blocking(move || {
            ensure_loaded(&state, &opts, try_gpu, &device)?;
            Ok(device.read().clone())
        })
        .await
        .map_err(|e| Error::internal(format!("hybrid onnx worker: {e}")))?
    }
}

fn ensure_loaded(
    slot: &Mutex<Option<Arc<Loaded>>>,
    opts: &OnnxOptions,
    try_gpu: bool,
    device: &RwLock<String>,
) -> Result<Arc<Loaded>> {
    let mut g = slot.lock();
    if let Some(l) = g.as_ref() {
        return Ok(l.clone());
    }
    let r = (|| {
        let (model, tok) = prepare(opts)?;
        let mk = |s: Session| Sess {
            has_tt: has_token_type(&s),
            session: Mutex::new(s),
            ok: AtomicBool::new(true),
        };
        let gpu =
            match try_gpu.then(|| session(&model, GPU_SESSION_THREADS, true, opts.low_priority)) {
                None => None,
                Some(Ok(s)) => Some(mk(s)),
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "hybrid embed: DirectML unavailable, CPU only");
                    None
                }
            };
        let cpu = match session(&model, opts.threads, false, opts.low_priority) {
            Ok(s) => Some(mk(s)),
            Err(e) if gpu.is_some() => {
                tracing::warn!(error = %e, "hybrid embed: CPU session failed, DirectML only");
                None
            }
            Err(e) => return Err(e),
        };
        Ok(Loaded { tok, gpu, cpu })
    })();
    match r {
        Ok(l) => {
            let l = Arc::new(l);
            let d = l.device();
            tracing::info!(
                device = %d,
                threads = opts.threads,
                low_priority = opts.low_priority,
                "hybrid onnx embedder loaded"
            );
            *device.write() = d;
            *g = Some(l.clone());
            Ok(l)
        }
        Err(e) => {
            *device.write() = "unavailable".into();
            Err(e)
        }
    }
}

/// Run one side of `job` with `sess` (no-op when the side has no usable session).
fn work_side(job: &HybridJob, side: Side, sess: Option<&Sess>, low_priority: bool) {
    let Some(s) = sess else {
        return;
    };
    // ORT runs part of every op on the calling thread too.
    let _prio = PriorityGuard::new(low_priority);
    job.work(side, |a, m, b, l| {
        let mut session = s.session.lock();
        hidden(&mut session, s.has_tt, a, m, b, l)
    });
}

impl Embedder for HybridEmbedder {
    fn id(&self) -> &str {
        "onnx"
    }
    fn kind(&self) -> EmbedderKind {
        EmbedderKind::Onnx
    }
    fn device(&self) -> String {
        self.device.read().clone()
    }
    fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a> {
        self.embed_with_cap(texts, crate::MAX_TOKENS)
    }

    fn embed_with_cap<'a>(&'a self, texts: &'a [String], max_tokens: usize) -> EmbedFuture<'a> {
        Box::pin(async move {
            if texts.is_empty() {
                return Ok(Vec::new());
            }
            let n = texts.len();
            let low_priority = self.opts.low_priority;
            let (loaded, ids) = {
                let state = self.state.clone();
                let opts = self.opts.clone();
                let device = self.device.clone();
                let try_gpu = self.try_gpu;
                let texts = texts.to_vec();
                tokio::task::spawn_blocking(move || -> Result<(Arc<Loaded>, Vec<Vec<i64>>)> {
                    let _prio = PriorityGuard::new(opts.low_priority);
                    let loaded = ensure_loaded(&state, &opts, try_gpu, &device)?;
                    let ids = loaded.tok.encode(&texts, opts.max_chars, max_tokens)?;
                    Ok((loaded, ids))
                })
                .await
                .map_err(|e| Error::internal(format!("hybrid onnx worker: {e}")))??
            };
            let gpu_on = Loaded::usable(&loaded.gpu);
            let cpu_on = Loaded::usable(&loaded.cpu);
            let plan = HybridPlan {
                chunk: CPU_BATCH,
                gpu_batch: GPU_BATCH,
                gpu_pad_to: GPU_PAD_TO,
                cpu_pad_to: 1,
                gpu_share: self.share,
            };
            let job = Arc::new(HybridJob::new(ids, plan, gpu_on, cpu_on));
            let spawn = |side: Side| {
                let job = job.clone();
                let loaded = loaded.clone();
                tokio::task::spawn_blocking(move || {
                    let sess = match side {
                        Side::Gpu => loaded.gpu.as_ref(),
                        Side::Cpu => loaded.cpu.as_ref(),
                    }
                    .filter(|s| s.ok.load(Ordering::Relaxed));
                    work_side(&job, side, sess, low_priority);
                })
            };
            let gpu_task = gpu_on.then(|| spawn(Side::Gpu));
            let cpu_task = cpu_on.then(|| spawn(Side::Cpu));
            // A worker that panics marks its side gone (ExitGuard), so join errors are only logged.
            if let Some(t) = gpu_task
                && let Err(e) = t.await
            {
                tracing::warn!(error = %e, "hybrid gpu worker crashed");
            }
            if let Some(t) = cpu_task
                && let Err(e) = t.await
            {
                tracing::warn!(error = %e, "hybrid cpu worker crashed");
            }
            for (side, sess) in [(Side::Gpu, &loaded.gpu), (Side::Cpu, &loaded.cpu)] {
                if job.failed(side)
                    && let Some(s) = sess
                    && s.ok.swap(false, Ordering::Relaxed)
                {
                    tracing::warn!(side = ?side, "hybrid embed: worker disabled for the rest of this rung's life");
                }
            }
            *self.device.write() = loaded.device();
            let (out, split) = job.finish()?;
            *self.split.lock() = split;
            tracing::debug!(
                gpu_items = split.gpu_items,
                cpu_items = split.cpu_items,
                "hybrid embed batch"
            );
            check_output(n, out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths;
    use crate::selftest::{reference_text, reference_vector};
    use std::path::PathBuf;

    const DLL_DML: &str = "D:/Download/onnxruntime/dml-1.24.4/onnxruntime.dll";
    const MODEL_DIR: &str = "D:/Download/models/Xenova-all-MiniLM-L6-v2";

    fn opts() -> Option<OnnxOptions> {
        if !Path::new(DLL_DML).is_file() || paths::onnx_model(Path::new(MODEL_DIR)).is_none() {
            eprintln!("skipping hybrid test: {DLL_DML} or model in {MODEL_DIR} missing");
            return None;
        }
        Some(OnnxOptions {
            dylib: Some(DLL_DML.into()),
            runtime_dir: String::new(),
            home: "D:/tmp".into(),
            model_dir: MODEL_DIR.into(),
            gpu: true,
            threads: 4,
            low_priority: false,
            max_chars: 2000,
        })
    }

    #[test]
    fn share_is_clamped_and_device_starts_unloaded() {
        let o = OnnxOptions {
            dylib: None,
            runtime_dir: String::new(),
            home: PathBuf::from("D:/tmp"),
            model_dir: PathBuf::from("D:/nope"),
            gpu: false,
            threads: 1,
            low_priority: false,
            max_chars: 0,
        };
        let h = HybridEmbedder::new(o.clone(), 5.0);
        assert!((h.share() - 0.9).abs() < 1e-6);
        assert_eq!(h.device(), "not loaded");
        assert_eq!(h.id(), "onnx");
        assert_eq!(h.last_split(), HybridSplit::default());
        assert!((HybridEmbedder::new(o, f32::NAN).share() - 0.7).abs() < 1e-6);
    }

    #[tokio::test]
    async fn missing_model_is_a_clean_error() {
        let h = HybridEmbedder::new(
            OnnxOptions {
                dylib: Some("D:/nope/onnxruntime.dll".into()),
                runtime_dir: "D:/nope".into(),
                home: "D:/nope".into(),
                model_dir: "D:/nope/model".into(),
                gpu: true,
                threads: 1,
                low_priority: true,
                max_chars: 0,
            },
            0.7,
        );
        assert!(h.embed(&["hi".to_string()]).await.is_err());
        assert_eq!(h.device(), "unavailable");
    }

    #[tokio::test]
    #[ignore = "uses the GPU; run with --ignored"]
    async fn hybrid_matches_reference_and_mixed_batch() {
        let Some(o) = opts() else { return };
        let h = HybridEmbedder::new(o, 0.7);
        let dev = h.load().await.unwrap();
        let v = h.embed(&[reference_text().to_string()]).await.unwrap();
        let c = crate::cosine(&v[0], &reference_vector());
        assert!(c >= 0.9999, "cosine {c} on {dev}");
        // reference inside a mixed batch keeps its vector, whichever worker ran it
        let mut mixed: Vec<String> = (0..20).map(|i| "word ".repeat(i * 7 + 1)).collect();
        mixed[9] = reference_text().to_string();
        let out = h.embed(&mixed).await.unwrap();
        assert_eq!(out.len(), 20);
        let c = crate::cosine(&out[9], &reference_vector());
        assert!(c >= 0.9999, "mixed cosine {c} on {dev}");
        let s = h.last_split();
        assert_eq!(s.gpu_items + s.cpu_items, 20, "{s:?}");
        // a bigger batch: both sides should get work when both sessions are up
        let big: Vec<String> = (0..160)
            .map(|i| format!("{} {}", reference_text(), "word ".repeat(i % 40)))
            .collect();
        let out = h.embed(&big).await.unwrap();
        assert_eq!(out.len(), 160);
        let s = h.last_split();
        eprintln!("device {} split {s:?}", h.device());
        if h.device().starts_with("hybrid") {
            assert!(s.gpu_items > 0, "{s:?}");
            assert!(s.cpu_items <= 48 + 8, "{s:?}");
        }
        let c = crate::cosine(&out[0], &v[0]);
        assert!(c >= 0.9999, "big batch cosine {c}");
    }

    #[tokio::test]
    async fn cpu_only_when_gpu_is_unavailable() {
        let Some(mut o) = opts() else { return };
        o.threads = 2;
        o.low_priority = true;
        let mut h = HybridEmbedder::new(o, 0.9);
        h.try_gpu = false;
        let mut mixed: Vec<String> = (0..20).map(|i| "word ".repeat(i * 7 + 1)).collect();
        mixed[9] = reference_text().to_string();
        let out = h.embed(&mixed).await.unwrap();
        let c = crate::cosine(&out[9], &reference_vector());
        assert!(c >= 0.9999, "cosine {c}");
        assert_eq!(h.device(), "cpu (gpu unavailable)");
        assert_eq!(
            h.last_split(),
            HybridSplit {
                gpu_items: 0,
                cpu_items: 20
            }
        );
    }
}
