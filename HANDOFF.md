# SiloForge — Handoff (2026-10-14)

Read this first, then `AGENTS.md` (rules), `TODO.md`, the tail of `log/devlog.md`, and the skill `siloforge-dev`.

## What SiloForge is now
A self-hosted internal-linking platform for WordPress, rebuilt in Rust (v1 TypeScript/Docker stack is **archived**).
One binary `siloforge.exe` = API + UI + job runner + scheduler + MCP server + backups. SQLite storage, ONNX Runtime
embeddings on the GPU (DirectML), multi-provider AI with fallback.

| Item | Where |
|---|---|
| Source (private, full history) | `D:\Code\SiloForge-Suite\siloforge-rs` (branch `main`) |
| Public repo (scrubbed snapshots) | https://github.com/NazmulGit/siloforge — releases v2.0.0, 2.0.1, 2.0.2, 2.1.0 (app zip + plugin zip) |
| Installed app | `D:\Final_Apps\SiloForge\app\siloforge.exe` (+ onnxruntime.dll, DirectML.dll); previous exes kept as `siloforge-<ver>.exe.bak` |
| Data home | `D:\Final_Apps\SiloForge\data` (siloforge.db, siloforge.toml, secrets.env, logs, models, runtime) |
| Release zips | `D:\Final_Apps\SiloForge\releases` (app 2.0.0–2.1.0, plugin 0.2.0) |
| Backups | `D:\Backups\SiloForge` (encrypted daily + pre-upgrade); v1 archive in `D:\Backups\SiloForge\v1-archive` (RESTORE-v1.md) |
| URLs | API + UI http://127.0.0.1:8600 and :8601 (both serve everything) |
| Autostart | Scheduled Task `siloforge-keepalive` every 5 min = `conhost.exe --headless siloforge.exe --home …\data serve` (2nd launch exits on the instance lock) |
| Admin login | `admin@siloforge.local`, password = `ADMIN_PASSWORD` in `data\secrets.env` (never print it) |

## Production state (verified 2026-10-14)
- **LIVE: v2.1.0**, health ok, 83 sites / 15.8k posts / 429k paragraphs migrated from v1 (verify PASS).
- AI: providers `codex-proxy` (http://127.0.0.1:8791/v1, dedicated proxy client key "SiloForge", models gpt-6-luna,
  gpt-5.6-luna, gpt-6-sol) and `dario` (http://127.0.0.1:3460/v1). Active chat chain **"Main: Luna -> Haiku"**
  (gpt-6-luna → claude-haiku-4-5). All 5 models probe OK.
- Hermes: profile `ap13_internallink` → `siloforge.exe mcp` with API token `hermes-ap13` (scopes read,write,admin);
  Hermes sees 117/118 tools (raw `siloforge_api_request` is off). Telegram notifications via Hermes gateway route `siloforge` work.
- Integrations repointed to v2 (Backup Hub, Mission Control watchdog, Model Control, teli scripts) — see `docs/INTEGRATIONS-v2.md`.

## Work in flight (NOT yet released/deployed)
| Version | What | State | Where |
|---|---|---|---|
| **2.2.0** | Link Runs (9-step guided bulk linking: crawl → opportunities → strategies → mode → run → review → deploy → results with "Open ↗" at the link → "Apply properly" repair), runs history + undo, Auto-Linker (scheduled, dry-run/live), 15 MCP tools | backend + UI + guide committed on `main`; a real-browser E2E on the local test WP (site69-local :8391) is running and fixing bugs in the **main checkout** (uncommitted changes in `crates/sf-jobs/src/pipelines/link_run/*`, `sf-server/src/v2/link_runs.rs`, tests, guide) | `main` |
| **2.3.0** | AI task tiers (low/main/high chains, seeded: low gpt-5.6-luna→haiku-4-5, high gpt-6-sol→haiku-5-5→luna), routing, batching, ai_cache, skip rules, budgets, token accounting in Logs (source `ai`) + Usage panel, **Always use AI** switch, run estimate | done, 546 tests green | branch `feature/v2.3`, worktree `D:\Code\SiloForge-Suite\siloforge-rs-v23` |
| **2.4.0** | SaaS: workspaces + roles, plans & quotas (402), login/register (closed/invite/open), email verify/reset, TOTP 2FA, sessions, SMTP (lettre+rustls) with admin outbox, admin console, workspace settings, branding, 162 MCP tools | done, 571 tests green; based on `feature/v2.3` | branch `feature/v2.4`, worktree `D:\Code\SiloForge-Suite\siloforge-rs-v24` |

Specs: `docs/specs/v2.2-link-runs.md`, `v2.3-ai-tiers.md`, `v2.4-saas.md` (each has a "deviations" section written by the builders).

## Next steps (in order)
1. Wait for / finish the v2.2 E2E (report: `docs/testing/link-runs-e2e.md`); review and commit its fixes on `main`; `cargo xtask ci` green.
2. Merge `feature/v2.3` then `feature/v2.4` into `main` (expect conflicts in `crates/sf-jobs/src/pipelines/link_run/*`,
   `crates/sf-server/src/v2/*`, `ui/src/pages/LinkRun.tsx`, `ui/src/api2.ts`, migrations numbering — keep both sides; migrations
   must stay strictly increasing and never renumber an applied one; production DB is at schema v6 = 0001–0006).
3. Bump version (VERSION, root Cargo.toml, ui/package.json) — release 2.2/2.3/2.4 either as one **2.4.0** or step by step;
   CHANGELOG sections; `cargo update --workspace --offline`; commit.
4. Combined test on a copy of prod (port 867x, temp home, never deploy against real sites from a copy) + a short E2E on
   site69-local; then `bash scripts/release.sh <ver>`.
5. Deploy (procedure in `docs/RELEASING.md` / skill): encrypted pre-upgrade backup → disable keepalive task → kill PID from
   `data\siloforge.pid` → swap exe (keep `.bak`) → enable + run task → `/health` version → smoke (login, 83 sites, MCP tools).
6. After deploy: set `ai.always_on = true` (user wants it); check `/api/v2/ai/usage` for errors with the codex reasoning models;
   registration stays **closed**, SMTP not configured (reset/invite links in Admin → Email); enable 2FA for the admin account;
   set `public_url`/`trust_proxy` only when exposing behind a TLS reverse proxy.
7. `bash scripts/publish-github.sh <ver>`; update devlog, CHANGELOG, `docs/guide`, KB note
   (`D:\Code\Claude_sessaion_saved_md_kb\siloforge\2026-10-14_siloforge-v2-rust-rebuild.md`), skill `siloforge-dev`, memory.
8. Remove the worktrees when merged: `git worktree remove ../siloforge-rs-v23` and `../siloforge-rs-v24`.

## Must-know rules / gotchas
- Never contact real WordPress sites in tests; local test WP = `D:\Code\SiloForge-Suite\_archive\v1\app\dev\site69-local` (127.0.0.1:8391). `wh-wp`/`wh-db` (:8089) are NOT ours.
- Never change a v1 endpoint contract; new things under `/api/v2`.
- Stop the server by PID (`data\siloforge.pid`), never `taskkill /IM siloforge.exe`.
- PowerShell/pwsh hang on this PC when `ALLUSERSPROFILE` or `SYSTEMDRIVE` is set → use bash, or `env -u ALLUSERSPROFILE -u SYSTEMDRIVE powershell …` (skill `powershell-startup-hang`). `WScript.Shell.Run` also hangs → no VBS keepers.
- Build: GNU toolchain (rustup override on the folders) + `export PATH="/d/Programs/winlibs/mingw64/bin:$PATH"`; own `CARGO_TARGET_DIR` under `D:\tmp\cargo-target\`.
- Python heredocs mangle Windows paths (`\v`, `\a`, `\t` become control chars) → write scripts to a file with the editor, or use raw strings.
- Everything on D:\ (never D:\Ryzen); secrets never printed or committed; public repo only via `scripts/publish-github.sh` (scrubs real domains, drops internal docs).

## Open items (beyond the release train)
- WP plugin 0.2.0 paragraph match ≈ 89% for linked paragraphs (skips safely otherwise); plugin mode is unused by the fleet.
- Anchor edge case "Bell Air Attack"; restart under the real keepalive task not yet exercised.
- Delete v1 Docker volumes/images after a long stable period (RESTORE-v1.md).
