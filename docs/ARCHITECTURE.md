# SiloForge v2 architecture

One process (`siloforge serve`) holds the HTTP API, the embedded UI, the MCP endpoint, the job runner, the scheduler,
the system monitor, the notifier and the backup scheduler. State is one directory (`SILOFORGE_HOME`). Decision record:
[adr/0001-single-binary-sqlite.md](adr/0001-single-binary-sqlite.md). Specs: [research/01](research/01-engine-spec.md)
(engine), [research/02](research/02-backend-spec.md) (v1 backend), [research/06](research/06-embedding-backends.md) (embeddings).

## 1. Crates

Lower crates never depend on upper ones.

| Crate | Role |
|---|---|
| `sf-core` | Config (TOML + `secrets.env` + env, v1 names honoured), errors, ids (UUID), time, enums with `Unknown(String)`, crypto: HS256 JWT, bcrypt, AES-256-GCM `v1:` blobs (v1-compatible), API token hashing, per-site secrets |
| `sf-store` | SQLite schema (`migrations/0001..`), `Db` = one writer thread + reader pool, `Db::read` / `Db::write` hop off the async runtime, row -> JSON helpers |
| `sf-engine` | Pure algorithms, no IO: HTML tokenizer/splitter, NLP (keywords, stems, intent, noun phrases), anchors and guards, block-aware insertion/removal + verify, LLM output validation, keyword linker, graph metrics, k-means, silo topologies, 11-dim scorer, density, vector search (SIMD, int8), site metrics, the rayon pool |
| `sf-embed` | `Embedder` trait and `EmbedChain`: remote (OpenAI-compatible) -> ONNX Runtime (DirectML / CPU, loaded dynamically) -> candle (optional feature); runtime/model downloads with pinned SHA-256 |
| `sf-ai` | LLM providers (OpenAI-compatible, OpenRouter, Anthropic, Gemini, Ollama, custom), chains site -> global -> env, retries, circuit breaker, deadlines, call log, probes |
| `sf-crawl` | HTTP client (rustls, body caps, per-host politeness, 429/503 backoff), SSRF guard, WP REST crawler (`context=edit`), sitemap + robots, link-health checker, WP writer with optimistic concurrency, live verify |
| `sf-jobs` | Queue on `job_runs`, runner, scheduler, pipelines (crawl, plan, inject, undo, keyword_link, link_health, audit, embed, backup, maintenance), per-site vector cache |
| `sf-ops` | Monitor (sysinfo), notifier + channels, error tracker, backup/restore, retention, instance pid file, v1 Postgres migrator |
| `sf-mcp` | MCP tools (rmcp): REST client, 28 tools, stdio and streamable-HTTP router |
| `sf-server` | axum router: `/api/v1` (contract), `/api/v2`, `/health`, auth extractors, audit, embedded UI (`rust-embed` of `ui/dist`) |
| `siloforge` | The binary: CLI and the `serve` wiring |
| `ui/` | React + Vite UI (from v1, plus System, All jobs, Notifications, Backups, API tokens, Integrations) |
| `xtask` | `cargo xtask ci | ui | release` |

## 2. Request and data flow

```
 UI (browser) / Hermes (MCP) / scripts (API token) / WP plugin (site JWT)
        │ HTTP 127.0.0.1:8600|8601
        ▼
 sf-server ── auth (JWT re-checked vs users row, API token scopes, site token) ── ownership gate (404 on foreign)
        │ reads: Db::read (reader pool)            │ mutations: Db::write (writer thread) + audit_log
        │                                          │ long work: enqueue job_runs row (dedupe) → 202 {job_run_id}
        ▼                                          ▼
     JSON (v1 shapes)                     sf-jobs runner (claims ≤ max_concurrent_jobs, per-site lock)
                                                   │
                     ┌─────────────────────────────┼───────────────────────────────┐
                     ▼                             ▼                               ▼
              IO on tokio                  CPU on the rayon pool            persistence in batches
     sf-crawl / sf-embed / sf-ai     sf-engine (split, NLP, score,       Db::write, thousands of rows
     (bounded concurrency)           cluster, anchors), budgeted by       per transaction
                                     resources.cpu_threads, below-normal
                                                   │
                                                   ▼
                    notifications (sf-ops notifier) · job_logs · resources · error tracker
```

Rules from AGENTS.md that shape the code: no blocking in async (DB and CPU work hop threads), no `unwrap`/`panic`
outside tests, no `unsafe` (except a documented model mmap in `sf-embed`), determinism (ties sorted by id, seeded PRNGs).

## 3. Job system

- **Queue:** the `job_runs` table (v1's, extended with `payload`, `attempts`, `max_attempts`, `priority`,
  `requested_by`, `heartbeat_at`, `resources`). No Redis.
- **Guarantees:** dedupe of identical queued jobs; a per-site mutex for mutating kinds (crawl, plan, inject, undo,
  keyword_link, audit); at most `resources.max_concurrent_jobs` running; heavy kinds deferred while machine CPU is above
  `defer_when_cpu_above`.
- **Liveness:** heartbeat every 15 s; the reaper (at boot and every 300 s) re-queues or fails stale `running` rows;
  hard timeout per kind (crawl/plan 2 h, inject 1 h, link_health 3 h, audit 6 h, else 30 min); panics in a job mark
  it failed and the runner survives.
- **Cancel:** `cancelled` is polled at checkpoints (at most once per second); a job is aborted after a 30 s grace.
  Finishing never overwrites a cancelled/reaped row.
- **Resources:** each run records wall time, CPU seconds, average CPU and peak RSS (`JobMeter`), plus phase timings for
  `plan` (`stats.timings_ms`) and log lines in `job_logs`.
- **Scheduler:** `site_schedules` with cron + IANA time zone (croner), compare-and-set on `next_run_at`, missed runs
  fire once, ticks every 60 s.

### Main pipelines
| Kind | Steps |
|---|---|
| crawl | WP REST (raw `context=edit` with the site's application password, rendered-only fallback) or sitemap -> upsert posts by content hash -> sync existing body links |
| plan | chunk changed posts (incremental by `chunked_hash`) -> embed missing vectors (batched by length, persisted per batch) -> keywords (diff write) -> graph metrics -> k-means clusters + pillars -> silo topology + semantic + archive candidates (skipping every existing pair) -> 11-dim score -> density after scoring -> anchors (parallel NLP, guards) -> save |
| inject | per post: fresh raw content -> splice links at hash-checked offsets (native) or validated LLM text -> snapshot -> one WP update with `modified_gmt` precondition (one re-read on conflict) -> verify rendered + live -> rollback on failure |
| undo | remove exactly the inserted link (`link_start`), or restore the snapshot (`mode=snapshot`) |
| link_health | extract links -> check (HEAD -> GET fallback, redirect chains, throttle) -> rollups, silo drift; `internal_only` skips external requests |
| audit | crawl (optional) + plan + link health + metrics snapshot |
| backup / maintenance | archive; retention, `ANALYZE`/optimize |

## 4. Storage

- One SQLite file `siloforge.db` in WAL mode. One writer connection on a dedicated thread; a pool of readers.
  Text JSON columns, UUID TEXT ids (v1 ids kept; bulk inserts use time-ordered UUIDv7), RFC 3339 UTC timestamps.
- Schema (`0001_init`) = v1 migrations 001-011 + v2 tables (`api_tokens`, `job_logs`, `notifications`,
  `notification_channels`, `errors`, `backups`, `audit_log`, `system_metrics`, `kv`, `inject_journal`); then
  `0002_raw_content` (raw content, `modified_gmt`, `link_start`), `0003_perf` (covering indexes for the v1 report
  reads), `0004_token_parent`, `0005_reliability`. Migrations run at start; a binary refuses a newer schema.
- Text ordering for v1 routes uses the custom collation `V1`, which reproduces Postgres `en_US.UTF-8` order.
- Vectors are f32 little-endian BLOBs on posts and paragraphs (384-d). Search loads a site's vectors into a cache
  (LRU within `vector_cache_mb`; ~1.6 KB per paragraph), int8-quantised candidate search with exact f32 re-rank.
  No ANN index is needed at this size (largest site 112k paragraphs = 173 MB f32).
- Secrets at rest: `sites.secrets_enc` (WP application password, GSC credentials), provider keys, notification
  channel configs are AES-256-GCM sealed with a key derived from `SILOFORGE_ENCRYPTION_KEY`.
- Home layout: `siloforge.db`, `siloforge.toml`, `secrets.env`, `logs/`, `backups/`, `models/all-MiniLM-L6-v2/`,
  `runtime/` (ONNX Runtime DLLs), `siloforge.pid`.

## 5. Embedding chain

`embed.chain` (default `remote, remote2, onnx, local`), tried in order per batch:

| Rung | What | Notes |
|---|---|---|
| `remote`, `remote2` | OpenAI-compatible `/embeddings` (OpenRouter, keys 1 and 2), model `sentence-transformers/all-minilm-l6-v2` | retries on 408/409/425/429/5xx/transport, honours Retry-After |
| `onnx` | ONNX Runtime 1.24 via `ort` (load-dynamic). DirectML when `embed.gpu` and a GPU exist, else CPU | DLL lookup: `SILOFORGE_ORT_DYLIB` -> `embed.onnx_runtime_dir` -> exe dir -> `<home>\runtime`; one lazy session; CPU 8 / DML 64 texts per sub-batch |
| `local` | candle (pure Rust, feature `candle`) | not in the default build |

Every rung truncates to `max_chars` and 256 tokens, and every vector is checked (count, 384 dims, finite, norm) and
L2-normalised. A rung that fails 3 times opens its circuit breaker (30 s, doubling to 10 min, half-open retry).
All rungs produce the same vector space as v1 (Xenova all-MiniLM-L6-v2): v2 re-embeddings match stored v1
paragraph vectors at cos >= 0.999999, so migrated vectors are reused. `doctor` runs a self-test against a stored
production vector.

## 6. AI (LLM) chain

Used to place `content_gap` links (rephrase a sentence / add a context sentence) and for optional anchor refinement.
- Resolution: the site's active chat chain -> the global chain -> the env fallback (`llm.fallback_url`,
  `llm.fallback_models`, i.e. v1's `DARIO_*`).
- Each rung: provider adapter, retries only on transport/408/409/425/429/5xx, per-call deadline (chat 300 s), circuit
  breaker (3 fails -> 30 s open, doubling), call log per attempt with tokens.
- Output is validated before anything is written: allowlisted inline tags only (no script/iframe/style, no `on*`
  attributes, no `javascript:`/`data:` URLs), exactly one new link to the target whose text contains the anchor,
  every original link and tag kept, length and wording overlap within limits; the canonicalised HTML is written,
  never the raw reply. One JSON repair attempt for structured calls.
- SSRF: link-local and cloud-metadata addresses are always blocked for providers, including DNS answers; loopback and
  private addresses are allowed (local dario / Ollama).

## 7. Security

| Layer | Mechanism |
|---|---|
| Network | binds `127.0.0.1` by default; `/mcp` requires a loopback `Host` and a bearer token; security headers on every response |
| Identity | bcrypt passwords; HS256 user JWTs re-checked against the user row (`token_version`); site JWTs from the plugin handshake, revoked on key rotation; API tokens `sfk_...` hashed, scoped, expiring, revocable, parent-bounded |
| Authorization | ownership gate on every site route (404 for foreign sites), admin routes, scope check per HTTP method |
| Abuse | failed-login/handshake limiter per IP + per-account lockout; body limit 25 MB; clamped limits; bounded inputs and recursion in every parser (hostile-input tests) |
| Secrets | AES-256-GCM at rest, never returned by API or MCP, stripped from MCP output and logs; `secrets.env` separate from config |
| Outbound | SSRF guard per redirect hop and at connect time; private hosts only for a site's own hosts with an admin flag; no redirects for notification webhooks |
| Writes to WordPress | approval gate, snapshot, `modified_gmt` precondition, verify, rollback, exact undo |
| Audit | `audit_log` row per mutation (actor, via session/token, action, target) |
| Backups | encrypted by default (AES-256-GCM, scrypt), SHA-256 manifest, refuse newer schema, atomic restore |
| Build | no `unsafe`, no OpenSSL (rustls + ring), clippy denies `unwrap`/`expect`/`panic` |

Audit reports: [testing/security-audit.md](testing/security-audit.md), [testing/reliability-audit.md](testing/reliability-audit.md).

## 8. Deployment

- `scripts\build-release.ps1`: UI build, `cargo build --release --locked -p siloforge` (GNU + WinLibs), stage
  `dist\<ver>\` (exe, ONNX Runtime + DirectML DLLs, notices, docs, scripts), zip + `.sha256`.
- `scripts\install.ps1`: `D:\Final_Apps\SiloForge\app\<ver>\` + `current` junction, data home, `init`, Scheduled
  Task `siloforge-keepalive` (logon + every 5 min, hidden keeper that exits if the port already listens).
- `scripts\cutover-from-v1.ps1` / `rollback-to-v1.ps1`: see [MIGRATION.md](MIGRATION.md).
