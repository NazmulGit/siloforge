# Changelog

All notable changes to SiloForge. Versions follow [SemVer](https://semver.org). v2 is a rewrite in Rust; the v1
history (TypeScript, Docker) is summarised at the end.

## [2.0.0] - 2026-10-13

The Rust rewrite: one binary (`siloforge.exe`) and one SQLite file replace the 7-container v1 stack. The `/api/v1`
contract is kept, so the v1 UI, the MCP tool names and the SiloForge Connect plugin work unchanged.

### Added
- **Single binary** with subcommands `serve`, `mcp`, `init`, `backup`, `restore`, `migrate`, `token`, `user`,
  `doctor`, `fetch-runtime`, `fetch-model`, `version`. Data directory `SILOFORGE_HOME` (default `<exe dir>\data`).
- **Embedded UI** (the v1 React UI) plus new pages: System (CPU/RAM/disk charts, queue, errors), All jobs (filters,
  progress, resources, cancel, live logs), notification bell, and Settings sections for Notifications, Backups,
  API tokens, Integrations (MCP + Hermes config) and the audit log.
- **`/api/v2`**: `system`, `system/stats`, `system/metrics`, `jobs` (+ `/{id}`, `/logs`, `/cancel`), `notifications`
  (+ channels CRUD and test, mark read), `errors` (+ resolve), `backups` (+ download, verify, delete), `tokens`, `audit`, `mcp`.
- `/api/v1` additions that do not change existing routes: `GET /jobs/run/{id}`, `POST /sites/{id}/rotate-key`,
  `run-now` with `{job_kind|schedule_id}`, JSON 404 for unknown `/api/*` paths.
- **API tokens** (`sfk_...`, hashed, scopes `read`/`write`/`admin`/`*`, expiry, revoke) for agents and scripts.
- **MCP server** (rmcp): the 20 v1 tools with the same names and parameters, plus `siloforge_system_status`,
  `siloforge_job_status`, `siloforge_job_logs`, `siloforge_cancel_job`, `siloforge_wait_for_job`,
  `siloforge_run_pipeline`, `siloforge_create_backup`, `siloforge_list_notifications` (28 tools). Transports: stdio
  (`siloforge mcp`) and streamable HTTP at `/mcp`. `list_edges` pages with `offset`/`next_offset`.
- **Job system** in SQLite: per-site lock for mutating jobs, dedupe of identical queued jobs, heartbeat and reaper,
  per-kind timeouts, cancel with checkpoints, bounded concurrency, per-job resources (wall, CPU s, peak RSS) and logs.
  Scheduler with cron + time zone that stamps `last_run_at` / `next_run_at` for every kind.
- **Embedding chain** remote (OpenRouter, 2 keys) -> ONNX Runtime (DirectML GPU or CPU) -> candle (optional build),
  with circuit breakers, vector validation and a `doctor` self-test against a stored production vector.
- **AI providers**: OpenAI-compatible, OpenRouter, Anthropic, Gemini, Ollama, custom; chains site -> global -> env,
  retries, circuit breaker, per-call deadline, call log, probes; LLM output validated before it is written.
- **Monitoring**: CPU/RAM/disk sampler with 1 h / 24 h / 7 d history, alerts, error tracker, JSON log files.
- **Notifications**: webhook, ntfy, Telegram, Hermes gateway webhook (HMAC V2), Windows toast, log; per-event rate limit.
- **Backups**: `VACUUM INTO` snapshot + config + secrets in a zip with a SHA-256 manifest, AES-256-GCM
  encryption (scrypt) by default (`init` generates `SILOFORGE_BACKUP_PASSPHRASE`; unencrypted archives leave
  `secrets.env` out unless `--include-secrets`), daily schedule, retention, verify, atomic restore that keeps `.pre-restore-<ts>` copies.
- **v1 migrator**: `migrate from-postgres` (read-only `COPY` through `docker exec siloforge_pg psql`), `export`/`import`
  via CSV for other PCs, `verify` (row counts + embedding samples). Ids, bcrypt hashes and 332k embeddings are kept.
- Link health payload `internal_only` (list external links without requesting them).
- Deployment scripts: `scripts/build-release.ps1`, `install.ps1` (Scheduled Task keepalive), `cutover-from-v1.ps1`,
  `rollback-to-v1.ps1`.

### Changed
- Storage: Postgres + pgvector + Qdrant + Redis -> SQLite WAL (one writer thread, reader pool), vectors as f32 BLOBs with
  SIMD brute-force search over a per-site LRU cache, int8 candidate search with exact re-rank.
- Crawl reads raw post content (`context=edit`) with the site's application password; rendered-only fallback.
- Injection writes into the raw content at a hash-checked byte offset, one update per post with a `modified_gmt`
  precondition, verifies the live page, auto-rolls back on a failed verify, and stores the actually linked text.
- Undo removes exactly the inserted link (byte-identical result in the pilot) instead of restoring a whole snapshot.
- Density limits are applied after scoring; anchors use 1-3-gram keywords, stems and guards (2-6 words, no one-word
  keyword fallback, no reuse per source, reserved exact anchors for cannibalization).
- Heavy `/api/v1` reads use covering indexes (migration 0003) and concurrent reads; on the largest site every
  measured report is at or below v1 (e.g. link-health CSV 1,038 -> 203 ms, dashboard 81 -> 14 ms).
- Text ordering in `/api/v1` responses follows v1's Postgres `en_US.UTF-8` collation (`COLLATE V1`).
- Plan pipeline is incremental (content hash per post, embeddings only for missing rows) and much faster:
  warm re-plan of a 112k-paragraph site 316-325 s (v1) -> 3.3-3.9 s; full plan with GPU embeddings 4,019 s -> 174 s.
- Row ids for bulk inserts are time-ordered UUIDv7 (still valid UUIDs).
- `SILOFORGE_ENCRYPTION_KEY` is separate from `JWT_SECRET` (v1 derived stored-secret encryption from the JWT secret).

### Fixed
- Duplicate concurrent jobs on one site (no per-site lock in v1); lost BullMQ locks and phantom `running` jobs.
- Rendered HTML pushed back to WordPress (destroyed Gutenberg blocks); injection overwriting edits made in WordPress.
- Click depth always 0 (graph roots), silo drift always 0, density cap applied before scoring, one-word anchors,
  O(n^2) anchor NLP, keyword phase rewriting every row, candidates skipping only exact (source, target, anchor) pairs.
- Title-fallback anchors cut mid-clause, glued table/review-list paragraphs, anchors crossing `<li>` boundaries,
  possessive tails, two native links in one paragraph (pilot findings).
- `is_orphan=1/0` rejected by v1 although the UI sends it; `edges?limit=2000` rejected (now clamped to 5000).
- Keyword rule `anchor_type` values that passed validation but violated the DB check (500).
- `ack` could force any edge to applied; schedules `run-now` always ran an audit; scheduler never stamped run times.

### Security
- WordPress application passwords and GSC credentials are encrypted (AES-256-GCM) and never returned by the API or
  MCP (v1 returned them in plaintext from `GET /sites`). Site `api_key` is only shown on create/rotate; rotation
  revokes the site's tokens.
- User JWTs are re-checked against the user row: password change, demotion or deletion revokes sessions.
- Self-registration is off by default (`auth.allow_registration`); failed logins/handshakes are rate limited per
  IP with a per-account lockout (5 attempts, then 30 s doubling to 15 min).
- API tokens: only `*`/`admin` tokens can mint tokens, a child token never outlives its parent, revoke cascades.
- Private/LAN crawl targets need a per-site admin flag `allow_private_host` (or the global flag), only for the site's
  own hosts; host aliases never bypass the IP check; cloud-metadata IPs are always blocked.
- Security headers on every response (X-Frame-Options, nosniff, Referrer-Policy, CSP frame-ancestors).
- SSRF guard on every redirect hop and at connect time (no DNS-rebinding window); link-local/metadata addresses are
  always blocked for LLM providers.
- MCP output strips secrets (`api_key`, passwords, tokens, `sf_...`/`sfk_...` strings); `/mcp` requires a bearer token
  and a loopback Host. Agents use scoped API tokens instead of the admin password.
- Settings whitelist with ranges, CSV formula-injection guard and sanitised CSV filenames, audit log on every mutation.
- No OpenSSL; rustls + ring. `unsafe` is forbidden workspace-wide.

### Removed
- Docker Compose stack (Fastify API, BullMQ workers, nginx UI, Postgres, Qdrant, Redis containers).
- transformers.js in-process embedding (replaced by ONNX Runtime / candle).
- `ADMIN_API_KEY` (dead config in v1). The `/health` keys `redis` and `qdrant` remain for old probes and report the
  components that replaced them.

### Known limitations
- Anchor text quality depends on a heuristic noun-phrase chunker (no POS model); review anchors before injecting.
- `content_gap` suggestions (about a quarter in the pilot) need an LLM to be placed.
- Not ported yet: GSC data pull, WordPress taxonomy/archive crawl, breadcrumb detection, an MCP link-health tool.
- Only one `serve` per data home is supported; a second instance with other ports is not refused yet (reliability audit R2).

## v1 history (TypeScript + Docker, `D:\Code\SiloForge-Suite\app`)

| Version | Date | Summary |
|---|---|---|
| 1.4.0 | 2026-08-19 | Score calibration and silo drift: adaptive weight renormalisation, silo drift feature, pilot config. Ran in production until the v2 cutover. |
| 1.3.0 | 2026-08-18 | Integration hardening (audit items R1-R4). |
| 1.2.0 | 2026-08-18 | Anchor system, NLP and semantic upgrade. |
| 1.1.0 | 2026-08-13 | Rust acceleration (`siloforge-rust-engine` in the worker image) and resource controls. |
| 1.0.0 | 2026-07-28 | First stable. Planner paragraph chunking made idempotent. |
| 0.2.1 | 2026-07-27 | Link-health failure taxonomy (dead vs unverifiable) and politeness/timeout fix. |
| 0.2.0 | 2026-07-27 | Checklist gaps closed, model provider system, migrations 006-009, secret crypto; UI for link health, schedules, metric trends, model providers. |
| 0.1.0 | 2026-07-16/17 | Monorepo (engine, api, worker, ui), multi-user auth, Keyword Linker, Anchor Manager, Bulk wizard, Link History, silo/cluster tools, first security audit fixes. |

Housekeeping after 1.4.0: the folder moved from `D:\Code\siloforge` to `D:\Code\SiloForge-Suite\app` (2026-09-28);
a frozen snapshot was written to `D:\Final_Apps\SiloForge-Suite\app` (2026-10-04). Full v1 notes:
`D:\Code\SiloForge-Suite\app\CHANGELOG.md` and its `releases\` folder.

[2.0.0]: https://github.com/NazmulGit/siloforge/releases/tag/v2.0.0
