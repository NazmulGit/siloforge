//! Embedding sanity check for `siloforge doctor`: embed a fixed paragraph and compare it with the
//! vector production (v1, transformers.js) stored for it. A healthy rung gives cosine >= 0.9999.

use std::time::Instant;

use serde::Serialize;
use sf_core::{EMBED_DIM, Error, Result};

use crate::Embedder;

const REF_TEXT: &str = include_str!("../assets/ref_text.txt");
const REF_VECTOR: &str = include_str!("../assets/ref_vector.json");

/// Cosine a healthy backend must reach against the reference.
pub const MIN_COSINE: f64 = 0.9999;

/// The reference paragraph (trimmed).
pub fn reference_text() -> &'static str {
    REF_TEXT.trim()
}

/// The production vector for [`reference_text`] (384 values; empty if the asset is corrupt).
pub fn reference_vector() -> Vec<f32> {
    serde_json::from_str::<Vec<f32>>(REF_VECTOR)
        .ok()
        .filter(|v| v.len() == EMBED_DIM)
        .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize)]
pub struct SelfTest {
    pub rung: String,
    pub device: String,
    pub cosine: f64,
    pub ok: bool,
    pub ms: u64,
}

/// Embed the reference paragraph with one backend and compare.
pub async fn self_test(e: &dyn Embedder) -> Result<SelfTest> {
    let t = Instant::now();
    let v = e.embed(&[reference_text().to_string()]).await?;
    let first = v
        .first()
        .ok_or_else(|| Error::internal("self-test: no vector returned"))?;
    Ok(finish(e.id().to_string(), e.device(), first, t))
}

pub(crate) fn finish(rung: String, device: String, v: &[f32], t: Instant) -> SelfTest {
    let cosine = crate::cosine(v, &reference_vector());
    SelfTest {
        rung,
        device,
        cosine,
        ok: cosine >= MIN_COSINE,
        ms: u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_are_sane() {
        assert!(reference_text().starts_with("Making genuine espresso"));
        let v = reference_vector();
        assert_eq!(v.len(), EMBED_DIM);
        let n: f64 = v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
        assert!((n - 1.0).abs() < 1e-4, "norm {n}");
    }
}
