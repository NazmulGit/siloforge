# AGENTS.md - SiloForge v2 (Rust)

Read this first. CLAUDE.md just points here.

## What this is
SiloForge is a self-hosted internal-linking platform for WordPress. It crawls a site, embeds paragraphs
(all-MiniLM-L6-v2, 384-d), scores candidate links, picks anchors, builds topic silos, and injects approved
links through WP REST. Every change can be undone. v2 is one Rust binary, `siloforge`, that replaces the
v1 TypeScript + Docker stack (7 containers). It serves the API and UI on 127.0.0.1:8600 and :8601, runs
an MCP server for Hermes Agent, and handles backup/restore and migration.

Location: `D:\Code\SiloForge-Suite\siloforge-rs`. Never create files outside D:\. Never touch D:\Ryzen.
v1 source (read-only reference): `D:\Code\SiloForge-Suite\app` (TS), `..\mcp`, `..\connect` (WP plugin).

## Specs (authoritative, read before coding your part)
- `docs/research/01-engine-spec.md`: every engine algorithm, constants, and the bugs to fix.
- `docs/research/02-backend-spec.md`: all 78 v1 routes, auth, jobs, schema, bugs. **Section 8 must match.**
- `docs/research/04-integrations.md`: Hermes, MCP, plugin, Backup Hub, and the cutover.
- `docs/research/05-ui-audit.md`: the exact JSON shapes the UI reads.
- `docs/PLAN.md`: architecture, crate map, work packets. `docs/adr/`: the decisions.

## Golden rules
1. **Never crash.** No `unwrap/expect/panic/todo/unreachable/unimplemented` outside tests (clippy denies them). Use `?`, `.get()`, checked math, bounded input sizes and bounded recursion.
2. **No `unsafe`.** The workspace forbids it. Exception: memory-mapping model weights in `sf-embed`, behind `#[allow(unsafe_code)]` with a SAFETY comment.
3. **One verify command:** `cargo xtask ci` (fmt check + clippy -D warnings + tests). For your own crate, at least run `cargo clippy -p <crate> --all-targets -- -D warnings` and `cargo test -p <crate>`.
4. **Pure-Rust / bundled deps only:** rustls + ring, rusqlite `bundled`. No OpenSSL, cmake or nasm. Toolchain: GNU (`rustup override` is already set for this folder) + WinLibs:
   `export PATH="/d/Programs/winlibs/mingw64/bin:$PATH"`.
5. **Never change a v1 endpoint contract** (paths, status codes, JSON field names). New features go under `/api/v2`. Text enums carry `Unknown(String)` (`sf_core::types`).
6. **Logging** via `tracing`. No `println!` except CLI output.
7. **No secrets in the repo**, and never print secret values. WP app passwords, GSC credentials and provider keys are stored encrypted (`sf_core::crypto::encrypt_secret`) and are never returned by the API.
8. **No blocking in async.** Use `Db::read` / `Db::write` (they already hop threads). CPU-heavy work goes to the shared rayon pool (`sf_engine::pool`), never to the tokio workers.
9. **Determinism:** the same input gives the same output (sort ties by id, seeded PRNGs). Tests rely on this.

## Layout (lower crates never depend on upper ones)
```
sf-core     config, errors, ids/time, enums, crypto (v1-compatible JWT/bcrypt/AES-GCM)
sf-store    SQLite: schema (migrations/), writer thread + reader pool, row helpers
sf-engine   pure algorithms, no IO: html/chunk/keywords/intent/anchor/inject + graph/cluster/silo/score/vector
sf-embed    Embedder chain: remote (OpenAI-compatible) -> onnx (ORT CPU/DirectML, optional) -> local (candle)
sf-ai       LLM providers + chains + fallback + call log + probes
sf-crawl    WP REST / sitemap crawler, SSRF guard, robots, link-health checker, WP REST writer
sf-jobs     job queue (job_runs), runner, per-site locks, scheduler, pipelines (crawl/plan/inject/undo/...)
sf-ops      monitor (CPU/RAM), notifications, error tracker, backup/restore, v1 Postgres migrator
sf-mcp      MCP tools (rmcp): stdio and streamable HTTP
sf-server   axum: /api/v1 (v1 contract), /api/v2 (new), /mcp, /health, embedded UI
siloforge   the binary: CLI (serve, mcp, init, backup, restore, migrate, doctor, token, version)
ui/         React UI (from v1, plus the v2 pages), built to ui/dist and embedded
```

## Parallel agents
- Set `CARGO_TARGET_DIR=D:\tmp\cargo-target\agent-<name>` (bash: `/d/tmp/cargo-target/agent-<name>`).
- Edit only the files/crates you own (your work packet lists them). The root `Cargo.toml` is owned by the lead. If you need a new workspace dependency, add it to **your crate's** Cargo.toml with an explicit version, and say so in your report.
- Re-read shared files before surgical edits. Never fix another agent's in-progress file. Delete your target dir when done.
- Before ending: append a terse entry to `log/devlog.md` (what landed, test counts, what is open).

## Tests
- Unit tests next to the code.
- Every parser and request handler gets a "hostile input never panics" test.
- External services (WordPress, providers, OpenRouter) are stubbed with a local axum server. Never call real providers or real WP sites in tests.
- Golden tests for the scorer and anchors use fixtures taken from v1 behaviour.

## Build / run
- Dev: `cargo run -p siloforge -- serve --home D:\tmp\sf-dev-home` (side ports via `SILOFORGE_BINDS=127.0.0.1:8610,127.0.0.1:8611`).
- Release: `cargo xtask release` -> `dist/<version>/`. Installed at `D:\Final_Apps\SiloForge\` (exe + `data\` home).
- The live v1 stack keeps running on 8600/8601 until cutover. Never stop it, and never write to its Postgres.
