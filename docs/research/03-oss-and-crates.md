# 03 — Open-source landscape and Rust crate choices for siloforge-rs

Researched 2026-10-13. Versions come from the crates.io API on that date. "Verified" means
**built and linked on this machine** with `cargo +stable-x86_64-pc-windows-gnu` (rustc 1.99.0,
WinLibs GCC 16.2 at `D:\Programs\winlibs\mingw64\bin`), and where stated also in Docker
`rust:1-bookworm` (rustc 1.99.0). The test projects are in `D:\tmp\siloforge-depcheck\`:
`core\` is the full stack, `embedcmp\` is the candle-vs-stored-vector test, `probes\*` hold one
risky crate each. They are scratch: copy `core\Cargo.toml` and `core\Cargo.lock` when the real
workspace is created.

---

## TL;DR

* **Upstream verdict: build our own.** No open-source project does what SiloForge already does
  (self-hosted, multi-site, paragraph-level semantic matching, composite scoring, silos,
  PageRank, injection with undo, MCP). The candidates are single-site WP plugins tied to OpenAI,
  small Python scripts, or general crawlers. Borrow ideas, not code.
* **Embeddings: candle 0.11 (pure Rust, CPU).** Verified on windows-gnu. On our own
  stored vectors, candle's MiniLM output with masked mean pooling and L2 norm matches
  **to cosine 1.0000000 (max abs diff 1.8e-7)**. The existing pgvector data can be kept as is (see §B1 for the truncation caveat).
* **ort / fastembed do not work as a single binary on windows-gnu.** ort has no prebuilt
  ONNX Runtime for `-gnu`, and upstream closed the request as NOT_PLANNED. The only option is
  `load-dynamic` plus shipping `onnxruntime.dll`. Avoid both.
* **Vector search: exact brute force in RAM** (dot product over normalized f32, chunked across
  rayon threads, or one candle matmul per batch). Add `hnsw_rs` (pure Rust, verified) only if
  a single site goes past about 300k paragraphs. usearch compiles with g++, but it is C++ and
  not needed.
* **Storage: SQLite via rusqlite `bundled`** (verified, SQLite 3.53.2, statically linked), with
  vectors in BLOB columns. Postgres plus pgvector stays an optional backend (sqlx 0.9 builds on GNU).
* **TLS: rustls with ring.** reqwest 0.13's **default TLS is now rustls with aws-lc-rs**. It
  builds on GNU through the `cc` builder (no cmake or nasm needed with aws-lc-sys 0.46), but
  ring is smaller and faster to compile. Whichever you pick, call
  `CryptoProvider::install_default()` at startup.
* The whole recommended stack (§C) builds and runs on windows-gnu and on Linux. The Windows exe
  imports **only system DLLs** (no libgcc, libstdc++ or winpthread DLLs).

---

## A. Open-source internal-linking and SEO projects

| Project | URL | License | Stars | Last push | Lang | What it is | Reuse / learn | Upstream? |
|---|---|---|---|---|---|---|---|---|
| **LOOM** (MarcinZmuda/loom) | https://github.com/MarcinZmuda/loom | none declared (GitHub: NOASSERTION) | 5 | 2026-09-21 | PHP | WP plugin, v2.4.0. Uses OpenAI 512-d embeddings, a "two-stage" search (64-d prefilter then 512-d), an **11-dimension composite score**, BFS click depth, internal PageRank, dead-end and bridge detection, GSC "striking distance" data, money pages, and GPT-4o-mini to choose anchors | Good **checklist of scoring dimensions** to compare against ours (GSC position, click depth, money-page boost, dead ends). The README documents a 16-step suggestion pipeline | No. Unlicensed, PHP, single site, needs OpenAI |
| **Internal Link Juicer** (wp.org slug `internal-links`) | https://wordpress.org/plugins/internal-links/ | GPL-2.0+ | 90k installs | 2026-08-13 | PHP | Keyword-to-URL auto-linker (rule based, no semantics) | Ideas for the UX of per-post keyword rules, blacklists and max links per post | No. GPL, PHP, rule based |
| **Link Whisper** (free) | https://wordpress.org/plugins/link-whisper/ | GPL (free tier) / commercial | 30k installs | 2026-10-13 | PHP | Commercial suggestion plugin | Product benchmark only. Note a past authenticated SQL-injection PoC (tahaafarooq/link-whisper-auth-sql-injection-exploit) as a reminder to keep our Connect plugin small | No |
| Interlinks Manager (daext) | https://wordpress.org/plugins/daext-interlinks-manager/ | GPL | 7k installs | 2026-08-08 | PHP | Link stats and auto links | Juice / link-equity UI ideas | No |
| Open-WP-Club/wp-internal-linking | https://github.com/Open-WP-Club/wp-internal-linking | Apache-2.0 | 1 | 2026-10-13 | PHP | Word-usage analysis and relationship map, optional auto-linking | Little | No |
| mfrauf/semantic-internal-linker | https://github.com/mfrauf/semantic-internal-linker | MIT | 0 | 2026-09-20 | Python | Sentence embeddings, cosine suggestions, inline/footer insertion via WP REST | Close to our core idea but a small script. Confirms the WP REST insertion approach | No |
| krysmalskipl/internal-linking | https://github.com/krysmalskipl/internal-linking | MIT | 1 | 2026-10-12 | Python | CMS-agnostic one-command suggestions with HTML/CSV report | Report format idea | No |
| martech-engineer/WebKnoGraph | https://github.com/martech-engineer/WebKnoGraph | Apache-2.0 | 23 | 2026-10-06 | Jupyter/Python | Research: crawl, embeddings, graph algorithms (PageRank, GNN link prediction) for internal linking | **Algorithm ideas** (PageRank-weighted targets, link prediction). Apache-2.0, so ideas are free to use | No (notebooks) |
| sanjuacodez/internal-link-map | https://github.com/sanjuacodez/internal-link-map | (unchecked) | low | 2026 | PHP | WP plugin: link graph plus "missing links", with any OpenAI-compatible endpoint or no key | Graph visual ideas | No |
| Kevinchamplin/champlin-ai-internal-linker | https://github.com/Kevinchamplin/champlin-ai-internal-linker | claims MIT (GitHub: NOASSERTION) | 0 | 2026-07-30 | PHP | OpenAI-embedding suggestions in WP | Little | No |
| ContextBridge (itsbariscan) | https://github.com/itsbariscan/ContextBridge-Semantic-Internal-Link-Tool | none | 4 | 2024-07 | Python | Semantic link script | Little | No |
| **spider-rs/spider** | https://github.com/spider-rs/spider | MIT | 2.8k | 2026-10-01 | Rust | Fast async crawler (crate `spider` 2.53.9), with many features (chrome, cache, cron, agents…) | Possible crawler engine (see §B5). `default-features=false, features=["basic"]` builds on GNU | Usable as a **library dependency**, not as a product upstream |
| SEOnaut | https://github.com/StJudeWasHere/seonaut | MIT | 807 | 2026-10-08 | Go | Open-source SEO audit crawler | List of audit checks (redirect chains, orphan pages, canonical, hreflang) | No (Go) |
| LibreCrawl | https://github.com/PhialsBasement/LibreCrawl | MIT | 1.0k | 2026-09-29 | Python | Screaming Frog alternative (already runs here as the `librecrawl` container) | Crawl-report UX ideas | No |
| advertools | https://github.com/eliasdabbas/advertools | MIT | 1.5k | 2026-06-30 | Python | Crawler, sitemap/robots parsing, SEO analysis | Reference for sitemap index and robots edge cases | No |
| crawl4ai | https://github.com/unclecode/crawl4ai | Apache-2.0 | 85k | 2026-10-13 | Python | LLM-oriented crawler (runs here as `hermes_crawl4ai`) | Optional external fetcher for JS-heavy sites over HTTP. Do not embed it | No |
| christophe-picciotto/seo-spider | https://github.com/christophe-picciotto/seo-spider | MIT | 0 | 2026-10-03 | Rust | Rust and Tauri desktop SEO crawler | Small. Shows a Rust page-weight crawler pattern | No |

GitHub searches for "internal linking" with language Rust found **no relevant Rust project**.

### Verdict

**Build our own; do not fork an upstream.** Reasons:

1. Nothing matches our scope. The most complete open project (LOOM) is unlicensed PHP running
   inside WordPress and is tied to OpenAI. Most others are scripts with fewer than 5 stars.
2. We already have a working spec in production. `app\docs\ARCHITECTURE.md`, `CONTRACTS.md`
   and `LINKBOSS_PARITY.md` describe it, along with ~430k paragraphs and ~332k stored
   vectors in `siloforge_pg`. A port is a re-implementation of our own contracts, not adoption.
3. Licensing: the WP plugins are GPL. Copying their code into a closed single binary is not
   allowed, and borrowing ideas is enough.

What to take from others:
* From LOOM: GSC striking-distance boost, click-depth (BFS) and dead-end/bridge signals,
  a money-page priority, and the anchor-candidate JSON schema for LLM anchor picking.
* From WebKnoGraph: PageRank-weighted target selection and link prediction as an optional
  scorer.
* spider (crate): a possible crawler library, but our own reqwest crawler is simpler (§B5).

---

## B. Crate choices (versions as of 2026-10-13)

Legend for **GNU**: ✅ built and linked on x86_64-pc-windows-gnu here. ✅L also built and ran in
Linux Docker. ⚠ builds with a caveat. ❌ does not work.

### B1. Local embeddings (all-MiniLM-L6-v2, 384-d)

| Crate | Version | License | GNU | Notes |
|---|---|---|---|---|
| **candle-core / candle-nn / candle-transformers** | 0.11.0 | MIT OR Apache-2.0 | ✅L | Pure Rust, `models::bert::BertModel`. Features: `cuda`, `cudnn`, `nccl`, `metal`, `accelerate`, `mkl`, `flash-attn` (all off by default). **GPU:** CUDA works on Linux (Docker `nvidia/cuda` base image). On Windows-GNU, CUDA is not practical because nvcc on Windows needs MSVC `cl.exe` as host compiler. Metal is macOS only. **Use CPU on Windows and an optional `--features cuda` in a Linux GPU image.** |
| tokenizers (HF) | 0.23.2 (use **0.22.x**, see note) | Apache-2.0 | ✅L | Defaults (`onig`, `esaxx_fast`, `progressbar`) compile with WinLibs gcc/g++ through `cc`. No DLLs end up in the exe. `default-features=false, features=["onig"]` is enough for BERT tokenizers. **Note:** candle-core 0.11 depends on tokenizers 0.22.2, so pin `tokenizers = "0.22"` to avoid compiling two copies. |
| hf-hub | 1.0.0 | Apache-2.0 | ✅ | Features `rustls-tls`/`blocking`/`socks`. Pulls reqwest's default rustls with aws-lc. **Recommendation: skip it.** Download the 3 files (`config.json`, `tokenizer.json`, `model.safetensors`, 90.9 MB) with our own reqwest client into the data dir on first run (verify SHA-256), or bundle them in the Docker image and ship them next to the exe |
| safetensors | 0.8.0 | Apache-2.0 | ✅ | Pulled in by candle |
| fastembed | 7.1.1 | Apache-2.0 | ❌ default / ⚠ `ort-load-dynamic` | Default features fail: `ort-sys@2.0.0-rc.13: no prebuilt binaries available for target x86_64-pc-windows-gnu`. With `default-features=false, features=["ort-load-dynamic","hf-hub-rustls-tls"]` it compiles, but needs `onnxruntime.dll` at runtime (an MSVC-built DLL, used only through the C API), so it is not a single binary |
| ort | 2.0.0-rc.14 | MIT OR Apache-2.0 | ❌ (no gnu prebuilts) | pykeio/ort#561 "prebuilt static binaries for windows-gnu" was closed NOT_PLANNED on 2026-04-02 ("ONNX Runtime does not actively support MinGW"). #331: MSVC binaries are ABI-incompatible with `-gnu`. Still RC. MSRV 1.92 |
| tract-onnx | 0.23.8 | MIT OR Apache-2.0 | ✅ | Pure Rust ONNX runtime. Would run the Xenova ONNX export. Slower than candle for BERT and has a big compile. Fallback only |
| model2vec-rs | 0.3.0 | crates.io says "non-standard" (repo MIT) | ✅ | Static distilled embeddings. Very fast, but a **different vector space**, so it is useless for keeping the existing vectors. Could serve as a cheap pre-filter later |

**Does candle match sentence-transformers? Yes, verified on our data.**
`D:\tmp\siloforge-depcheck\embedcmp` loads `sentence-transformers/all-MiniLM-L6-v2`
(`model.safetensors`, F32, CPU), runs `BertModel::forward(ids, type_ids, Some(mask))`, then
masked mean pooling (`sum(h*mask)/sum(mask)`) and L2 normalization (the same code as candle's
`examples/bert`, whose default model is all-MiniLM-L6-v2). It compares the result with vectors
read (read-only) from `siloforge_pg.paragraphs.embedding`:

| Sample | Truncation | Result |
|---|---|---|
| 5 paragraphs, 60–900 chars | 128 or 256 | cosine 0.9999998–1.0000002, max abs diff 1.4e-7–1.8e-7 → **identical (float noise)** |
| 3 paragraphs > 2500 chars | 256 | 2 of 3 cos 1.0000000. The third matches at 512 (cos 0.99978) |
| same | 128 | 0.85–0.94 (wrong) |

Caveats to implement:
1. **Set truncation explicitly to 256 word pieces.** The `tokenizer.json` in the HF repo has
   `truncation.max_length = 128` and fixed padding to 128. sentence-transformers uses
   `max_seq_length = 256` (`sentence_bert_config.json`). Call
   `with_truncation(Some(TruncationParams{max_length:256,..}))` and
   `with_padding(Some(PaddingParams::default()))` (BatchLongest).
2. **Old stored vectors use two different windows.** The Node app's OpenRouter rung uses
   256 tokens. Its local transformers.js rung (`Xenova/all-MiniLM-L6-v2`) truncates at 512.
   The app's own comment (`packages/shared/src/config.ts`) measures cosine 0.88–0.93 between
   the two for multi-KB inputs. Paragraphs under about 1,300 chars are identical either way.
   Plan: use 256 in Rust, and optionally re-embed only paragraphs with more than 256 tokens
   (a one-off job, cheap with candle).
3. Speed on this 24-thread CPU (release build): 5 short paragraphs in about 100 ms. 3 long
   ones take 0.9 s at 256 and 3.0 s at 512. Batch 16–32 and use `spawn_blocking` or a
   dedicated thread pool.
4. Default `token_type_ids` = zeros, model DType F32. Load with
   `VarBuilder::from_mmaped_safetensors` (this needs `unsafe` because of the mmap). Our lint
   rule is no `unsafe`. Alternative: read the file into memory and use
   `VarBuilder::from_buffered_safetensors` (90 MB, safe).

**Recommendation:** candle 0.11 + tokenizers 0.22, CPU by default, plus an optional `cuda`
cargo feature that is built only in the Linux GPU Docker image. Keep the OpenRouter rung
(bit-identical at 256 tokens) as an optional remote fallback with raw reqwest.

### B2. Vector search

| Option | Version | License | GNU | Notes |
|---|---|---|---|---|
| **Brute force (own code)** | — | — | ✅ | Data size: about 430k paragraphs × 384 f32 = 660 MB if everything is in RAM at once. Per site it is far less, so load per site. Vectors are normalized, so a dot product is the cosine. One query over 430k vectors is about 165M multiply-adds, roughly tens of ms. Batch jobs (all posts × all paragraphs): use `candle` matmul (gemm, multithreaded) or rayon chunks plus a top-k heap. Exact results with no index to maintain. Optionally store f16 or i8 to halve or quarter RAM |
| simsimd | 6.5.16 | Apache-2.0 | ✅ | SIMD distance kernels (C via cc). Builds on GNU. Optional speed-up for the brute-force loop. (Successor crate `numkong` 7.8.5 exists.) |
| **hnsw_rs** | 0.3.5 | MIT/Apache-2.0 | ✅ | Pure Rust HNSW with file dump/reload. The recommended ANN if needed |
| instant-distance | 0.6.1 | MIT OR Apache-2.0 | ✅ | Pure Rust HNSW, but **no release since 2023-06**. Skip |
| arroy | 0.8.0 | MIT | ✅ | Meilisearch ANN on **LMDB (heed)**. Adds a second storage engine. Skip |
| usearch | 2.26.4 | Apache-2.0 | ✅ (compiles with WinLibs g++) | C++ core through cxx. It compiled on GNU, but adds C++ and libstdc++ static-link risk for no gain. Skip |
| sqlite-vec | 0.1.9 | MIT/Apache-2.0 | ✅L | SQLite extension (C, compiled by cc, registered with `sqlite3_auto_extension`). Brute-force `vec0` virtual tables in SQL. Still 0.1.x, last release 2026-05. Optional convenience; own brute force is just as fast and simpler |

**Recommendation:** exact brute force in memory, with per-site vector caches loaded from SQLite
BLOBs and invalidated on reindex. Keep `hnsw_rs` behind a feature flag for very large sites.
Drop Qdrant.

### B3. Storage

| Option | Version | License | GNU | Notes |
|---|---|---|---|---|
| **rusqlite** `bundled` | 0.40.2 (libsqlite3-sys 0.38.2, SQLite 3.53.2) | MIT | ✅L | Static, no DLL. Use WAL, `busy_timeout`, and one writer connection plus N readers. Async access through `tokio-rusqlite` 0.8.0 or `spawn_blocking`. Vectors as `BLOB` (384×4 = 1,536 bytes) |
| sqlx (sqlite + postgres) | 0.9.0 | MIT OR Apache-2.0 | ✅ | `default-features=false, features=["runtime-tokio","sqlite","postgres","macros","migrate","tls-rustls-ring-webpki"]` builds (ring only). **MSRV 1.94.** Pick it if both SQLite and Postgres backends are wanted from one API |
| tokio-postgres + pgvector | 0.7.18 / 0.4.2 | MIT / MIT OR Apache-2.0 | ✅ | Builds. Only needed for a "connect to the existing siloforge_pg" migration or import tool |

**Recommendation:** rusqlite bundled as the only runtime store (the single-binary goal).
Write a one-off `siloforge-rs import-pg` command (tokio-postgres + pgvector, or `COPY … TO
STDOUT` over a Postgres URL) to migrate the existing data, vectors included, without
re-embedding.

### B4. Web / auth / crypto

| Crate | Version | License | GNU | Notes |
|---|---|---|---|---|
| axum | 0.8.9 | MIT | ✅L | `macros` feature |
| tower-http | 0.7.1 | MIT | ✅L | Features `fs`, `compression-gzip`, `compression-br`, `cors`, `trace`, `timeout`, `limit` all built |
| tower | 0.5.3 | MIT | ✅ | |
| rust-embed | 8.13.0 | MIT | ✅L | Embeds `ui/dist` in the binary. Use `features=["mime-guess"]`, plus `debug-embed` if you want release-like behaviour in debug builds. Serve it with a fallback handler (SPA: unknown path → `index.html`) |
| jsonwebtoken | 11.1.0 | MIT | ✅L | **Since v10 you must choose a backend**: `default-features=false, features=["rust_crypto","use_pem"]` (pure Rust) or `aws_lc_rs` |
| bcrypt | 0.19.3 | MIT | ✅L | **Verified:** `bcrypt::verify` accepted a real **bcryptjs** hash made by the app's own `node_modules/bcryptjs` (`$2a$10$…`), returned true, rejected a wrong password, and also accepted the same hash with the prefix rewritten to `$2b$`. So existing user passwords can be migrated as is |
| argon2 | 0.6.0 | MIT OR Apache-2.0 | ✅L | Use for new hashes. On login, rehash bcrypt to argon2id |
| aes-gcm | 0.11.1 | Apache-2.0 OR MIT | ✅L | For encrypting WP app passwords and API keys at rest. With rust-version 1.88 the resolver picked `aes 0.9.2` (0.9.3 needs 1.89) |
| axum-extra | 0.12.6 | MIT | — | Cookies and typed headers if needed |
| tower_governor | 0.8.0 | MIT/Apache | — | Rate limiting for the login route (last release 2025-08) |

### B5. HTML and crawler

| Crate | Version | License | GNU | Notes |
|---|---|---|---|---|
| **lol_html** | 3.0.1 | BSD-3-Clause | ✅L | Cloudflare streaming rewriter. **Best fit for link injection**: element/text handlers, `before/after/replace` with `ContentType::Html`, original bytes kept everywhere else (no re-serialization, so WP block comments `<!-- wp:… -->` survive). Text chunks can be split across callbacks, so buffer the text per element before matching an anchor phrase |
| **scraper** | 0.27.0 | ISC | ✅L | Built on html5ever. For read-side extraction (paragraphs, headings, existing links). The existing Rust engine already uses scraper (0.20) |
| html5ever | 0.40.1 | MIT/Apache | ✅ | Comes in through scraper. Do not use it directly |
| ammonia | 4.2.3 | MIT/Apache | — | Sanitize any LLM-produced HTML before injection |
| **reqwest** | 0.13.5 | MIT/Apache | ✅L | **0.13 changed the default TLS to rustls with aws-lc-rs.** Use `default-features=false, features=["rustls-no-provider","json","gzip","brotli","stream","charset","http2"]` plus `rustls` with `ring`. The platform verifier (Windows cert store or Linux webpki) is included |
| spider | 2.53.9 | MIT | ✅ (`basic` only) | Default features include Linux-only things (`io_uring`, `splice`, `numa`, `zero_copy`). With `default-features=false, features=["basic"]` it builds on GNU, but its lockfile pulls reqwest-default (aws-lc) **and native-tls/openssl-sys**. That means libssl-dev in Linux Docker unless you choose its rustls features. **Recommendation: write our own crawler** (bounded frontier, per-host semaphore, robots, sitemaps, ETag/Last-Modified). It is about 400 lines on reqwest, and we mostly crawl WP REST JSON anyway |
| texting_robots | 0.2.2 | MIT OR Apache-2.0 | ✅L | robots.txt parser, fuzzed, used in production by Common Crawl. Last release 2023-03, but stable. The alternative `robotstxt` 0.3.0 (Google port) has not been updated since 2021 |
| sitemap | 0.4.1 | MIT | — | **Unmaintained since 2020. Do not use.** Parse `sitemap.xml` and sitemap index files with **quick-xml 0.42.0** (✅L) in about 80 lines. Handle `.xml.gz` with flate2 |
| url | 2.5.8 | MIT/Apache | ✅L | Normalization and same-host checks |

### B6. NLP

| Crate | Version | License | GNU | Notes |
|---|---|---|---|---|
| tokenizers | 0.22.x (see B1) | Apache-2.0 | ✅L | onig and esaxx compile with WinLibs. The exe imports no extra DLLs |
| rust-stemmers | 1.2.0 | MIT/BSD-3 | ✅L | Snowball stemmers. Old (2019) but stable and very widely used |
| unicode-segmentation | 1.13.3 | MIT/Apache | ✅L | Words and sentences for anchor-phrase windows |
| yake-rust | 1.0.3 | MIT | ✅L | YAKE keyword extraction (last release 2025-02). Good enough for anchor candidates and silo labels |
| rake | 0.3.6 | MIT | — | Older, simpler RAKE. Prefer yake-rust |
| keyword_extraction | 1.5.0 | — | — | TF-IDF, RAKE, TextRank in one crate (2024-10). An alternative to yake-rust |
| stop-words | 0.10.1 | — | — | Stop-word lists |
| aho-corasick | 1.1.5 | MIT/Unlicense | ✅ | Fast multi-pattern matching of anchor phrases in paragraph text |
| whatlang | 0.18.0 | MIT | — | Language detection to pick a stemmer |

### B7. MCP server

* **rmcp 3.5.1** (official `modelcontextprotocol/rust-sdk`, Apache-2.0, MSRV 1.88, 4.0k ★,
  pushed 2026-10-12). It implements MCP spec **2026-07-28** and stays compatible with 2025-11-25.
  ✅L with `features=["server","macros","transport-io","transport-streamable-http-server","schemars"]`.
  3.x has breaking changes compared with 0.x/1.x tutorials (migration guide: discussion #969).
* Transports: `transport-io` (stdio), `transport-streamable-http-server`
  (`StreamableHttpService` is a Tower service you mount with `Router::nest_service("/mcp", svc)`).
  It is stateless for 2026-07-28 clients automatically. Set `.with_legacy_session_mode(false)`
  so older clients are also served statelessly, and `.with_json_response(true)` to get plain
  JSON replies.
* Tool macro (from the README):

```rust
use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router, ServiceExt, transport::stdio};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SuggestParams { site_id: i64, post_id: i64, limit: Option<u32> }

#[derive(Clone)]
struct SiloForgeMcp { state: AppState }

#[tool_router(server_handler)]
impl SiloForgeMcp {
    #[tool(description = "Suggest internal links for a post")]
    async fn suggest_links(&self, Parameters(p): Parameters<SuggestParams>) -> String { /* … */ String::new() }
}
// stdio:  SiloForgeMcp{..}.serve(stdio()).await?.waiting().await?;
// http:   Router::new().nest_service("/mcp", StreamableHttpService::new(move || Ok(mcp.clone()),
//             LocalSessionManager::default().into(), StreamableHttpServerConfig::default()
//             .with_legacy_session_mode(false).with_json_response(true)));
```

The same binary serves both: `siloforge mcp` (stdio, for the Hermes `ap13_internallink`
profile, which today spawns `siloforge-mcp\src\index.mjs`) and `/mcp` on the HTTP port.

### B8. LLM provider clients

| Crate | Version | License | GNU | Notes |
|---|---|---|---|---|
| async-openai | 0.42.2 | MIT | ✅ | OpenAI-typed and very large (dozens of feature-gated APIs). Custom `base_url` works for OpenRouter, Gemini's OpenAI-compatible endpoint and Codex Proxy, but not for Anthropic's native API. Default `rustls` pulls aws-lc |
| genai | 0.6.5 | MIT OR Apache-2.0 | ✅ | Multi-provider (OpenAI, Anthropic, Gemini, Ollama…) with one API. Pre-1.0 with frequent breaking changes. Pulls aws-lc |
| rig-core | 0.44.0 | MIT | — | Agent framework, too heavy. MSRV 1.95 |
| **raw reqwest + serde** | — | — | ✅ | **Recommended.** SiloForge needs exactly 2–3 calls: chat completion with JSON-schema output (anchor picking, silo naming), embeddings (OpenRouter MiniLM fallback), and optionally Anthropic Messages. One `trait LlmProvider { async fn chat_json(...) }` with `OpenAiCompat{base_url,key}` (covers OpenAI, OpenRouter, Gemini OpenAI-compat, Codex Proxy, dario) and `Anthropic`. A `FallbackChain(Vec<Box<dyn LlmProvider>>)` with retry/backoff per rung, mirroring the existing embed chain. That is about 300 lines, no extra TLS stack, and no churn from upstream breaking changes |

### B9. System metrics

* **sysinfo 0.39.6** (MIT, **MSRV 1.95**) ✅L. Per-process CPU and RSS (`System::refresh_processes`),
  total and used RAM, disks. Works on Windows-GNU (it imports pdh/psapi/powrprof, all system DLLs).
  With `rust-version = "1.88"` the resolver warned but still used it, because the actual
  compiler was 1.99. **Set the workspace `rust-version = "1.95"`.**
* **nvml-wrapper 0.13.0** (MIT/Apache) ✅L. It loads `nvml.dll` / `libnvidia-ml.so` at
  **runtime**, so it builds without CUDA. Handle the `NvmlError::LibloadingError` result
  as "no NVIDIA GPU". Put it behind an optional `gpu-metrics` feature (it builds everywhere, so
  it can also stay on by default).

### B10. Scheduling and job queue

| Crate | Version | License | GNU | Notes |
|---|---|---|---|---|
| **croner** | 4.0.1 | MIT | ✅L | Cron expression parser with next-occurrence, timezone aware. A tokio loop with `sleep_until(next)` is enough |
| cron | 0.17.0 | MIT/Apache | — | Alternative parser |
| tokio-cron-scheduler | 0.15.1 | MIT/Apache | ✅ | Works, but last release 2025-10 and it brings its own persistence model. Not needed |
| apalis | 1.0.0 (released 2026-10-12) | MIT/Apache | ✅ | Full job framework with SQLite/Postgres backends. 1.0 is one day old. Too heavy for us |
| **own SQLite queue** | — | — | ✅ | **Recommended.** A `jobs` table (`id, kind, payload_json, status, attempts, run_after, locked_by, locked_until, progress, error`), claimed with `UPDATE … WHERE id=(SELECT … LIMIT 1) RETURNING *` in a single writer transaction, N tokio workers, lease renewal, and cancel/resume. This is what the current BullMQ/worker setup does, minus Redis |

### B11. Notifications

* Webhook, Telegram (`POST https://api.telegram.org/bot<token>/sendMessage`) and ntfy
  (`POST https://ntfy.sh/<topic>`): **raw reqwest**, no crates needed. (teloxide 0.17 is a
  full bot framework and overkill.)
* Windows toast: **notify-rust 4.18.2** (MIT/Apache, MSRV 1.89) ✅L. On Windows it uses
  `tauri-winrt-notification` 0.8.1 (the `windows` crate, WinRT, which builds on GNU). On Linux
  it uses zbus (pure Rust D-Bus). In Docker/headless there is no notification daemon, so treat a
  failure as non-fatal. `winrt-notification` 0.5.1 (2022) is abandoned. Do not use it.

### B12. Backup

| Crate | Version | License | GNU | Notes |
|---|---|---|---|---|
| zip | 9.0.2 | MIT | ✅L | Use `default-features=false, features=["deflate","zstd","aes-crypto","time"]`. The default set adds bzip2, lzma, xz and ppmd, which you don't need. Note that candle-core 0.11 pulls in **zip 8.6.0** as well, so two copies get compiled (harmless) |
| zstd | 0.14.2 (zstd-sys 2.1.1+zstd.1.5.7) | BSD-3 | ✅L | **zstd-sys builds fine on GNU through `cc`** (no cmake) |
| age | 0.12.1 | MIT/Apache | ✅L | Passphrase (scrypt) or X25519 recipient encryption of backup archives. Pure Rust |
| rusqlite `backup` feature | — | — | — | Online `VACUUM INTO 'snapshot.db'` or the backup API for a consistent DB snapshot before zipping |

---

## C. Recommended dependency set (verified together)

Built in release mode on **x86_64-pc-windows-gnu** (rustc 1.99.0, WinLibs GCC 16.2, 4 m 23 s
cold, 631 packages) and on **Linux** (`rust:1-bookworm`, rustc 1.99.0). Both binaries ran:
SQLite 3.53.2, the bcryptjs hash verified, a candle tensor op, sysinfo. The Windows exe imports
only system DLLs. The Linux binary needs only libc, libm and libgcc_s. Lockfile:
`D:\tmp\siloforge-depcheck\core\Cargo.lock`.

```toml
[package]
edition = "2024"
rust-version = "1.95"          # sysinfo 0.39 needs 1.95; sqlx 0.9 needs 1.94

[dependencies]
# runtime / web
tokio            = { version = "1.53", features = ["full"] }
axum             = { version = "0.8.9", features = ["macros"] }
tower-http       = { version = "0.7.1", features = ["fs", "compression-gzip", "compression-br", "cors", "trace", "timeout", "limit"] }
rust-embed       = { version = "8.13", features = ["mime-guess"] }
serde            = { version = "1", features = ["derive"] }
serde_json       = "1"
# auth / crypto
jsonwebtoken     = { version = "11.1", default-features = false, features = ["rust_crypto", "use_pem"] }
argon2           = "0.6"
bcrypt           = "0.19"
aes-gcm          = "0.11"
# http client / TLS (ring only, no aws-lc, no openssl)
reqwest          = { version = "0.13.5", default-features = false, features = ["rustls-no-provider", "json", "gzip", "brotli", "stream", "charset", "http2"] }
rustls           = { version = "0.23.45", default-features = false, features = ["ring", "std", "tls12", "logging"] }
# storage / vectors
rusqlite         = { version = "0.40.2", features = ["bundled", "load_extension"] }   # add "backup" if using the backup API
sqlite-vec       = "0.1.9"        # optional
hnsw_rs          = "0.3.5"        # optional, feature-gated
# embeddings
candle-core         = "0.11"
candle-nn           = "0.11"
candle-transformers = "0.11"
tokenizers          = { version = "0.22", default-features = false, features = ["onig"] }  # tested with 0.23.2; 0.22 dedupes with candle-core
safetensors         = "0.8"
# html / crawl / nlp
lol_html         = "3.0.1"
scraper          = "0.27"
texting_robots   = "0.2.2"
quick-xml        = "0.42"
url              = "2.5"
rust-stemmers    = "1.2"
unicode-segmentation = "1.13"
yake-rust        = "1.0.3"
petgraph         = "0.8"          # PageRank, components (or hand-rolled power iteration)
# mcp
rmcp             = { version = "3.5.1", features = ["server", "macros", "transport-io", "transport-streamable-http-server", "schemars"] }
schemars         = "1.2"
# ops
sysinfo          = "0.39.6"
nvml-wrapper     = "0.13"
croner           = "4.0"
zip              = { version = "9.0.2", default-features = false, features = ["deflate", "zstd", "aes-crypto", "time"] }
zstd             = "0.14.2"
age              = "0.12.1"
notify-rust      = "4.18"
# plumbing
anyhow = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

[features]
cuda = ["candle-core/cuda", "candle-nn/cuda", "candle-transformers/cuda"]   # Linux GPU image only

[profile.release]
codegen-units = 1
lto = "thin"
panic = "abort"   # reconsider: the job runner may want catch_unwind around jobs
strip = true      # GNU builds keep symbols otherwise (the unstripped test exe was 22 MB)
```

Also tested and working on GNU, but not in the recommended set: sqlx 0.9 (sqlite+postgres,
ring), tokio-postgres 0.7.18 + pgvector 0.4.2, tract-onnx 0.23.8, model2vec-rs 0.3.0,
usearch 2.26.4, simsimd 6.5.16, instant-distance 0.6.1, arroy 0.8.0, apalis 1.0.0,
tokio-cron-scheduler 0.15.1, async-openai 0.42.2, genai 0.6.5, hf-hub 1.0.0 (rustls-tls),
spider 2.53.9 (`basic`), aws-lc-rs 1.18.2, reqwest 0.13.5 (defaults), tokenizers 0.23.2 (defaults),
fastembed 7.1.1 (`ort-load-dynamic` only). **Failed:** fastembed 7.1.1 defaults (ort-sys has no
gnu prebuilts).

### GNU toolchain gotchas

1. **Toolchain:** `rustup default` here is `1.88.0-x86_64-pc-windows-msvc`, which cannot link.
   Use `stable-x86_64-pc-windows-gnu` (currently **1.99.0**) through a `rust-toolchain.toml`
   (`channel = "stable"`, `targets`, and on Windows the `-gnu` host) or with
   `cargo +stable-x86_64-pc-windows-gnu`. Put `D:\Programs\winlibs\mingw64\bin` first on PATH.
   Set `CARGO_TARGET_DIR` on D:\.
2. **MSRV:** several current crates need a newer rustc than 1.88: sysinfo 1.95, sqlx 1.94,
   ort 1.92, tract 1.91, notify-rust/uuid/aes 1.89. Do not keep 1.88. If a project pins
   `rust-version` low, the resolver (`incompatible-rust-versions = "fallback"`, the default in
   edition 2024) silently picks older patch versions (we saw aes 0.9.2 and uuid 1.26.1).
3. **WinLibs ships cmake.exe and nasm.exe** in `mingw64\bin`, which can hide a cmake dependency.
   Checked: aws-lc-sys 0.46 printed `Building with: CC` on windows-gnu, so it does not need cmake.
   Still, CI/Docker should not rely on that PATH. Keep the ring-only TLS set.
4. **One rustls provider:** reqwest 0.13 defaults, hf-hub, async-openai, genai and spider all
   enable `rustls/aws-lc-rs`. If both ring and aws-lc end up in the tree, rustls cannot choose
   automatically and panics on the first TLS connection. Always call
   `rustls::crypto::ring::default_provider().install_default()` first thing in `main`, and
   check `cargo tree -i aws-lc-rs` in CI.
5. **Static C/C++ code compiles cleanly with WinLibs:** SQLite, sqlite-vec, onig, esaxx (C++),
   zstd, ring, gemm. The release exe imports no `libgcc_s_seh-1.dll`, `libstdc++-6.dll` or
   `libwinpthread-1.dll` (checked with `objdump -p`). Recheck after adding any C++ crate
   (usearch) and add `-static-libstdc++` / `-static` link args if a DLL shows up.
6. **No `unsafe` lint vs candle mmap:** use `VarBuilder::from_buffered_safetensors`, or confine
   the single `from_mmaped_safetensors` call to a module with `#[allow(unsafe_code)]` and a
   SAFETY comment.
7. **Linux Docker:** `rust:1-bookworm` builder and `debian:bookworm-slim` runtime. The binary
   needs only glibc and libgcc_s, no libssl. For GPU, use a `nvidia/cuda:*-devel` builder with
   `--features cuda` and a `-runtime` image. The model files (91 MB) go in a volume or are
   baked into the image, not into the exe.
8. **Embedding the UI:** rust-embed reads `ui/dist` at compile time. Build the React app first
   (xtask or Dockerfile stage). Without the `debug-embed` feature, debug builds read from disk.

---

## Sources

* crates.io API (`https://crates.io/api/v1/crates/<name>`), queried 2026-10-13, for all versions, licenses and MSRVs
* https://github.com/MarcinZmuda/loom · https://wordpress.org/plugins/internal-links/ · https://wordpress.org/plugins/link-whisper/ · https://wordpress.org/plugins/daext-interlinks-manager/
* https://github.com/Open-WP-Club/wp-internal-linking · https://github.com/mfrauf/semantic-internal-linker · https://github.com/sanjuacodez/internal-link-map · https://github.com/martech-engineer/WebKnoGraph · https://github.com/krysmalskipl/internal-linking
* https://github.com/spider-rs/spider · https://github.com/StJudeWasHere/seonaut · https://github.com/PhialsBasement/LibreCrawl · https://github.com/eliasdabbas/advertools · https://github.com/unclecode/crawl4ai
* https://github.com/huggingface/candle (README; `candle-examples/examples/bert/main.rs` pooling code)
* https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2 (`sentence_bert_config.json`, `tokenizer.json`, `modules.json`)
* https://github.com/pykeio/ort/issues/561 · https://github.com/pykeio/ort/issues/331 · https://ort.pyke.io/setup/linking
* https://github.com/modelcontextprotocol/rust-sdk (README: tools, transports, stateless streamable HTTP)
* https://docs.rs/bcrypt
* Local evidence: `D:\tmp\siloforge-depcheck\{core,embedcmp,probes}`, logs `core.log`, `probes.log`, `linux.log`
