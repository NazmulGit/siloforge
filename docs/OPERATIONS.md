# SiloForge v2 operations runbook

Paths below are the default install on this PC. Adjust `<Root>` / `<home>` for other installs.

| Thing | Where |
|---|---|
| Install root `<Root>` | `D:\Final_Apps\SiloForge` |
| Binary | `<Root>\current\siloforge.exe` (`current` = junction to `app\<version>\`) |
| Data home `<home>` (`SILOFORGE_HOME`) | `<Root>\data`: `siloforge.db` (+ `-wal`, `-shm`), `siloforge.toml`, `secrets.env`, `logs\`, `models\`, `runtime\`, `siloforge.pid` |
| Backups | `D:\Backups\SiloForge\v2` (production install; `backup.dir` in `siloforge.toml`), else `<home>\backups` |
| v1 final dump | `D:\Backups\SiloForge\v1-final-<ts>.dump` (written by the cutover) |
| Autostart | Scheduled Task `siloforge-keepalive` -> `wscript <Root>\keeper\siloforge-keepalive.vbs` |
| Install / cutover logs | `<Root>\logs\` (`install-history.log`, `cutover-<ts>.log`, `rollback-<ts>.log`) |
| Ports | `127.0.0.1:8600` and `127.0.0.1:8601` (both serve API + UI + `/mcp`) |

In the examples, `sf` means `& 'D:\Final_Apps\SiloForge\current\siloforge.exe'` with `--home D:\Final_Apps\SiloForge\data`
(or `$env:SILOFORGE_HOME` set). Every command accepts `--home`.

---

## 1. Start, stop, restart

The keepalive task runs at logon and every 5 minutes. Its hidden keeper exits at once if port 8600 is already
listening, otherwise it starts `siloforge.exe serve` with `SILOFORGE_HOME` set. So "start" means "run the task".

```powershell
Start-ScheduledTask siloforge-keepalive                       # start now (no-op if running)
# stop: disable first, or the task brings it back within 5 min
Disable-ScheduledTask siloforge-keepalive; Stop-ScheduledTask siloforge-keepalive
Get-CimInstance Win32_Process -Filter "Name='siloforge.exe'" | Where-Object CommandLine -match ' serve' |
  ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
Enable-ScheduledTask siloforge-keepalive; Start-ScheduledTask siloforge-keepalive   # start again
```

Stopping kills the process (a hidden process cannot receive Ctrl-C). That is safe for the data: SQLite WAL is
crash-safe, and jobs left `running` are re-queued or failed by the reaper at the next start (heartbeat older than
the timeout). **Do not stop it during an inject**: a post may be updated in WordPress before the edge row is marked
`applied`. Check first: `Invoke-RestMethod http://127.0.0.1:8600/health` -> `jobs.running` must be 0.

Foreground (debugging): stop the task as above, then `sf serve` in a console. Ctrl-C shuts down gracefully
(20 s grace for running jobs). Run only one `serve` per home. A second one on the same ports fails to bind; one
started with different `SILOFORGE_BINDS` would run, so never do that (two job runners on one database).
`serve` writes `<home>\siloforge.pid`, which `restore` uses to refuse overwriting a running instance.

## 2. Health and logs

- `GET http://127.0.0.1:8600/health` (no auth): `{ok, version, uptime_secs, services:{db,redis,qdrant,queue}, jobs:{running,queued}}`.
  `redis`/`qdrant` are kept for old probes and mirror `db`. Mission Control probes this URL.
- `sf doctor`: config, `PRAGMA quick_check`, schema version, counts, embedding rungs + self-test (cosine vs a stored
  production vector, must be >= 0.9999), ports, disk. Exit code 1 if anything fails.
- Logs: `<home>\logs\siloforge.log.<date>` (JSON lines, daily files). Filter with `$env:SILOFORGE_LOG`
  (default `info,hyper=warn,reqwest=warn,ort=warn`; e.g. `debug,sf_jobs=trace`), set in the keeper or the console.
  ```powershell
  Get-Content (Get-ChildItem D:\Final_Apps\SiloForge\data\logs | Sort LastWriteTime | Select -Last 1).FullName -Tail 50 -Wait
  ```
- Per-job logs: UI -> All jobs -> Logs, `GET /api/v2/jobs/{id}/logs?after_id=`, or MCP `siloforge_job_logs`.
- Errors (deduplicated by fingerprint): UI -> System, `GET /api/v2/errors`.
- System metrics: UI -> System, `GET /api/v2/system`, `/system/stats`, `/system/metrics?window=1h|24h|7d`.

## 3. Jobs

Kinds: `crawl`, `plan`, `inject`, `undo`, `keyword_link`, `link_health`, `audit`, `embed`, `backup`, `maintenance`.

- Mutating jobs on one site never overlap (per-site lock); identical queued jobs are deduplicated.
- `resources.max_concurrent_jobs` (default 3) jobs run at once across sites.
- Timeouts: crawl/plan 2 h, inject 1 h, link_health 3 h, audit 6 h, others 30 min.
- Heartbeat every 15 s; the reaper runs at boot and every 5 min and fails or re-queues stale `running` rows.
- Cancel: UI, `POST /api/v2/jobs/{id}/cancel`, MCP `siloforge_cancel_job`. A running job stops at its next
  checkpoint (aborted after 30 s grace). Links already applied by a cancelled inject stay applied (use undo).
- List: `GET /api/v2/jobs?status=running|queued|failed&kind=&site_id=&limit=`; per site: `GET /api/v1/jobs/{site}`.
- Schedules (cron + IANA time zone) per site and kind: UI -> site -> Schedules, `/api/v1/schedules/{site}`.
  The scheduler ticks every 60 s. A `link_health` schedule can carry `{"internal_only": true}`.

## 4. Backups and restore

- Automatic: `backup.schedule` (default `17 3 * * *`, local time), keeps `backup.keep` (10) archives.
- Manual: `sf backup create [--kind full|db|config] [--note text]`; UI Settings -> Backups; `POST /api/v2/backups`;
  MCP `siloforge_create_backup`. `full` = DB snapshot (`VACUUM INTO`, consistent while running) + `siloforge.toml` +
  `secrets.env`; `db` = DB only; `config` = TOML + secrets.
- Encryption: `siloforge init` writes a random `SILOFORGE_BACKUP_PASSPHRASE` into `secrets.env`, so archives are
  `.zip.sfenc` (AES-256-GCM, scrypt) by default. **Keep a copy of that passphrase outside this PC** (password
  manager): a restore on a new PC needs it. Without a passphrase the archive is a plain zip and `secrets.env` is
  left out (manifest `secrets_excluded`) unless you pass `--include-secrets`; copy `secrets.env` by hand then.
- Verify: `sf backup verify <file> [--passphrase ...]` (manifest + SHA-256 of every file).
- List: `sf backup list`.
- Restore (server stopped, see 1.):
  ```powershell
  sf restore D:\Backups\SiloForge\v2\<archive>.zip           # add --passphrase for .sfenc
  ```
  Restore is atomic and keeps the replaced files as `*.pre-restore-<ts>`. It refuses a backup from a newer schema
  and refuses while a server runs on the home (`--force` overrides the latter). `runtime\` and `models\` are not in
  backups: copy them or run `sf fetch-runtime` and `sf fetch-model`.
- Backup Hub: run `sf backup create --kind db` as a pre-hook and include the backup folder (see MIGRATION.md 1.6).

## 5. Upgrades

```powershell
# 1. build or download the new release, then:
powershell -ExecutionPolicy Bypass -File <release>\scripts\install.ps1
```
`install.ps1` refuses while jobs are running (`-Force` overrides), makes a `backup create --kind db` with the
installed version, stops the server, copies the release to `app\<new version>\`, switches the `current` junction,
and starts + health-checks the new version. Database migrations run automatically at start.

Downgrade: the old version stays in `app\<old>\`. Stop the server, point the junction back
(`cmd /c rmdir D:\Final_Apps\SiloForge\current` then `cmd /c mklink /J D:\Final_Apps\SiloForge\current D:\Final_Apps\SiloForge\app\<old>`),
and restore the pre-upgrade backup if the new version migrated the schema (an older binary refuses a newer schema).

## 6. Moving to another PC

The home directory is the whole state. Nothing is stored in the registry or under the user profile.

Option A, copy the home (fastest, exact):
1. Old PC: stop the server (1.).
2. Copy `<home>` (including `siloforge.db-wal` if present) to the new PC, e.g. `robocopy D:\Final_Apps\SiloForge\data <dest> /E`.
3. New PC: unzip the release and run `install.ps1` (it finds the existing `secrets.env` and skips `init`).

Option B, backup + restore:
1. Old PC: `sf backup create --kind full` (with a passphrase).
2. New PC: `install.ps1`, stop the server, `sf restore <archive> --passphrase ...`, copy `models\` + `runtime\`
   (or `fetch-model` / `fetch-runtime`), start.

After the move: `sf doctor`; WordPress sites must be reachable from the new PC; DirectML picks the new GPU
automatically (or falls back to CPU). Keep the same `secrets.env`: `SILOFORGE_ENCRYPTION_KEY` decrypts the stored
WordPress credentials and `JWT_SECRET` keeps tokens valid. Update consumers that point at the old PC (Hermes MCP
config, Mission Control, Backup Hub).

## 7. Several PCs

Each PC runs its own instance with its own home; there is no shared live database (SQLite has one writer, and
ADR 0001 keeps multi-PC sharing out of scope). Patterns:
- **One primary, others idle**: keep a cold copy via backups; promote by restoring the latest backup.
- **Split by site**: different sites on different PCs. Move a site with `migrate export --site` / `migrate import`
  only from a v1 source; between v2 instances, move the whole home.
- **Never run two instances against the same WordPress sites at once**: both would plan and inject independently.
- Remote access: binds are loopback by default. To expose to the LAN, set `server.binds` to a LAN address and put
  it behind a firewall rule for trusted hosts only. `/mcp` only accepts a loopback `Host` header.

## 8. Resource tuning

| Setting | Default | Effect (measured on a 12C/24T Ryzen 9 3900X, see docs/testing/performance.md) |
|---|---|---|
| `resources.cpu_threads` | 0 = logical/4 (6 here) | Size of the compute pool (scoring, graph, anchors, CPU embeddings). With CPU embeddings: 2 -> 1.8 cores avg, 134 texts/s; 6 -> 3.8 cores, 204 texts/s; 12 -> 7.0 cores, 221 texts/s. 12 costs 75 % more CPU for 8 % more speed. Env override `SILOFORGE_CPU_THREADS`. |
| `resources.max_concurrent_jobs` | 3 | Jobs on different sites at once. Lower to 1 on small machines. |
| `resources.low_priority_jobs` | true | Compute threads run at below-normal priority. (ONNX Runtime's own threads are not lowered yet.) |
| `resources.defer_when_cpu_above` | 0 (off) | Heavy jobs wait while total CPU is above this percent. |
| `resources.vector_cache_mb` | 512 | RAM for cached site vectors (~1.6 KB per paragraph; the 112k-paragraph site needs ~270 MB). Vectors stay cached after a plan until LRU eviction. |
| `embed.gpu` | true | DirectML on Windows when a GPU is present (387-715 texts/s at ~0.9 core). `false` forces CPU. |
| `embed.chain` | remote, remote2, onnx, local | Order of embedding rungs. `["onnx"]` = local only, no OpenRouter calls. |
| `embed.batch_size` | 64 | Texts per batch (batches are length-sorted). |
| `crawl.concurrency` | 4 | Parallel requests per crawl (with per-host politeness and 429/503 backoff). |

GPU off: set `embed.gpu = false` (or remove `DirectML.dll` from the runtime folder); restart. GPU-less helper PCs
fall back to the CPU session automatically.

## 9. Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `bind 127.0.0.1:8600 ... is v1 or another instance still running?` | v1 containers still hold the port, or a second instance | `docker ps` (stop `siloforge_api`/`siloforge_ui`), or find the other `siloforge.exe` |
| `restore` refuses: server running | `<home>\siloforge.pid` names a live process | stop the server; after a crash a stale pid file is harmless (only a live pid blocks), `--force` overrides |
| `JWT_SECRET must be set` at start | home has no `secrets.env` | `sf init` (or `install.ps1`) |
| Task runs but nothing listens | keeper fails before `serve` | run `sf serve` in a console to see the error; check `<home>\logs` |
| Port forbidden (WinError 10013) | Hyper-V reserved a port range over 8600 | `netsh int ipv4 show excludedportrange tcp`; keep the dynamic range at 49152+ (hub-pc-migration skill) |
| `doctor`: embeddings FAIL, no onnxruntime.dll | runtime missing | `sf fetch-runtime` (or copy the DLLs into `<home>\runtime`) |
| `doctor`: embeddings FAIL, model not found | model missing | `sf fetch-model` |
| Embeddings slow, `device cpu` | DirectML failed to initialise (driver) | update the GPU driver; check the log line for the DML error; CPU still works |
| Plan finishes but many `content_gap` edges | no LLM configured | add a chat model chain (Settings -> Models) or keep `llm.fallback_url` pointing at dario |
| Inject: edge stays approved with an error "anchor not found" | the post was edited in WordPress after planning | re-plan the site, or edit the anchor |
| Inject: `conflict` | the post changed between read and write twice | retry; the edit made in WordPress is never overwritten |
| Crawl gets 401 | WordPress application password missing/wrong, or app passwords disabled on plain http | set credentials on the site (PATCH `/sites/{id}`); the crawler falls back to rendered content |
| Crawl blocked for `localhost`/LAN/`site01.example` sites | SSRF guard | as admin: `PATCH /api/v1/sites/{id}` with `{"settings":{"allow_private_host":true}}`, or the global `crawl.allow_private_hosts = true` (only the site's own hosts may then be private; metadata IPs never) |
| UI says "Feature not available on this server" | v2 route missing or returned 503 | the binary is older than the UI, or a service (monitor/notifier) failed to start: check the log |
| Login 429 / account locked | failed-login limit per IP, or per-account lockout (5 attempts, then 30 s doubling to 15 min) | wait; check for a script with an old password |
| MCP tool returns 401 | token revoked/expired | `sf token create ...`, update the Hermes config |
| Restore refuses: newer schema | backup from a newer version | install that version first |
| High RAM after a plan | site vectors cached | expected (bounded by `vector_cache_mb`); lower it if needed |
| `database is locked` in logs | an external tool holds a write lock on `siloforge.db` | never write to the DB with other tools; read-only queries are fine (WAL) |
