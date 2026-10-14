//! sf-embed: the SiloForge embedding chain.
//!
//! Every backend produces the same all-MiniLM-L6-v2 sentence vectors: 384-d, mean pooling over the
//! attention mask, L2-normalised, input truncated to `max_chars` characters and 256 word-pieces.
//!
//! * [`RemoteEmbedder`]: OpenAI-compatible `/embeddings` (OpenRouter), retries with backoff.
//! * `OnnxEmbedder` (feature `onnx`, default): ONNX Runtime loaded at runtime (`onnxruntime.dll`),
//!   DirectML EP when asked and available, CPU EP otherwise.
//! * `HybridEmbedder` (feature `onnx`, `embed.mode = "hybrid"`): DirectML and CPU sessions share
//!   each batch through a work-stealing queue.
//! * `CandleEmbedder` (feature `candle`): pure Rust, no DLL, slow.
//! * [`EmbedChain`]: ordered rungs with a circuit breaker, a per-batch deadline and streaming batches.
//! * [`runtime`]: opt-in downloads of the ONNX Runtime DLLs and the model files (SHA-256 verified).

use std::future::Future;
use std::pin::Pin;

use serde::Serialize;
use sf_core::{EMBED_DIM, Error, Result};

mod batching;
pub mod budget;
pub mod chain;
pub mod paths;
pub mod remote;
pub mod runtime;
pub mod selftest;

#[cfg(any(feature = "onnx", feature = "candle"))]
mod tok;

#[cfg(feature = "onnx")]
pub mod onnx;

#[cfg(feature = "onnx")]
pub mod hybrid;

#[cfg(feature = "candle")]
pub mod candle;

pub use batching::HybridSplit;
pub use budget::CpuBudget;
pub use chain::{BatchDone, ChainOptions, EmbedChain, RungStatus};
pub use remote::RemoteEmbedder;
pub use selftest::{SelfTest, reference_text, reference_vector, self_test};

#[cfg(feature = "onnx")]
pub use hybrid::HybridEmbedder;
#[cfg(feature = "onnx")]
pub use onnx::{OnnxEmbedder, OnnxOptions};

#[cfg(feature = "candle")]
pub use candle::CandleEmbedder;

/// Maximum sequence length (word-pieces, including `[CLS]`/`[SEP]`) on every local rung.
pub const MAX_TOKENS: usize = 256;

/// Long word-piece cap (the model's position limit). v1's local rung (transformers.js) truncated
/// at 512, so posts it embedded need 512 to be reproduced (docs/testing/anchor-quality.md).
pub const MAX_TOKENS_LONG: usize = 512;

/// Hard cap on characters fed to a tokenizer when `max_chars` is 0 (off). 256 word-pieces never
/// need more than this, and it bounds tokenizer work on hostile input.
pub const HARD_MAX_CHARS: usize = 32_768;

/// Boxed future returned by [`Embedder::embed`] (keeps the trait object-safe).
pub type EmbedFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<Vec<f32>>>> + Send + 'a>>;

/// Backend family of a rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedderKind {
    Remote,
    Onnx,
    Candle,
}

impl EmbedderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::Onnx => "onnx",
            Self::Candle => "candle",
        }
    }
}

/// One embedding backend.
///
/// Contract: `embed` returns exactly one 384-d, finite, L2-normalised vector per input, in input
/// order. An empty input list returns an empty list. Implementations never block the async runtime.
pub trait Embedder: Send + Sync {
    /// Rung id (`remote`, `remote2`, `onnx`, `local`, ...).
    fn id(&self) -> &str;
    fn kind(&self) -> EmbedderKind;
    /// Where it runs right now (`remote`, `directml`, `cpu`, `not loaded`, ...).
    fn device(&self) -> String {
        self.kind().as_str().to_string()
    }
    fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a>;

    /// Embed with a word-piece cap of `max_tokens` ([`MAX_TOKENS`] or [`MAX_TOKENS_LONG`]).
    /// Backends that cannot change their cap (remote providers) ignore it.
    fn embed_with_cap<'a>(&'a self, texts: &'a [String], max_tokens: usize) -> EmbedFuture<'a> {
        let _ = max_tokens;
        self.embed(texts)
    }
}

/// Prepare one input the same way on every rung: cap at `max_chars` characters (0 = off, then the
/// [`HARD_MAX_CHARS`] cap), and replace empty/whitespace-only text with a single space (v1 behaviour).
pub fn prepare_text(text: &str, max_chars: usize) -> String {
    let cap = if max_chars == 0 {
        HARD_MAX_CHARS
    } else {
        max_chars.min(HARD_MAX_CHARS)
    };
    let cut = match text.char_indices().nth(cap) {
        Some((i, _)) => text.get(..i).unwrap_or(text),
        None => text,
    };
    if cut.trim().is_empty() {
        " ".to_string()
    } else {
        cut.to_string()
    }
}

/// Validate a vector (dim 384, finite, norm > 1e-8) and L2-normalise it in place.
pub fn to_unit_vector(v: &mut [f32]) -> Result<()> {
    if v.len() != EMBED_DIM {
        return Err(Error::upstream(format!(
            "embedding has {} dims, expected {EMBED_DIM}",
            v.len()
        )));
    }
    if v.iter().any(|x| !x.is_finite()) {
        return Err(Error::upstream("embedding contains NaN/inf"));
    }
    let norm = v
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt();
    if norm <= 1e-8 {
        return Err(Error::upstream("embedding has zero norm"));
    }
    for x in v.iter_mut() {
        *x = (f64::from(*x) / norm) as f32;
    }
    Ok(())
}

/// Plain cosine similarity; 0 when either vector is zero or the lengths differ.
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0f64;
    let mut na = 0f64;
    let mut nb = 0f64;
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Validate a whole backend answer: one vector per input, each a unit vector.
pub(crate) fn check_output(n: usize, mut out: Vec<Vec<f32>>) -> Result<Vec<Vec<f32>>> {
    if out.len() != n {
        return Err(Error::upstream(format!(
            "backend returned {} vectors for {n} inputs",
            out.len()
        )));
    }
    for v in &mut out {
        to_unit_vector(v)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_truncates_by_chars_and_fills_empty() {
        assert_eq!(prepare_text("", 10), " ");
        assert_eq!(prepare_text("   \n", 10), " ");
        assert_eq!(prepare_text("héllo wörld", 5), "héllo");
        assert_eq!(prepare_text("abc", 0), "abc");
        let big = "x".repeat(1 << 20);
        assert_eq!(prepare_text(&big, 0).len(), HARD_MAX_CHARS);
        assert_eq!(prepare_text(&big, 2000).len(), 2000);
    }

    #[test]
    fn unit_vector_validation() {
        let mut v = vec![0.0f32; EMBED_DIM];
        assert!(to_unit_vector(&mut v).is_err());
        v[0] = 3.0;
        v[1] = 4.0;
        assert!(to_unit_vector(&mut v).is_ok());
        assert!((v[0] - 0.6).abs() < 1e-6);
        let mut bad = vec![1.0f32; EMBED_DIM];
        bad[5] = f32::NAN;
        assert!(to_unit_vector(&mut bad).is_err());
        assert!(to_unit_vector(&mut [1.0; 3]).is_err());
    }

    #[test]
    fn cosine_edge_cases() {
        assert_eq!(cosine(&[], &[]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert!((cosine(&[1.0, 2.0], &[2.0, 4.0]) - 1.0).abs() < 1e-12);
    }
}
