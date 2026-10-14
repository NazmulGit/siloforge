//! Pure-Rust candle rung (no DLL). Slow (~15 texts/s CPU) but always works.
//!
//! Weights are read into memory (`from_buffered_safetensors`), so no `unsafe` mmap is needed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use parking_lot::{Mutex, RwLock};
use sf_core::config::EmbedCfg;
use sf_core::{Error, Result};

use crate::batching::run_sorted;
use crate::budget::CpuBudget;
use crate::tok::Tok;
use crate::{EmbedFuture, Embedder, EmbedderKind, check_output, paths};

/// Sorted sub-batch size (batch 8/16 was fastest in the spike).
const BATCH: usize = 8;

struct State {
    model: BertModel,
    tok: Tok,
}

pub struct CandleEmbedder {
    dir: PathBuf,
    max_chars: usize,
    budget: CpuBudget,
    /// Private rayon pool of `budget.threads` workers (candle's gemm otherwise uses the global pool
    /// with every logical core at normal priority; R11). Built on first use.
    pool: Arc<Mutex<Option<Arc<rayon::ThreadPool>>>>,
    state: Arc<Mutex<Option<State>>>,
    device: Arc<RwLock<String>>,
}

fn ce(e: candle_core::Error) -> Error {
    Error::internal(format!("candle: {e}"))
}

impl CandleEmbedder {
    /// `dir` holds `config.json`, `tokenizer.json`, `model.safetensors`.
    pub fn new(dir: impl Into<PathBuf>, max_chars: usize) -> Self {
        Self {
            dir: dir.into(),
            max_chars,
            budget: CpuBudget::default(),
            pool: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(None)),
            device: Arc::new(RwLock::new("not loaded".into())),
        }
    }

    pub fn from_config(cfg: &EmbedCfg, home: &Path) -> Self {
        Self::new(paths::model_dir(&cfg.model_dir, home), cfg.max_chars)
    }

    /// Thread count and priority for inference (default: 1 thread, normal priority).
    pub fn with_budget(mut self, budget: CpuBudget) -> Self {
        self.budget = budget;
        self
    }
}

fn compute_pool(
    slot: &Mutex<Option<Arc<rayon::ThreadPool>>>,
    budget: CpuBudget,
) -> Result<Arc<rayon::ThreadPool>> {
    let mut g = slot.lock();
    if let Some(p) = g.as_ref() {
        return Ok(p.clone());
    }
    let low = budget.low_priority;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(budget.threads.max(1))
        .thread_name(|i| format!("sf-candle-{i}"))
        .start_handler(move |_| {
            if low {
                crate::budget::lower_current_thread();
            }
        })
        .build()
        .map_err(|e| Error::internal(format!("candle pool: {e}")))?;
    let pool = Arc::new(pool);
    *g = Some(pool.clone());
    Ok(pool)
}

fn open(dir: &Path) -> Result<State> {
    let cfg_path = dir.join("config.json");
    let cfg: Config = serde_json::from_str(
        &std::fs::read_to_string(&cfg_path)
            .map_err(|e| Error::internal(format!("{}: {e}", cfg_path.display())))?,
    )
    .map_err(|e| Error::internal(format!("{}: {e}", cfg_path.display())))?;
    let w = dir.join("model.safetensors");
    let bytes = std::fs::read(&w).map_err(|e| {
        Error::internal(format!(
            "{}: {e} (run `siloforge fetch-model --candle`)",
            w.display()
        ))
    })?;
    let vb = VarBuilder::from_buffered_safetensors(bytes, DType::F32, &Device::Cpu).map_err(ce)?;
    let model = BertModel::load(vb, &cfg).map_err(ce)?;
    let tok = Tok::load(&paths::tokenizer(dir))?;
    Ok(State { model, tok })
}

fn hidden(m: &BertModel, ids: &[i64], mask: &[i64], b: usize, l: usize) -> Result<Vec<f32>> {
    let d = Device::Cpu;
    let ids: Vec<u32> = ids.iter().map(|&x| u32::try_from(x).unwrap_or(0)).collect();
    let mask: Vec<u32> = mask.iter().map(|&x| u32::from(x != 0)).collect();
    let ids = Tensor::from_vec(ids, (b, l), &d).map_err(ce)?;
    let mask = Tensor::from_vec(mask, (b, l), &d).map_err(ce)?;
    let tt = ids.zeros_like().map_err(ce)?;
    let out = m.forward(&ids, &tt, Some(&mask)).map_err(ce)?;
    out.to_dtype(DType::F32)
        .and_then(|t| t.flatten_all())
        .and_then(|t| t.to_vec1::<f32>())
        .map_err(ce)
}

impl Embedder for CandleEmbedder {
    fn id(&self) -> &str {
        "local"
    }
    fn kind(&self) -> EmbedderKind {
        EmbedderKind::Candle
    }
    fn device(&self) -> String {
        self.device.read().clone()
    }
    fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(async move {
            if texts.is_empty() {
                return Ok(Vec::new());
            }
            let state = self.state.clone();
            let device = self.device.clone();
            let dir = self.dir.clone();
            let max_chars = self.max_chars;
            let budget = self.budget;
            let pool_slot = self.pool.clone();
            let texts = texts.to_vec();
            let n = texts.len();
            let out = tokio::task::spawn_blocking(move || -> Result<Vec<Vec<f32>>> {
                let _prio = crate::budget::PriorityGuard::new(budget.low_priority);
                let pool = compute_pool(&pool_slot, budget)?;
                let mut g = state.lock();
                if g.is_none() {
                    match open(&dir) {
                        Ok(s) => {
                            *device.write() = "cpu".into();
                            *g = Some(s);
                        }
                        Err(e) => {
                            *device.write() = "unavailable".into();
                            return Err(e);
                        }
                    }
                }
                let st = g
                    .as_ref()
                    .ok_or_else(|| Error::internal("candle model missing"))?;
                let ids = st.tok.encode(&texts, max_chars, crate::MAX_TOKENS)?;
                pool.install(|| {
                    run_sorted(&ids, BATCH, 1, |a, m, b, l| hidden(&st.model, a, m, b, l))
                })
            })
            .await
            .map_err(|e| Error::internal(format!("candle worker: {e}")))??;
            check_output(n, out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selftest::{reference_text, reference_vector};

    #[tokio::test]
    async fn candle_reference_cosine() {
        let dir = Path::new("D:/Download/models/all-MiniLM-L6-v2");
        if !dir.join("model.safetensors").is_file() {
            eprintln!("skipping candle test: model missing in {}", dir.display());
            return;
        }
        let e = CandleEmbedder::new(dir, 2000);
        let v = e
            .embed(&[reference_text().to_string(), String::new()])
            .await
            .unwrap();
        assert!(crate::cosine(&v[0], &reference_vector()) >= 0.9999);
    }
}
