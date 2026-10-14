# Performance: SiloForge v2 (Rust) vs v1, plan pipeline on real data

Date: 2026-10-13. Author: perf agent. Data: `D:\tmp\sf-test-home` (a migrated copy of all v1 data), copied to a throw-away
bench home for each run. **No crawl, inject, undo, link_health or audit ran. Nothing contacted WordPress or any real site.**
Only `plan` jobs ran (they chunk, embed, extract keywords, build the graph, cluster, score, pick anchors and save), on
the copy only. v1 was read with `SELECT`s and `docker stats --no-stream`, nothing else.

## Setup
- Machine: Ryzen 9 3900X (12C/24T), 64 GB RAM, Radeon RX 5700 XT, Windows 11. **The machine was not idle.** Other
  processes (python, governor, Hermes) used 15-67 % of the CPU during the runs (column "machine" below). Wall times
  vary by about ±30 % because of this. Compare runs of the same row type, and treat single numbers as approximate.
- v2 release build (`cargo build --release -p siloforge`), `serve` on 127.0.0.1:8630. Jobs were enqueued with
  `POST /api/v1/sites/:id/plan {topology: reverse}`, polled with `/api/v1/jobs/run/:id`, and read back with
  `/api/v2/jobs/:id` (+ `/logs`).
- Embeddings: chain `["onnx"]`, ONNX Runtime 1.24.4 DirectML, Xenova all-MiniLM-L6-v2 fp32 (`model.onnx`).
- A PowerShell sampler read the server process once per second: CPU time delta (% of the whole 24-thread machine and
  cores used), working set, private bytes, GPU-engine utilisation for the pid, and total machine CPU.
  Scripts and raw per-second CSVs: `D:\tmp\sf-bench-scripts\` (`run.py`, `sample.ps1`, `runs\*.csv|json`).
- `plan` now writes per-phase wall times to the job log and to `stats.timings_ms` (new in this pass).
- Builds compared: **t1** = HEAD + phase timers only (baseline). **t2** = keywords diff + anchors parallel prep.
  **t3** = t2 + time-ordered row ids + stable cluster ids + skip unchanged graph rows + exact-capacity vector cache.
  **t4** = t3 + length-sorted embedding batches. The "h" builds were made from a clean HEAD worktree plus only these
  changes. Another agent was editing sf-engine anchor code at the same time, so the "h" builds keep the edge output
  directly comparable with t1.

## Runs: site24.example (2,621 posts, 112,706 paragraphs, 14,721 edges) unless noted
CPU % is of the whole machine (24 threads; 4.2 % ≈ one core). RAM = peak working set of the process (MB).

| Run | Build | Wall | Phases (s) | CPU s | CPU avg / peak | RAM peak | Embeds/s | Edges new / rescored |
|---|---|---|---|---|---|---|---|---|
| (a) cold, first v2 plan on migrated data | t1 | 17.3 s | keywords 10.1, anchors 2.7, cluster 1.4, vectors 0.8, save 0.7, graph 0.6 | 23.0 | 4.3 % / 7.8 % | 340 | n/a | 1643 / 6419 |
| (a) cold | t2h | 14.1 s | keywords 8.3, anchors 1.5, cluster 1.2 | 21.6 | 4.9 % / 8.7 % | 440 | n/a | 1643 / 6419 |
| **(a) cold** | **t3h** | **9.1 s** | keywords 4.4, anchors 1.4, vectors 0.9, graph 0.6, save 0.6, cluster 0.25 | **16.5** | 4.9 % / 10.2 % | **295** | n/a | 1643 / 6419 |
| (b) warm re-plan, nothing changed | t1 | 5.6 s | anchors 2.7, cluster 1.3, score 0.3, save 0.3 | 6.4 | 2.6 % / 6.6 % | 426 | n/a | 8 / 8062 |
| **(b) warm** | **t3h** | **3.3-3.9 s** | anchors 1.4, embed check 0.3, score 0.4, save 0.3, cluster 0.25, keywords 0.15 | **5.6-6.1** | 3.0 % / 10.1 % | 330-382 | n/a | 0-8 / 8062-8070 |
| (c) site05.example (1,716 posts), 26,956 paragraphs to embed, GPU | t3h | 121.7 s | embed 117.3, keywords 2.1, anchors 0.7 | 51.4 | 1.7 % / 8.4 % | 585* | 230 | 717 / 6515 |
| **(c) same, GPU** | **t4h** | **43.8 s** | embed 39.0, keywords 2.2, anchors 0.8 | 44.3 | 3.9 % / 9.4 % | 374 | **691** | 717 / 6515 |
| (c) same, warm | t4h | 1.6 s | anchors 0.7, save 0.2 | 1.9 | 1.8 % / 7.5 % | 418 | n/a | 3 / 7232 |
| (d) site24, 5,000 paragraphs re-embedded, **CPU** (6 thr) | t3h | 33.9 s | embed 30.1 | 167.3 | 18.0 % / 29.5 % | 700 | 166 | 0 / 8070 |
| (d) same, CPU (6 thr) | t4h | 28.5 s | embed 24.5 | 131.8 | 16.0 % / 28.7 % | 587 | 204 | 0 / 8070 |
| (d) same, **GPU** | t3h | 34.9 s | embed 30.7 | 20.6 | 2.3 % / 9.1 % | 559 | 163 | 0 / 8070 |
| (d) same, GPU | t4h | 21.1 s | embed 12.9 | 20.0 | 3.2 % / 5.9 % | 587 | 387 | 0 / 8070 |
| (e) 5,000 re-embedded, CPU, `cpu_threads=2` | t4h | 42.1 s | embed 37.3 | 82.7 | 7.4 % / 10.2 % | 592 | 134 | n/a |
| (e) same, `cpu_threads=6` (default = logical/4) | t4h | 28.5 s | embed 24.5 | 131.8 | 16.0 % / 28.7 % | 587 | 204 | n/a |
| (e) same, `cpu_threads=12` | t4h | 26.4 s | embed 22.6 | 230.6 | 29.3 % / 57.6 % | 586 | 221 | n/a |
| (f) **site24, full cold plan with ALL 115,327 texts embedded (GPU)** | t4h | **174.3 s** | embed 161.3, keywords 6.0, anchors 1.7, vectors 1.5, cluster 1.3 | 167.0 | 3.9 % / 10.5 % | 725 | **715** | 4952 / 5914 |

\* (c, t3h) ran with site24's vectors still in the LRU cache. Every other RAM number has one site cached.
With DirectML, private bytes rise to 1.5-1.9 GB because of the driver's GPU-shared/staging commit. The resident
working set stays at 0.4-0.7 GB.

**Edges are sane and unchanged by the optimisations.** I dumped every site24 `link_edges` row (source, target,
anchor, type, paragraph, module, score, status, origin) after cold runs on t1, t2h and t3h. The dumps are identical
(0 differing rows of 16,002). Of the 8,075 suggested edges, 3,442 have a paragraph. In 93.3 % of those the anchor
appears in the paragraph text, and in 100 % of the 193 `native` edges. No blank anchors. The 730 one-word anchors are
older v1 suggestions that `plan` only rescored. Re-embedded paragraph vectors match the v1-stored ones (5,000/5,000
with cos ≥ 0.999999), so the sorted batching does not change the vectors.

## v1 baseline (live v1 DB, read-only)
`job_runs` with `kind='plan'`: 28 done (average 1,462 s, max 9,292 s) and 10 failed (average 13,778 s, max 67,727 s,
mostly stuck in "Embedding paragraphs").

| Site | v1 plan | v1 duration | v2 equivalent | v2 duration |
|---|---|---|---|---|
| site24.example | full plan, 112,706 paragraphs embedded (remote OpenRouter) | **4,019 s** (7 earlier attempts failed after 76 s to 18.8 h) | (f) full cold plan, everything re-embedded on the local GPU | **174 s** |
| site24.example | incremental re-plan (0 embedded, 181-202 scored) | **316-325 s** | (b) warm re-plan (0 embedded, 8,519 scored) | **3.3-3.9 s** |
| site05.example | full plan, 48,329 embedded | **9,292 s** | (c) 26,956 paragraphs embedded + plan | **43.8 s** |

v1 at idle (`docker stats`): worker-1 144 MB, worker-2 115 MB, api 149 MB, pg 1.24 GB, qdrant 227 MB, redis 26 MB,
ui 20 MB, for about **1.9 GB resident with nothing running**. v1 worker containers are capped at 2 CPUs each
(`NanoCpus=2e9`). A v1 incremental site24 plan therefore cost up to about 320 s × 2 cores = **≤ 640 CPU-s** in
the worker, plus Postgres/Qdrant time (not measured). v2 used **5.6-6.1 CPU-s** for the warm re-plan and 16.5 CPU-s
cold.
v2 idle: 26 MB after start. After a plan it is 270-320 MB, because the site's vectors stay in the LRU cache
(`resources.vector_cache_mb`, default 512).

**How comparable this is:** v1's numbers include remote embedding over the internet (16 texts per request, done one
after another) and, for the failed runs, stalls and duplicate runs. v2 embeds locally. v1's incremental runs scored
about 200 candidates. v2 rescores every pending suggestion (about 8.5k) on every run, which is more work, yet it is
still about 100× faster. The machine was busy during the v2 runs and its load is not known for the v1 runs. The order
of magnitude is solid. The exact ratios are not.

## Bottlenecks found and fixed (all in sf-jobs, plus one helper in sf-engine `vector`)
| # | Problem (measured) | Fix | Before → after |
|---|---|---|---|
| 1 | **Keywords phase was the slowest part of a plan** (10-31 s on site24). Each changed post's keywords were deleted and re-inserted. Deletes and random-UUID inserts scatter writes over the 663k-row keyword PK index. | `keywords.rs`: write a diff against the stored rows (delete removed, insert added, `UPDATE` changed scores, skip identical rows). Uses cached statements. Logs extract/write ms and the row diff. | site24 keywords phase 10.1 s → 8.3 s (t2) → **4.4 s** (with #3). Warm 0.15 s. |
| 2 | **Anchors**: HTML split + NLP of every source ran on one thread. The "exact anchors reserved by other targets" set was rebuilt from the whole pool for every edge, which is O(edges × exact anchors). | `anchors.rs`: per batch of 100 sources, split + `ParagraphNlp` run in parallel (`par_iter`, order kept). Only the picks are sequential. Reserved set kept incrementally (`PoolState::add_exact/take_reserved`). | anchors 2.7 s → **1.4 s** (cold and warm). Output identical. |
| 3 | **Random UUIDv4 primary keys** on bulk inserts. Measured on the 1.8 GB DB: 118k keyword inserts took 8.7 s with v4 and 1.7 s with time-ordered ids. | New `pipelines/ids.rs` `row_id()` = UUIDv7 (48-bit ms + the v4's random bits). Still a valid UUID. Used for keyword, paragraph, existing-edge, cluster and plan-edge inserts. | keywords write 6.5 s → **3.0 s** for +113k/~149k/-12k rows |
| 4 | **Clusters rewrote every post row twice per plan.** "Delete all auto clusters" (FK `ON DELETE SET NULL`) was followed by re-assigning every member. A `posts` UPDATE rewrites the whole wide row (about 40 KB on site24: HTML + text + embedding). | `plan.rs::write_clusters`: an auto cluster with an unchanged member set keeps its id. Only stale clusters are deleted and only moved posts are updated (`cluster_id IS NOT ?`). k-means is seeded, so a re-plan of an unchanged site writes no post rows. | cluster 1.2-1.4 s → **0.25 s** |
| 5 | The graph persisted metrics for every post on every run (also wide-row rewrites). | `graph.rs`: `UPDATE ... WHERE id=? AND (any metric IS NOT new)`. | graph warm about 0.2 s, cold 0.6 s (first run still writes) |
| 6 | **Vector cache over-allocated.** The paragraph `Vec<f32>` grew by doubling, so 112,706 × 384 f32 (173 MB) could hold up to 268 MB. | `VectorIndex::with_capacity` + `shrink_to_fit` (sf-engine). `load_site_vectors` counts rows first and reserves exactly. | idle-after-plan private bytes about 310 → 270-280 MB. Cold peak working set 340-440 → **295 MB** |
| 7 | **Embedding batches padded to their longest text.** Rows were fed in id order (random lengths) in chunks of 64, so the GPU and CPU spent most of their time on padding. | `embed.rs`: read 2,048 rows at a time and sort them by text length before the 64-text batches. | GPU 163-230 → **387-715 texts/s** (2.4-3×). CPU (6 threads) 166 → 204 texts/s, 167 → 132 CPU-s per 5k texts. site05 plan 121.7 s → 43.8 s |
| 8 | No per-phase timing. | `plan.rs` `PhaseTimer`: one job-log line with phase timings and `stats.timings_ms`. | n/a |

Tests: `cargo test -p sf-jobs` (6 unit + 14 integration, including the new `replace_keywords_writes_only_the_diff`
and `row_id_is_a_time_ordered_v7_uuid`) and `cargo test -p sf-engine` (146) pass. `cargo clippy -p sf-jobs -p sf-engine
--all-targets -D warnings` is clean.

## Conclusions for the goals
1. **Lower CPU in bulk linking: yes, by a lot.** A full re-plan of the biggest site costs about 6 CPU-s (warm) or
   about 17 CPU-s (cold, first run on migrated data). v1 cost up to about 640 CPU-s plus database time. The plan
   phases themselves never went above about 2.5 cores (10 % of the machine), because most of the remaining time is
   SQLite I/O on one writer thread. Embedding is the only heavy CPU consumer, and it moves to the GPU.
2. **Lower RAM on large sites: yes.** v2 peaks at 0.3-0.45 GB for a site24 plan without embedding, and at
   0.6-0.73 GB working set while embedding with ORT. At idle it holds about 0.3 GB of cached vectors, against about
   1.9 GB idle for the v1 container set. RAM scales with the paragraph count (about 1.6 KB per paragraph in the
   cache), and `vector_cache_mb` bounds what stays resident between jobs.
3. **The CPU budget and priority work.** The rayon pool was 6 threads by default (logical/4). With CPU embeddings, the
   process average follows `cpu_threads`: 2 → 1.8 cores (peak 2.4), 6 → 3.8 cores (peak 6.9), 12 → 7.0 cores (peak
   13.8). Throughput: 134 → 204 → 221 texts/s. 12 threads cost 75 % more CPU for 8 % more speed, so the default of 6
   is the sweet spot. Compute threads log "below-normal priority". ORT's own intra-op threads are not lowered (see
   open items), and their spinning pushes the peak a little above `cpu_threads`.
4. **GPU offload works.** DirectML on the RX 5700 XT was picked automatically (`device="directml"`). After fix #7,
   site24's 115k texts embed in 161 s (715/s) with the process at about 0.9 cores. CPU embedding of 5k texts
   uses 132 CPU-s and GPU embedding uses 20 CPU-s, which is 6-8× less CPU and up to 3.5× faster.

## Open items (not fixed here)
- **ORT thread priority/spinning:** ONNX Runtime's intra-op threads run at normal priority and spin. Consider
  `with_intra_op_spinning(false)` when `low_priority_jobs` is set, and/or a thread-start hook to lower their
  priority (sf-embed).
- **Global `sf_core::new_id()` is UUIDv4.** Other bulk paths would gain from v7 ids, for example link_health,
  keyword_link, crawl, and the v1 migrator. That is the lead's call (sf-core). The sf-jobs helper is `pipelines/ids.rs`.
- **Wide `posts` rows:** every `UPDATE posts` (meta.kw_hash, cluster, metrics, embedding) rewrites about 40 KB of
  HTML/text. Moving `content_html/content_raw/content_text` to a side table would make these updates cheap. That
  needs a schema migration, so it is the sf-store owner's call.
- **Idle vector cache:** after a plan the site's vectors (about 270 MB for site24) stay resident until LRU
  eviction. An idle TTL (for example 15 min) would bring idle RSS back to about 30 MB.
- **v1 post-vector parity (accuracy, not perf):** re-embedded post vectors differ from the v1-stored ones for
  posts whose text is longer than about 8.7k chars (cos 0.92-0.94; short posts match exactly). Paragraph vectors
  match exactly. So on the first v2 re-embed of posts, the clusters and semantic candidates change (run f: 4,952
  new suggestions vs 1,643 with the migrated v1 vectors). v1 probably embedded post text from older content.
  Engine owner to confirm.
- The `embed` phase spends 0.15-0.5 s counting unembedded rows even when there are none (a join over all
  paragraphs). It is minor.

## v2.1 embed modes: hybrid vs GPU vs CPU (2026-10-14)
Setup: `cargo run --release -p sf-embed --example bench -- <mode> 6 <dup> [share]`, 64-paragraph fixture
`D:/tmp/sf-spike-ort/texts.txt` repeated 4x (256 texts) or 8x (512), ORT 1.24.4 DirectML build, all-MiniLM-L6-v2,
RX 5700 XT, 6 CPU threads, best of 5-10 timed runs after a warm-up. The machine was shared with the live production
server, so the numbers are indicative (the CPU-only figure moved between 142 and 240 texts/s between runs).
Hybrid = DirectML session (1 intra-op thread) + CPU session (6 threads) pulling length-sorted chunks of 8 from one queue:
GPU from the long end (up to 64 sequences per run), CPU from the short end, capped at `1 - hybrid_gpu_share` of the items.

| Mode | share | chain batch 64 (texts/s, 2 runs) | chain batch 256 (texts/s, 2-3 runs) | split of one batch (gpu/cpu) |
|---|---|---|---|---|
| cpu  | -    | 142 / 147 / 224 | 227 / 240 / 232 | - |
| gpu  | -    | 395 / 307 / 386 | 568 / 581 / 569 | - |
| hybrid | 0.3  | 369 / 494 | - | 24/40 of 64 |
| hybrid | 0.5  | 538 / 526 / 629 | **706 / 708 / 708** | 32/32 of 64; 136-144/112-120 of 256 |
| hybrid | 0.6  | - | 616 / 594 | 160/96 of 256 |
| hybrid | 0.7  | 426 / 425 / 428 | 600 / 593 | 48/16 of 64; 184/72 of 256 |
| hybrid | 0.85 | 383 / 380 / 385 | - | 56/8 of 64 |

Reference cosine (reference paragraph embedded inside a full benchmark batch) was 1.000000 (>= 0.9999) in every mode.
The `hybrid_matches_reference_and_mixed_batch` test (GPU, `--ignored`) passes: the reference paragraph scores >= 0.9999
alone and inside a mixed 20-text batch, and a 160-text batch splits 128/32.

**Conclusion.** With share 0.5, hybrid is about 1.25-1.4x faster than the GPU alone (706 vs 570 texts/s at batch 256;
about 530-630 vs 310-395 at batch 64). At 0.7 it gains only 5-10 % at batch 256, and at 0.85 it gains nothing. The GPU
always gets the longest texts, so an item share of 0.7 is about 80 % of the tokens, and the GPU becomes the bottleneck
while the CPU waits at its cap. At 0.5 the CPU finishes under its cap (work stealing balances the batch), which is the
best case on this box. The gain costs CPU. In hybrid mode the CPU session uses the whole `cpu_threads` budget for the
whole job, while GPU-only mode uses a fraction of one core (about 6-8x less CPU, see above). Larger chain batches help
every GPU mode (GPU 390 → 570 texts/s from 64 to 256).
**Recommendation:** keep `auto` (= GPU when DirectML works) as the default. That is the best throughput per CPU-second
on a shared server. Offer `hybrid` for one-off bulk re-embeds on an otherwise idle machine, and lower the default
`hybrid_gpu_share` from 0.7 to **0.5** (sf-core default, lead's call). If `embed_batch_size` can be raised to 256 for
local rungs, that gains more than hybrid does at 0.7.
