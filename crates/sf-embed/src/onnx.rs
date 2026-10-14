//! ONNX Runtime rung (`ort` 2.0.0-rc.12, load-dynamic, ORT 1.24).
//!
//! `onnxruntime.dll` is loaded at runtime from an explicit path (see [`crate::paths::find_dylib`]).
//! With `gpu` the DirectML EP is tried first (`error_on_failure`, so we know which device is
//! active); if it cannot start, a CPU session is built instead. One session is created lazily and
//! reused behind a mutex; tokenisation and inference run on a blocking thread, never on a tokio
//! worker.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ort::session::Session;
use ort::session::builder::{GraphOptimizationLevel, SessionBuilder};
use ort::value::TensorRef;
use parking_lot::{Mutex, RwLock};
use sf_core::config::EmbedCfg;
use sf_core::{Error, Result};

use crate::batching::run_sorted;
use crate::budget::{CpuBudget, LowPriorityThreads, PriorityGuard};
use crate::tok::Tok;
use crate::{EmbedFuture, Embedder, EmbedderKind, check_output, paths};

/// Sub-batch size on the CPU EP (sorted small batches minimise padding work).
pub(crate) const CPU_BATCH: usize = 8;
/// Sub-batch size on DirectML.
pub(crate) const GPU_BATCH: usize = 64;
/// Pad sequence length to a multiple of this on DirectML (fewer distinct shapes to compile).
pub(crate) const GPU_PAD_TO: usize = 32;

#[derive(Debug, Clone)]
pub struct OnnxOptions {
    /// Explicit `onnxruntime.dll` path (highest priority).
    pub dylib: Option<PathBuf>,
    /// `cfg.embed.onnx_runtime_dir` (may be empty).
    pub runtime_dir: String,
    /// SiloForge home (for `$HOME/runtime`).
    pub home: PathBuf,
    /// Directory with `tokenizer.json` and `model.onnx` (or `onnx/model.onnx`).
    pub model_dir: PathBuf,
    /// Try DirectML first.
    pub gpu: bool,
    /// Intra-op threads for the CPU EP (physical cores budget).
    pub threads: usize,
    /// `resources.low_priority_jobs`: no spinning, below-normal ORT and driver threads (R11).
    pub low_priority: bool,
    pub max_chars: usize,
}

impl OnnxOptions {
    pub fn from_config(cfg: &EmbedCfg, home: &Path, threads: usize) -> Self {
        Self::from_config_budget(cfg, home, CpuBudget::new(threads, false))
    }

    pub fn from_config_budget(cfg: &EmbedCfg, home: &Path, budget: CpuBudget) -> Self {
        Self {
            dylib: None,
            runtime_dir: cfg.onnx_runtime_dir.clone(),
            home: home.to_path_buf(),
            model_dir: paths::model_dir(&cfg.model_dir, home),
            gpu: cfg.gpu,
            threads: budget.threads.max(1),
            low_priority: budget.low_priority,
            max_chars: cfg.max_chars,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Device {
    DirectMl,
    Cpu,
}

struct State {
    session: Session,
    tok: Tok,
    device: Device,
    has_token_type: bool,
}

pub struct OnnxEmbedder {
    opts: OnnxOptions,
    state: Arc<Mutex<Option<State>>>,
    device: Arc<RwLock<String>>,
}

impl OnnxEmbedder {
    pub fn new(opts: OnnxOptions) -> Self {
        Self {
            opts,
            state: Arc::new(Mutex::new(None)),
            device: Arc::new(RwLock::new("not loaded".into())),
        }
    }

    pub fn from_config(cfg: &EmbedCfg, home: &Path, threads: usize) -> Self {
        Self::new(OnnxOptions::from_config(cfg, home, threads))
    }

    pub fn from_config_budget(cfg: &EmbedCfg, home: &Path, budget: CpuBudget) -> Self {
        Self::new(OnnxOptions::from_config_budget(cfg, home, budget))
    }

    /// Load the runtime + model now (normally lazy). Returns the active device.
    pub async fn load(&self) -> Result<String> {
        let state = self.state.clone();
        let opts = self.opts.clone();
        let device = self.device.clone();
        tokio::task::spawn_blocking(move || {
            let mut g = state.lock();
            ensure_loaded(&mut g, &opts, &device)?;
            Ok(device.read().clone())
        })
        .await
        .map_err(|e| Error::internal(format!("onnx worker: {e}")))?
    }
}

fn ensure_loaded(
    slot: &mut Option<State>,
    opts: &OnnxOptions,
    device: &RwLock<String>,
) -> Result<()> {
    if slot.is_some() {
        return Ok(());
    }
    match open(opts) {
        Ok(s) => {
            let d = match s.device {
                Device::DirectMl => "directml",
                Device::Cpu if opts.gpu => "cpu (gpu unavailable)",
                Device::Cpu => "cpu",
            };
            tracing::info!(
                device = d,
                threads = opts.threads,
                low_priority = opts.low_priority,
                "onnx embedder loaded"
            );
            *device.write() = d.to_string();
            *slot = Some(s);
            Ok(())
        }
        Err(e) => {
            *device.write() = "unavailable".into();
            Err(e)
        }
    }
}

fn ort_err(ctx: &str) -> impl Fn(ort::Error<SessionBuilder>) -> Error + '_ {
    move |e| Error::internal(format!("onnx {ctx}: {e}"))
}

/// Whether the intra-op pool may busy-wait between ops. Spinning keeps CPU-EP batch runs ~25%
/// faster, but on DirectML it only burns CPU, and with `low_priority_jobs` it would hold cores the
/// desktop wants.
fn spinning(gpu: bool, low_priority: bool) -> bool {
    !gpu && !low_priority
}

fn base_builder(threads: usize, gpu: bool, low_priority: bool) -> Result<SessionBuilder> {
    let b = Session::builder()
        .map_err(|e| Error::internal(format!("onnx session builder: {e}")))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(ort_err("optimization level"))?
        .with_intra_threads(threads)
        .map_err(ort_err("threads"))?
        .with_intra_op_spinning(spinning(gpu, low_priority))
        .map_err(ort_err("spinning"))?
        .with_inter_op_spinning(spinning(gpu, low_priority))
        .map_err(ort_err("inter spinning"))?;
    let b = if low_priority {
        // ORT's own pool threads start below normal (the session's pool is per-session, so the
        // custom thread hooks apply to it).
        b.with_thread_manager(LowPriorityThreads)
            .map_err(ort_err("thread manager"))?
    } else {
        b
    };
    if !gpu {
        return Ok(b);
    }
    b.with_parallel_execution(false)
        .map_err(ort_err("execution mode"))?
        .with_memory_pattern(false)
        .map_err(ort_err("memory pattern"))?
        .with_execution_providers([ort::ep::DirectML::default().build().error_on_failure()])
        .map_err(ort_err("directml"))
}

/// Load `onnxruntime.dll` (once per process) and the tokenizer; returns the model path.
pub(crate) fn prepare(opts: &OnnxOptions) -> Result<(PathBuf, Tok)> {
    let dylib = paths::find_dylib(opts.dylib.as_deref(), &opts.runtime_dir, &opts.home)
        .map_err(Error::internal)?;
    let model = paths::onnx_model(&opts.model_dir).ok_or_else(|| {
        Error::internal(format!(
            "model.onnx not found in {} (run `siloforge fetch-model` or set {})",
            opts.model_dir.display(),
            paths::ENV_EMBED_MODEL
        ))
    })?;
    let tok = Tok::load(&paths::tokenizer(&opts.model_dir))?;
    // Process-global: the first successfully loaded library stays for the process lifetime.
    let _ = ort::init_from(&dylib)
        .map_err(|e| Error::internal(format!("load {}: {e}", dylib.display())))?
        .with_name("siloforge")
        .commit();
    Ok((model, tok))
}

/// One session: DirectML (`gpu`, fails instead of silently falling back) or the CPU EP.
pub(crate) fn session(
    model: &Path,
    threads: usize,
    gpu: bool,
    low_priority: bool,
) -> Result<Session> {
    let mut b = base_builder(threads, gpu, low_priority)?;
    b.commit_from_file(model).map_err(|e| {
        let what = if gpu { "directml session" } else { "session" };
        Error::internal(format!("onnx {what} {}: {e}", model.display()))
    })
}

/// Does the model take `token_type_ids`?
pub(crate) fn has_token_type(s: &Session) -> bool {
    s.inputs().iter().any(|i| i.name() == "token_type_ids")
}

fn open(opts: &OnnxOptions) -> Result<State> {
    let (model, tok) = prepare(opts)?;
    let attempt = if opts.gpu {
        session(&model, opts.threads, true, opts.low_priority)
            .map_err(|e| {
                tracing::warn!(error = %e, "DirectML unavailable, using the CPU execution provider");
                e
            })
            .ok()
            .map(|s| (s, Device::DirectMl))
    } else {
        None
    };
    let (session, device) = match attempt {
        Some(x) => x,
        None => (
            session(&model, opts.threads, false, opts.low_priority)?,
            Device::Cpu,
        ),
    };
    let has_token_type = has_token_type(&session);
    Ok(State {
        session,
        tok,
        device,
        has_token_type,
    })
}

pub(crate) fn hidden(
    s: &mut Session,
    has_tt: bool,
    ids: &[i64],
    mask: &[i64],
    b: usize,
    l: usize,
) -> Result<Vec<f32>> {
    let e = |e: ort::Error| Error::internal(format!("onnx run: {e}"));
    let shape = [b, l];
    let tt = vec![0i64; ids.len()];
    let mut inputs = ort::inputs![
        "input_ids" => TensorRef::from_array_view((shape, ids)).map_err(e)?,
        "attention_mask" => TensorRef::from_array_view((shape, mask)).map_err(e)?,
    ];
    if has_tt {
        inputs.push((
            "token_type_ids".into(),
            TensorRef::from_array_view((shape, &tt[..]))
                .map_err(e)?
                .into(),
        ));
    }
    let out = s.run(inputs).map_err(e)?;
    if out.len() == 0 {
        return Err(Error::internal("onnx run: no outputs"));
    }
    let (_shape, data) = out[0].try_extract_tensor::<f32>().map_err(e)?;
    Ok(data.to_vec())
}

impl Embedder for OnnxEmbedder {
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
            let state = self.state.clone();
            let opts = self.opts.clone();
            let device = self.device.clone();
            let texts = texts.to_vec();
            let n = texts.len();
            let out = tokio::task::spawn_blocking(move || -> Result<Vec<Vec<f32>>> {
                // ORT runs part of every op on the calling thread too.
                let _prio = PriorityGuard::new(opts.low_priority);
                let mut g = state.lock();
                ensure_loaded(&mut g, &opts, &device)?;
                let st = g
                    .as_mut()
                    .ok_or_else(|| Error::internal("onnx session missing"))?;
                let ids = st.tok.encode(&texts, opts.max_chars, max_tokens)?;
                let (batch, pad_to) = match st.device {
                    Device::DirectMl => (GPU_BATCH, GPU_PAD_TO),
                    Device::Cpu => (CPU_BATCH, 1),
                };
                let has_tt = st.has_token_type;
                let session = &mut st.session;
                run_sorted(&ids, batch, pad_to, |a, m, b, l| {
                    hidden(session, has_tt, a, m, b, l)
                })
            })
            .await
            .map_err(|e| Error::internal(format!("onnx worker: {e}")))??;
            check_output(n, out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selftest::{reference_text, reference_vector};

    const DLL_DML: &str = "D:/Download/onnxruntime/dml-1.24.4/onnxruntime.dll";
    const MODEL_DIR: &str = "D:/Download/models/Xenova-all-MiniLM-L6-v2";

    fn opts(gpu: bool) -> Option<OnnxOptions> {
        if !Path::new(DLL_DML).is_file() || paths::onnx_model(Path::new(MODEL_DIR)).is_none() {
            eprintln!("skipping onnx test: {DLL_DML} or model in {MODEL_DIR} missing");
            return None;
        }
        Some(OnnxOptions {
            dylib: Some(DLL_DML.into()),
            runtime_dir: String::new(),
            home: "D:/tmp".into(),
            model_dir: MODEL_DIR.into(),
            gpu,
            threads: 4,
            low_priority: false,
            max_chars: 2000,
        })
    }

    #[test]
    fn spinning_only_on_cpu_at_normal_priority() {
        assert!(spinning(false, false));
        assert!(!spinning(true, false));
        assert!(!spinning(false, true));
        assert!(!spinning(true, true));
    }

    #[test]
    fn budget_flows_into_options() {
        let cfg = EmbedCfg::default();
        let o = OnnxOptions::from_config_budget(&cfg, Path::new("D:/tmp"), CpuBudget::new(3, true));
        assert_eq!(o.threads, 3);
        assert!(o.low_priority);
        let o = OnnxOptions::from_config(&cfg, Path::new("D:/tmp"), 0);
        assert_eq!(o.threads, 1);
        assert!(!o.low_priority);
    }

    #[tokio::test]
    async fn low_priority_cpu_session_matches_reference() {
        let Some(mut o) = opts(false) else { return };
        o.threads = 2;
        o.low_priority = true;
        let e = OnnxEmbedder::new(o);
        let v = e.embed(&[reference_text().to_string()]).await.unwrap();
        let c = crate::cosine(&v[0], &reference_vector());
        assert!(c >= 0.9999, "cosine {c}");
    }

    #[tokio::test]
    async fn missing_runtime_is_a_clean_error() {
        let e = OnnxEmbedder::new(OnnxOptions {
            dylib: Some("D:/nope/onnxruntime.dll".into()),
            runtime_dir: "D:/nope".into(),
            home: "D:/nope".into(),
            model_dir: MODEL_DIR.into(),
            gpu: false,
            threads: 1,
            low_priority: true,
            max_chars: 0,
        });
        // may succeed if a dll sits next to the test exe or SILOFORGE_ORT_DYLIB is set; must not panic
        let r = e.embed(&["hi".to_string()]).await;
        if let Err(err) = r {
            assert!(!err.to_string().is_empty());
        }
    }

    #[tokio::test]
    async fn reference_cosine_cpu_and_hostile() {
        let Some(o) = opts(false) else { return };
        let e = OnnxEmbedder::new(o);
        let v = e.embed(&[reference_text().to_string()]).await.unwrap();
        let c = crate::cosine(&v[0], &reference_vector());
        assert!(c >= 0.9999, "cosine {c}");
        assert!(e.device().starts_with("cpu"));
        let hostile = vec![
            String::new(),
            "   ".into(),
            "x".repeat(1 << 20),
            "é".repeat(5000),
        ];
        let out = e.embed(&hostile).await.unwrap();
        assert_eq!(out.len(), 4);
        // reference inside a mixed batch keeps its vector (padding correctness)
        let mut mixed: Vec<String> = (0..20).map(|i| "word ".repeat(i * 7 + 1)).collect();
        mixed[9] = reference_text().to_string();
        let out = e.embed(&mixed).await.unwrap();
        assert!(crate::cosine(&out[9], &reference_vector()) >= 0.9999);
    }

    #[tokio::test]
    #[ignore = "uses the GPU; run with --ignored"]
    async fn reference_cosine_directml() {
        let Some(o) = opts(true) else { return };
        let e = OnnxEmbedder::new(o);
        let dev = e.load().await.unwrap();
        let v = e.embed(&[reference_text().to_string()]).await.unwrap();
        let c = crate::cosine(&v[0], &reference_vector());
        assert!(c >= 0.9999, "cosine {c} on {dev}");
    }
}
