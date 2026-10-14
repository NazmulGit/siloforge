//! Throughput through `EmbedChain` (onnx rung only).
//! usage: cargo run --release -p sf-embed --example bench -- <gpu|cpu|hybrid> [threads] [dup] [share]
//! Env: BENCH_DLL_DIR, BENCH_MODEL_DIR, BENCH_TEXTS (one paragraph per line), BENCH_BATCH (chain
//! batch size, default 64), BENCH_RUNS (timed runs, default 5).
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use sf_core::config::EmbedCfg;
use sf_embed::{CpuBudget, EmbedChain, Embedder, HybridEmbedder, reference_text};

#[tokio::main]
async fn main() -> sf_core::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).cloned().unwrap_or_else(|| "cpu".into());
    let threads: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(12);
    let dup: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4);
    let share: f32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0.7);
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let batch: usize = env("BENCH_BATCH", "64").parse().unwrap_or(64);
    let runs: usize = env("BENCH_RUNS", "5").parse().unwrap_or(5);
    let cfg = EmbedCfg {
        chain: vec!["onnx".into()],
        model_dir: env(
            "BENCH_MODEL_DIR",
            "D:/Download/models/Xenova-all-MiniLM-L6-v2",
        ),
        onnx_runtime_dir: env("BENCH_DLL_DIR", "D:/Download/onnxruntime/dml-1.24.4"),
        gpu: mode != "cpu",
        mode: mode.clone(),
        hybrid_gpu_share: share,
        batch_size: batch,
        ..EmbedCfg::default()
    };
    let raw = std::fs::read_to_string(env("BENCH_TEXTS", "D:/tmp/sf-spike-ort/texts.txt"))?;
    let base: Vec<String> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect();
    let texts: Vec<String> = (0..dup).flat_map(|_| base.clone()).collect();
    let home = Path::new("D:/tmp");
    // hybrid: build the rung directly so the split can be read back
    let hybrid = (mode == "hybrid").then(|| {
        Arc::new(HybridEmbedder::from_config_budget(
            &cfg,
            home,
            CpuBudget::new(threads, false),
        ))
    });
    let chain = match &hybrid {
        Some(h) => EmbedChain::new(
            vec![h.clone() as Arc<dyn Embedder>],
            sf_embed::ChainOptions::from_config(&cfg),
        ),
        None => EmbedChain::from_config(&cfg, home, threads),
    };
    let st = chain.self_test().await?;
    println!(
        "mode={mode} threads={threads} share={share} self-test: rung={} device={} cosine={:.6} ok={}",
        st.rung, st.device, st.cosine, st.ok
    );
    chain.embed(&texts).await?; // warm-up (shape compilation on DML)
    let mut best = f64::MAX;
    for _ in 0..runs.max(1) {
        let t = Instant::now();
        let n = chain.embed_batched(&texts, |_| async { Ok(()) }).await?;
        best = best.min(t.elapsed().as_secs_f64());
        assert_eq!(n, texts.len());
    }
    let split = hybrid
        .as_ref()
        .map(|h| {
            let s = h.last_split();
            format!(" split(last batch) gpu={} cpu={}", s.gpu_items, s.cpu_items)
        })
        .unwrap_or_default();
    // vectors still match the reference inside the benchmark batch
    let mut probe = texts.clone();
    probe.truncate(batch.saturating_sub(1).max(1));
    probe.push(reference_text().to_string());
    let v = chain.embed(&probe).await?;
    let cos = v
        .last()
        .map_or(0.0, |x| sf_embed::cosine(x, &sf_embed::reference_vector()));
    println!(
        "{} texts, batch {}, device {}: best {:.3}s = {:.0} texts/s, ref cosine in batch {:.6}{split}",
        texts.len(),
        chain.options().batch_size,
        chain.active_device(),
        best,
        texts.len() as f64 / best,
        cos
    );
    Ok(())
}
