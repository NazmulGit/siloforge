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
use crate::tok::Tok;
use crate::{EmbedFuture, Embedder, EmbedderKind, check_output, paths};

/// Sub-batch size on the CPU EP (sorted small batches minimise padding work).
const CPU_BATCH: usize = 8;
/// Sub-batch size on DirectML.
const GPU_BATCH: usize = 64;
/// Pad sequence length to a multiple of this on DirectML (fewer distinct shapes to compile).
const GPU_PAD_TO: usize = 32;

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
    pub max_chars: usize,
}

impl OnnxOptions {
    pub fn from_config(cfg: &EmbedCfg, home: &Path, threads: usize) -> Self {
        Self {
            dylib: None,
            runtime_dir: cfg.onnx_runtime_dir.clone(),
            home: home.to_path_buf(),
            model_dir: paths::model_dir(&cfg.model_dir, home),
            gpu: cfg.gpu,
            threads: threads.max(1),
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
            tracing::info!(device = d, threads = opts.threads, "onnx embedder loaded");
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

fn base_builder(threads: usize, gpu: bool) -> Result<SessionBuilder> {
    let b = Session::builder()
        .map_err(|e| Error::internal(format!("onnx session builder: {e}")))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(ort_err("optimization level"))?
        .with_intra_threads(threads)
        .map_err(ort_err("threads"))?
        // spinning keeps CPU-EP batch runs ~25% faster; on DirectML it only burns CPU
        .with_intra_op_spinning(!gpu)
        .map_err(ort_err("spinning"))?;
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

fn open(opts: &OnnxOptions) -> Result<State> {
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

    let mut attempt = if opts.gpu {
        base_builder(opts.threads, true)
            .and_then(|mut b| {
                b.commit_from_file(&model)
                    .map_err(|e| Error::internal(format!("onnx directml session: {e}")))
            })
            .map(|s| (s, Device::DirectMl))
            .map_err(|e| {
                tracing::warn!(error = %e, "DirectML unavailable, using the CPU execution provider");
                e
            })
            .ok()
    } else {
        None
    };
    if attempt.is_none() {
        let mut b = base_builder(opts.threads, false)?;
        let s = b
            .commit_from_file(&model)
            .map_err(|e| Error::internal(format!("onnx session {}: {e}", model.display())))?;
        attempt = Some((s, Device::Cpu));
    }
    let (session, device) =
        attempt.ok_or_else(|| Error::internal("onnx session could not be created"))?;
    let has_token_type = session
        .inputs()
        .iter()
        .any(|i| i.name() == "token_type_ids");
    Ok(State {
        session,
        tok,
        device,
        has_token_type,
    })
}

fn hidden(
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
            max_chars: 2000,
        })
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
