# SiloForge v2 — correctness & reliability audit

Date: 2026-10-13. Scope: the whole workspace at HEAD `21cf110` plus the uncommitted tree (`sf-engine/src/{nlp,anchor,insert}`
and `sf-jobs/src/pipelines/{anchors,plan,embed}.rs` + `ctx.rs` were being edited during the audit, so line numbers there may drift).
This was a read-only pass over the code. The only files written are the experiment under `D:\tmp\sf-rel\` and this report.
Paths are relative to `crates/` unless they start with `ui/` or `docs/`.

## Summary

| | Count |
|---|---|
| Reliability findings (R-*) | 22: 5 H, 11 M, 6 L |
| v1 bugs re-checked (01 §B #1–45, 02 §6.1–6.5) | ~86: ~50 fixed, 20 partial, 8 open, 9 n/a |
| Ghost issues (G2-*) | 19 |
| API mismatches (MCP↔server, UI↔server) | 13: 2 H, 4 M, 7 L |
| Test suite | 336 passed, 0 failed, 9 ignored (`--no-fail-fast`), with 1 **flaky** test (below) |

**Test suite.** `cargo test --workspace -q` ran twice:
- The first run stopped at `sf-ops` because `backup::tests::create_verify_restore_roundtrip` panicked at `sf-ops/src/backup/mod.rs:915` (`assertion failed: third.path.exists()`).
- The second run with `--no-fail-fast` passed every test.
- Running that one test six times failed 2 of 6.

This is a real bug, not a test bug: see R1. No other failures or flakes were seen.

## 1. Findings

| ID | Sev | Where | Failure scenario | Fix |
|---|---|---|---|---|
| R1 | **H** | `sf-ops/src/backup/mod.rs:198-199,265` (`short = new_id().get(..8)`); `sf-core/src/lib.rs:22` | `new_id()` became UUIDv7 in 21cf110, so its first 8 hex characters are the top 32 bits of the millisecond clock. They stay the same for about 65 s. Two consequences: **(a)** Two backups of the same kind and encryption created in the same second get the same file name. The second `rename` overwrites the first archive. `prune` then deletes the older row's path, which is now the newer backup's file, so the newest backup row is `ok` but points at a missing file. This is the flaky test. **(b)** Concurrent backups within about 65 s (scheduled plus a UI or MCP backup) share `.tmp-{short}`. `VACUUM INTO` fails because the file exists, or one run's `remove_dir_all` deletes the other's staging files. | Use `crypto::random_hex(6)` (or the full id) for `short` and the tmp dir. Open the final file with `create_new` instead of renaming over it. Grep every other `new_id()` use that relies on a prefix being unique. |
| R2 | **H** | `sf-ops/src/instance.rs:17-22`; `siloforge/src/serve.rs:91,126,141`; `sf-jobs/src/runner.rs:166` | `InstanceGuard::acquire` never checks for a live instance; it overwrites `siloforge.pid`. `serve` then starts the runner, whose boot reaper runs `reap_stale(0)`, **before** it binds the ports. Starting a second `serve` on the same home (logon task plus a manual start, or dev side ports via `SILOFORGE_BINDS`) causes three problems. **(1)** Every job the live instance is running is marked `failed`, but the jobs keep running. Site exclusivity is lost, so a second plan or inject can start on the same site. **(2)** The second instance exits on the bind error, and its `Drop` deletes the live instance's pid file, so `restore` no longer refuses to overwrite a running database. **(3)** With different binds, both run fully: two runners, two schedulers and two backup loops on one DB. | Hold an exclusive OS lock (lock file opened with share mode 0 on Windows) for the process lifetime. Fail if `running_instance()` is another live pid. Bind the listeners before starting the runner and scheduler. Have `Drop` remove the pid file only if it still holds our pid. |
| R3 | **H** | `sf-jobs/src/queue.rs:355-375` (cancel), `:197-208` (exclusivity), `runner.rs:303-307`, `pipelines/inject.rs:131` | Cancelling a running job sets `status='cancelled'` at once. But `claim_next` treats a site as busy only while a row is `running`, and the pipeline keeps running until its next checkpoint, or until the 30 s grace period ends and it is aborted. Inject checkpoints only between posts, and one post (LLM call plus `verify_live` retries) can take minutes. **Scenario:** cancel an inject, then press inject again. A second inject (or plan, or crawl) starts on the same site while the first is mid-update on WordPress, and two jobs write the same post or `link_edges`. | Add a `cancelling` state, or include it in the busy check. Set `cancelled` only in `supervise` once the task has actually ended. Alternatively, have `claim_next` also consult the runner's in-memory running map. |
| R4 | **H** | `pipelines/inject.rs:301-304` → `:358-396`, rollback `:461-511`; abort sites `runner.rs:122-125,306`; `sf-crawl/src/wp.rs:571-576` | The WordPress write and its DB bookkeeping are not atomic, and an abort (cancel grace, hard timeout or shutdown) can land at any `.await`. If it lands after `update_post_content` succeeds but before `persist`, the link is live in WordPress while its edge stays `approved` with no `snapshot_id` or `link_start`. A later inject sees `AlreadyLinked` and marks the edge `failed`. The link is then untracked for good, because bulk undo only selects `applied` edges. The rollback path has the mirror problem: WordPress is restored while the edges stay `applied`. A transport timeout on the POST after WordPress committed has the same effect: the edges are marked "WordPress update failed" while the link is live. | Before the PUT, persist an in-flight marker (edge `applying` plus snapshot id). Run the PUT through persist as a non-abortable section (a detached task the supervisor awaits). On a transport error, re-read the post and compare it with `new_content`. Add a boot-time reconciler for `applying` edges. |
| R5 | **H** (perf / availability) | `sf-store/migrations/0001_init.sql:122` (`link_edges.paragraph_id … ON DELETE SET NULL`, no index); `pipelines/chunk.rs:228-231` | Every `DELETE FROM paragraphs` makes SQLite full-scan `link_edges` (all sites) to apply SET NULL. Measured with `D:\tmp\sf-rel\fk_bench.py` on 150k edges: **23.5 ms per delete without an index vs 0.02 ms with one**. A re-chunk that drops 10k paragraphs (a splitter change, or the first credentialed crawl of migrated posts) holds the single writer for about 4 min. Every API write, heartbeat, login audit row and job log queues behind it. These other FK children have no index either: `link_edges.snapshot_id`, `link_health.target_post_id`, `clusters.pillar_post_id`, `site_schedules.last_job_run_id`, `site_metrics_history.job_run_id`, `model_call_log.chain_id`. Deleting posts, snapshots or job_runs scans those tables. | Migration 0004 with `CREATE INDEX … ON link_edges(paragraph_id)` and the other FK child indexes, plus a test asserting that every FK child column is indexed. |
| R6 | M | `sf-jobs/src/queue.rs:173-176,192-209` | `claim_next` looks only at the 50 oldest queued rows, then skips busy sites and deferred heavy kinds. If those 50 rows all belong to a busy site, or are heavy while the CPU is busy, every other runnable job starves. **Example:** an MCP agent enqueues 60 inject jobs for site A with different `edge_ids`, so they don't dedupe. Jobs for site B then wait until A's backlog drops below 50. | Do the filtering in SQL (`NOT EXISTS` a running row for the same site; kind filter), or page through the queue. |
| R7 | M | `sf-store/src/lib.rs:107-109,142-150,214-225` | DB closures run without `catch_unwind`. Indexing and slicing are not clippy-denied, so panics are possible. A panic in a write closure kills the `sf-db-writer` thread: every later write fails with "writer thread gone" until restart, while the server keeps answering. A panic in a read closure drops that pooled connection; after 6, every read fails with "reader pool empty". `read_blocking` also bypasses the semaphore, so a concurrent async read can get "reader pool empty" instead of waiting. | Wrap `job(conn)` and `f(&conn)` in `catch_unwind` and return the connection through a drop guard. Supervise and respawn the writer. Make `read_blocking` take a permit. |
| R8 | M | `siloforge/src/serve.rs:121-123` (`_backup_task`, `_retention_task`); `sf-ops/src/monitor.rs:197`; `notify.rs:226`; `sf-jobs/src/scheduler.rs:52`; `retention.rs:95` | Each background loop is a bare `tokio::spawn` whose handle is dropped. A panic or early return ends the service silently: no errors row, no `/health` flag, no notification. Backups, retention, monitoring or cron simply stop. | Use a supervisor that restarts the loop with backoff, records it via `ErrorTracker`, and reports service liveness in `/health` and `/api/v2/system`. |
| R9 | M | `pipelines/undo.rs:285-288` | `mode=snapshot` reads the current `modified_gmt` and immediately writes the earliest snapshot, so the "precondition" can never fail. Every editor change made since that snapshot is silently lost (v1 bugs #2 and #3, back as an opt-in). | Refuse when the post's `modified_gmt` differs from the value stored after the inject, unless `force` is set. Report which edits would be lost. |
| R10 | M | `sf-crawl/src/wp.rs:554-576`; `client.rs:509-531` | The `modified_gmt` precondition is GET, compare, then POST, not an atomic conditional write, and the timestamp has 1-second resolution. An editor save between the GET and the POST is overwritten. 429 and 503 on the POST are retried; the content is identical so this is harmless, but each retry adds a WordPress revision. | After the write, compare the returned `content.raw` with `new_content`. Re-GET just before the POST. Document the residual window. |
| R11 | M | `sf-embed/src/candle.rs:109`; `onnx.rs:264`; `sf-engine/src/pool/mod.rs:79-92` | Embedding (the heaviest stage) runs on tokio `spawn_blocking`. Candle's gemm uses rayon's **global** pool (all logical cores, normal priority), not the private `sf_engine` pool. So `resources.cpu_threads` and BELOW_NORMAL priority don't apply. `spawn_blocking` work can't be cancelled either: after a job is aborted, inference keeps running and holds the model mutex. | Run candle inside `sf_engine::pool` via `install`, or make the compute pool the global rayon pool. Use a bounded batch so an aborted job stops within one batch. |
| R12 | M | `sf-ops/src/backup/mod.rs:466-487` | Restore swaps the DB, `siloforge.toml` and `secrets.env` one rename at a time with no rollback. On Windows a rename can fail because AV, the indexer or an editor holds `secrets.env`. The home is then left with the new DB and the old secrets, so stored WordPress, GSC and provider secrets can't be decrypted if the keys differ. | On any error, rename the already-swapped files back from their `.pre-restore-{ts}` copies. Swap secrets first and the DB last. |
| R13 | M | `pipelines/inject.rs:301,795`; `sf-ops/src/retention.rs:51-57` | A snapshot (the full raw HTML) is inserted **before** the PUT. On a conflict, a WordPress error or a local refusal after a re-read, `persist(…, None)` leaves it orphaned. Snapshots, orphaned or not, are never pruned. | Insert the snapshot in the same transaction as the post-PUT persist (keep the raw in memory). Prune snapshots of reverted or failed edges after N days. |
| R14 | M | `sf-ops/src/migrate_v1/mod.rs:14,482` | The importer uses `INSERT OR IGNORE`, so a re-run never refreshes rows that changed in v1 after the first import. Before cutover (after side-by-side running), edges that v1 approved, applied or rejected in the meantime keep their stale status in v2, and new crawl data in v1 is ignored. | Add a final `--refresh` mode (UPSERT by id) or wipe and re-import at cutover. Document it in the cutover runbook. |
| R15 | M | `sf-jobs/src/queue.rs:394-410` | Boot and periodic reaping mark rows `failed` even when the kind is retryable and attempts remain. A crawl interrupted by a reboot or a Windows logoff (see R17) is not retried, and the schedule only tries again at its next cron slot. | Requeue the row (`status='queued'`, backoff) when `attempts < max_attempts` and the kind is idempotent (crawl, link_health, embed). |
| R16 | M | `sf-jobs/src/ctx.rs:373`; `pipelines/embed.rs:165-167` | The vector cache is invalidated only when the embed step embedded something. A re-chunk that only deletes or reorders paragraphs leaves stale paragraph ids and `idx` in `para_meta`. The next plan then ranks "top paragraphs" by indices that point at different paragraphs. Inject heals this through hash relocation, but anchors and scores are computed on the wrong text. | Invalidate in chunk and crawl whenever a site's paragraphs change, or key the cache on a per-site generation counter. |
| R17 | L | `siloforge/src/serve.rs:164-186` | On Windows only Ctrl-C triggers a graceful shutdown. Stop-ScheduledTask, `taskkill`, logoff and shutdown kill the process: no final WAL checkpoint, and running jobs are reaped as `failed` at the next boot (R15). The HTTP drain (`s.await`) has no timeout, so an MCP-over-HTTP `wait_for_job` or `run_pipeline` call (up to 900 or 1800 s) can delay exit. | Handle `tokio::signal::windows::{ctrl_close, ctrl_shutdown, ctrl_logoff}` and put a timeout on the drain. |
| R18 | L | `sf-jobs/src/scheduler.rs:129-165` | The compare-and-set claim succeeds before the enqueue. If the enqueue then fails, that firing is lost. Any DB error (`?`) aborts the rest of the tick for all schedules. | Enqueue and claim in one transaction, and log per row instead of using `?`. |
| R19 | L | `sf-store/src/migrate.rs:356-365` | Migration checksums are stored but never compared, so an edited, already-applied migration file goes unnoticed. | Compare checksums on open and warn or refuse. |
| R20 | L | `pipelines/undo.rs:275` | `.ok()` turns a real DB error into "snapshot row is gone". | Propagate the error. |
| R21 | L | `sf-core/src/types.rs:62-73` (`LinkHealth` is not exclusive); `pipelines/link_health.rs:298` | link_health can run beside a crawl. Its per-post rollup `UPDATE posts` (link counts) is computed from pre-crawl HTML and can overwrite counts for posts the crawl just refreshed. | Re-derive the rollups at the end, or make link_health exclusive with crawl. |
| R22 | L | `sf-ops/src/monitor.rs:542-555` | `JobMeter` samples the whole process, so `job_runs.resources` mixes every job that runs concurrently (also listed as a ghost issue). | Label it as process-wide, or meter per thread or task. |

Checked and found sound:
- **Writer deadlocks:** write closures are synchronous `FnOnce(&Transaction)`, so they can't await. `write_blocking` is called only from the migrator's CLI thread.
- **Nested reads:** reads never nest.
- **bcrypt, scrypt and backup verify** all run in `spawn_blocking`.
- **Enqueue dedupe** is a SELECT and INSERT inside one IMMEDIATE transaction, backed by the `job_runs_dedupe_uq` partial unique index.
- **`finish` and `fail`** never overwrite a `cancelled` row.
- **Reaping and heartbeats:** the periodic reaper computes its cutoff before queueing, so a long writer stall does not reap jobs whose heartbeat is merely queued.
- **Undo:** precise undo is self-healing after an abort (`LinkNotFound` maps to `reverted`).
- **Decide routes** only move edges out of `suggested`/`reverted` (`v1/edges.rs:121-122`), so an approved edge can't change under a running inject.
- **Secrets** come from `rand` (`crypto.rs:175-183`), not from `new_id()`.

## 2. v1 bug status (01-engine-spec §B, 02-backend-spec §6.1–6.5)

| ID | Status | Evidence / remaining gap |
|---|---|---|
| #1 rendered HTML written back | Fixed | `sf-crawl/src/wp.rs:5-10`; inject splices fresh `context=edit` raw (`inject.rs:279`) |
| #2 lost updates | **Partial** | GET-compare-POST precondition is not atomic (R10). Snapshot-mode undo clobbers edits (R9). |
| #3 per-edge snapshots | Fixed | One snapshot and one PUT per post (`inject.rs:301`). Precise undo by URL+anchor (`undo.rs:143-164`). |
| #4 unvalidated LLM output | Fixed | `sf-engine/src/insert/llm.rs:354-441` (one `<a>`, shingle coverage ≥0.8, sanitising, refusal detection) |
| #5 splitter or index mismatch | Fixed* | Hash-checked splice plus relocation (`insert/splice.rs:108-125`, `inject.rs:681-716`). *The plugin path still sends `paragraph_index` without a hash (`v1/sync.rs:223`). |
| #6 re-plan re-suggests | **Partial** | Linked, approved, pending and rejected pairs are skipped (`silo/mod.rs:109-117`). Reverted and failed pairs are re-suggested. A pending suggestion whose pair became linked survives re-plans (`plan.rs:599`). |
| #7 density before scoring | Fixed | `plan.rs:238-270` |
| #8 click depth | **Partial** | Home and nav roots are fixed (`graph/mod.rs:427`), but `settings.root_urls` is rejected by PATCH (`v1/sites.rs:273`), and nav targets are always empty (#40). |
| #9 unigram keywords / one-word anchors | Fixed | 1–3-grams with deterministic tie-breaks (`anchor/pick.rs:242,470`) |
| #10 existing links pollute guards | Fixed | `anchors.rs:486-519` |
| #11 stale existing edges | Fixed | Re-synced on re-chunk (`chunk.rs:239-252`). A plan edge whose link an editor deleted stays `applied`. |
| #12 drift tautology | Fixed | `silo/mod.rs:541-544` |
| #13 scores never refreshed | Fixed | `plan.rs:599` rescoring of pending edges |
| #14 inbound counts rows | Fixed | DISTINCT pairs (`graph.rs:45`, `plan.rs:995`) |
| #15 placement units | Open (parity) | `score/mod.rs:376-383` |
| #16 tier availability | Open (parity) | `score/mod.rs:98-101,502-511` |
| #17 module ignores inParagraph | Fixed | `insert/mod.rs:83` |
| #18 wrapAnchor inside words / non-prose | Fixed | `splice.rs:308-311`, `html/blocks.rs:97-113` |
| #19 non-prose chunks | Fixed | `html/blocks.rs:79-81,484` |
| #20 keyword linker bypasses density | **Partial** | Word boundaries and URL normalisation are fixed. Still bypasses density and scoring (`keyword_link.rs:192`, score fixed at 0.5). |
| #21 GSC | **Open** | No GSC pull in v2. Migrated GSC rows never refresh but still enable the gsc dimension (`plan.rs:964-984`). |
| #22 nondeterminism | Fixed | ORDER BY id (`plan.rs:570`, `anchors.rs:498,592`) |
| #23 `/?p=` → home | **Partial** | `url_path_key` still drops the query (`graph/mod.rs:80-92`) |
| #24 k-means++ fallback | **Partial** | Fix exists behind a flag; plan uses the legacy params (`plan.rs:786`) |
| #25 matchIntent | Fixed | `nlp/intent.rs` |
| #26–28 SSRF, blocklist, XSS | Fixed | `sf-crawl/src/ssrf.rs:168-214,322`; `insert/llm.rs:33-46` |
| #29–31 anchor CPU, quadratic loops, per-row round trips | Fixed | Memoised NLP on rayon, running counts, batched transactions |
| #32 embed all-or-nothing | Fixed | Per-batch commit (`pipelines/embed.rs`) |
| #33 Qdrant wipe | N/A | |
| #34 repeated parsing | Partial | link_health re-parses every post each run (`link_health.rs:50`) |
| #35 index-shift churn | Fixed | Matched by hash (`chunk.rs:212-226`) |
| #36 sequential inject | Fixed | One PUT and one verify per post |
| #37 unbounded growth | **Partial** | Snapshots are never pruned (R13); `job_runs` too (G2-3) |
| #38 k-means seeding cost | Fixed | `cluster/mod.rs:195` |
| #39 engine disabled after timeout | N/A | In-process engine |
| #40 nav provenance dead | **Open** | Body-only HTML (`link_health.rs:50`, `sf-crawl/src/html.rs:64`) |
| #41 / G1 embed chains ignored | **Open** | Settings › Models embed chains have no production caller. `pipelines/embed.rs` uses the config `EmbedChain`. |
| #42 mixed-rung vectors | Partial | Same 256-token window on every rung, but no per-vector model provenance |
| #43 catch-all swallow | Fixed | Only ConstraintViolation is swallowed (`plan.rs:1225-1227`) |
| #44 inbound cap | **Open** | Implemented (`density/mod.rs:82`), but PATCH rejects `max_new_inbound_per_target` (`v1/sites.rs:270`), so it can't be turned on |
| #45 doc drift | N/A | |
| S1, S3, S4, S6–S8, S10–S12 | Fixed | See the server-v1 devlog entry. S2 partial (site `api_key` stored in plaintext). S5 partial (the encryption key falls back to `JWT_SECRET`). S9 partial (private addresses allowed by design). S13 open (JWT in `localStorage`). |
| B1–B8, B13 | Fixed | B5: run-now honours `schedule_id`/`job_kind` (`v1/schedules.rs:366-371`). B6: `scheduler.rs:1-5`. |
| B9 | Open | `/suggestions?status` is ignored (`v1/sync.rs:191`). `?duplicates=1` is ignored (`v1/reports.rs:23-31`). |
| B11 | Partial | PATCH can still change `dim` of a chained model (`v1/providers.rs:741`) |
| B10, B12 | N/A | |
| J1 duplicate runs | Fixed* | Dedupe plus site-exclusive claims. Reopened by R2 (second instance) and R3 (cancel window). |
| J2 lock loss / no timeout | Fixed | Heartbeat plus per-kind timeout (`runner.rs:23-34`) |
| J3 orphaned running rows | Fixed | Boot and periodic reaper. Over-eager with a second instance (R2), and never requeues (R15). |
| J4 crawl retry | Fixed | 2 attempts with backoff (`queue.rs:116-119,330-335`) |
| J5, J6 | N/A / Fixed | Durable SQLite queue; compare-and-set scheduler |
| P1, P2, P5, P6 | Fixed | Incremental chunking, single-transaction sync, WAL readers, ANALYZE in 0003 |
| P3 | Partial | link-health and history CSV are still built fully in memory (`v1/linkhealth.rs:195`) |
| P4 | Partial | Correlated counts remain but are covered by 0003 indexes |
| P8 retention | **Partial** | `job_logs` and `model_call_log` are pruned daily. `job_runs`, `snapshots`, `errors` and `site_metrics_history` are never pruned (G2-3). |
| G2 site chat chains | Fixed | `inject.rs` passes `site_id` |
| G3 undo without snapshot | Partial | Precise undo needs no snapshot. A link rendered by the plugin but absent from raw content is marked `reverted` ("link not present") although it is still live. |
| G4 | Partial | `GSC_CREDENTIALS_JSON` is imported but never consumed |
| G5 dashboard 0 vs unknown | Open | `v1/dashboard.rs:178-195` |
| G6 `replacement_html` null | Open | `v1/sync.rs:224` |

## 3. Ghost issues (configured, claimed or displayed, but not real)

| ID | Sev | Issue | Evidence |
|---|---|---|---|
| G2-1 | H | **Breadcrumb report is fake.** `has_breadcrumbs`, `has_breadcrumb_schema` and `breadcrumb_report` are never written outside the migrator, and breadcrumb body capture is never enabled. The Link Health "breadcrumbs" tab shows every post as having none. | `0001_init.sql:71-73`; `sf-crawl/src/health.rs:114,124`; `v1/linkhealth.rs:516-535`; `ui/src/pages/LinkHealth.tsx:36-38` |
| G2-2 | H | **GSC is a shell.** Credentials are sealed and `connected: true` is stored, but nothing ever calls Google, and `keywords.volume`, `position` and `striking` are never written. The Knowledgebase claims GSC supplies volume and position. | `v1/gsc.rs:48-67`; `pipelines/keywords.rs:185`; `ui/src/components/Knowledgebase.tsx:294` |
| G2-3 | H | **The maintenance and backup job kinds can never be enqueued.** The `site_schedules.job_kind` CHECK allows only crawl, plan, link_health and audit, and no route enqueues them. So `pipelines/maintenance.rs` (including its 180-day `job_runs` prune) is dead, and `job_runs`, `snapshots`, `errors` and `site_metrics_history` grow forever. | `0001_init.sql:232`; `pipelines/mod.rs:45-46`; `retention.rs:51-57` |
| G2-4 | M | `server.cors_origins` is parsed but never applied; no CORS layer exists. | `sf-core/src/config.rs:34`; `siloforge.example.toml:9` |
| G2-5 | M | `GSC_CREDENTIALS_JSON` is copied by `init --from-v1-env` and recommended in the UI, but nothing reads it. | `siloforge/src/cmds.rs:97`; `Knowledgebase.tsx:291` |
| G2-6 | M | `max_new_inbound_per_target` and `root_urls` are read by the engine, but the settings PATCH whitelist rejects them. | `plan.rs:495`; `v1/sites.rs:270-273` |
| G2-7 | M | `GET /api/v2/mcp-info` always returns `"tools": []`, so the MCP page never lists tools. It advertises `--home`, which `siloforge mcp` ignores. Its `client_config` omits `SILOFORGE_API_URL`. | `v2/misc.rs:108-127`; `cmds.rs:55-59` |
| G2-8 | M | `job_runs.resources` is described as "measures one job" but is process-wide (R22). | `monitor.rs:10,542-555` |
| G2-9 | M | `PRAGMA incremental_vacuum` runs daily but `auto_vacuum` is never set, so it is a no-op and the file never shrinks. | `retention.rs:85`; `sf-store/src/lib.rs:43-54` |
| G2-10 | M | "int8 quantised candidate search" is claimed, but `quantize` and `search_quantized` have no production callers. | `docs/PLAN.md:45`; `docs/adr/0001…:26`; `sf-engine/src/vector/mod.rs:744-763` |
| G2-11 | M | `init --force` silently regenerates `JWT_SECRET` and `SILOFORGE_ENCRYPTION_KEY`. Existing logins break, and stored secrets in an existing DB become undecryptable. | `cmds.rs:106-113` |
| G2-12 | L | "Per-edge NLP cached by paragraph hash" is claimed; the cache is per run and keyed by index, and `cache_key` is never looked up. | `PLAN.md:44`; `anchors.rs:442,471`; `nlp/mod.rs:83` |
| G2-13 | L | The settings validator accepts 0..1000 but the engine clamps to 200 without saying so. | `v1/sites.rs:249`; `density/mod.rs:77-80` |
| G2-14 | L | `money_page_urls` is accepted by the API but has no UI. | `v1/sites.rs:252`; `ui/src/types.ts:81` |
| G2-15 | L | `plan.ready` also fires after audits, with candidates=0. | `runner.rs:454` |
| G2-16 | L | The error tracker only records job failures; server-side errors never reach the Errors page. | `runner.rs:401` (only caller) |
| G2-17 | L | The desktop notification channel reports success on non-Windows, where it does nothing. | `notify.rs:434-437,590-591` |
| G2-18 | L | `notifications` (90 d) and `audit_log` (365 d) retention periods are hardcoded, not configurable. | `retention.rs:27-29` |
| G2-19 | L | Migration checksums are stored but never verified (R19). | `migrate.rs:356-365` |

`defer_when_cpu_above` works but defaults to 0 (off). BELOW_NORMAL priority, the vector-cache LRU and `PRAGMA optimize` are real.

## 4. API mismatches

Coverage:
- **MCP:** 33 calls checked (31 OK).
- **UI:** 98 calls checked (`api.ts` 73, `api2.ts` 25; 91 OK).
- **Server routes:** 79 v1 and 31 v2.
- **Paths and methods:** every one exists. No route uses `deny_unknown_fields`, so unknown body fields are silently dropped.

| ID | Sev | Side | Mismatch | Evidence |
|---|---|---|---|---|
| A1 | **H** | UI+MCP | `mcp-info` returns `api_url: http://{bind}/api`. The MCP panel puts that into the Hermes config as `SILOFORGE_API_URL`, and the MCP client appends `/api/v1/...`, giving `/api/api/v1/...`. Every stdio-MCP tool returns 404 for anyone who copies the panel's config. | `v2/misc.rs:114`; `ui/src/components/McpPanel.tsx:57,73`; `sf-mcp/src/client.rs:246`; `config.rs:66-68` |
| A2 | **H** | MCP | `siloforge_list_jobs` sends `offset`, but `GET /api/v2/jobs` has no offset (the query struct and SQL lack it), so paging always returns page 1. | `sf-mcp/src/tools.rs:99,820,837`; `v2/jobs.rs:16-22,61-65` |
| A3 | M | MCP | `system_status`, `create_backup` and `list_notifications` call admin-only routes. The UI says read+write scopes are enough, so they return 403. | `tools.rs:661,695,699`; `v2/system.rs:42`, `backups.rs:61`, `notifications.rs:103`; `auth.rs:305`; `McpPanel.tsx:115` |
| A4 | M | UI | The notification bell polls the admin-only `GET /api/v2/notifications` every 30 s for every user. Non-admins get a 403 per poll and the bell always shows 0. | `NotificationBell.tsx:23-27`; `v2/notifications.rs:100-103` |
| A5 | M | UI | The Notifications settings section is shown to non-admins (no `adminOnly`), but every channel route needs admin, so they see a 403. | `ui/src/pages/Settings.tsx:80`; `v2/notifications.rs:13-86` |
| A6 | M | UI | The MCP tool list is always empty (see G2-7). | `v2/misc.rs:127`; `McpPanel.tsx:121` |
| A7 | L | UI | `BackupRow.schema_version` and `BackupVerifyResult.sha256` are typed but never sent by the server. | `ui/src/types2.ts:194,200`; `sf-ops/src/backup/mod.rs:640`; `v2/backups.rs:183` |
| A8 | L | UI | There is no way to request an unencrypted backup when a passphrase is configured (`unencrypted` is never sent). | `api2.ts:165`; `v2/backups.rs:71-77` |
| A9 | L | UI | `/reports/links?duplicates=1` is ignored by the server (B9). | `api.ts:245`; `v1/reports.rs:23-31` |
| A10 | L | UI | The All Jobs filter is missing the `embed` and `maintenance` kinds. | `ui/src/pages/AllJobs.tsx:15` |
| A11 | L | MCP | `ANCHOR_TYPES` omits `naked` and `cta`. | `tools.rs:35`; `sf-core/src/types.rs:42-43` |
| A12 | L | MCP | The `create_backup` description promises a "job_run_id" (the server returns `{id,status,kind,encrypted}`). The `list_notifications` description mentions events that don't exist. | `tools.rs:354,362` |
| A13 | L | Docs | `05-ui-audit.md:237-243` and `04-integrations.md:136,263` list endpoints that were never built (restore route, `/api/jobs/*` pause/delete, notification test routes). They are not marked as superseded. | |

## 5. Suggested order of fixes

1. **R1 backup naming:** a one-line fix, and it stops the flaky test and silent backup loss.
2. **R2 instance lock**, then bind before starting the runner.
3. **R5:** migration 0004 with the FK child indexes.
4. **R3 and R4:** a `cancelling` state, plus a non-abortable PUT→persist section with an `applying` marker.
5. **G2-3:** make maintenance reachable (daily loop or schedulable), and prune snapshots and `job_runs`.
6. **A1 and A2:** two small server fixes that unbreak MCP for Hermes.
7. **R6, R7, R8:** queue head-of-line blocking, writer panic guard, service supervisor.

Experiment: `D:\tmp\sf-rel\fk_bench.py` (stdlib sqlite3, in-memory). Test logs: `D:\tmp\sf-rel\test1.log` (fail-fast run) and `test2.log` (`--no-fail-fast`).

## 6. Resolution (2026-10-14, reliability-fix pass)

| ID | Resolution |
|---|---|
| R1 | Fixed earlier by the security pass (random backup name, verified: backup roundtrip test green). |
| R2 | Fixed. `sf_ops::instance`: exclusive OS lock on `siloforge.lock` (`File::try_lock`) taken as the very first step of `serve` (before the DB opens); listeners are bound before notifier/monitor/backup/runner/reaper start; the pid file is only removed by its owner; `restore` holds the lock for the whole restore. Tests: `instance::lock_is_exclusive_and_pid_file_is_owned`, `serve::second_instance_fails_before_touching_anything`. |
| R3 | Fixed. Cancel of a running job sets `job_runs.cancel_requested_at` (migration 0005); the row stays `running` (site busy for `claim_next`) until the runner sees the pipeline stop; API/MCP/UI see `status: "cancelled"` (+ v2 `cancelling: 1`), wire shape unchanged. Tests: `queue::cancel_keeps_site_lock_until_pipeline_stops`, `reliability::runner_holds_site_lock_until_cancelled_pipeline_stops`. |
| R4 | Fixed. `inject_journal` (0005) written with the snapshot before every PUT (inject, verify rollback, precise + snapshot undo) and deleted in the same transaction as the bookkeeping; failed PUTs are re-read (`put_checked`: landed / not written / unknown); `reconcile_site` runs at the start of every inject/undo and the runner queues a `reconcile_only` inject per affected site at boot. Tests: `reliability::inject_journal_is_reconciled_against_live_content`, `lost_update_response_is_confirmed_by_reread`, `runner_boot_queues_reconcile_for_unfinished_writes`. |
| R5 | Fixed. Migration `0005_reliability`: indexes on every FK child column (link_edges.paragraph_id/snapshot_id, link_health.target_post_id, clusters.pillar_post_id, site_schedules.last_job_run_id, site_metrics_history.job_run_id, model_chains.site_id, model_chain_rungs.model_id, model_call_log.chain_id, api_tokens.user_id) + ANALYZE. Test `migrate::every_foreign_key_child_column_is_indexed`. |
| R6 | Fixed. `claim_next` filters busy sites / heavy kinds in SQL over the whole queue, ordered by priority, fewest running jobs per site, age. Test `queue::busy_site_backlog_does_not_starve_other_sites`. |
| R7 | Fixed. Write/read/with_writer closures run under `catch_unwind` (rollback, error to caller), readers return to the pool via a drop guard, writer loop has a backstop; `read_blocking` takes a permit or opens a temporary reader. Test `panicking_closures_do_not_kill_writer_or_readers`. |
| R8 | Fixed. `sf_ops::supervise` restarts monitor, notifier, backup scheduler, retention, job scheduler and runner claim loop with backoff, records `errors` (`service:<name>`), liveness in `/health` (`background`, `background_ok`) and `/api/v2/system`. Test `supervise::panicking_loop_is_restarted_and_recorded`. |
| R9 | Fixed. Snapshot-mode undo compares live `modified_gmt` with `posts.sf_written_gmt` (0005, set on every SiloForge write) and refuses unless `force: true` (stats `refused`). Test `snapshot_undo_refuses_after_editor_change_unless_forced`. |
| R10 | Mitigated. `UpdateOutcome.raw` (content.raw returned by WP) is compared with what was sent; a difference is a job warning and the stored raw is kept. GET-compare-POST window remains (WP has no conditional write). |
| R11 | Fixed (v2.0.1). `sf_embed::budget::CpuBudget {threads, low_priority}` from `resources.cpu_threads` / `low_priority_jobs` (`EmbedChain::from_config_budget`, used by `serve`). ONNX: intra-op threads = budget; with low priority intra/inter-op spinning off and ORT pool threads created below normal by a custom `ThreadManager` (`LowPriorityThreads`); the blocking thread that drives a batch is lowered by `PriorityGuard` and restored afterwards. candle: inference runs inside a private rayon pool of `threads` workers (below normal when low priority) instead of the global pool. Cancellation granularity is one chain batch (`embed.batch_size`). Tests `budget::*`, `onnx::tests::{spinning_only_on_cpu_at_normal_priority, budget_flows_into_options, low_priority_cpu_session_matches_reference}`. |
| R12 | Fixed. Restore swaps secrets → config → DB and undoes every rename on failure. Test `swap_failure_rolls_back_every_file`. |
| R13 | Fixed. Snapshot is inserted with the journal and deleted when the write did not land; old snapshots not backing an applied edge are pruned (`monitor.snapshot_retain_days`). |
| R14 | Fixed (v2.0.1). `MigrateOptions.refresh` / CLI `migrate from-postgres --refresh`, `migrate import --refresh`: UPSERT by id that only touches rows whose imported columns differ (`updated` column in the report; `clusters.pillar_post_id` is never nulled). Never deletes; documented in MIGRATION.md. Tests `refresh_overwrites_rows_changed_in_v1`, `insert_sql_shapes`. |
| R15 | Fixed. Reaper re-queues crawl/link_health/embed/gsc/maintenance with attempts left, cancels cancel-requested rows, fails the rest. Test `reaper_requeues_idempotent_jobs`. |
| R16 | Fixed (v2.0.1). `chunk_changed` invalidates the site's vector cache whenever it re-chunked a post (insert/delete/re-index), not only when embed produced vectors. Test `reliability::rechunk_without_new_embeddings_invalidates_vector_cache` (fails without the fix). |
| R17 | Fixed. Windows ctrl_close/logoff/shutdown trigger graceful shutdown; HTTP drain capped at 10 s. |
| R18 | Fixed. Per-row errors are logged and skipped; a failed enqueue gives the firing back. |
| R19 | Fixed. Checksums compared on open (CRLF-normalised), warning only. Test `checksum_mismatch_is_reported`. |
| R20 | Fixed (snapshot lookup uses `optional()`, real errors propagate). |
| R21 | Fixed (v2.0.1). link_health remembers each post's `content_hash` when it extracts links; the rollup `UPDATE posts ... WHERE id=? AND content_hash=?` skips posts a concurrent crawl changed (`stats.rollupStale`, job log line). Test `link_health::tests::rollups_skip_posts_changed_by_a_concurrent_crawl`. |
| R22 / G2-8 | Fixed by labelling (v2.0.1). `job_runs.resources` gains `scope: "process"` and `concurrent_jobs_max` (jobs metered at the same time); the Jobs page shows "(shared ×N)" and the column tooltips say process-wide; documented in API.md / OPERATIONS.md. Test `job_meter_reports_overlapping_jobs`. |
| G2-1 | Fixed: breadcrumb detection (`sf_crawl::breadcrumbs`) in the link_health job writes the three columns. Test `link_health_writes_breadcrumb_columns`. |
| G2-2 | Fixed: `gsc` job + audit step (service-account JWT RS256 via jsonwebtoken, searchAnalytics paging) writes keywords(source='gsc'); `POST /api/v2/sites/{id}/gsc/sync`. Tests in `sf-jobs/tests/gsc.rs` (stub Google). |
| G2-3 | Fixed: scheduler queues `maintenance` daily (`monitor.maintenance_cron`); retention now prunes job_runs (keep newest `job_runs_keep_per_site` per site+kind), unused snapshots, resolved errors; notifications/audit periods configurable (G2-18); site_metrics_history kept. Tests `scheduler_queues_daily_maintenance`, `prunes_job_runs_snapshots_and_errors`. |
| G2-4 | Fixed: CorsLayer when `server.cors_origins` is non-empty. Test `cors_layer_follows_config`. |
| G2-5 | Open: `GSC_CREDENTIALS_JSON` still unused (per-site credentials are the supported path). |
| G2-6 | Fixed: PATCH accepts `max_new_inbound_per_target`, `root_urls`, `ai_anchor_refine`, `ai_anchor_refine_{max,concurrency,timeout_s}`, `anchor_semantic`, `anchor_semantic_off` with validation. Test `settings_whitelist`. |
| G2-7 / A6 | Fixed: mcp-info lists the MCP tool catalogue, no `--home`, `client_config` has `SILOFORGE_API_URL`. |
| A1 | Fixed: `api_url` is the origin; the MCP client also strips a trailing `/api`. Test `api_url_with_api_suffix_is_normalised`. |
| A2 | Fixed: `GET /api/v2/jobs?offset=`; the status filter matches the API-visible status. |
| A3 | Fixed (docs/descriptions; routes stay admin-only). MCP descriptions of `siloforge_system_status` (degrades to /health + `system_error` without admin), `siloforge_create_backup`, `siloforge_list_notifications` state the admin scope; MCP panel and HERMES.md say the same. |
| A4 | Fixed. The notification bell is rendered (and polls) only for admins. |
| A5 | Fixed. Settings -> Notifications is `adminOnly`. |
| A7 | Fixed. `BackupRow.schema_version` removed (`sha256` is sent and kept); `BackupVerifyResult` now types `manifest` instead of a non-existent `sha256`. |
| A8 | Fixed. Create-backup dialog has an "Unencrypted" checkbox that sends `unencrypted: true`. |
| A9 | Fixed (UI). `fetchLinksReport` no longer sends the ignored `?duplicates=1`; v1 route unchanged (contract). |
| A10 | Fixed. All Jobs kind filter lists `embed`, `gsc` and `maintenance`. |
| A11 | Fixed. MCP `ANCHOR_TYPES` includes `naked` and `cta` (server accepts all seven). |
| A12 | Fixed. `siloforge_create_backup` documents `{id, status, kind, encrypted, secrets_included}`; `siloforge_list_notifications` lists the real events. |
| A13 | Fixed. 05-ui-audit.md §5 carries a "superseded" note pointing to API.md; 04-integrations.md marks the `/api/jobs` and `/test` paths as Hermes / Mission Control routes. |
| Other G2-* | G2-5 still open (`GSC_CREDENTIALS_JSON` unused). |
