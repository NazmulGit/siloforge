# SiloForge v2 HTTP API

Base URL: `http://127.0.0.1:8600` (the same router also listens on `:8601`). JSON in and out unless noted.
Errors use the v1 body `{"error": "<message>"}` with the status codes listed below. Unknown `/api/*` paths return a
JSON 404 (never the UI's `index.html`).

- `/health`: liveness, no auth.
- `/api/v1/*`: the v1 contract, unchanged (78 routes incl. `/health`) plus a few additions.
- `/api/v2/*`: new features.
- `/mcp`: MCP over streamable HTTP (see [HERMES.md](HERMES.md)).
- anything else: the embedded web UI (SPA).

Exhaustive v1 request/response shapes: [research/02-backend-spec.md](research/02-backend-spec.md) section 1 and
[research/05-ui-audit.md](research/05-ui-audit.md). This page summarises them and documents everything v2 adds or changes.

---

## 1. Authentication

`Authorization: Bearer <credential>`, where the credential is one of:

| Credential | How to get it | Acts as |
|---|---|---|
| **User JWT** (HS256, 30 days, claims `role`, `user_id`) | `POST /api/v1/auth/login {email, password}` -> `{token, user:{id,email,name,role}}` | the user (`admin` or `user`) |
| **Site JWT** (claims `role: site`, `site_id`) | `POST /api/v1/auth/handshake {api_key}` -> `{token, site_id}` (WP plugin) | that site only (plugin routes) |
| **API token** `sfk_...` | `POST /api/v2/tokens`, UI Settings -> API tokens, or `siloforge token create` | its owner, limited by scopes |

v1 JWTs stay valid when the v2 home uses v1's `JWT_SECRET`. Unlike v1, every user JWT is re-checked against the
`users` row (cached 30 s): deleting or demoting a user, or changing the password (bumps `token_version`), revokes
existing tokens. `POST /api/v1/auth/change-password` returns `{ok:true, token}` with a fresh token.
Rotating a site key (`POST /api/v1/sites/{id}/rotate-key`) revokes that site's plugin tokens.

### API token scopes
| Scope | Allows |
|---|---|
| `read` | GET/HEAD/OPTIONS on routes the owner may use |
| `write` | every method on non-admin routes (includes read) |
| `admin` | admin routes too (owner must be an admin) |
| `*` | everything the owner can do |

Rules: tokens are stored hashed (only the prefix is kept in clear), shown once, optionally expire (1-3650 days),
can be revoked, and record `last_used_at`. Only a session or a `*`/`admin` token may create tokens; a token-created
token gets at most its parent's scopes, never outlives it, and is revoked with it.

### Ownership
Admins see every site. Users see their own sites; a foreign site answers **404** (never 403). Site tokens only reach
their own site. Admin-only routes answer 403 `admin role required` (and 403 for a token without `admin`/`*` scope).

### Rate limits
Failed logins and handshakes are limited per client IP; repeated failures for one account lock it for 30 s, doubling
up to 15 min (5 failures are free). Expect 429.

---

## 2. `/health`

`GET /health` (no auth), always 200:
```json
{"ok": true, "version": "2.0.0", "uptime_secs": 9,
 "services": {"db": true, "redis": true, "qdrant": true, "queue": true},
 "jobs": {"running": 0, "queued": 0}}
```
`redis` and `qdrant` no longer exist; they mirror the components that replaced them so v1 probes stay green.
v1's `/health` has no `version` field, which is how the cutover scripts tell the two apart.

---

## 3. `/api/v1` (v1 contract)

Auth column: **user** = any human token; **own** = plus site ownership; **any** = human or site token; **admin**.
202 = job enqueued (`{job_run_id}`); jobs are deduplicated, so a repeat while one is queued returns the same id.

### Auth
| Method | Path | Auth | Body -> response |
|---|---|---|---|
| POST | `/auth/register` | none | `{email, password, name?}` -> 201 `{token, user}`. Off unless `auth.allow_registration` or the DB has no users (first user = admin) |
| POST | `/auth/login` | none | `{email, password}` -> `{token, user}`; 401 `invalid email or password` |
| GET | `/auth/me` | user | `{id, email, name, role}` |
| POST | `/auth/change-password` | user | `{current_password, new_password}` -> `{ok, token}` (old sessions revoked) |
| POST | `/auth/handshake` | none | `{api_key}` -> 200 `{token, site_id}` (WP plugin) |

### Sites, crawl, plan
| Method | Path | Auth | Notes |
|---|---|---|---|
| GET | `/sites` | user | `Site[]` = site row + `posts_count`, `edge_counts{status:n}`, `has_wp_credentials`, `wp_user`. **No `api_key`, no credentials** |
| POST | `/sites` | user | `{domain, wp_url?, mode? wp_rest\|sitemap}` -> 201 site **with `api_key`** (only here and on rotate) |
| GET / PATCH | `/sites/{id}` | own | PATCH `{settings?, wp_url?, mode?}`; `settings.wp_user` + `settings.wp_app_password` are sealed into encrypted storage; `null` removes a key; whitelisted keys with ranges; `allow_private_host` is admin-only |
| POST | `/sites/{id}/rotate-key` | own | v2 addition: new `api_key`, old site tokens revoked |
| POST | `/sites/{id}/crawl` | own | 202 |
| POST | `/sites/{id}/plan` | own | `{topology? reverse\|serial\|priority\|circle\|bidirectional, archive?, ai_refine?}` -> 202. `ai_refine` (v2 addition, alias `aiRefine`) overrides the site setting `ai_anchor_refine` for this run |
| POST | `/sites/{id}/archive-plan` | own | `{topology?, ai_refine?}` -> 202 |
| GET / POST / DELETE | `/sites/{id}/gsc` | own | status `{connected, client_email, saved_at}` / connect `{credentials_json}` (encrypted) / disconnect |
| POST | `/sites/{id}/link-health` | own | optional `{internal_only: true}` -> 202 |
| GET | `/usage` | user | `{sites, posts, paragraphs, suggested, approved, applied, applied_links, clusters}` |
| GET | `/posts/{site}` | own | `?search&post_type&cluster_id&is_orphan(true/false/1/0)&limit(1-1000)&offset` -> `{posts[], total, post_types[], clusters[]}` |

Site `settings` keys accepted by PATCH (others answer 400 `unknown settings key`): `preset`, `silo_topology`,
`insert_modules`, `max_links_per_post`, `min_links_per_post`, `max_new_links_per_pass`, `link_overload_threshold`,
`money_page_urls`, `allow_private_host` (admin), `skip_inject_verify`, `max_new_inbound_per_target`, `root_urls`, and the
v2 anchor keys `ai_anchor_refine` (bool, default false), `ai_anchor_refine_max` (0-5000, default 200),
`ai_anchor_refine_concurrency` (1-16, default 4), `ai_anchor_refine_timeout_s` (5-3600, default 300),
`anchor_semantic_off` (bool) and `anchor_semantic` (`"true"`/`"false"`). The UI edits the anchor keys under
Settings -> Linking Rules -> Anchor text.

### Plugin channel
| Method | Path | Auth | Notes |
|---|---|---|---|
| POST | `/sync` | any | `{domain, posts:[{wp_post_id, url, slug, title, content_html, post_type, post_status, builder?, meta_description?, published_at?, modified_at?}]}` -> `{received, upserted, skipped}` (single transaction) |
| GET | `/suggestions/{site}` | any | approved edges with a `wp_post_id`: `[{edge_id, wp_post_id, target_url, anchor, paragraph_index, replacement_html, insert_module}]` |
| POST | `/suggestions/{site}/ack` | any | `{edge_id, ok, error?}`; only `approved -> applied/failed` (409 otherwise, idempotent repeat) |
| POST | `/suggestions/{site}/revert-ack` | any | `{edge_id}`; only edges the plugin applied |

### Edges, bulk, rules
| Method | Path | Auth | Notes |
|---|---|---|---|
| GET | `/edges/{site}` | own | `?status&tool&limit&offset` -> `{edges[], total}`; `limit` clamped to 5000 (v1: 400 above 1000); rows add `failure_kind`, `link_start` |
| POST | `/edges/{edge}/decide` | own | `{decision approved\|rejected}` (from suggested/reverted) |
| PATCH | `/edges/{edge}/anchor` | own | `{anchor, anchor_type?}` (not for applied edges) |
| POST | `/bulk/decide` | user | `{edge_ids[], decision}` -> `{updated}` |
| POST | `/bulk/run` | own | `{site_id, edge_ids?}` -> 202 `{job_run_id, count}` (inject; all approved if no ids) |
| POST | `/bulk/undo` | own | `{site_id, edge_ids?}` -> 202 `{job_run_id, count}`; only edges SiloForge injected (not `origin=existing`) |
| GET / POST | `/rules/keywords/{site}` | own | list / `{keyword, target_url, anchor_type?, max_links? 1-100}` -> 201 |
| PATCH / DELETE | `/rules/keywords/{rule}` | own | `{active?, max_links?, anchor_type?}` (validated) |
| POST | `/rules/keywords/{site}/run` | own | 202 (keyword linker) |
| GET / POST | `/rules/ignore/{site}` | own | list / `{phrase}` -> 201 |
| DELETE | `/rules/ignore/{phrase}` | own | |

### Dashboard, reports, jobs
| Method | Path | Notes |
|---|---|---|
| GET | `/dashboard/{site}` | health score, orphans, dead ends, edges by status, clusters, cannibalization (<=20), recent jobs, opportunities, http links, outbound domains, duplication, orphan posts |
| GET | `/reports/links/{site}` | per post: inbound, outbound, pagerank, click_depth, link_flow_score, duplicate_count |
| GET | `/reports/graph/{site}` | `?max_nodes(<=750)&max_edges(<=2500)` -> `{nodes, edges, total_nodes, total_edges, node_limit, edge_limit, truncated}` |
| GET | `/reports/anchors/{site}` | `{total, byType, byAnchor(top 50), warnings}` |
| GET | `/reports/link-health/{site}` | `?limit&offset` JSON summary + lists, or `?format=csv` |
| GET | `/reports/metrics/{site}` | `?trend=1-500` -> `{latest, deltas, trend}` |
| GET | `/history/{site}` | `?limit(1-100)&offset` -> `{rows, total, limit, offset}`, or `?format=csv` |
| GET | `/jobs/{site}` | latest 25 `job_runs` rows (+ v2 fields `attempts`, `max_attempts`, `priority`, `requested_by`, `heartbeat_at`, `resources`) |
| GET | `/jobs/run/{id}` | v2 addition: one job row (the UI polls this) |

### Silo, CSV, schedules
| Method | Path | Notes |
|---|---|---|
| GET | `/silo/{site}/clusters` | `[{id, label, source, pillar_url, members}]` |
| GET | `/silo/{site}/clusters/export` | `{clusters:[{label, pillar_url, member_urls[]}]}` |
| POST | `/silo/{site}/clusters/import` | same shape -> `{clusters, members, skipped}` (one transaction) |
| POST | `/csv/import/{site}` | raw CSV (`url`, `content_html`, `title?`) -> `{received, upserted, skipped}` |
| GET | `/csv/export/{site}` | `text/csv`: `source_url,target_url,anchor,anchor_type,status,insert_module` |
| GET / POST | `/schedules/{site}` | list / `{job_kind crawl\|plan\|link_health\|audit, cron, tz?, active?, payload?}` (upsert per kind) |
| DELETE | `/schedules/{schedule}` | |
| POST | `/schedules/{site}/run-now` | `{job_kind? \| schedule_id?, topology?, skip_crawl?}` -> `{job_run_id}`; uses the stored payload |

### Model providers (admin)
`/providers` (GET, POST), `/providers/health`, `/providers/{id}` (GET, PATCH, DELETE), `/providers/{id}/probe`,
`/models` (GET `?provider_id&role`, POST), `/models/{id}` (PATCH, DELETE), `/models/{id}/probe`,
`/model-chains` (GET `?role&site_id`, POST), `/model-chains/{id}` (PATCH, DELETE), `/model-chains/{id}/activate`,
`/deactivate`, `/model-chains/{id}/rungs` (GET, PUT `{rungs:[{model_row_id, timeout_ms?, max_retries?, enabled?}]}`, POST),
`/model-chains/{id}/rungs/reorder` `{rung_ids[]}`, `/model-chain-rungs/{id}` (PATCH, DELETE).
Provider kinds: `openai-compatible`, `openrouter`, `anthropic`, `gemini`, `ollama`, `custom`. Keys are write-only
(`api_key_masked`, `has_api_key` on read). Probe -> `{ok, latency_ms, status_code, error, dim?, sample?}`.

---

## 4. Deviations from v1 (all intentional)

Verified with a JSON diff against live v1 on the migrated data (203 requests, 0 real mismatches; details in
[testing/contract-diff.md](testing/contract-diff.md)):

| Area | v1 | v2 |
|---|---|---|
| Site `api_key` | in every site payload | only on create / rotate-key |
| WP / GSC credentials | plaintext in `settings` | sealed; `has_wp_credentials`, `wp_user` top-level |
| `GET /edges?limit=2000` | 400 | 200, clamped to 5000 |
| `GET /posts?is_orphan=1/0` | 400 (UI sends exactly this) | accepted |
| `GET /posts?cluster_id=` (empty) | 400 | 200, unfiltered |
| History CSV `Applied At` | JS `Date.toString()` | ISO-8601 |
| Ties inside `ORDER BY` | microsecond timestamps | millisecond timestamps, ties broken by id/rowid |
| `/reports/anchors` `byAnchor` ties | unspecified | broken by anchor |
| Text ordering | Postgres en_US.UTF-8 | same order via `COLLATE V1` |
| Extra fields | | `/health` `version`/`uptime_secs`/`jobs`/`services.queue`; job rows `attempts`, `max_attempts`, `priority`, `requested_by`, `heartbeat_at`, `resources`; edges `failure_kind`, `link_start` |
| `/auth/register` | open | off unless enabled or no users |
| `/auth/change-password` | `{ok}`, old tokens valid | `{ok, token}`, old tokens revoked |
| `ack` | any status -> applied | approved -> applied/failed only (409 otherwise) |
| `run-now` | always an audit | runs the schedule's kind with its stored payload |
| `bulk/undo` count | included existing body links | only SiloForge-injected edges |
| Plan topology | schema refused `bidirectional` | all 5 accepted |
| Unknown `/api/v1/*` | Fastify 404 | JSON `{"error":"route not found"}` 404 |

---

## 5. `/api/v2`

All routes need a token. "admin" = admin role (and `admin`/`*` scope for API tokens).

### System (admin)
| Method | Path | Response |
|---|---|---|
| GET | `/system` | `{version, uptime_secs, started_at, capacity, current, jobs:{running, queued, failed_24h}, disk_free_mb, db_size_bytes, build:{os, arch, debug, pid}, home, db:{path, size_bytes, schema_version, latest_schema_version}, config (no secrets), services:{monitor, notifier}}` |
| GET | `/system/stats` | `{ts, cpu_pct, proc_cpu_pct, mem_used_bytes, mem_total_bytes, proc_rss_bytes, disk_free_bytes, uptime_s, version, queue:{queued, running, failed_24h}}` |
| GET | `/system/metrics?window=1h\|24h\|7d` | `{window, resolution: raw\|1m\|10m, sample_secs?, samples:[...]}` |

### Jobs (user; own sites, admins all)
| Method | Path | Response |
|---|---|---|
| GET | `/jobs?status=&kind=&site_id=&limit=(<=500, default 100)&offset=` | `{jobs:[job + site_domain], count}` newest first |
| GET | `/jobs/{id}` | job row: `id, site_id, kind, status (queued\|running\|done\|failed\|cancelled), stats, error, payload, attempts, max_attempts, priority, requested_by, heartbeat_at, resources{peak_rss_mb, avg_cpu_pct, cpu_secs, wall_ms, scope:"process", concurrent_jobs_max}, started_at, finished_at, created_at` |
| GET | `/jobs/{id}/logs?after_id=&limit=` | `{logs:[{id, seq, ts, level, message, msg, data}], done, status, last_id}` (tail with `after_id=last_id`) |
| POST | `/jobs/{id}/cancel` | `{id, status}`; running jobs stop at the next checkpoint |

Job kinds: `crawl`, `plan`, `inject`, `undo`, `keyword_link`, `link_health`, `audit`, `embed`, `gsc`, `backup`, `maintenance`.

`resources` is sampled for the **whole process** while the job runs (`scope: "process"`). When other jobs ran at the
same time (`concurrent_jobs_max` > 1), RAM and CPU are shared figures, not this job's alone.

### Notifications (admin)
| Method | Path | Notes |
|---|---|---|
| GET | `/notifications?unread=1&limit=` | `{notifications:[{id, event, level, title, body, data, site_id, job_run_id, delivered, read_at, created_at}], unread}` |
| POST | `/notifications/read` | `{ids:[..]}` or `{all:true}` (empty body = all) -> `{updated}` |
| GET | `/notifications/channels` | `{channels:[{id, name, kind, config (secrets "***"), events, enabled, created_at}], kinds, events}` |
| POST | `/notifications/channels` | `{name, kind, config, events?: ["job.failed", ...] or ["*"], enabled?}` |
| PATCH / DELETE | `/notifications/channels/{id}` | config merged key by key; `"***"` keeps a stored secret |
| POST | `/notifications/channels/{id}/test` | `{ok, error, attempts}` |

Kinds and config: `webhook {url, headers?}`, `ntfy {url, token?}`, `telegram {bot_token, chat_id, thread_id?, api_base?}`,
`hermes {url, secret?}` (HMAC V2 headers `X-Webhook-Timestamp`, `X-Webhook-Signature-V2` = hex HMAC-SHA256 of
`"<ts>.<body>"`), `desktop {}`, `log {}`. Events: `job.done`, `job.failed`, `plan.ready`, `inject.done`,
`backup.done`, `backup.failed`, `system.alert` (+ `test`). Webhook payload:
```json
{"id": 42, "source": "siloforge", "event": "job.failed", "level": "error", "title": "...", "body": "...",
 "text": "<title>\n<body>", "data": {}, "site_id": "...", "job_run_id": "...", "created_at": "2026-10-13T..."}
```

### Errors, audit (admin)
| Method | Path | Response |
|---|---|---|
| GET | `/errors` | `{errors:[{fingerprint, source, message, context, count, first_seen, last_seen, resolved_at}]}` |
| POST | `/errors/{fingerprint}/resolve` | `{resolved: true, fingerprint}` |
| GET | `/audit?limit=&before_id=` | `{entries:[{id, ts, actor, via, action, target, detail}]}` newest first |

### Backups (admin)
| Method | Path | Notes |
|---|---|---|
| GET | `/backups` | `{backups:[{id, path, kind, size_bytes, sha256, encrypted, app_version, status, note, created_at}], dir}` |
| POST | `/backups` | `{kind? full\|db\|config, note?, passphrase?, unencrypted?, include_secrets?}` -> `{id, status:"running", kind, encrypted, secrets_included}` (built in the background). `unencrypted: true` skips the configured passphrase (UI: "Unencrypted" checkbox) |
| GET | `/backups/{id}` | one row |
| GET | `/backups/{id}/download` | the archive |
| POST | `/backups/{id}/verify` | `{passphrase?}` -> `{ok, manifest}` or `{ok:false, error}` |
| DELETE | `/backups/{id}` | `{deleted: true}` |

Restore is CLI-only (`siloforge restore`), because the server must be stopped.

### Tokens (user: own tokens; admin `?all=1`)
| Method | Path | Notes |
|---|---|---|
| GET | `/tokens?all=&include_revoked=` | `{tokens:[{id, user_id, name, prefix, scopes, created_at, last_used_at, expires_at, revoked_at, parent_id}]}` |
| POST | `/tokens` | `{name, scopes?: ["read","write"], expires_in_days?: 1-3650}` -> `{token: "sfk_...", id, user_id, name, prefix, scopes, created_at, last_used_at, expires_at}` (**token shown once**) |
| DELETE | `/tokens/{id}` | `{deleted: true, id}` (children revoked too) |

### MCP info (user)
`GET /mcp` (alias `/mcp/info`) -> `{enabled, transport, url, http_url, api_url, stdio_command, stdio_args,
transports:{stdio, http}, auth, client_config:{mcpServers:{siloforge:{command, args, env}}}, tools}`. Used by the UI's
Integrations page to show a ready-to-paste client config.

---

## 6. Examples

```powershell
$t = (Invoke-RestMethod -Method Post http://127.0.0.1:8600/api/v1/auth/login -ContentType application/json `
      -Body '{"email":"admin@siloforge.local","password":"..."}').token
$h = @{ Authorization = "Bearer $t" }
$tok = Invoke-RestMethod -Method Post http://127.0.0.1:8600/api/v2/tokens -Headers $h -ContentType application/json `
      -Body '{"name":"scripts","scopes":["read","write"],"expires_in_days":90}'
$h = @{ Authorization = "Bearer $($tok.token)" }          # use the token from now on
$site = (Invoke-RestMethod http://127.0.0.1:8600/api/v1/sites -Headers $h) | Where-Object domain -eq 'example.com'
$job = Invoke-RestMethod -Method Post "http://127.0.0.1:8600/api/v1/sites/$($site.id)/crawl" -Headers $h -ContentType application/json -Body '{}'
Invoke-RestMethod "http://127.0.0.1:8600/api/v2/jobs/$($job.job_run_id)" -Headers $h
```
