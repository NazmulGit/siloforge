# Migration guide

Two paths: **v1 -> v2** (Docker stack to `siloforge.exe`, once) and **PC -> PC** (moving a v2 install).
Background: [research/04-integrations.md](research/04-integrations.md) sections 8-10, [adr/0001](adr/0001-single-binary-sqlite.md).

---

## 1. v1 -> v2

### 1.1 What carries over
`siloforge migrate from-postgres` reads the live v1 database **read-only** (`docker exec siloforge_pg psql ... COPY
... TO STDOUT`, every session `SET default_transaction_read_only = on`) and writes the v2 SQLite file:

- All tables of v1 migrations 001-011: users (bcrypt hashes kept), sites, posts, paragraphs, keywords, link edges,
  clusters, rules, schedules, link health, metrics, model providers/models/chains, job history.
- UUIDs are kept, so links in the UI, MCP calls, kanban cards and the WP plugin stay valid.
- Embeddings (pgvector text) become f32 BLOBs: no re-embedding. Verified: paragraph vectors re-embedded by v2 match
  the stored ones (cos >= 0.999999).
- `sites.settings.wp_user` / `wp_app_password` / GSC credentials move into the encrypted `sites.secrets_enc`.
- v1 jobs still `queued`/`running` become `failed` (their BullMQ payload is gone).
- `posts.chunked_hash = content_hash`, so unchanged posts are not re-chunked. (The first crawl with credentials adds
  the raw content, which changes the hash, so each post is re-chunked once; paragraph ids and vectors survive by text hash.)

Not carried over: the Redis queue, Qdrant (v2 searches the stored vectors directly), and v1's `.env` except what
`init --from-v1-env` imports (`JWT_SECRET`, `ADMIN_EMAIL`, `ADMIN_PASSWORD`, `OPENROUTER_*`, `EMBED_REMOTE_MODEL`,
`EMBED_CHAIN`, `DARIO_*`, `GSC_CREDENTIALS_JSON`).

Keeping v1's `JWT_SECRET` keeps existing UI sessions and plugin site tokens valid. A new one only means one re-login
(the UI and the plugin both handle 401).

### 1.2 Behaviour that differs from v1 (intentional)
- `api_key` is only returned on site create / `rotate-key`; WP and GSC credentials are never returned
  (`has_wp_credentials` and `wp_user` instead).
- `edges?limit` above 1000 is clamped (max 5000) instead of 400; `posts?is_orphan=1|0` is accepted.
- History CSV writes ISO-8601 timestamps.
- Self-registration is off; registration of the first user is still possible on an empty database.
- LAN / `site01.example` sites need the admin-only site setting `allow_private_host: true` (SSRF guard).
- Inject writes raw content with a `modified_gmt` precondition and verifies the live page; undo removes exactly the link.
- The 12,108 v1 edges "marked for plugin pull" stay `approved`. v2 injects approved edges through WP REST when the
  site has credentials, so review that backlog before running a bulk inject on those sites.

Full list: [API.md](API.md) section 4.

### 1.3 Rehearsal (side by side, no downtime)
v1 keeps running on 8600/8601 throughout.
```powershell
$sf = 'D:\Code\SiloForge-Suite\siloforge-rs\dist\2.0.0\siloforge.exe'
& $sf init --home D:\tmp\sf-rehearsal --from-v1-env D:\Code\SiloForge-Suite\app\.env
& $sf migrate from-postgres --home D:\tmp\sf-rehearsal          # minutes; prints rows per table
& $sf migrate verify --home D:\tmp\sf-rehearsal                 # counts + 100 embedding samples per table
$env:SILOFORGE_BINDS = '127.0.0.1:8610,127.0.0.1:8611'
& $sf serve --home D:\tmp\sf-rehearsal                          # UI on http://127.0.0.1:8611/
```
This is what the contract diff (`docs/testing/contract-diff.md`) and the perf runs used. Delete the folder afterwards.

### 1.4 Cutover (scripted)
Prerequisites: a release built (`scripts\build-release.ps1`), v1 running and healthy, no v1 jobs running, T8 kanban
cards paused and no teli bulk script running (one writer at a time).

```powershell
cd D:\Code\SiloForge-Suite\siloforge-rs\dist\2.0.0\scripts     # or the unzipped release
# 1. install v2 next to v1 (task registered DISABLED because v1 holds :8600; nothing starts)
powershell -ExecutionPolicy Bypass -File .\install.ps1 -FromV1Env D:\Code\SiloForge-Suite\app\.env
# 2. dry run: read-only pre-checks + the plan
powershell -ExecutionPolicy Bypass -File .\cutover-from-v1.ps1
# 3. do it
powershell -ExecutionPolicy Bypass -File .\cutover-from-v1.ps1 -Execute
```

What `-Execute` does (log: `D:\Final_Apps\SiloForge\logs\cutover-<ts>.log`):

| Step | Action | On failure |
|---|---|---|
| 1 | Pre-checks: containers present, `siloforge_pg` running, no queued/running v1 `job_runs` (phantom rows older than N hours can be ignored with `-StaleJobHours N`), v2 exe + home + task present, >= 10 GB free on D:, JWT_SECRET parity | stops, nothing changed |
| 2 | `docker stop` `siloforge_api`, `siloforge_ui`, `siloforge-worker-1`, `siloforge-worker-2` (never removed). pg/redis/qdrant keep running | v1 restarted |
| 3 | `pg_dump -Fc` of the frozen DB to `D:\Backups\SiloForge\v1-final-<ts>.dump`, checked with `pg_restore --list` | automatic rollback |
| 4 | Old `siloforge.db*` in the home moved to `pre-cutover-<ts>\`; fresh `migrate from-postgres` | automatic rollback |
| 5 | `migrate verify` | automatic rollback |
| 6 | Enable + start `siloforge-keepalive`, wait for v2 `/health` (has a `version` field; v1's does not) | automatic rollback |
| 7 | Smoke: admin login (credentials from `secrets.env`, never printed), `/api/v1/sites` count == v1 count (83), UI on :8601, MCP `tools/list` == 28, `doctor` | reported; v2 keeps running, you decide |
| 8 | Prints the manual follow-ups below | |

The import is a full fresh import because the importer uses `INSERT OR IGNORE`: re-importing into an older test DB
would keep stale rows. Writers are stopped before the dump so the dump and the import see the same data.

To catch up an existing v2 home instead (for example a side-by-side test home after v1 kept running), use
`siloforge migrate from-postgres --refresh` (or `migrate import <dir> --refresh`): rows are UPSERTed by id, so edges
that v1 approved, applied or rejected meanwhile, re-crawled posts and changed settings overwrite the v2 copy; unchanged
rows are left alone (report columns `inserted`, `updated`, `ignored`). Refresh never deletes: rows that v1 no longer has
(for example paragraphs of a re-chunked post) stay in v2, so use a fresh home when you need an exact copy. Do not
refresh a home that v2 has already written to (plans, injects): v1's older copy of a row would overwrite v2's.

### 1.5 Manual follow-ups (in this order)
Before editing any file below, copy it to `<file>.bak-pre-sf2`. `rollback-to-v1.ps1` restores exactly those copies.

1. **Hermes ap13** (`D:\Agent\hermes\profiles\ap13_internallink\config.yaml`): create a token and switch the MCP to
   the Rust binary. Snippet in [HERMES.md](HERMES.md). This removes the plaintext admin password (risk R1). Old config
   backups under `D:\Agent\hermes` still contain that password: change the admin password afterwards
   (`siloforge user set-password admin@siloforge.local --password ...`, which also revokes old sessions).
2. **Mission Control** (`D:\Code\mission-control-port\services.json`): rows `siloforge-api` (probe `:8600/health`) and
   `siloforge-ui` (probe `:8601/`) keep working unchanged. Update the notes; optionally set the start command to
   `schtasks /Run /TN siloforge-keepalive` and enable the watchdog.
3. **Backup Hub** (`D:\Code\Ops\backup-hub\data\config.json`, every tier profile and the Hermes A-Z job): remove the
   `pg_dump siloforge_pg` hook and the `siloforge_qdrantdata` / `siloforge_pgdata` volume hooks; add a pre-hook
   `D:\Final_Apps\SiloForge\current\siloforge.exe backup create --kind db --home D:\Final_Apps\SiloForge\data` and
   include `D:\Backups\SiloForge\v2`. Do not back up `data\secrets.env` unencrypted to Drive; v2 backup archives
   are encrypted already. Run one backup and check the hook step.
4. **Model Control** (`D:\Code\Hermes-Tools\model-control\core\inventory.js`, `core\localroutes.js`): the `siloforge`
   entry edited `DARIO_MODEL_PRIMARY/FALLBACK` and `EMBED_MODEL` in `app\.env`. Point it at
   `D:\Final_Apps\SiloForge\data\secrets.env` (same key names; v2 reads `DARIO_*` and `EMBED_*` at start) and change
   the restart hint to `Stop-ScheduledTask siloforge-keepalive; <stop siloforge.exe>; Start-ScheduledTask siloforge-keepalive`.
   Better long-term: configure models in the UI (Settings -> Models), which needs no restart.
5. **teli scripts and skills** that use `docker exec siloforge_pg psql` or `printenv ADMIN_PASSWORD` stop working
   (the containers are stopped). Replace them with MCP tools or REST calls with an API token. Never write to
   `siloforge.db` from scripts.
6. **Hermes notifications** (optional): webhook route + `hermes` channel, see [HERMES.md](HERMES.md) section 5.
7. Docs: `D:\Code\README.md` ports table, the hub-pc-migration skill's verify list (SiloForge is now a Scheduled Task,
   not 7 containers), `D:\Final_Apps\SiloForge-Suite\README.md`.
8. Reboot test: after logon the task starts the server within a minute; `/health` answers.
9. Unpause T8 cards one at a time.

Keep the stopped v1 containers and their volumes for at least a week. After that they can be removed; the final dump
and `D:\Final_Apps\SiloForge-Suite` (2026-10-04 snapshot) remain the v1 baseline.

### 1.6 Rollback
```powershell
powershell -ExecutionPolicy Bypass -File D:\Final_Apps\SiloForge\current\scripts\rollback-to-v1.ps1 -WhatIf
powershell -ExecutionPolicy Bypass -File D:\Final_Apps\SiloForge\current\scripts\rollback-to-v1.ps1
```
It disables the task and stops v2, `docker start`s the 7 v1 containers in dependency order, waits for v1's `/health`,
and restores every `*.bak-pre-sf2` config copy (the v2 version is kept as `*.sf2-rolledback-<ts>`; `-KeepConfigs`
skips this). v1's database was never written by v2, so no restore is needed. Changes made in v2 after the cutover
(approvals, injections) are not copied back to v1: links injected by v2 are live in WordPress, and v1 does not know
about them. Re-crawl the affected sites in v1 if you stay on v1.

If the v1 DB itself is damaged: `pg_restore --clean --if-exists --no-owner` the `v1-final-<ts>.dump` into
`siloforge_pg`; last resort `D:\Final_Apps\SiloForge-Suite\app\RESTORE.md`.

---

## 2. PC -> PC (v2)

The data home is the whole state (database, config, secrets, logs, models, runtime). Nothing lives in the registry
or in the user profile, apart from the Scheduled Task, which `install.ps1` recreates.

### 2.1 Copy the home (recommended)
1. Old PC: `Disable-ScheduledTask siloforge-keepalive`, stop `siloforge.exe` (OPERATIONS.md section 1), check
   `/health` no longer answers.
2. Copy `D:\Final_Apps\SiloForge\data` (with `siloforge.db-wal` / `-shm` if present) to the same path on the new PC.
   `robocopy D:\Final_Apps\SiloForge\data \\NEWPC\D$\Final_Apps\SiloForge\data /E /COPY:DAT`.
3. New PC: unzip the release, `install.ps1` (finds `secrets.env`, skips `init`, registers the task, starts).
4. `siloforge doctor --home D:\Final_Apps\SiloForge\data`. DirectML picks the new GPU or falls back to CPU.

### 2.2 Backup + restore
1. Old PC: `siloforge backup create --kind full` (encrypted with the passphrase from `secrets.env`).
2. New PC: `install.ps1` (fresh home), stop the server, then
   `siloforge restore <archive> --passphrase <the OLD install's passphrase> --home D:\Final_Apps\SiloForge\data`.
   The restore brings the old `secrets.env` back (encryption key, JWT secret, passphrase).
3. Copy `models\` and `runtime\` or run `fetch-model` / `fetch-runtime`; start the task.

### 2.3 With the whole Hermes stack (hub-pc-migration)
SiloForge v2 needs no Docker. In the hub-pc-migration flow it is one more D:\ folder (`D:\Final_Apps\SiloForge`) plus
one Scheduled Task (`siloforge-keepalive`, exported with the other tasks). Verify with `/health` on :8600 and the
Mission Control rows. The old gotcha "SiloForge containers exit 255 at reboot" no longer applies after cutover.

### 2.4 After any move
- Sites behind `site01.example` or LAN addresses must resolve on the new PC (`[crawl.host_aliases]`).
- Point consumers at the new PC if the hostname changed (Hermes MCP URL, Mission Control, Backup Hub).
- Never run the old and the new instance at the same time against the same WordPress sites.
