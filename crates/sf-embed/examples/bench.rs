//! Throughput through `EmbedChain` (onnx rung only).
//! usage: cargo run --release -p sf-embed --example bench -- <gpu|cpu> [threads] [dup]
//! Env: BENCH_DLL_DIR, BENCH_MODEL_DIR, BENCH_TEXTS (one paragraph per line).
use std::path::Path;
use std::time::Instant;

use sf_core::config::EmbedCfg;
use sf_embed::EmbedChain;

#[tokio::main]
async fn main() -> sf_core::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let gpu = args.get(1).is_some_and(|a| a == "gpu");
    let threads: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(12);
    let dup: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4);
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let cfg = EmbedCfg {
        chain: vec!["onnx".into()],
        model_dir: env(
            "BENCH_MODEL_DIR",
            "D:/Download/models/Xenova-all-MiniLM-L6-v2",
        ),
        onnx_runtime_dir: env("BENCH_DLL_DIR", "D:/Download/onnxruntime/dml-1.24.4"),
        gpu,
        ..EmbedCfg::default()
    };
    let raw = std::fs::read_to_string(env("BENCH_TEXTS", "D:/tmp/sf-spike-ort/texts.txt"))?;
    let base: Vec<String> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect();
    let texts: Vec<String> = (0..dup).flat_map(|_| base.clone()).collect();
    let chain = EmbedChain::from_config(&cfg, Path::new("D:/tmp"), threads);
    let st = chain.self_test().await?;
    println!(
        "self-test: rung={} device={} cosine={:.6} ok={}",
        st.rung, st.device, st.cosine, st.ok
    );
    chain.embed(&texts).await?; // warm-up (shape compilation on DML)
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        let n = chain.embed_batched(&texts, |_| async { Ok(()) }).await?;
        best = best.min(t.elapsed().as_secs_f64());
        assert_eq!(n, texts.len());
    }
    println!(
        "{} texts, batch {}: best {:.3}s = {:.0} texts/s",
        texts.len(),
        chain.options().batch_size,
        best,
        texts.len() as f64 / best
    );
    Ok(())
}
