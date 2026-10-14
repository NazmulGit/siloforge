# 06 — Embedding backends for all-MiniLM-L6-v2 (Rust, Windows GNU)

Spike date: 2026-10-13. Spike code: `D:\tmp\sf-spike-ort` (single binary, backends behind cargo features).
Baseline: `D:\tmp\sf-spike-embed` (candle 0.11).

Target: 384-d sentence embeddings, mean pooling over the attention mask + L2 normalise, max 256 tokens,
same vectors as production (cosine ≥ 0.9999 vs `D:\tmp\sf-spike-embed\e.txt`).
Machine: Ryzen 9 3900X (12C/24T), Radeon RX 5700 XT, 64 GB, Windows 11, Rust stable GNU + WinLibs (no MSVC).

## TL;DR

| Backend | texts/s (64 real paragraphs, 103–837 chars) | CPU used | cosine vs prod | ships |
|---|---|---|---|---|
| **ort 2.0.0-rc.12 + ONNX Runtime 1.24.4, DirectML EP** (batch 64) | **330–390** (547 with 256 texts, sorted, batch 64) | ~0.3–0.8 core | 1.000000 | exe 6.3 MB + 35 MB DLLs |
| **ort + ORT 1.24.4, CPU EP**, 12 intra threads, sorted, batch 8 | **200–220** quiet machine (70–130 while box was at 100% load from other jobs) | 12–13 cores | 1.000000 | exe 6.3 MB + 14 MB DLL |
| ort CPU, unsorted single batch of 64 | 57–64 | 7–10 cores | 1.000000 | |
| ort CPU, int8 `model_quantized.onnx` | 174 | 11 cores | **0.9907 — fails** | |
| ort DirectML, `model_fp16.onnx` | 384 (no gain) | <1 core | 0.999995 | |
| tract-onnx 0.23.8 + `multithread-mm`, batch 8 sorted | 44 | 6 cores | 1.000000 | pure Rust, ~+20 MB exe |
| tract, single-threaded default | 8–27 | 1 core | 1.000000 | |
| candle 0.11 (baseline), batch 64 | 8.6 (baseline measured 11) | 4–5 cores | 1.000000 | 28 MB exe |
| candle, sorted batch 8/16, `RAYON_NUM_THREADS=12` | 13–16.5 | 2–4 cores | 1.000000 | |
| candle F16 on CPU | 8.5 | | **NaN — broken** | |

Single-text latency: DML 7–11 ms, ORT CPU 9–20 ms, tract ~45 ms, candle ~100 ms.

Impact: the 97,164 paragraphs currently without embedding take ~4 min on DirectML, ~8 min on ORT CPU,
~2.5 h on candle.

`ort` works fine on the GNU toolchain with `load-dynamic` — nothing ONNX-related is linked at build time; the
exe imports only kernel32/ntdll/msvcrt/ws2_32/userenv/bcryptprimitives (checked with `objdump -p`).

Note on numbers: the box was running other agents (Hindsight, Hermes, rustc builds; load 100% for parts of
the run), so CPU numbers are noisy; each figure is best-of-5/10 reps after a warm-up. GPU numbers are
barely affected by CPU load.

## Recommendation

1. **Default backend: `ort` (load-dynamic) + ONNX Runtime DirectML build, DirectML EP with CPU EP fallback
   in the same session.** One DLL set (`onnxruntime.dll` from `Microsoft.ML.OnnxRuntime.DirectML` +
   `DirectML.dll` from `Microsoft.AI.DirectML`) serves both EPs: the DML `onnxruntime.dll` on CPU-only was
   measured at the same speed as the plain CPU build. Register DML *without* `error_on_failure()` so a
   machine with no D3D12 GPU (RDP sessions, helper PCs, VMs) gets the CPU EP (ort logs a warning; this
   path was not tested on a GPU-less box — verify on a helper PC before release).
2. **GPU backend:** DirectML (works on AMD/NVIDIA/Intel, no CUDA/ROCm install). Batch 64, texts sorted by
   token length. fp16 model gives no speed-up on the 5700 XT — use the fp32 `model.onnx` everywhere (one model
   file, identical vectors).
3. **CPU settings:** `intra_threads = physical cores` (12 here), spinning **on** while a batch job is running
   (disabling it cost ~25%: 150 vs 202 texts/s), sort by token length and use **small batches (8)**: padding
   waste dominates — one unsorted batch of 64 is 3–4× slower. 24 threads (SMT) is much slower (36 texts/s,
   oversubscription). For an idle server, keep the session but spinning only matters during runs.
4. **Fallback chain:** `ort DirectML → ort CPU (same DLL) → fail with a clear error`
   (“onnxruntime.dll not found at …; run `siloforge-rs fetch-runtime` or set `SILOFORGE_ORT_DYLIB`”).
   Optional last resort behind a cargo feature: `tract` (pure Rust, 44 texts/s, no DLL). Drop candle for this
   model — it is the slowest option and only ever uses 2–5 cores (only the matmuls are parallel).
5. **Do not** use the int8 quantized model (cos 0.991 — vectors would no longer match the 429k existing
   production embeddings).

### Shipping the DLLs

- **Side-by-side with the exe** (default lookup): `siloforge-rs.exe`, `onnxruntime.dll`, `DirectML.dll`
  in the same folder. Verified: when `onnxruntime.dll` is loaded by full path, it loads `DirectML.dll` from its
  own folder (process module list shows `D:\Download\onnxruntime\dml-1.24.4\DirectML.dll`), not the old
  `C:\Windows\System32\DirectML.dll` (1.8.0, 2022) — still ship it next to the exe to be safe.
  `onnxruntime_providers_shared.dll` is not needed (only for CUDA/TensorRT-style EPs).
- **Path config:** lookup order `--ort-dylib` flag → `SILOFORGE_ORT_DYLIB` env → `<exe dir>\onnxruntime.dll`
  → `<data dir>\runtime\ort-1.24.4-dml\onnxruntime.dll`. Model: `SILOFORGE_EMBED_MODEL` → `<exe dir>\models\all-MiniLM-L6-v2\{model.onnx,tokenizer.json}`.
- **Auto-download (optional `fetch-runtime` subcommand / first run):** fetch the two NuGet packages
  (they are plain zips), extract `runtimes/win-x64/native/onnxruntime.dll` and `bin/x64-win/DirectML.dll`,
  verify SHA-256 (below), write to the runtime dir. Same for the model from Hugging Face. Keep it opt-in
  (servers may be offline); the release zip should simply contain the DLLs.
- **Version pinning:** the DirectML NuGet package stops at **1.24.4** (DirectML is in sustained engineering;
  1.25+ is CPU/other EPs only). `ort` 2.0.0-rc.12 targets ORT 1.24 (`api-24`), so pin
  `ort = "=2.0.0-rc.12"`. rc.13/rc.14 target ORT 1.28/1.31 and would need a CPU-only DLL newer than any
  DirectML build. Re-evaluate when moving to WebGPU EP / Windows ML.
- Licences: ONNX Runtime MIT; DirectML redistributable under the Microsoft DirectML licence (redistribution
  with the app allowed) — include both notices in the release zip.

| File | Source | Size | SHA-256 |
|---|---|---|---|
| `onnxruntime.dll` (DML build) | `Microsoft.ML.OnnxRuntime.DirectML` 1.24.4 nupkg | 17.3 MB | `e7eedec6a6f26dc39dc948276a75ef6d2bee3fff944d874ceed0bbd3b97bff40` |
| `DirectML.dll` | `Microsoft.AI.DirectML` 1.15.4 nupkg, `bin/x64-win` | 18.5 MB | `9c9e6d822561c6c41b90e6994b3e8857cf1d66dbfb1e0c4c799c7c89b4e92da1` |
| `onnxruntime.dll` (CPU-only alt.) | `Microsoft.ML.OnnxRuntime` 1.24.4 nupkg | 14.2 MB | `b95efb2113b603bbbf3f191061c5516a871ed546893c820e4f3b7b6c358dbf2a` |
| `model.onnx` (fp32) | `Xenova/all-MiniLM-L6-v2` `onnx/model.onnx` | 90.4 MB | `759c3cd2b7fe7e93933ad23c4c9181b7396442a2ed746ec7c1d46192c469c46e` |
| `tokenizer.json` | same repo (identical to sentence-transformers) | 0.7 MB | |

nupkg URLs: `https://api.nuget.org/v3-flatcontainer/<id>/<ver>/<id>.<ver>.nupkg` (id lower-case).
Local copies: `D:\Download\onnxruntime\{nupkg,cpu-1.24.4,dml-1.24.4}`, `D:\Download\models\Xenova-all-MiniLM-L6-v2`.

Binary sizes: spike exe with ort only 10.8 MB, **6.3 MB stripped**; with tract+candle 64 MB; candle baseline 28 MB.

## Code

### Cargo.toml

```toml
[dependencies]
ort = { version = "=2.0.0-rc.12", default-features = false, features = ["std", "load-dynamic", "directml", "api-24"] }
tokenizers = { version = "0.22", default-features = false, features = ["fancy-regex"] }
anyhow = "1"

# optional pure-Rust fallback
tract-onnx   = { version = "0.23", optional = true }
tract-linalg = { version = "0.23", features = ["multithread-mm"], optional = true }  # otherwise tract is single-threaded
```

`api-24` is required with `default-features = false` (without it rc.12 fails to compile: missing
`SessionOptionsAppendExecutionProvider_VitisAI`). Do not enable `download-binaries`/`copy-dylibs`.

### Session (ort)

```rust
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::TensorRef;

pub fn open(dylib: &Path, model: &Path, threads: usize, try_gpu: bool) -> anyhow::Result<Session> {
    // load-dynamic: load onnxruntime.dll from an explicit path (side-by-side or configured)
    let _ = ort::init_from(dylib).map_err(|e| anyhow!("{e}"))?.with_name("siloforge").commit();
    let mut b = Session::builder().map_err(|e| anyhow!("{e}"))?
        .with_optimization_level(GraphOptimizationLevel::Level3).map_err(|e| anyhow!("{e}"))?
        .with_intra_threads(threads).map_err(|e| anyhow!("{e}"))?          // = physical cores
        .with_intra_op_spinning(true).map_err(|e| anyhow!("{e}"))?;
    if try_gpu {
        b = b
            .with_parallel_execution(false).map_err(|e| anyhow!("{e}"))?   // DML requirement
            .with_memory_pattern(false).map_err(|e| anyhow!("{e}"))?       // DML requirement
            // no .error_on_failure(): if DML can't start, ORT falls back to the CPU EP
            .with_execution_providers([ort::ep::DirectML::default().build()]).map_err(|e| anyhow!("{e}"))?;
    }
    Ok(b.commit_from_file(model).map_err(|e| anyhow!("{e}"))?)
}

/// returns last_hidden_state flattened [b * l * 384]
fn hidden(s: &mut Session, ids: &[i64], mask: &[i64], b: usize, l: usize) -> anyhow::Result<Vec<f32>> {
    let tt = vec![0i64; ids.len()];
    let shape = [b, l];
    let inputs = ort::inputs![
        "input_ids"      => TensorRef::from_array_view((shape, ids)).map_err(|e| anyhow!("{e}"))?,
        "attention_mask" => TensorRef::from_array_view((shape, mask)).map_err(|e| anyhow!("{e}"))?,
        "token_type_ids" => TensorRef::from_array_view((shape, &tt[..])).map_err(|e| anyhow!("{e}"))?,
    ];
    let out = s.run(inputs).map_err(|e| anyhow!("{e}"))?;
    let (_shape, data) = out[0].try_extract_tensor::<f32>().map_err(|e| anyhow!("{e}"))?;
    Ok(data.to_vec())
}
```

`Session::run` takes `&mut self`: keep one session per embedding worker (a dedicated thread fed by a channel;
DML sessions should not be shared across threads anyway). Session load ≈ 0.3–0.5 s, first DML run with a new
shape 60–500 ms, then cached; a stream with changing shapes (rotating batch composition) lost only ~13%
(322 vs 371 texts/s); rounding the padded length up to a multiple of 32 (`SF_PADTO=32`) recovered most of
it (367 texts/s) — worth doing for the DML path only (on CPU it just adds padding work).

### Tokenise, sort, batch, pool

```rust
let mut tok = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(anyhow::Error::msg)?;
tok.with_padding(None); // we pad per sub-batch ourselves
tok.with_truncation(Some(TruncationParams { max_length: 256, ..Default::default() })).map_err(anyhow::Error::msg)?;

fn embed(s: &mut Session, tok: &Tokenizer, texts: &[String], batch: usize) -> anyhow::Result<Vec<Vec<f32>>> {
    let enc = tok.encode_batch(texts.to_vec(), true).map_err(anyhow::Error::msg)?;
    let mut order: Vec<usize> = (0..texts.len()).collect();
    order.sort_by_key(|&i| enc[i].get_ids().len());            // length bucketing: biggest CPU win
    let mut out = vec![Vec::new(); texts.len()];
    for chunk in order.chunks(batch) {                          // CPU: 8, DirectML: 64
        let l = chunk.iter().map(|&i| enc[i].get_ids().len()).max().unwrap_or(1);
        let b = chunk.len();
        let (mut ids, mut mask) = (vec![0i64; b * l], vec![0i64; b * l]);
        for (r, &i) in chunk.iter().enumerate() {
            for (t, (&id, &m)) in enc[i].get_ids().iter().zip(enc[i].get_attention_mask()).enumerate() {
                ids[r * l + t] = id as i64;
                mask[r * l + t] = m as i64;
            }
        }
        let h = hidden(s, &ids, &mask, b, l)?;
        for (r, v) in pool(&h, &mask, b, l).into_iter().enumerate() { out[chunk[r]] = v; }
    }
    Ok(out)
}

/// mean pooling over attention mask + L2 normalise
fn pool(h: &[f32], mask: &[i64], b: usize, l: usize) -> Vec<Vec<f32>> {
    (0..b).map(|i| {
        let mut v = vec![0f32; 384];
        let mut n = 0f32;
        for t in 0..l {
            if mask[i * l + t] == 0 { continue; }
            n += 1.0;
            for (a, x) in v.iter_mut().zip(&h[(i * l + t) * 384..(i * l + t + 1) * 384]) { *a += x; }
        }
        v.iter_mut().for_each(|x| *x /= n.max(1e-9));
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        v.iter_mut().for_each(|x| *x /= norm);
        v
    }).collect()
}
```

Padding correctness was verified: the reference paragraph embedded inside a padded batch of 64 also gives
cosine 1.000000.

### tract fallback (pure Rust)

```rust
use tract_onnx::prelude::*;
tract_linalg::multithread::set_default_executor(tract_linalg::multithread::Executor::multithread(threads));
let mut m = tract_onnx::onnx().model_for_path(model)?;
let (b, s) = (m.symbols.sym("batch_size"), m.symbols.sym("sequence_length"));
for i in 0..m.input_outlets()?.len() { m.set_input_fact(i, i64::fact([b.to_dim(), s.to_dim()]).into())?; }
let plan: Arc<TypedSimplePlan> = m.into_optimized()?.into_runnable()?;
// run: plan.run(tvec![ids_tensor.into(), mask_tensor.into(), tt_tensor.into()])?
//      out[0].to_plain_array_view::<f32>()  (tract 0.23 renamed as_slice)
```

## Reproduce

```bash
export PATH="/d/Programs/winlibs/mingw64/bin:$PATH" CARGO_TARGET_DIR=/d/tmp/cargo-target/sf-spike-ort
cd /d/tmp/sf-spike-ort
cargo +stable-x86_64-pc-windows-gnu build --release                      # ort only
cargo +stable-x86_64-pc-windows-gnu build --release --features tract,candle
E=$CARGO_TARGET_DIR/release/sf-spike-ort.exe
$E ort-cpu --threads 12 --batch 8 --sort --reps 10       # [--dylib PATH] [--model PATH] [--nospin]
$E ort-dml --batch 64 --sort --reps 10                   # [--dup 4] [--rot]; SF_PADTO=32 env
$E tract   --batch 8 --sort
$E candle  --batch 8 --sort ; $E candle-f16 ...
```

Test texts: `D:\tmp\sf-spike-ort\texts.txt` (64 paragraphs from `siloforge_pg`, read-only query,
length 100–1200, median 324 chars). Each run prints cosine vs the production vector, cosine of the same
paragraph inside a padded batch, single-text latency, texts/s and cores busy (process CPU time / wall).
