# SiloForge

Self-hosted internal linking for WordPress, in one Rust binary.

SiloForge crawls a WordPress site, splits posts into paragraphs, embeds them (all-MiniLM-L6-v2, 384-d),
scores candidate links, picks anchor text, builds topic silos, and writes approved links back through the
WordPress REST API. Every change is snapshotted and can be undone. It serves a web UI, a REST API and an
MCP server for AI agents, and it handles its own backups and data migration.

Version 2 replaces the TypeScript stack of v1 (7 Docker containers: Fastify API, two BullMQ workers,
Postgres + pgvector, Qdrant, Redis, nginx) with a single `siloforge.exe` and one SQLite file. The `/api/v1`
contract is unchanged, so the v1 UI, the MCP tool names and the SiloForge Connect WordPress plugin keep working.

- License: MIT. Platform tested: Windows 11 x64 (GNU toolchain build). Default ports: `127.0.0.1:8600` and `:8601`.
- Docs: [Operations](docs/OPERATIONS.md) · [Migration](docs/MIGRATION.md) · [API](docs/API.md) ·
  [Hermes / MCP](docs/HERMES.md) · [Architecture](docs/ARCHITECTURE.md) · [Changelog](CHANGELOG.md)

---

## Features

### 1. Core and performance
- One process: HTTP API, embedded UI, job runner, scheduler, system monitor, MCP server, backups.
- No Docker, Postgres, Qdrant or Redis. Data lives in one directory (`SILOFORGE_HOME`).
- All CPU-heavy work runs on one bounded rayon pool (`resources.cpu_threads`, default logical cores / 4),
  at below-normal OS priority (`resources.low_priority_jobs`).
- Incremental pipeline: only changed posts are re-chunked, only rows without a vector are embedded.
- Optional GPU: ONNX Runtime with DirectML (AMD, NVIDIA, Intel on Windows). Falls back to CPU automatically.

### 2. Linking engine
- Full-site crawl through WP REST with `context=edit` (raw post content, so blocks are preserved), or sitemap mode.
- One HTML splitter for chunking and insertion (exact byte spans); Gutenberg block comments are never touched.
- Keywords as 1-3-grams, search intent, anchor selection with guards (2-6 words, no reuse per source, no
  linking inside headings/code/buttons/existing links, cannibalization-aware, diversity and over-optimisation penalties).
- 11-dimension composite score, density limits applied after scoring, 5 silo topologies
  (reverse, serial, priority, circle, bidirectional), k-means clusters with pillars, silo drift, keyword linker rules.
- Safe injection: fresh raw content is read, the link is spliced at a hash-checked position, the post is updated
  with a `modified_gmt` precondition (an edit made in WordPress after planning is never overwritten), the live
  page is verified, and a snapshot is kept. Undo removes exactly the inserted link.
- Link health checks (internal only, or internal + external) with a failure taxonomy, plus site metrics and trends.

### 3. AI
- Embedding chain: remote OpenAI-compatible endpoint (OpenRouter, two keys) -> local ONNX Runtime
  (DirectML or CPU) -> optional pure-Rust candle build. Circuit breaker per rung, vectors validated.
- LLM providers: OpenAI-compatible, OpenRouter, Anthropic, Gemini, Ollama, custom. Chains per site or global,
  retries, fallback, circuit breaker, call log, probes. Used to place `content_gap` links that have no
  natural anchor in the text. LLM output is validated before it is written.

### 4. API, MCP and Hermes
- `/api/v1`: the 78 v1 routes, contract-compatible (diffed against live v1: 0 mismatches in 203 requests).
- `/api/v2`: system status and metrics, global job list with logs and cancel, notifications, errors,
  backups, API tokens with scopes, audit log.
- MCP server with 28 tools (the 20 v1 tool names unchanged, plus job status/logs/cancel/wait, a crawl+plan
  pipeline, backups, notifications, system status), over stdio (`siloforge mcp`) or streamable HTTP (`/mcp`).
- Revocable API tokens (`sfk_...`) instead of an admin password in agent configs.
- Notifications to webhooks, ntfy, Telegram, a Hermes gateway webhook route (HMAC-signed), Windows toasts.

### 5. Backup and migration
- `siloforge backup create` (database snapshot + config + secrets, zip with a SHA-256 manifest, AES-256-GCM
  encrypted by default with the passphrase `init` generates), scheduled daily by default, verify and restore.
- `siloforge migrate from-postgres` imports all v1 data read-only from the v1 container, keeping ids,
  password hashes and embeddings (no re-embedding). WordPress credentials are encrypted on the way in.
- Portable: move to another PC by copying the data directory, or with backup + restore.

### 6. Monitoring
- CPU, RAM and disk sampling with history (1 h raw, 24 h, 7 d), alerts with thresholds.
- Per-job resources (wall time, CPU seconds, peak RSS), job logs, an error tracker with fingerprints,
  JSON log files with daily rotation. UI pages: System, All jobs, Notifications.

### 7. Database
- SQLite in WAL mode: one writer thread, a pool of readers, batched transactions.
- Vectors as f32 BLOBs, searched by SIMD brute force over a per-site cache (LRU within `vector_cache_mb`),
  int8 candidate search with exact re-rank.
- Indexes per access path, retention jobs, `ANALYZE`/optimize, v1-compatible text ordering (collation).

### 8. Testing
- Unit tests per crate, hostile-input tests for every parser and handler, golden tests for the scorer
  and anchors from v1 behaviour, integration tests against local stub servers (WordPress, LLM providers,
  embeddings). No test calls a real provider or a real site.
- Field tests: a JSON contract diff against live v1 on the migrated data, a pilot against a local WordPress
  (crawl -> plan -> approve -> inject -> verify -> undo), and a performance comparison on the largest site.

---

## Architecture

```
  Browser UI ──┐       Hermes Agent (MCP stdio / HTTP, API token)       WordPress + Connect plugin
               │                    │                                           │
               ▼                    ▼                                           ▼
        ┌──────────────────────────── siloforge.exe ─────────────────────────────────────┐
        │  sf-server (axum)   /api/v1 (v1 contract)  /api/v2  /mcp  /health  embedded UI  │
        │        │ reads                         │ enqueue                                │
        │        ▼                               ▼                                        │
        │  sf-store: SQLite WAL  ◀── writer thread ── sf-jobs: runner + scheduler         │
        │  (siloforge.db)                         (per-site lock, dedupe, heartbeat)     │
        │                                                │ pipelines                      │
        │      ┌─────────────────────┬──────────────────┼───────────────────┐            │
        │      ▼                     ▼                  ▼                   ▼            │
        │  sf-crawl (WP REST,    sf-embed (remote ->  sf-ai (LLM chains,  sf-engine      │
        │  sitemap, SSRF guard,  ONNX DirectML/CPU    fallback, probes)   (rayon pool:   │
        │  WP writer, health)    -> candle)                               score, silo,   │
        │                                                                 anchors, ...)  │
        │  sf-ops: monitor · notifications · errors · backup/restore · v1 migrator       │
        └─────────────────────────────────────────────────────────────────────────────────┘
               │ data: SILOFORGE_HOME = siloforge.db, siloforge.toml, secrets.env, logs/, models/, runtime/, backups/
```

Details: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), decision record [docs/adr/0001](docs/adr/0001-single-binary-sqlite.md).

---

## Quick start (Windows)

### From a release zip
1. Unzip `siloforge-<version>-windows-x64.zip` (it holds `siloforge.exe`, the ONNX Runtime DLLs, docs and `scripts\`).
2. Install with a keepalive task (default `D:\Final_Apps\SiloForge`, data in `...\data`):
   ```powershell
   powershell -ExecutionPolicy Bypass -File .\siloforge-2.0.0\scripts\install.ps1
   ```
   The installer copies the app, creates the data home, runs `siloforge init` (new random secrets),
   registers the Scheduled Task `siloforge-keepalive` (at logon + every 5 min, hidden) and checks `/health`.
   The embedding model is copied from `D:\Download\models\...` if present; otherwise add `-FetchModel`
   (downloads all-MiniLM-L6-v2 from Hugging Face, SHA-256 pinned).
3. Create the first admin and open the UI:
   ```powershell
   $env:SILOFORGE_NEW_PASSWORD = '<a password>'
   D:\Final_Apps\SiloForge\current\siloforge.exe user set-password admin@siloforge.local --create --home D:\Final_Apps\SiloForge\data
   start http://127.0.0.1:8600/
   ```
4. `siloforge doctor --home <home>` checks config, database, embeddings (self-test cosine vs a stored vector), ports and disk.

The scripts follow the author's layout and refuse paths outside `D:\`. Set `$env:SILOFORGE_ALLOW_ANY_DRIVE='1'`
to install elsewhere (for example `-Root C:\SiloForge`).

### Without the installer
```powershell
siloforge init --home D:\SiloForge\data            # siloforge.toml + secrets.env (values never printed)
siloforge fetch-runtime --home D:\SiloForge\data   # ONNX Runtime DirectML build into <home>\runtime (--cpu for CPU-only)
siloforge fetch-model --home D:\SiloForge\data     # all-MiniLM-L6-v2 ONNX + tokenizer into <home>\models
siloforge serve --home D:\SiloForge\data           # API + UI on 127.0.0.1:8600 and :8601
```

### Build from source
Requirements: Rust 1.88+ with the `x86_64-pc-windows-gnu` toolchain, WinLibs MinGW-w64 on `PATH`, Node.js (for the UI).
No OpenSSL, CMake or NASM (rustls + ring, bundled SQLite).
```powershell
powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1   # npm ci + build, cargo build --release --locked,
                                                                     # dist\<ver>\ + siloforge-<ver>-windows-x64.zip + .sha256
cargo xtask ci                                                       # fmt check + clippy -D warnings + tests
cargo run -p siloforge -- serve --home D:\tmp\sf-dev-home            # dev (side ports: SILOFORGE_BINDS=127.0.0.1:8610,127.0.0.1:8611)
```

---

## Configuration

`<home>\siloforge.toml` holds settings ([siloforge.example.toml](siloforge.example.toml) documents every key).
Secrets live in `<home>\secrets.env` or the process environment, never in the TOML.

| Section | Keys (defaults) |
|---|---|
| `[server]` | `binds = ["127.0.0.1:8600", "127.0.0.1:8601"]` (both serve API + UI + `/mcp`), `body_limit_mb = 25`, `cors_origins` |
| `[auth]` | `allow_registration = false`, `admin_email` |
| `[resources]` | `cpu_threads = 0` (= logical/4), `max_concurrent_jobs = 3`, `vector_cache_mb = 512`, `low_priority_jobs = true`, `defer_when_cpu_above = 0` |
| `[embed]` | `chain = ["remote","remote2","onnx","local"]`, `remote_url`, `remote_model`, `batch_size = 64`, `max_chars = 2000`, `gpu = true`, `model_dir`, `onnx_runtime_dir` |
| `[llm]` | `fallback_url = "http://127.0.0.1:3460/v1"`, `fallback_models`, `timeout_ms` (used when no model chain is active) |
| `[crawl]` | `allow_private_hosts = false`, `concurrency = 4`, `timeout_ms`, `max_pages`, `[crawl.host_aliases]` |
| `[backup]` | `dir` (default `<home>\backups`), `keep = 10`, `schedule = "17 3 * * *"`, `passphrase_env = "SILOFORGE_BACKUP_PASSPHRASE"` |
| `[monitor]` | `sample_secs = 5`, `retain_days = 14`, `job_log_retain_days = 30`, alert thresholds |

Environment / `secrets.env`: `JWT_SECRET`, `SILOFORGE_ENCRYPTION_KEY`, `ADMIN_EMAIL`, `ADMIN_PASSWORD` (seeds the first
admin when there are no users), `OPENROUTER_API_KEY`, `OPENROUTER_API_KEY_T2`, `OPENROUTER_URL`, `EMBED_REMOTE_MODEL`,
`EMBED_CHAIN`, `DARIO_URL`, `DARIO_MODEL_PRIMARY`, `DARIO_MODEL_FALLBACK`, `CRAWL_ALLOW_PRIVATE_HOSTS`,
`SILOFORGE_BINDS`, `SILOFORGE_CPU_THREADS`, `SILOFORGE_BACKUP_PASSPHRASE`, `SILOFORGE_LOG` (tracing filter).
The v1 names are honoured, so a v1 `.env` can be imported with `siloforge init --from-v1-env`.

---

## CLI

```
siloforge [--home <dir>] <command>        # home: --home, else $SILOFORGE_HOME, else <exe dir>\data
  serve                                   run API + UI + MCP over HTTP + jobs + scheduler + monitor + backups
  mcp                                     MCP over stdio (needs SILOFORGE_API_URL + SILOFORGE_TOKEN of a running server)
  init [--from-v1-env <.env>] [--force]   create siloforge.toml + secrets.env (generated secrets)
  backup create [--kind full|db|config] [--passphrase ..] [--note ..] [--include-secrets]
  backup list | backup verify <file>
  restore <file> [--passphrase ..] [--force]
  migrate from-postgres [--site <uuid>] [--tables a,b] [--dry-run]    read-only import from the v1 container
  migrate export <dir> [--site ..] | migrate import <dir> [--site ..]  v1 -> CSV -> v2 (other PC)
  migrate verify [--site ..] [--sample 100]                           counts + embedding samples vs v1
  token create --name <n> [--user <email>] [--scopes read,write] [--expires-days N]   (printed once)
  token list | token revoke <id-or-prefix>
  user set-password <email> --password <p> | env SILOFORGE_NEW_PASSWORD [--create]
  user list
  doctor                                  config, DB integrity, embeddings self-test, ports, disk
  fetch-runtime [--cpu] | fetch-model     download ONNX Runtime / the model (SHA-256 pinned)
  version
```

---

## API and MCP

- **`/api/v1`**: the v1 contract (paths, status codes, JSON field names). Auth: `Authorization: Bearer <token>`
  where the token is a user JWT (`POST /api/v1/auth/login`), a site JWT (plugin handshake) or an API token.
- **`/api/v2`**: new features: `system`, `jobs` (+ logs, cancel), `notifications` (+ channels), `errors`,
  `backups`, `tokens`, `audit`, `mcp` (client config). Admin-only where noted.
- **`/mcp`**: streamable HTTP MCP endpoint (loopback only, bearer token required). `siloforge mcp` is the stdio variant.
- **`/health`**: unauthenticated `{ok, version, uptime_secs, services, jobs}`.

Full list with shapes: [docs/API.md](docs/API.md). MCP tools and agent workflows: [docs/HERMES.md](docs/HERMES.md).

---

## Performance vs v1

Measured on 2026-10-13 on the author's workstation (Ryzen 9 3900X, 64 GB, Radeon RX 5700 XT, Windows 11) with a
migrated copy of the production data. The machine was busy with other work during the runs, so wall times vary by
about ±30 %; the order of magnitude is solid, exact ratios are not. Sources:
[docs/testing/performance.md](docs/testing/performance.md), [docs/testing/contract-diff.md](docs/testing/contract-diff.md).

Plan pipeline, largest site (site36.example: 2,621 posts, 112,706 paragraphs):

| Run | v1 | v2 |
|---|---|---|
| Incremental re-plan, nothing to embed | 316-325 s | **3.3-3.9 s** (rescoring ~8.5k suggestions vs ~200 in v1) |
| Full plan with every text embedded | 4,019 s (remote embeddings; 7 earlier attempts failed) | **174 s** (115,327 texts on the GPU, 715 texts/s) |
| CPU time, incremental re-plan | up to ~640 CPU-s (worker only) | **5.6-6.1 CPU-s** |
| RAM | ~1.9 GB resident at idle (7 containers) | 26 MB after start; 0.3-0.45 GB peak during a plan; 0.6-0.73 GB while embedding |

Embedding throughput (all-MiniLM-L6-v2, length-sorted batches): DirectML 387-715 texts/s at ~0.9 core;
CPU with 6 threads 204 texts/s; cosine vs the stored v1 vectors 0.999999+ for paragraphs.

REST API (203 requests over 66 endpoint templates and 4 sites): **0 real contract mismatches**. After the read-path
pass (covering indexes, concurrent reads), every heavy read on the largest site is at or below v1 (curl p50, ms):

| Endpoint (site36.example) | v1 | v2 |
|---|---|---|
| `/reports/link-health?limit=250` | 43.0 | 27.2 |
| `/reports/link-health?format=csv` (46k rows) | 1037.9 | 203.4 |
| `/reports/links` | 56.1 | 41.6 |
| `/reports/graph` | 14.1 | 12.4 |
| `/dashboard` | 81.0 | 14.3 |
| `/posts?limit=500` | 44.5 | 20.5 |
| `/edges?limit=1000&offset=1000` | 87.0 | 33.6 |

Small endpoints: median p50 over all 203 requests 3.2 ms (v2) vs 8.9 ms (v1), measured before that pass.

Pilot on a local WordPress (360 posts): crawl 3.1 s, plan 48.8 s (14,573 paragraphs embedded), 11 approved links
injected and verified, Gutenberg block counts unchanged, an edit made in WordPress after planning preserved, undo
byte-identical ([docs/testing/pilot-e2e.md](docs/testing/pilot-e2e.md)).

### Known limitations (2.0.0)
- Anchor text is the weak spot: the noun-phrase chunker has no POS model, so some anchors contain verbs or are
  product names. In the pilot sample about half of the native anchors were natural noun phrases. Review anchors
  before injecting (the approval gate is mandatory anyway).
- About a quarter of suggestions are `content_gap` (no natural anchor in the text). They need an LLM to be placed;
  without one they are reported, not written.
- Not ported from v1 yet: Google Search Console data pull, WordPress taxonomy/archive crawl, breadcrumb detection.
- ONNX Runtime's own threads run at normal priority. There is no MCP tool for link health.

---

## Security model

- Binds to `127.0.0.1` by default. `/mcp` additionally checks for a loopback `Host` and requires a bearer token.
- Passwords: bcrypt (v1 hashes kept). User JWTs (HS256, v1-compatible) are re-checked against the user row on use,
  so a password change, demotion or deletion revokes existing sessions. Self-registration is off.
- API tokens: `sfk_...`, stored hashed, scopes `read` / `write` / `admin` / `*`, optional expiry, revocable,
  `last_used_at` tracked. Use them for agents and scripts instead of passwords.
- Stored secrets (WordPress application passwords, GSC credentials, provider keys, notification channel secrets)
  are encrypted with AES-256-GCM under `SILOFORGE_ENCRYPTION_KEY` and are never returned by the API or MCP.
  Site API keys are only shown on create/rotate; rotating revokes the site's tokens.
- Failed logins and plugin handshakes are rate limited per IP, with a per-account lockout (5 free attempts, then
  30 s doubling up to 15 min). Every mutation is written to an audit log. Responses carry
  `X-Frame-Options`, `nosniff`, `Referrer-Policy` and a CSP `frame-ancestors` header.
- API tokens can only mint tokens with `*`/`admin` scope, a child never outlives its parent, and revoking a token
  revokes its children.
- Crawler and link checker: SSRF guard on every redirect hop and at connect time (DNS answers are checked too).
  Private/LAN hosts are allowed only for a site's own hosts and only when an admin sets `allow_private_host` on the
  site (or the global `crawl.allow_private_hosts`); cloud-metadata addresses are always blocked.
- Backups are encrypted by default: `siloforge init` generates `SILOFORGE_BACKUP_PASSPHRASE` in `secrets.env`
  (keep a copy elsewhere: restores need it). An unencrypted backup leaves `secrets.env` out unless you force it.
- Audits: [docs/testing/security-audit.md](docs/testing/security-audit.md),
  [docs/testing/reliability-audit.md](docs/testing/reliability-audit.md) (open items are listed there).

---

## Backup and migration

- Daily backup at 03:17 by default (`backup.schedule`), 10 kept. Manual: `siloforge backup create`, UI Settings -> Backups,
  `POST /api/v2/backups`, or the MCP tool `siloforge_create_backup`. `runtime\` and `models\` are not in backups
  (re-run `fetch-runtime` / `fetch-model` or copy them).
- v1 -> v2: [docs/MIGRATION.md](docs/MIGRATION.md) and `scripts\cutover-from-v1.ps1` (dry run by default) /
  `scripts\rollback-to-v1.ps1`.
- PC -> PC: copy the data directory (server stopped), or `backup create` + `restore`. See [docs/OPERATIONS.md](docs/OPERATIONS.md).

---

## Acknowledgements

- [LOOM](https://github.com/MarcinZmuda/loom) by Marcin Zmuda: the 11-dimension composite link score and its default
  weights follow LOOM's published scorer (via SiloForge v1). The Rust code is our own; no LOOM source is included.
- [ONNX Runtime](https://github.com/microsoft/onnxruntime) (MIT) and [DirectML](https://github.com/microsoft/DirectML)
  (Microsoft license, redistributable) for local and GPU embeddings, via the [`ort`](https://github.com/pykeio/ort) crate.
- [candle](https://github.com/huggingface/candle) and [tokenizers](https://github.com/huggingface/tokenizers) by Hugging Face.
- [all-MiniLM-L6-v2](https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2) (sentence-transformers, Apache-2.0)
  and its [ONNX export by Xenova](https://huggingface.co/Xenova/all-MiniLM-L6-v2).
- [rmcp](https://github.com/modelcontextprotocol/rust-sdk), the official Rust MCP SDK.
- [axum](https://github.com/tokio-rs/axum), [tokio](https://tokio.rs), [rusqlite](https://github.com/rusqlite/rusqlite)
  and [SQLite](https://sqlite.org), [rayon](https://github.com/rayon-rs/rayon), [reqwest](https://github.com/seanmonstar/reqwest)
  + [rustls](https://github.com/rustls/rustls), [scraper](https://github.com/causal-agent/scraper),
  [texting_robots](https://github.com/Smerity/texting_robots), [croner](https://github.com/Hexagon/croner-rust),
  [sysinfo](https://github.com/GuillaumeGomez/sysinfo), and the React / Vite / React Flow projects for the UI.

Third-party license texts ship in the release zip (`THIRD_PARTY_NOTICES.txt`, `licenses\`).

## License

[MIT](LICENSE) © 2026 Nazmul Hasan
