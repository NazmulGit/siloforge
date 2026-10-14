# SiloForge v2 operations runbook

Paths below are the default install on this PC. Adjust `<Root>` / `<home>` for other installs.

| Thing | Where |
|---|---|
| Install root `<Root>` | `D:\Final_Apps\SiloForge` |
| Binary | `<Root>\current\siloforge.exe` (`current` = junction to `app\<version>\`) |
| Data home `<home>` (`SILOFORGE_HOME`) | `<Root>\data`: `siloforge.db` (+ `-wal`, `-shm`), `siloforge.toml`, `secrets.env`, `logs\`, `models\`, `runtime\`, `siloforge.pid` |
| Backups | `D:\Backups\SiloForge\v2` (production install; `backup.dir` in `siloforge.toml`), else `<home>\backups` |
| v1 archive (dump, images, restore guide) | `D:\Backups\SiloForge\v1-archive\` (`RESTORE-v1.md`); v1 source `D:\Code\SiloForge-Suite\_archive\v1` |
| Autostart | Scheduled Task `siloforge-keepalive` (every 5 min) -> `conhost.exe --headless "<Root>\app\siloforge.exe" --home <Root>\data serve` |
| Install / cutover logs | `<Root>\logs\` (`install-history.log`, `cutover-<ts>.log`, `rollback-<ts>.log`) |
| Ports | `127.0.0.1:8600` and `127.0.0.1:8601` (both serve API + UI + `/mcp`) |

In the examples, `sf` means `& 'D:\Final_Apps\SiloForge\current\siloforge.exe'` with `--home D:\Final_Apps\SiloForge\data`
(or `$env:SILOFORGE_HOME` set). Every command accepts `--home`.

---

## 1. Start, stop, restart

The keepalive task runs every 5 minutes and starts `siloforge.exe serve` headless (no window) through
`conhost --headless`. If SiloForge is already running, the new launch exits at once because the running process
holds an exclusive lock on `data\siloforge.lock`. So "start" means "run the task". (The earlier wscript/VBS keeper
was retired on 2026-10-14: `WScript.Shell.Run` hangs on this PC, see the `powershell-startup-hang` skill.)

From cmd/bash (works even when PowerShell is affected by that hang):
```bat
schtasks /run /tn siloforge-keepalive                          :: start now (no-op if running)
schtasks /change /tn siloforge-keepalive /disable              :: stop: disable first ...
taskkill /F /PID <pid from D:\Final_Apps\SiloForge\data\siloforge.pid>   :: ... then kill by PID, never /IM
schtasks /change /tn siloforge-keepalive /enable & schtasks /run /tn siloforge-keepalive   :: start again
```

PowerShell equivalent (start PowerShell with ALLUSERSPROFILE and SYSTEMDRIVE cleared on this PC):
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
- Search Console (`gsc` job, also a step of every `audit`): per-site credentials (Settings -> Search Console) win;
  a site without them uses the install-wide `GSC_CREDENTIALS_JSON` from the environment or `secrets.env`
  (service-account JSON inline, or a path to the JSON file; v1 parity). The value is never logged, serialised or
  returned; job stats say `"credentials": "site" | "global"`. With the global fallback every audit tries GSC for
  every site: a site the service account cannot read only logs a warning.
- Keyword rules (`keyword_link`) create at most the site's `max_links_per_post` suggestions per source post per run.
- Database: the daily `maintenance` job prunes, runs `PRAGMA optimize` + `incremental_vacuum` and checkpoints the WAL.
  New databases use `auto_vacuum=INCREMENTAL`; a database created before 2.0.2 shrinks only after a one-off
  `sf vacuum` (server stopped, needs free disk about the DB size, ~1-2 min per GB).

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
| `resources.cpu_threads` | 0 = logical/4 (6 here) | Size of the compute pool (scoring, graph, anchors) and the intra-op thread count of local CPU embeddings (ONNX Runtime CPU EP, candle's private pool). With CPU embeddings: 2 -> 1.8 cores avg, 134 texts/s; 6 -> 3.8 cores, 204 texts/s; 12 -> 7.0 cores, 221 texts/s. 12 costs 75 % more CPU for 8 % more speed. Env override `SILOFORGE_CPU_THREADS`. |
| `resources.max_concurrent_jobs` | 3 | Jobs on different sites at once. Lower to 1 on small machines. |
| `resources.low_priority_jobs` | true | Compute threads run at below-normal priority. Local embedding inference too: ONNX Runtime's intra-op threads are created below normal (custom thread manager) with busy-wait spinning off, and the thread that drives a batch is lowered for the call (restored afterwards). CPU-EP embedding is ~20-25 % slower without spinning; set `false` for maximum throughput on a dedicated box. |
| `resources.defer_when_cpu_above` | 0 (off) | Heavy jobs wait while total CPU is above this percent. |
| `resources.vector_cache_mb` | 512 | RAM for cached site vectors (~1.6 KB per paragraph; the 112k-paragraph site needs ~270 MB). Vectors stay cached after a plan until LRU eviction. |
| `embed.gpu` | true | DirectML on Windows when a GPU is present (387-715 texts/s at ~0.9 core). `false` forces CPU. |
| `embed.chain` | remote, remote2, onnx, local | Order of embedding rungs. `["onnx"]` = local only, no OpenRouter calls. |
| `embed.batch_size` | 64 | Texts per batch (batches are length-sorted). |
| `crawl.concurrency` | 4 | Parallel requests per crawl (with per-host politeness and 429/503 backoff). |

`job_runs.resources` (Jobs page "Peak RAM"/"CPU") is sampled for the whole process while the job runs
(`scope: "process"`); when `concurrent_jobs_max` > 1 other jobs ran at the same time and the figures are shared.

GPU off: set `embed.gpu = false` (or remove `DirectML.dll` from the runtime folder); restart. GPU-less helper PCs
fall back to the CPU session automatically.

## 9. Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `bind 127.0.0.1:8600 ... is v1 or another instance still running?` | v1 containers still hold the port, or a second instance | `docker ps` (stop `siloforge_api`/`siloforge_ui`), or find the other `siloforge.exe` |
| `restore` refuses: server running | `<home>\siloforge.pid` names a live process | stop the server; after a crash a stale pid file is harmless (only a live pid blocks), `--force` overrides |
| Enqueue answers 429 | the site already has 100 queued jobs (L-09) | wait for the queue to drain or cancel stale jobs (`GET /api/v2/jobs?status=queued&site_id=`) |
| Fresh install: registration refused | first-user registration only from `127.0.0.1` / `localhost` (L-17, DNS-rebinding guard) | open the UI as `http://127.0.0.1:8601`, or `sf user set-password <email> --create` |
| Lost a session / token leaked | | `POST /api/v2/auth/logout-all` (ends all sessions; revokes your API tokens unless `{"revoke_api_tokens": false}`) |
| `JWT_SECRET must be set` at start | home has no `secrets.env` | `sf init` (or `install.ps1`). `sf init --force` on an existing home re-imports but keeps `JWT_SECRET`, `SILOFORGE_ENCRYPTION_KEY`, the backup passphrase and every other existing line |
| `siloforge.db` never shrinks | database created before 2.0.2 (`auto_vacuum=NONE`) | stop the server, `sf vacuum`, start it again |
| GSC sync answers 409 "not connected" | no per-site credentials and no `GSC_CREDENTIALS_JSON` | connect the site in Settings -> Search Console, or set `GSC_CREDENTIALS_JSON` in `secrets.env` and restart |
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

## 10. SaaS deployment (2.4)

A single-user install needs none of this: keep the loopback binds and closed registration. Everything below is for
offering SiloForge to other people. User-facing summary: guide page `saas-deployment`.

### 10.1 TLS in front, always

**Never expose SiloForge over plain HTTP.** Passwords, session JWTs and `sfk_` tokens would cross the network in
clear text. Keep SiloForge on `127.0.0.1` and let a TLS reverse proxy on the same machine reach it. Only bind
`0.0.0.0` (`server.binds` in `siloforge.toml`, or `SILOFORGE_BINDS=0.0.0.0:8600`) when the proxy runs on another
host, the connection between them is private, and a firewall rule allows only the proxy to reach port 8600.

**Caddy** (automatic Let's Encrypt certificates), `Caddyfile`:
```caddy
links.example.com {
    encode gzip
    reverse_proxy 127.0.0.1:8600
}
```
Caddy sets `X-Forwarded-For` / `X-Forwarded-Proto` itself. Test with `caddy run --config Caddyfile`, then run it as
a service, with its data dir on `D:\`.

**Cloudflare Tunnel** (no inbound ports at all), `config.yml` for `cloudflared`:
```yaml
tunnel: <tunnel-id>
credentials-file: D:\Programs\cloudflared\<tunnel-id>.json
ingress:
  - hostname: links.example.com
    service: http://127.0.0.1:8600
  - service: http_status:404
```
`cloudflared tunnel route dns <tunnel-id> links.example.com`, then run `cloudflared tunnel run <tunnel-id>` as a
service. Turn on "Always Use HTTPS" for the zone. Optionally put Cloudflare Access in front of `/api/v2/admin/*`.

Notes:
- `/mcp` over HTTP accepts only a loopback `Host` header; remote agents should use the stdio transport with
  `SILOFORGE_API_URL=https://links.example.com` and a token (plus `SILOFORGE_WORKSPACE` to pick a workspace).
- CORS and auth are unchanged (Bearer tokens, no cookies); CSP and security headers are already sent.

### 10.2 Settings → Platform

`GET/PUT /api/v2/admin/platform` (stored in the DB, kv `platform_settings`; applies live and travels with backups):
- `deployment.public_url` = `https://links.example.com`. Used in every emailed link (verify, reset, invite,
  set-password). Without it links point at the loopback address and are useless to remote users.
- `deployment.trust_proxy` = `true` **only** when a proxy is in front. SiloForge then takes the client IP from
  `X-Forwarded-For` (rate limits, lockouts, sessions, audit) and the scheme from `X-Forwarded-Proto`. With it on and
  no proxy, any client can fake its IP and dodge the rate limits.

### 10.3 Email (SMTP)

Admin → Email (`smtp` section): host, port, TLS mode (`starttls` on 587, `tls` on 465, `none` only for a local relay),
username, password (write-only; stored encrypted with the secret key; `"***"` keeps it, `""` clears it), from
address and name (for example `SiloForge <no-reply@example.com>`). Then **Send test email**
(`POST /api/v2/admin/email/test {"to":"admin@example.com"}` -> `{ok, delivery, error?}`).

Without SMTP nothing breaks: every mail is written to the **outbox** (`mail_outbox`) with its link. Platform admins
copy the link from Admin → Email (`GET /api/v2/admin/email`), deliver it by hand and delete the entry
(`DELETE /api/v2/admin/email/outbox/{id}`). Invite creation also returns the `link` directly when delivery is manual.

### 10.4 Registration modes

`registration.mode`: `closed` (default; only platform admins create users, the first-run bootstrap from loopback still
works) | `invite_only` (sign-up needs a valid invite token, the user joins that workspace) | `open` (sign-up creates a
user and a workspace on `registration.default_plan_id`, default `free`). Further keys:
`require_email_verification` (unverified users can log in but not create sites), `allowed_domains` /
`denied_domains`, `rate_limit_per_hour` (per IP, default 5). Legacy `auth.allow_registration = true` counts as
`open` until the platform settings are saved once.

### 10.5 Security checklist

- [ ] TLS proxy (Caddy / Cloudflare Tunnel) in front; port 8600 not reachable from the internet; no plain HTTP.
- [ ] `public_url` set; `trust_proxy` on only behind the proxy.
- [ ] Registration **closed** by default; open it as `invite_only`, or `open` with `require_email_verification`.
- [ ] Email verification on for open sign-up; SMTP tested, or the outbox watched.
- [ ] **2FA for every platform admin** (Admin → Users shows who has it; recovery codes stored offline).
- [ ] Backups scheduled and verified (§4), copied off the machine; restore tested.
- [ ] `secrets.env` (`JWT_SECRET`, `SILOFORGE_ENCRYPTION_KEY`, backup passphrase) readable only by the service
      account, never in a repo or shared folder, and backed up separately from the data.
- [ ] Admin console audit (`/api/v2/admin/audit`) reviewed regularly: admin actions, view-as, failed logins.
- [ ] Rate limits left on: login lockout, per-IP sign-up limit, forgot-password limit.
- [ ] Hermes / agent tokens use the least scope (`read,write`); `admin` only for operator agents; unused tokens revoked.
