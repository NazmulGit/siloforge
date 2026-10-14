# 02 — SiloForge backend spec (TypeScript → Rust drop-in)

Audit date: 2026-10-13. Source: `D:\Code\SiloForge-Suite\app` (api, worker, shared, db/migrations),
`connect\plugin\siloforge-connect` (WP plugin 0.1.1), `mcp\src` (MCP server). Live data read-only from
`siloforge_pg` (PostgreSQL 16.14, pgvector 0.8.5, pgcrypto 1.3), `siloforge_qdrant` (1.18.2) and `siloforge_redis`.
Nothing was modified.

File:line references are relative to `app/packages/` unless stated. "Contract" = `app/CONTRACTS.md`.

**Compatibility rule for the Rust rebuild:** the wire behaviour below (paths, methods, status codes, JSON
field names, error body `{ "error": string }`, CSV headers) is what the three consumers depend on. Where
the implementation and CONTRACTS.md disagree, **the implementation wins** (consumers were built against it);
the drift is listed in §1.4.

---

## 0. Topology

| Component | Tech | Notes |
|---|---|---|
| api | Fastify 4, `@fastify/jwt`, `@fastify/cors`, `@fastify/helmet`, `bcryptjs`, `pg`, `bullmq` (producer only), `@qdrant/js-client-rest` | container `siloforge_api`, `127.0.0.1:8600`. Image ships only `shared`+`api` (no engine) — hence SQL mirrors of engine logic in `routes/silo.ts`, `routes/schedules.ts`, `routes/providers.ts`. |
| worker | BullMQ `Worker` on queue `siloforge`, **2 replicas × concurrency 2** (`worker/src/index.ts:88-91`, compose `deploy.replicas: 2`) = up to 4 jobs in parallel, any mix of sites | imports `@siloforge/engine` (crawler, embed, graph, silo, score, anchor, inject, link-health, metrics, providers). Optional Rust engine sidecar via `SILOFORGE_RUST_ENGINE`/`_PATH`. |
| ui | nginx serving Vite build, proxies `/api/` → `api:8600/api/` | `127.0.0.1:8601` |
| postgres | `pgvector/pgvector:pg16`, internal only | migrations mounted to initdb **and** applied by API boot runner (`shared/src/migrate.ts`, advisory lock key `873452001`, ledger table `schema_migrations(filename, checksum sha256 of LF-normalised file, applied_at)`; checksum mismatch only logs). |
| qdrant | `qdrant/qdrant:latest` (unpinned) | collections `sf_posts`, `sf_paragraphs` |
| redis | `redis:7-alpine`, anonymous volume `/data`, RDB `save 3600 1 300 100 60 10000`, **AOF off** | BullMQ state + Job Schedulers |

API boot order (`api/src/index.ts`): register plugins/routes → `SELECT 1` retry 10×3 s → `runMigrations()` (fatal on failure) → `seedAdmin()` → `ensureCollections()` (best-effort) → listen `0.0.0.0:API_PORT`.

Global HTTP behaviour to replicate:
- `bodyLimit` 25 MiB (`api/src/index.ts:29`).
- `application/json` with empty/whitespace body parses to `{}` (custom parser `index.ts:34-42`); invalid JSON → 400.
- `text/csv`, `text/plain`, `application/csv` bodies parsed as raw string (`routes/csv.ts:81-85`). The parser is added inside the `csvRoutes` plugin and `fastify-plugin` is not used anywhere, so it is encapsulated: only the CSV routes accept text bodies; every other route answers 415 for `text/*`.
- Fastify JSON-schema validation errors → 400 `{statusCode, code:"FST_ERR_VALIDATION", error:"Bad Request", message}` (Fastify default shape, NOT `{error}` only). Unhandled throws → 500 `{statusCode:500, error:"Internal Server Error", message}`.
- CORS allow-list from `CORS_ORIGINS` (default `http://127.0.0.1:8601,http://localhost:8601`); helmet with CSP off, `X-Frame-Options: DENY`, nosniff, referrer-policy.
- Logger: pino JSON to stdout.

---

## 1. Endpoint inventory (78 routes)

Auth legend:
- **none** — no token.
- **user** — `requireUser`: valid JWT with `role ∈ {admin,user}`; site token → 403 `{"error":"user token required"}`; missing/invalid/expired → 401 `{"error":"unauthorized"}`.
- **user+own** — user, plus `assertSiteAccess` (admin: site exists; user: `sites.user_id = token.user_id`). Fail → **404** (never 403).
- **any+own** — `requireAuth` (human OR site JWT) + `assertSiteAccess` (site token: `site_id` must equal path site).
- **admin** — `requireAdmin` (`routes/providers.ts:40`): 401 if bad token, **403** `{"error":"admin role required"}` if role≠admin (site tokens also 403).

Consumers: **UI** = `ui/src/api.ts`; **WP** = plugin; **MCP** = `mcp/src/index.mjs`. Status code shown when not 200.

### 1.1 Auth & health

| # | Method | Path | Auth | Request | Response | Consumers | Notes / side effects |
|---|---|---|---|---|---|---|---|
| 1 | GET | `/health` | none | — | `{ ok: <db bool>, services: {db,redis,qdrant} }` always 200 | compose healthcheck (`r.ok`) | Opens and quits a NEW Redis connection per call (`routes/health.ts:18-24`). |
| 2 | POST | `/api/v1/auth/register` | none | `{email*, password*, name?}` | **201** `{token, user:{id,email,name,role}}`; 400 invalid email (`/^[^\s@]+@[^\s@]+\.[^\s@]+$/`) / password <8; 409 `email already registered` | UI | **Open self-registration**, role always `user`. Email trimmed+lowercased. |
| 3 | POST | `/api/v1/auth/login` | none | `{email*, password*}` | `{token, user}`; 401 `invalid email or password` | UI, MCP | Constant-time-ish: compares against dummy bcrypt hash when user missing. No rate limit / lockout. |
| 4 | GET | `/api/v1/auth/me` | user | — | `{id,email,name,role}`; 401 if user row gone | UI | Role returned from DB here, but authorization elsewhere uses the role baked into the token. |
| 5 | POST | `/api/v1/auth/change-password` | user | `{current_password*, new_password*}` | `{ok:true}`; 400 wrong current / new <8 | UI | Does NOT invalidate existing tokens (stateless JWT, no `iat` check). Undocumented in contract. |
| 6 | POST | `/api/v1/auth/handshake` | none | `{api_key*}` | `{token, site_id}` (**must be 200** — plugin checks `200 === $code`); 401 `invalid api key` | WP | Site JWT 30 d. Plain equality lookup `sites.api_key = $1`. No rate limit. |

### 1.2 Sites, crawl, plan, GSC, usage

| # | Method | Path | Auth | Request | Response | Consumers | Notes |
|---|---|---|---|---|---|---|---|
| 7 | GET | `/api/v1/sites` | user (scoped) | — | `Site[]` = `sites.*` + `posts_count:int` + `edge_counts:{status:n}`; ordered `created_at ASC` | UI, MCP | Admin sees all. Returns **`api_key` and `settings.wp_user`/`settings.wp_app_password` in plaintext** (only `settings.gsc.credentials` is redacted to `"***"`). Correlated count subqueries per site. |
| 8 | POST | `/api/v1/sites` | user | `{domain* (minLength1), wp_url?, mode? ∈ wp_rest\|sitemap}` | **201** full `sites` row (incl. `api_key`); 409 `domain already exists` | UI, MCP | `api_key = "sf_" + 32 lowercase hex` (16 random bytes). `domain` is globally UNIQUE across all users → user A can squat a domain user B owns (409 leaks existence). No domain normalisation. |
| 9 | GET | `/api/v1/sites/:siteId` | user+own | — | `Site` (gsc redacted) | UI | |
| 10 | PATCH | `/api/v1/sites/:siteId` | user+own | `{settings?: object, wp_url?, mode?}` | updated `sites` row — **NOT gsc-redacted** (`routes/sites.ts:148`) | UI (`patchSiteSettings`, partial objects) | `settings = settings \|\| $2::jsonb` (shallow top-level merge; a key can never be removed; a client can overwrite `gsc`/`wp_app_password` with anything). |
| 11 | POST | `/api/v1/sites/:siteId/crawl` | user+own | `{}` | **202** `{job_run_id}` | UI, MCP | Inserts `job_runs(kind='crawl',status='queued')`, enqueues `crawl` (attempts 2). No dedupe — repeated clicks = parallel crawls of one site. |
| 12 | POST | `/api/v1/sites/:siteId/plan` | user+own | `{topology? ∈ reverse\|serial\|priority\|circle, archive?: bool}` (nullable) | **202** `{job_run_id}` | UI, MCP | Enqueues `plan`. Schema rejects `bidirectional` (`routes/sites.ts:176`) although the type/DB/schedules accept it. |
| 13 | POST | `/api/v1/sites/:siteId/archive-plan` | user+own | `{topology?}` | **202** `{job_run_id}` | UI | Same as plan with `archive:true`. |
| 14 | POST | `/api/v1/sites/:siteId/gsc` | user+own | `{credentials_json*: string}` (must parse to object with `type` or `client_email`) | `{connected:true, client_email}`; 400 | UI | Stores raw service-account JSON (private key) **unencrypted** in `sites.settings.gsc.credentials` (`routes/gsc.ts:44-55`). |
| 15 | DELETE | `/api/v1/sites/:siteId/gsc` | user+own | — | `{connected:false}` | UI | `settings - 'gsc'` |
| 16 | GET | `/api/v1/sites/:siteId/gsc` | user+own | — | `{connected:bool, client_email, saved_at}` | UI | |
| 17 | GET | `/api/v1/usage` | user (scoped) | — | `{sites, posts, paragraphs, suggested, approved, applied, applied_links, clusters}` | UI | Admin = global. Counts `paragraphs` via IN-subquery over 429k rows. |
| 18 | GET | `/api/v1/posts/:siteId` | user+own | `?search&post_type&cluster_id(uuid)&is_orphan(bool)&limit(1-1000,def100)&offset` | `{posts:[{id,url,title,post_type,cluster_id,cluster_label,is_orphan,is_dead_end,suggested_out,suggested_in}], total, post_types:string[], clusters:[{id,label,count}]}` | UI | Sorted by a correlated subquery (`suggested_out`) → full per-post count before LIMIT. `search` is ILIKE with unescaped `%`/`_`. |

### 1.3 Plugin channel (sync / suggestions)

| # | Method | Path | Auth | Request | Response | Consumers | Notes |
|---|---|---|---|---|---|---|---|
| 19 | POST | `/api/v1/sync` | any; site token → own site; human → `?site_id=` + ownership | `SyncBatch {domain*, posts*: [{wp_post_id*:int, url*(min1), slug*, title*, content_html*, meta_description?, post_type*, post_status*, builder? ∈ gutenberg\|elementor\|divi\|classic\|unknown, published_at?, modified_at?}]}` | `{received, upserted, skipped}`; 400 human without site_id; 404 | WP (50 posts/batch) | Per post: SELECT hash → skip if equal sha256(content_html) → UPSERT on `(site_id,url)`; `meta = posts.meta \|\| {description}`; `content_text` via regex stripTags; `word_count`. **Sequential N+1, no transaction.** `domain` field ignored. Updates `sites.last_sync_at`. Does NOT null `embedding`, does not re-chunk (unlike CSV import). URL not validated as http(s). |
| 20 | GET | `/api/v1/suggestions/:siteId` | any+own | `?status` (ignored) | `SuggestionForPlugin[]` = `{edge_id, wp_post_id:number, target_url, anchor, paragraph_index:int\|null, replacement_html:null, insert_module ('native' default)}` for `status='approved'` edges whose source has `wp_post_id`, `ORDER BY created_at` | WP | Unbounded (no LIMIT; a site with 12k approved edges returns all). Non-http(s) `target_url` filtered out. `replacement_html` always null (API has no engine). |
| 21 | POST | `/api/v1/suggestions/:siteId/ack` | any+own | `{edge_id*, ok*:bool, error?}` | updated `link_edges` row; 404 | WP | ok → `status='applied', applied_at=now(), error=NULL`; !ok → `status='failed', error`. **No current-status guard** — any edge (suggested/rejected/reverted) can be forced to applied/failed. |
| 22 | POST | `/api/v1/suggestions/:siteId/revert-ack` | any+own | `{edge_id*}` | updated row; 404 `no applied edge to revert` | WP | `applied → reverted`, `applied_at=NULL`. |

### 1.4 Edges, bulk, rules

| # | Method | Path | Auth | Request | Response | Consumers | Notes |
|---|---|---|---|---|---|---|---|
| 23 | GET | `/api/v1/edges/:siteId` | user+own | `?status ∈ EdgeStatus, tool (=origin), limit 1-1000 def 100, offset` | `{edges: (link_edges.* + source_url, source_title, target_url, target_title)[], total}` ordered `composite_score DESC, created_at` | UI, MCP | 400 invalid status. |
| 24 | POST | `/api/v1/edges/:edgeId/decide` | user (+own via edge.site) | `{decision* ∈ approved\|rejected}` | updated row; 400 if status ∉ {suggested,reverted} | UI, MCP | sets `decided_at`. |
| 25 | PATCH | `/api/v1/edges/:edgeId/anchor` | user (+own) | `{anchor* (trimmed, ≠'', sliced to 255), anchor_type? ∈ AnchorType}` | updated row; 400 applied; 409 unique clash | UI, MCP | |
| 26 | POST | `/api/v1/bulk/decide` | user (scoped in SQL) | `{edge_ids*: string[] minItems1, decision*}` | `{updated:int}` | UI, MCP | Non-uuid ids silently dropped. |
| 27 | POST | `/api/v1/bulk/run` | user+own (body.site_id) | `{site_id*, edge_ids?}` | **202** `{job_run_id, count}`; 400 `no approved edges to inject` | UI, MCP | Collects approved edges (all if omitted), enqueues `inject` with the id list. No guard against a second concurrent inject of the same edges. |
| 28 | POST | `/api/v1/bulk/undo` | user+own | `{site_id*, edge_ids?}` | **202** `{job_run_id,count}` or **200** `{job_run_id:null,count:0}` | UI, MCP | Enqueues `undo` for applied edges. |
| 29 | GET | `/api/v1/rules/keywords/:siteId` | user+own | — | `KeywordRule[]` (created_at DESC) | UI | |
| 30 | POST | `/api/v1/rules/keywords/:siteId` | user+own | `{keyword*, target_url* (http/s), anchor_type? def exact, max_links? 1-100 def 3}` | **201** row; 409 | UI, MCP | `anchor_type` `naked`/`cta` pass validation but violate the DB CHECK (never widened) → **500** (see §6 B1). |
| 31 | PATCH | `/api/v1/rules/keywords/:ruleId` | user (+own via rule) | `{active?, max_links?, anchor_type?}` (**no JSON schema**) | updated row | UI | `max_links` unvalidated (0, negative, string → 500). |
| 32 | DELETE | `/api/v1/rules/keywords/:ruleId` | user (+own) | — | `{deleted:true}` | UI | |
| 33 | POST | `/api/v1/rules/keywords/:siteId/run` | user+own | `{}` | **202** `{job_run_id}` | UI, MCP | enqueues `keyword_link`. |
| 34 | GET | `/api/v1/rules/ignore/:siteId` | user+own | — | `IgnoreKeyword[]` | UI | |
| 35 | POST | `/api/v1/rules/ignore/:siteId` | user+own | `{phrase*}` | **201** row; 409 | UI, MCP | |
| 36 | DELETE | `/api/v1/rules/ignore/:phraseId` | user (+own) | — | `{deleted:true}` | UI | |

Route-pattern overlap the Rust router must allow: `GET/POST /rules/keywords/:siteId` vs `PATCH/DELETE /rules/keywords/:ruleId`; `GET /edges/:siteId` vs `POST /edges/:edgeId/decide`; `GET/POST /schedules/:siteId` vs `DELETE /schedules/:scheduleId`; `GET /providers/health` must win over `GET /providers/:id`. (Same param position, distinguished only by method — axum allows this if the param name is identical per path; normalise names.)

### 1.5 Dashboard & reports

| # | Method | Path | Auth | Request | Response | Consumers | Notes |
|---|---|---|---|---|---|---|---|
| 37 | GET | `/api/v1/dashboard/:siteId` | user+own | — | `{site (sites.* UNREDACTED), health_score, posts, orphans, dead_ends, avg_links_per_post, edges_by_status, clusters, cannibalization_warnings:[{anchor,target_a,target_a_url,target_a_title,target_b,…}] (≤20), recent_jobs (job_runs.* ≤10), opportunities, http_links, outbound_domains, link_duplication, orphan_posts:[{post_id,url,title}] (≤20)}` | UI, MCP | `health = clamp(round(100*(1 − .5·orphanRatio − .2·deadEndRatio − .3·(1 − min(1, avgLinks/5)))),0,100)`, `avgLinks = count(applied OR origin='existing')/posts`. `http_links`/`outbound_domains` come from `link_health` (contract says posts.content_html). 10 parallel queries. |
| 38 | GET | `/api/v1/reports/links/:siteId` | user+own | `?duplicates=1` (ignored) | `[{post_id,url,title,inbound,outbound,pagerank,click_depth,link_flow_score,duplicate_count}]` ordered pagerank DESC | UI, MCP | **Unpaginated** (2.6k rows on biggest site). `link_flow = round(100*(.5·pr/maxPr + .3·min(1,in/5) + .2·min(1,out/5)))`. live edges = applied/approved or origin existing. |
| 39 | GET | `/api/v1/reports/graph/:siteId` | user+own | `?max_nodes (1-750, def 500) &max_edges (0-2500, def 1500)` | `{nodes:[{id,url,title,cluster_id,is_orphan,pagerank}], edges:[{source,target,status}], total_nodes, total_edges, node_limit, edge_limit, truncated}` | UI | |
| 40 | GET | `/api/v1/jobs/:siteId` | user+own | — | `job_runs.*` latest 25 | UI (wizard progress polling), MCP | |
| 41 | GET | `/api/v1/reports/anchors/:siteId` | user+own | — | `{total, byType:{type:n}, byAnchor:[{anchor,count}] (top50), warnings:string[]}` | UI, MCP | warnings exact>40%, generic>30%. |
| 42 | GET | `/api/v1/history/:siteId` | user+own | `?limit (1-100 def50) &offset` or `?format=csv` | JSON `{rows:[{edge_id,source_url,source_title,target_url,target_title,anchor,anchor_type,tool,status,applied_at,decided_at}], total, limit, offset}`; CSV `text/csv` attachment `link-history-<domain>.csv`, header `Domain,Source,Target,Tool,Anchor,Status,Applied At`, CRLF, formula guard | UI, MCP | Contract says array — implementation returns object. CSV `applied_at` is a JS Date → `String(Date)` ("Mon Oct 13 2026 …" format!) — replicate or fix knowingly. Filename header not escaped. |
| 43 | GET | `/api/v1/reports/link-health/:siteId` | user+own | `?limit (def100,max1000)&offset&format=csv` | JSON `{summary:{total_links,internal,external,broken,unverified,blocked,server_error,timeouts,rate_limited,dns,connection,hop_cap,redirected,chains_over_two,loops,temp_redirects,nofollow,sponsored,ugc,last_checked_at}, broken[], redirects[], nofollow[], unverified[], blocked[], server_errors[], breadcrumbs:{posts,checked,with_visible,with_schema,mismatched,missing[]}, limit, offset}`; list rows = `LinkRow` (§4 link_health cols + source_url/title) | UI | CSV unbounded (46k rows for site36). 9 parallel queries. |
| 44 | POST | `/api/v1/sites/:siteId/link-health` | user+own | `{}` | **202** `{job_run_id}` | UI | enqueues `link_health`. |
| 45 | GET | `/api/v1/reports/metrics/:siteId` | user+own | `?trend=` (1-500, def 30) (contract says `?n=`) | `{latest: SiteMetricsSnapshot\|null, deltas:[{key,current,previous,delta,pct,improved}], trend: SiteMetricsSnapshot[] (ASC)}` | UI | direction table `routes/schedules.ts:144-165`. |

### 1.6 Silo / CSV

| # | Method | Path | Auth | Request | Response | Consumers | Notes |
|---|---|---|---|---|---|---|---|
| 46 | GET | `/api/v1/silo/:siteId/clusters` | user+own | — | `[{id,label,source,pillar_url,members}]` | UI, MCP | |
| 47 | GET | `/api/v1/silo/:siteId/clusters/export` | user+own | — | `{clusters:[{label,pillar_url,member_urls[]}]}` | UI | Exports auto AND custom clusters. |
| 48 | POST | `/api/v1/silo/:siteId/clusters/import` | user+own | `{clusters*:[{label?, pillar_url?: string\|null, member_urls?: string[]}]}` | `{clusters, members, skipped}` | UI | DELETE custom clusters then per-URL UPDATE — **no transaction**, N×M round trips; a failure mid-way leaves the site with half its custom clusters. |
| 49 | POST | `/api/v1/csv/import/:siteId` | user+own | raw CSV body (`text/csv\|text/plain\|application/csv`), header must contain `url`,`content_html` (`title` optional) | `{received, upserted, skipped}`; 400 | UI | Non-http(s) URL skipped. Inserts `builder='unknown', post_type='post', post_status='publish'`, nulls `embedding` on update. N+1. |
| 50 | GET | `/api/v1/csv/export/:siteId` | user+own | — | `text/csv` attachment `siloforge-edges-<domain>.csv`, header `source_url,target_url,anchor,anchor_type,status,insert_module` | UI, MCP (raw text) | approved+applied edges. |

### 1.7 Schedules

| # | Method | Path | Auth | Request | Response | Consumers | Notes |
|---|---|---|---|---|---|---|---|
| 51 | GET | `/api/v1/schedules/:siteId` | user+own | — | `SiteSchedule[]` | UI | |
| 52 | POST | `/api/v1/schedules/:siteId` | user+own | `{job_kind? ∈ crawl\|plan\|link_health\|audit (def audit), cron* (5/6-field, no macros), tz? (IANA, def UTC), active? (def true), payload?: object}` (no JSON schema; manual validation) | upserted row + `{scheduled:true}` or `{scheduled:false, warning}` if Redis fails | UI | UPSERT on `(site_id, job_kind)`, then `upsertJobScheduler('sf:<siteId>:<kind>', {pattern, tz}, {name: kind, data, opts:{removeOnComplete:500, removeOnFail:1000}})`, writes `next_run_at` from `getJobScheduler().next`. Inactive → `removeJobScheduler`. |
| 53 | DELETE | `/api/v1/schedules/:scheduleId` | user (+own) | — | `{deleted:true}` | UI | removes scheduler best-effort. |
| 54 | POST | `/api/v1/schedules/:siteId/run-now` | user+own | `{topology? (incl. bidirectional), skip_crawl?}` | `{job_run_id}` (200) | UI | **Always enqueues an `audit`**, regardless of which schedule the user clicked. |

### 1.8 Model providers (admin only)

| # | Method | Path | Request | Response | UI |
|---|---|---|---|---|---|
| 55 | GET | `/api/v1/providers` | — | `ModelProvider[]` (name ASC); `api_key_masked` = `'*'×min(12,len−4) + last4` or `'****'` if undecryptable; `has_api_key` | ✓ |
| 56 | GET | `/api/v1/providers/health` | — | `{providers:[ModelProvider & {models: ModelRow[]}], index_dim}` | ✓ |
| 57 | POST | `/api/v1/providers` | `{name* 1-64, label? ≤128, kind* ∈ openai-compatible\|anthropic\|openrouter\|ollama\|custom, base_url* (http/s, trailing / stripped), api_key?, headers?: {str:str}, enabled?}` | **201** provider; 409 name | ✓ |
| 58 | GET | `/api/v1/providers/:id` | — | provider; 404 | — |
| 59 | PATCH | `/api/v1/providers/:id` | partial; `api_key` absent = keep, `''` = clear | provider | ✓ |
| 60 | DELETE | `/api/v1/providers/:id` | — | `{deleted:true}` (cascades models → rungs) | ✓ |
| 61 | POST | `/api/v1/providers/:id/probe` | — | `ProbeResult {ok, latency_ms, status_code, error, dim?, sample?}`; probes first enabled model (chat preferred); writes `last_probe_*` on model + provider | ✓ |
| 62 | GET | `/api/v1/models` | `?provider_id (uuid) &role` | `ModelRow[]` | ✓ |
| 63 | POST | `/api/v1/models` | `{provider_id*, model_id* ≤200, role* ∈ chat\|embed, label?, context_tokens?≥1, dim?≥1, enabled?, price_in?≥0, price_out?≥0, notes?}` | **201**; 409; 404 provider | ✓ |
| 64 | PATCH | `/api/v1/models/:id` | partial (no `role`, no `provider_id`) | ModelRow | ✓ |
| 65 | DELETE | `/api/v1/models/:id` | — | `{deleted:true}` | ✓ |
| 66 | POST | `/api/v1/models/:id/probe` | — | `ProbeResult & {index_dim}` | ✓ |
| 67 | GET | `/api/v1/model-chains` | `?role &site_id (uuid or "global")` | `[ModelChain & {rungs:[rung + wire_model_id, model_role, dim, provider_name]}]` | ✓ |
| 68 | POST | `/api/v1/model-chains` | `{name* ≤128, role*, site_id?: uuid\|null, active?, notes?}` | **201** chain; 404 site | ✓ |
| 69 | PATCH | `/api/v1/model-chains/:id` | `{name?, notes?}` | chain | ✓ |
| 70 | DELETE | `/api/v1/model-chains/:id` | — | `{deleted:true}` | ✓ |
| 71 | POST | `/api/v1/model-chains/:id/activate` | — | chain (siblings in same role+site scope deactivated in one tx) | ✓ |
| 72 | POST | `/api/v1/model-chains/:id/deactivate` | — | chain | ✓ |
| 73 | GET | `/api/v1/model-chains/:id/rungs` | — | `ModelChainRung[]` | — |
| 74 | PUT | `/api/v1/model-chains/:id/rungs` | `{rungs*:[{model_row_id*, timeout_ms?≥1, max_retries? 0-10, enabled?}]}` (contract says `model_id` — impl uses **`model_row_id`**) | rung list; 400 role mismatch / embed dim ≠ `EMBED_DIM` / null dim | ✓ |
| 75 | POST | `/api/v1/model-chains/:id/rungs` | single rung body | **201** rung appended at `max(position)+1` | ✓ |
| 76 | POST | `/api/v1/model-chains/:id/rungs/reorder` | `{rung_ids*: string[]}` (must be exact permutation) | rung list | ✓ |
| 77 | PATCH | `/api/v1/model-chain-rungs/:rungId` | `{timeout_ms?, max_retries?, enabled?}` | rung | — |
| 78 | DELETE | `/api/v1/model-chain-rungs/:rungId` | — | `{deleted:true}` (leaves a position gap) | ✓ |

Not present (contract lists them): `GET /model-chains/:id`, `GET /models/:id`. No site DELETE, no api-key rotation, no user management (list/disable/role change) endpoints exist at all.

### 1.9 Consumer matrix (who really calls what)

- **UI** (`ui/src/api.ts`): everything in 1.1–1.8 **except** `/health`, `/auth/handshake`, `/sync`, `/suggestions/*`, `GET /providers/:id`, `GET /model-chains/:id/rungs`, `PATCH /model-chain-rungs/:id`. Token in `localStorage['sf_token']`; any 401 on a non-auth call → clears token + redirect `/login`. CSV endpoints fetched with raw `fetch` + Bearer.
- **WP plugin** (`connect/plugin/siloforge-connect/includes/class-sfc-client.php`, `-sync.php`, `-linker.php`): `POST /auth/handshake` (expects exactly 200 + `token`, stores `site_id` option, caches JWT in transient `sfc_jwt` for **25 days** vs 30-day token), `POST /sync`, `GET /suggestions/:siteId`, `POST /suggestions/:siteId/ack`, `POST /suggestions/:siteId/revert-ack`. On 401 it re-handshakes once and retries. Default API URL `http://localhost:8600`. Reads `wp_post_id`, `edge_id`, `target_url`, `anchor`, `paragraph_index`, `replacement_html`, `insert_module`. Exposes WP route `GET /wp-json/siloforge/v1/status` (manage_options).
- **MCP** (`mcp/src/client.mjs`, `index.mjs`): logs in with `SILOFORGE_EMAIL`/`SILOFORGE_PASSWORD` (or static `SILOFORGE_TOKEN`), re-login on 401. Calls: GET `/sites`, `/dashboard/:id`, `/edges/:id?status&tool&limit`, `/reports/links/:id`, `/reports/anchors/:id`, `/history/:id`, `/jobs/:id`, `/silo/:id/clusters`, `/csv/export/:id`; POST `/sites`, `/sites/:id/crawl`, `/sites/:id/plan`, `/edges/:id/decide`, `/bulk/decide`, `/bulk/run`, `/bulk/undo`, `/rules/keywords/:id`, `/rules/keywords/:id/run`, `/rules/ignore/:id`; PATCH `/edges/:id/anchor`. Site lookup by id-or-domain via full `/sites` list (so every MCP tool call pulls all sites incl. plaintext secrets into the LLM context).

---

## 2. Auth model (exact, for migrating existing users/secrets)

### 2.1 JWT
- Library `@fastify/jwt` with `secret: JWT_SECRET` → **HS256** (default), header `{"alg":"HS256","typ":"JWT"}`.
- Claims: human `{role:"admin"|"user", user_id:<uuid>, iat, exp}`; site `{role:"site", site_id:<uuid>, iat, exp}`. `exp = iat + 30 d` (`expiresIn:'30d'`, `auth.ts:73`, `:220`). No `iss/aud/sub/jti`, no `nbf`.
- Transport: `Authorization: Bearer <jwt>` only (no cookies).
- Verification: signature + exp only. **Role and existence are NOT re-checked against `users`** (except `/auth/me` and change-password, which 401 if the row is gone). Consequence: demoted/deleted users and sites with rotated keys keep working until expiry; there is no revocation.
- Default `JWT_SECRET` fallback `dev-secret-do-not-use` (`shared/src/config.ts:27`) — the process boots with it if unset.
- Rust: `jsonwebtoken` HS256 with the same secret validates all existing tokens unchanged; keep `role`/`user_id`/`site_id` claim names and numeric seconds `iat`/`exp`. Leeway: fast-jwt default clockTolerance 0.

### 2.2 Passwords
- `bcryptjs`, **cost 10**, `$2a$` prefix (live: 2/2 users `$2a$10$`). Hash at register, change-password, and admin seed (`seed.ts:16`, cost hard-coded 10).
- Rust `bcrypt` crate verifies `$2a$` hashes; emit `$2b$`/`$2a$` — both fine. Keep the dummy-hash compare on unknown email (`$2a$10$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy`). Note bcrypt's 72-byte input truncation (bcryptjs truncates too) — identical behaviour.
- Policy: min length 8 only.

### 2.3 Admin seed
On boot, if `users` is empty and `ADMIN_PASSWORD` set: insert `ADMIN_EMAIL` (lowercased), name `Admin`, role `admin`. Then `UPDATE sites SET user_id = <oldest admin> WHERE user_id IS NULL`. `ADMIN_API_KEY` is dead config.

### 2.4 Site API key & handshake
- Format `sf_` + 32 lowercase hex (`randomBytes(16)`), stored **plaintext** in `sites.api_key` (UNIQUE), returned by `GET/POST /sites`, `/sites/:id`, `/dashboard`.
- Handshake: `POST /auth/handshake {api_key}` → `SELECT id FROM sites WHERE api_key=$1` (non-constant-time DB compare) → site JWT 30 d. No rotation endpoint; rotating requires SQL, and old site JWTs remain valid for up to 30 days.

### 2.5 Secret encryption (model provider keys)
- `shared/src/crypto.ts`: **AES-256-GCM**, 12-byte random IV, 16-byte tag.
- Key = `scryptSync(secret, "siloforge.provider.keys.v1", 32)` with **Node defaults N=16384, r=8, p=1, maxmem 32 MiB**, where `secret = process.env.SILOFORGE_ENCRYPTION_KEY ?? JWT_SECRET` (note `??`: an empty-string `SILOFORGE_ENCRYPTION_KEY=` would be used as the key).
- Blob format `v1:<iv b64>:<tag b64>:<ciphertext b64>` (standard base64 with padding); empty plaintext ↔ empty string.
- Live: `SILOFORGE_ENCRYPTION_KEY` is **not set** in `.env` and not in `.env.example` → provider keys are keyed off `JWT_SECRET`; rotating the JWT secret silently makes every stored provider key undecryptable (`tryDecryptSecret` → `''` → requests go out with no auth). Currently `model_providers` has **0 rows**, so there is nothing to migrate today.
- Rust: `scrypt` crate `Params::new(14, 8, 1, 32)` + `aes-gcm` reproduces it byte-for-byte.

### 2.6 Other stored credentials (NOT encrypted)
- `sites.settings.wp_user` + `sites.settings.wp_app_password` (WordPress Application Password used by engine push-mode inject, `engine/src/pipeline/inject.ts:32-49`) — **80 of 83 live sites**, plaintext, returned by every site-returning endpoint.
- `sites.settings.gsc.credentials` — GCP service-account JSON (private key), plaintext; redacted to `"***"` only in GET `/sites`, GET `/sites/:id`. 0 live sites.
- `GSC_CREDENTIALS_JSON`, `OPENROUTER_API_KEY(_T2)` env.

---

## 3. Job system

### 3.1 Queue
- One BullMQ queue `siloforge` (`shared/src/queue.ts`), Redis via ioredis `maxRetriesPerRequest:null`. Job name = `kind`. Ad-hoc enqueue opts: `removeOnComplete:500, removeOnFail:1000, attempts: crawl ? 2 : 1`, no backoff, default BullMQ lock (30 s, auto-renewed) and stalled checks (default `maxStalledCount 1`). Scheduled jobs: `removeOnComplete:500, removeOnFail:1000`, attempts 1.
- Worker: 2 replicas × `concurrency: 2`. **No per-site mutual exclusion, no job dedupe id, no global job timeout.** Live `job_runs.error` evidence: "duplicate concurrent run from cron misfire", "stale/orphaned plan … after BullMQ lock loss" (×3), `duplicate key … paragraphs_post_id_idx_key` (chunker race), embed rung "infinite fallback loop" for 30+ min.
- Dispatcher (`worker/src/index.ts:46-72`): `ensureJobRun()` replaces an empty/unknown `jobRunId` with a freshly inserted `job_runs` row (queued), then switches on `job.name`.

### 3.2 Job types

| kind | payload (all have `siteId`, `jobRunId`) | enqueued by | handler | engine call(s) | progress / stats written to `job_runs.stats` |
|---|---|---|---|---|---|
| `crawl` | — | `POST /sites/:id/crawl`, schedule | `jobs/crawl.ts` | `crawlWpRest(site)` or `crawlSitemap(site)`, then `chunkPost(id)` **for every post of the site sequentially** | done: `{upserted, skipped, paragraphs}` |
| `plan` | `topology?`, `archive?` | `/plan`, `/archive-plan`, schedule | `jobs/plan.ts` | `planSite(siteId, topology, {archive, onProgress})` (chunk→embed→keywords→graph→cluster→topology→score→anchors[→archive silo]) | incremental `stats.progress = {phase, pct}` (only on pct change); done: PlanStats + `progress:{phase:'done',pct:100}` |
| `inject` | `edgeIds[]` | `/bulk/run` | `jobs/inject.ts` | `injectEdges(siteId, edgeIds)` (snapshot → WP REST push with app password, or leave for plugin pull; live verify + auto-rollback) | done: InjectStats |
| `undo` | `edgeIds[]` | `/bulk/undo` | `jobs/undo.ts` | `undoEdges` (restore snapshots → `reverted`) | done: `{reverted}` |
| `keyword_link` | — | `/rules/keywords/:id/run` | `jobs/keywordLink.ts` | `runKeywordLinker` → edges `origin='keyword_linker'` | done: `{created}` |
| `link_health` | — | `/sites/:id/link-health`, schedule | `jobs/linkHealth.ts` | `runLinkHealthCheck` (p-queue, SSRF guard, adaptive throttle) → `link_health` rows + posts rollups | done: LinkHealthStats |
| `audit` | `topology?`, `skipCrawl?` | `/schedules/:id/run-now`, schedule | `jobs/audit.ts` | crawl(+chunk) → link_health → plan → `captureMetrics` only if all stages ok | incremental `stats.stages=[{stage,ok,ms,stats?,error?}]`; done `{stages, degraded, metrics_captured, metrics?}`; **failed if plan stage fails** |

`job_runs` lifecycle: API inserts `queued` → `markRunning` (`status='running', started_at`) → `markDone` (`status='done', stats=<replace>, finished_at`) or `markFailed` (`status='failed', error=message, finished_at`) and rethrow (BullMQ marks failed). `mergeStats` = `stats || partial` (shallow). On crawl retry (attempt 2) the same row goes failed→running→done. A worker crash/OOM leaves the row `running` forever (no reaper). Live: 176 rows (crawl 18 done/2 failed, inject 19, keyword_link 2, link_health 97, plan 28 done/10 failed); none currently queued/running.

### 3.3 Schedules (cron)
- Source of truth `site_schedules` (one per site+kind). Scheduler id `sf:<siteId>:<jobKind>`, cron `pattern` + `tz` (never `every`). Template data carries `jobRunId: ''`.
- API is the writer (upsert/remove in-request). Worker `syncSchedules()` at boot: upsert every active row, list all schedulers (paginated `getJobSchedulers(start,end)` inclusive range) and prune `sf:`-prefixed ids not desired. Runs on both replicas.
- `recordScheduleRun()` (sets `last_run_at`, `last_job_run_id`, `next_run_at`) is only called from the **audit** handler — scheduled `crawl`/`plan`/`link_health` never update their row (§6 B6). Manual run-now audits also stamp the audit schedule row.
- Cron validator: 5 or 6 fields, `*`, lists, ranges, steps, month/day names, `?` only in DOM/DOW, no macros (`@daily` rejected). Same code duplicated in `api/src/routes/schedules.ts` and `worker/src/scheduler.ts`.
- Live: 0 schedules, 0 metric snapshots.
- Redis has RDB only (worst case 1 h of scheduler/queue state lost), anonymous volume (lost on `docker compose down -v`/recreate) — schedules recover via boot sync; queued jobs do not.

---

## 4. Database schema (after 001–011) with live volumes

Extensions: `vector` 0.8.5, `pgcrypto` (for `gen_random_uuid()`). All PKs `uuid DEFAULT gen_random_uuid()` except `model_call_log.id bigserial`. All timestamps `timestamptz`. No Postgres enums — every enum is a `text` + `CHECK`. **No vector (HNSW/IVFFlat) indexes in Postgres** — ANN is Qdrant's job; pg vectors are plain storage.

Live totals (exact `count(*)`): sites 83 · users 2 (1 admin, 1 user) · posts 15,757 · paragraphs 429,134 (97,164 without embedding) · keywords 663,125 · link_edges 85,126 · link_health 169,069 · clusters 287 · job_runs 176 · snapshots 1 · keyword_rules 1 · ignore_keywords 1 · all model_* tables 0 · site_schedules 0 · site_metrics_history 0 · schema_migrations 11.
Sizes: paragraphs 764 MB (644 MB heap), posts 234 MB, link_health 151 MB, keywords 148 MB, link_edges 81 MB, everything else <200 kB. `pg_stat_user_tables.n_live_tup` reads 0 for every table → stats were reset / never analysed since restart — run `ANALYZE` (planner is flying blind).

Top 10 sites (posts / paragraphs / edges / link_health): site36.example 2621/112706/14721/45970 · site04.example 1716/46655/10785/0 · site35.example 1676/70196/0/32786 · site20.example 1438/35600/10218/29852 · site66.example 1223/23535/7669/0 · site74.example 1159/21859/6196/0 · site62.example 979/16615/4619/12510 · site80.example 976/16697/5366/13319 · site51.example 835/18163/5045/13928 · site44.example 672/7957/3283/4471. All `wp_rest`.

Edges by status/origin: suggested/plan 46,136 · applied/existing 21,228 · approved/plan 12,107 · suggested/archive 3,891 · applied/plan 1,689 · failed/plan 39 · reverted/plan 32 · suggested/keyword_linker 2 · approved/keyword_linker 2.

### 4.1 Tables

**sites** — `id` PK; `domain text NOT NULL UNIQUE`; `wp_url text`; `mode text NOT NULL DEFAULT 'wp_rest' CHECK IN (wp_rest,sitemap)`; `api_key text NOT NULL UNIQUE`; `settings jsonb NOT NULL DEFAULT '{}'` (live keys: `insert_modules`, `wp_user`, `wp_app_password`; schema'd keys in `SiteSettings`: preset, silo_topology, insert_modules, max_links_per_post, min_links_per_post, max_new_links_per_pass, link_overload_threshold, money_page_urls, gsc{connected,saved_at,client_email,credentials}, skip_inject_verify); `created_at NOT NULL DEFAULT now()`; `last_sync_at`; `user_id uuid FK users ON DELETE CASCADE` (002, nullable). Idx: `sites_user_idx(user_id)`.

**users** (002) — `id`; `email text NOT NULL UNIQUE`; `password_hash text NOT NULL`; `name text NOT NULL DEFAULT ''`; `role text NOT NULL DEFAULT 'user' CHECK IN (admin,user)`; `created_at DEFAULT now()` (nullable!).

**posts** — `id`; `site_id FK sites CASCADE NOT NULL`; `wp_post_id bigint`; `url text NOT NULL`; `slug`; `title/content_html/content_text/content_hash text NOT NULL DEFAULT ''`; `builder text NOT NULL DEFAULT 'gutenberg'` (no CHECK; values gutenberg|elementor|divi|classic|unknown|archive); `post_type text NOT NULL DEFAULT 'post'` (incl. `archive`); `post_status text NOT NULL DEFAULT 'publish'`; `meta jsonb NOT NULL DEFAULT '{}'`; `word_count int NOT NULL DEFAULT 0`; `cluster_id uuid FK clusters ON DELETE SET NULL`; `pagerank double NOT NULL DEFAULT 0`; `click_depth int NOT NULL DEFAULT -1`; `is_orphan, is_dead_end bool NOT NULL DEFAULT false`; `embedding vector(384)`; `published_at, modified_at`; `crawled_at NOT NULL DEFAULT now()`; (006) `outbound_link_count, internal_link_count, broken_link_count, nofollow_link_count, redirect_link_count int NOT NULL DEFAULT 0`, `has_breadcrumbs, has_breadcrumb_schema bool NOT NULL DEFAULT false`, `breadcrumb_report jsonb NOT NULL DEFAULT '{}'`, `link_health_checked_at`; (010) `is_contextual_orphan, is_nav_only bool NOT NULL DEFAULT false`, `body_inbound_count, nav_inbound_count int NOT NULL DEFAULT 0`. `UNIQUE(site_id,url)`. Idx: `posts_site_idx(site_id)` (redundant with unique), `posts_cluster_idx(cluster_id)`, `posts_site_type_idx(site_id,post_type)`, `posts_link_overload_idx(site_id) WHERE outbound_link_count>100`, `posts_contextual_orphan_idx(site_id) WHERE is_contextual_orphan`.

**paragraphs** — `id`; `post_id FK posts CASCADE NOT NULL`; `idx int NOT NULL`; `text NOT NULL`; `hash NOT NULL`; `embedding vector(384)`; `UNIQUE(post_id,idx)`; idx `paragraphs_post_idx(post_id)` (redundant).

**clusters** — `id`; `site_id FK sites CASCADE NOT NULL`; `label text NOT NULL DEFAULT ''`; `pillar_post_id FK posts SET NULL`; `created_at`; (005) `source text NOT NULL DEFAULT 'auto' CHECK IN (auto,custom)`. Idx `clusters_site_source_idx(site_id,source)`. (Circular FK with posts.cluster_id.)

**keywords** — `id`; `post_id FK posts CASCADE`; `keyword NOT NULL`; `source CHECK IN (title,htag,meta,body,gsc)`; `score double NOT NULL DEFAULT 0`; `volume int`; `position double`; `striking bool NOT NULL DEFAULT false`; `UNIQUE(post_id,keyword,source)`; idx `keywords_post_idx`.

**link_edges** — `id`; `site_id FK sites CASCADE`; `source_post_id`, `target_post_id FK posts CASCADE NOT NULL`; `paragraph_id FK paragraphs SET NULL`; `anchor text NOT NULL DEFAULT ''`; `anchor_type NOT NULL DEFAULT 'partial' CHECK IN (exact,partial,branded,generic,lsi,naked,cta)` (009); `composite_score double NOT NULL DEFAULT 0`; `score_breakdown jsonb NOT NULL DEFAULT '{}'` (11 dims + weights); `silo_topology CHECK IN (reverse,serial,priority,circle,bidirectional)` (009); `insert_module CHECK IN (native,rephrase,context,content_gap)`; `status NOT NULL DEFAULT 'suggested' CHECK IN (suggested,approved,rejected,applied,reverted,failed)`; `origin text NOT NULL DEFAULT 'plan'` (plan|manual|keyword_linker|existing|archive, no CHECK); `snapshot_id FK snapshots SET NULL`; `error`; `created_at`, `decided_at`, `applied_at`. `UNIQUE(site_id,source_post_id,target_post_id,anchor)`. Idx: `(site_id,status)`, `(source_post_id)`, `(target_post_id)`.

**snapshots** — `id`; `post_id FK posts CASCADE`; `content_html NOT NULL`; `reason NOT NULL DEFAULT 'pre-inject'`; `taken_at`. Idx `(post_id)`.

**job_runs** — `id`; `site_id FK sites CASCADE` (nullable); `kind text NOT NULL` (no CHECK); `status NOT NULL DEFAULT 'queued' CHECK IN (queued,running,done,failed)`; `stats jsonb NOT NULL DEFAULT '{}'`; `error`; `started_at`, `finished_at`, `created_at`. Idx `(site_id, created_at DESC)`. Never pruned.

**keyword_rules** (003) — `id`; `site_id FK CASCADE`; `keyword`, `target_url NOT NULL`; `anchor_type NOT NULL DEFAULT 'exact' CHECK IN (exact,partial,branded,generic,lsi)` (**not widened**); `max_links int NOT NULL DEFAULT 3`; `active bool NOT NULL DEFAULT true`; `created_at`; `UNIQUE(site_id,keyword,target_url)`; idx `(site_id)`.

**ignore_keywords** (003) — `id`; `site_id FK CASCADE`; `phrase NOT NULL`; `created_at`; `UNIQUE(site_id,phrase)`; idx `(site_id)`.

**link_health** (006/011) — `id`; `site_id FK CASCADE`; `source_post_id FK posts CASCADE` (nullable); `href NOT NULL`; `requested_url NOT NULL`; `resolved_url`; `target_post_id FK posts SET NULL`; `scope NOT NULL DEFAULT 'internal' CHECK IN (internal,external)`; `status_code int`; `ok, is_broken bool NOT NULL DEFAULT false`; `redirect_hops int NOT NULL DEFAULT 0`; `redirect_chain jsonb NOT NULL DEFAULT '[]'` (`[{url,status,order}]`); `has_redirect_loop, has_temp_redirect bool`; `rel text NOT NULL DEFAULT ''`; `is_nofollow, is_sponsored, is_ugc bool`; `anchor NOT NULL DEFAULT ''`; `placement NOT NULL DEFAULT 'body' CHECK IN (body,nav,header,footer,aside,unknown)`; `error`; `checked_at`; (011) `failure_kind NOT NULL DEFAULT 'none' CHECK IN (none,not_found,gone,loop,hop_cap,server_error,blocked,rate_limited,timeout,dns,connection,other)`, `is_unverified bool NOT NULL DEFAULT false`. `UNIQUE(site_id,source_post_id,href,anchor)` (NULL source → duplicates allowed). Idx: `(site_id)`, partial `(site_id) WHERE is_broken`, `WHERE is_nofollow`, `WHERE redirect_hops>0`, `(source_post_id)`, `(site_id,failure_kind) WHERE failure_kind<>'none'`.

**site_schedules** (007) — `id`; `site_id FK CASCADE`; `job_kind CHECK IN (crawl,plan,link_health,audit)`; `cron NOT NULL`; `tz NOT NULL DEFAULT 'UTC'`; `active bool NOT NULL DEFAULT true`; `payload jsonb NOT NULL DEFAULT '{}'`; `last_run_at`; `last_job_run_id FK job_runs SET NULL`; `next_run_at`; `created_at`, `updated_at`; `UNIQUE(site_id,job_kind)`; idx `(active) WHERE active`.

**site_metrics_history** (007) — `id`; `site_id FK CASCADE`; `job_run_id FK job_runs SET NULL`; `source NOT NULL DEFAULT 'crawl' CHECK IN (crawl,plan,link_health,audit,manual)`; `metrics jsonb NOT NULL DEFAULT '{}'`; `captured_at`; STORED generated: `m_posts, m_orphans, m_broken_links, m_internal_links, m_health_score int`, `m_avg_click_depth numeric(6,2)` (from `metrics->>key`). Idx `(site_id, captured_at DESC)`. Rust must never INSERT into generated columns.

**model_providers**, **models**, **model_chains**, **model_chain_rungs**, **model_call_log** (008) — see §5.

**schema_migrations** — `filename text PK`, `checksum text NOT NULL`, `applied_at`. The Rust service must keep honouring this ledger (same filenames, same checksum algorithm) or share the runner, otherwise it will try to re-apply 001.

### 4.2 Qdrant
| collection | points | vectors | distance | payload index | HNSW | notes |
|---|---|---|---|---|---|---|
| `sf_posts` | 15,757 | 384 | Cosine | `site_id` keyword | m16 ef100, full_scan_threshold 10000 | `indexed_vectors_count 0` (each segment below threshold → brute force; fine at this size) |
| `sf_paragraphs` | 331,970 | 384 | Cosine | `site_id` keyword | same | point id = paragraphs.id; matches the 331,970 embedded paragraph rows |

Point id = Postgres row uuid, payload `{site_id, post_id, url}`. Embedding model: all-MiniLM-L6-v2 (OpenRouter `sentence-transformers/all-MiniLM-L6-v2` ≡ local `Xenova/all-MiniLM-L6-v2`), mean-pooled, L2-normalised. Changing model/`EMBED_MAX_CHARS` requires full reindex of both pg columns and both collections.

---

## 5. Model provider system (migration 008)

### 5.1 Tables
- **model_providers** — `name text UNIQUE NOT NULL` (slug), `label`, `kind CHECK IN (openai-compatible, anthropic, openrouter, ollama, custom)`, `base_url text NOT NULL DEFAULT ''`, `api_key_enc text NOT NULL DEFAULT ''` (§2.5), `headers jsonb NOT NULL DEFAULT '{}'`, `enabled bool DEFAULT true`, `last_probe_at/ok/error/ms`, `created_at`, `updated_at`.
- **models** — `provider_id FK model_providers CASCADE`, `model_id text` (wire id), `label`, `role CHECK IN (chat,embed)`, `context_tokens int`, `dim int`, `enabled`, `price_in/out numeric(12,6)` (USD/1M tok), `notes`, `last_probe_*`, `created_at`; `UNIQUE(provider_id, model_id, role)`; idx `models_role_idx(role) WHERE enabled`.
- **model_chains** — `name`, `role`, `site_id FK sites CASCADE` (NULL = global), `active bool DEFAULT false`, `notes`, timestamps. Partial unique: `(role, site_id) WHERE active AND site_id IS NOT NULL`; `(role) WHERE active AND site_id IS NULL` → at most one active chain per scope.
- **model_chain_rungs** — `chain_id FK CASCADE`, `model_id uuid FK models CASCADE` (note: column named `model_id` but holds `models.id`; API body calls it `model_row_id`), `position int CHECK ≥0`, `timeout_ms`, `max_retries`, `enabled`; `UNIQUE(chain_id, position)` + redundant idx `(chain_id, position)`.
- **model_call_log** — `id bigserial`, `model_row_id FK models CASCADE` (NULL for env rungs), `chain_id FK model_chains SET NULL`, `role`, `rung int`, `ok bool`, `status_code`, `latency_ms`, `error` (≤500 chars), `called_at`. Idx `(model_row_id, called_at DESC)`. Migration comment says "pruned by the API" — **nothing prunes it** and no endpoint reads it.

### 5.2 Wire adapters (`engine/src/providers/adapters.ts`, mirrored in `api/src/routes/providers.ts:319-460`)
| kind | chat | embed | auth header |
|---|---|---|---|
| openai-compatible / custom | `POST {base}/chat/completions {model, messages, max_tokens, temperature}` → `choices[0].message.content` | `POST {base}/embeddings {model, input:[…]}` → `data[i].embedding` | `Authorization: Bearer <key>` if key ≠ '' |
| openrouter | same as OpenAI + `HTTP-Referer: https://siloforge.local`, `X-Title: SiloForge` | same | Bearer |
| anthropic | `POST {base}/v1/messages` (`anthropic-version: 2023-06-01`) → join `content[].text` | not supported (throws) | `x-api-key` |
| ollama | `POST {base}/api/chat {stream:false, options.num_predict}` → `message.content` | `POST {base}/api/embeddings {model, prompt}` → `embedding` | Bearer if key |
Custom `headers` are merged last (can override auth). Probe: chat with `max_tokens:1` "ping", embed with `["test"]`, 20 s timeout, never throws, reports `dim`/`sample`.

### 5.3 Resolution & fallback (`engine/src/providers/registry.ts:851-1000`, `chain.ts`)
1. `resolveChainDetail(role, siteId)`: site-scoped active chain → global active chain → env fallback. Rungs = `model_chain_rungs` JOIN models JOIN providers WHERE rung, model and provider all `enabled`, ORDER BY position; key decrypted per call (undecryptable → `''`). Active-but-empty chain → warn + env fallback. Any DB error → env fallback.
2. Env fallback: **chat only** — `DARIO_MODEL_PRIMARY` then `DARIO_MODEL_FALLBACK` (if different) against `DARIO_URL`, `openai-compatible`, no auth, provider name `env:dario`. Embed: none (env embed chain lives separately in `engine/src/embed/embedder.ts`: `EMBED_PROVIDER` remote-first|remote-only|local or `EMBED_CHAIN` tokens `openrouter,openrouter2,local`).
3. `execute()`: per rung `attempts = max(1, (max_retries ?? 0)+1)`; retry same rung (500 ms sleep) only on transport errors or HTTP 408/409/425/429/500/502/503/504; otherwise fall through. Output validation is part of success: chat text non-empty after trim; embed count = inputs, each vector width = `EMBED_DIM` (384), finite, norm >1e-8, then L2-normalised. All rungs failed → throw listing every rung + reason. Each attempt best-effort logged to `model_call_log`.
4. Timeouts: per-rung `timeout_ms`, else adapter default (chat 120 s), embed default `EMBED_TIMEOUT_MS` (20 s).
5. Save-time guard: embed rungs require `models.dim = EMBED_DIM` and role match (`providers.ts:255-276`).

**Who actually uses it:** only `engine/src/pipeline/llm.ts` → `runChat(messages, {maxTokens:2000, temperature:0.3})` for the rephrase/context insertion modules, **without siteId** (site chains are never consulted). `embedTexts()` (all embedding in plan/crawl/intent) does **not** call `runEmbed` — embed chains configured in Settings are dead config (§6 G1).

---

## 6. Bugs, security holes, loopholes, bottlenecks, ghost issues

Severity: **C** critical, **H** high, **M** medium, **L** low. Paths under `app/packages/` unless noted.

### 6.1 Security
| ID | Sev | Finding | Where |
|---|---|---|---|
| S1 | **C** | WordPress Application Passwords (`settings.wp_user`/`wp_app_password`, 80/83 live sites) stored plaintext and returned by `GET /sites`, `GET /sites/:id`, `PATCH /sites/:id`, `GET /dashboard/:id`. `redactGsc` only masks GSC. The MCP server pipes the full `/sites` list into the LLM context on every site lookup (`mcp/src/client.mjs:71`). | `api/src/routes/sites.ts:28-37,40-59,148`; `routes/dashboard.ts:58,168` |
| S2 | H | Site `api_key` returned in every site payload (and to MCP/LLM); stored plaintext; no rotation endpoint; site JWTs survive key rotation for 30 d. | `routes/sites.ts:79-87`; `auth.ts:201-223` |
| S3 | H | GSC service-account JSON (private key) stored unencrypted in jsonb; `PATCH /sites/:id` and `GET /dashboard` return it unredacted. | `routes/gsc.ts:44-55`; `routes/sites.ts:148`; `routes/dashboard.ts:168` |
| S4 | H | JWT role/existence never re-checked: a demoted admin keeps admin (incl. provider keys/probes) and a deleted user keeps access for up to 30 d; password change does not revoke tokens. No revocation list / token version. | `auth.ts:27-35,73`; `routes/providers.ts:40-50` |
| S5 | H | Insecure defaults boot silently: `JWT_SECRET` → `dev-secret-do-not-use`, DB password `change-me`. Provider-key encryption falls back to `JWT_SECRET` (not set in live `.env`), coupling two secrets: rotate JWT → all provider keys undecryptable → requests silently sent with no auth. | `shared/src/config.ts:23,27`; `shared/src/crypto.ts:21-27`; `.env.example` lacks `SILOFORGE_ENCRYPTION_KEY` |
| S6 | M | Open self-registration with no rate limit, no email verification, no CAPTCHA; login and handshake unthrottled (bcrypt cost 10 → online guessing is cheap; handshake guessing is infeasible at 128-bit but floods the DB). Mitigated only by 127.0.0.1 binding. | `auth.ts:81-123,201` |
| S7 | M | `POST /suggestions/:siteId/ack` has no status precondition: a site token (or owner) can flip any edge (suggested/rejected) to `applied`, corrupting history/metrics and bypassing the approval gate in reporting. | `routes/suggestions.ts:90-104` |
| S8 | M | `PATCH /sites/:id` merges arbitrary client JSON into `settings` (shallow): can plant `wp_app_password`, `gsc`, `skip_inject_verify`, out-of-range density values — there is no settings schema. Keys can never be removed. | `routes/sites.ts:110-148` |
| S9 | M | Admin-controlled SSRF: provider `base_url` + custom headers are fetched server-side (probe and chain execution) with no private-range guard (crawler has one via `CRAWL_ALLOW_PRIVATE_HOSTS`, providers don't). Probe error echoes 200 chars of the response body. | `routes/providers.ts:408-431` |
| S10 | M | `domain` globally UNIQUE across tenants: user B gets 409 "domain already exists" for a domain user A registered → cross-tenant existence oracle + squatting, contradicting the "never reveal foreign sites" rule. | `routes/sites.ts:89-91`; `001_init.sql` |
| S11 | L | `/sync` accepts any `url` scheme (no `isHttpUrl`, unlike CSV import) — the suggestions filter catches dangerous targets later, but sources/titles land unvalidated. `domain` field ignored. | `routes/sync.ts:63-117` |
| S12 | L | `Content-Disposition` filenames embed `domain` unescaped (header injection if a domain contains `"`/CRLF — domain has no format validation). | `routes/reports.ts:295`; `routes/csv.ts:195`; `routes/linkhealth.ts:137` |
| S13 | L | UI stores JWT in `localStorage` (XSS → 30-day token theft). | `ui/src/api.ts:47-55` |
| S14 | L | `qdrant/qdrant:latest` unpinned; Qdrant has no API key (internal network only). | `docker-compose.yml` |

### 6.2 Functional bugs
| ID | Sev | Finding | Where |
|---|---|---|---|
| B1 | H | `keyword_rules.anchor_type` CHECK was never widened in 009: API (and `isAnchorType`) accept `naked`/`cta` → INSERT/UPDATE violates CHECK → **500**. | `db/migrations/003_rules.sql:7-8`, `009_widen_enums.sql`; `routes/rules.ts:52-60,90-96` |
| B2 | M | `PATCH /rules/keywords/:ruleId` has no body schema — `max_links` 0/negative/non-int → stored or 500. | `routes/rules.ts:75-98` |
| B3 | M | `POST /sites/:id/plan` and `/archive-plan` reject `bidirectional` (enum missing) while types, DB, schedules and run-now accept it. MCP schema also omits it. | `routes/sites.ts:176,205`; `mcp/src/index.mjs:139` |
| B4 | M | Silo cluster import is non-transactional (DELETE custom clusters, then N inserts × M updates); partial failure loses the user's custom silo. | `routes/silo.ts:97-132` |
| B5 | M | `run-now` always runs an `audit`, whichever schedule row the UI button belongs to. | `routes/schedules.ts:367-394` |
| B6 | M | Scheduled `crawl`/`plan`/`link_health` never stamp `site_schedules.last_run_at/last_job_run_id/next_run_at` — only `audit` calls `recordScheduleRun`. UI shows stale "next run". | `worker/src/jobs/audit.ts:146`; `worker/src/scheduler.ts:308` |
| B7 | M | History CSV `applied_at` is a JS `Date` passed through `String()` → `"Mon Oct 13 2026 10:00:00 GMT+0000 (Coordinated Universal Time)"`, not ISO. Link-health CSV booleans render `true/false`. | `routes/reports.ts:288` |
| B8 | L | `/sync` vs `/csv/import` diverge: sync never nulls `embedding` on content change (stale vectors until a plan re-embeds), CSV does. | `routes/sync.ts:76-97`; `routes/csv.ts:135-146` |
| B9 | L | `/suggestions/:siteId?status=` documented but ignored; `/reports/links?duplicates=1` ignored. | `routes/suggestions.ts:12-62`; `routes/reports.ts:18` |
| B10 | L | `audit.ts` keeps its own duplicate `ensureJobRun` (dead after dispatcher change). Cron validator, metric direction table, probe client, chain SQL are copy-pasted between api/worker/engine — drift risk (B1/B3 are exactly this class). | `worker/src/jobs/audit.ts:42-49`; `routes/schedules.ts:1-20`; `routes/providers.ts:7-12` |
| B11 | L | `PATCH /models/:id` can change `dim` (or `enabled`) after the model is chained, bypassing the save-time 384-d guard; runtime validation then fails every call on that rung. | `routes/providers.ts:747-791` |
| B12 | L | Deleting a rung leaves a position gap; reorder requires the exact set. Harmless but the Rust impl must accept gaps. | `routes/providers.ts:1114-1123` |
| B13 | L | `users.created_at` nullable; `seedAdmin` picks "oldest admin" by created_at — NULLs sort last. | `002_users.sql` |

### 6.3 Concurrency / job-system loopholes
| ID | Sev | Finding | Evidence |
|---|---|---|---|
| J1 | **H** | No per-site lock or job dedupe: 4 parallel slots (2 replicas × 2) can run two crawls/plans/injects for the same site simultaneously (double-click, schedule + manual, cron misfire). Plans race on `link_edges`/`clusters`/`paragraphs`; injects can apply the same edge twice. | live `job_runs.error`: "duplicate concurrent run from cron misfire", "duplicate key … paragraphs_post_id_idx_key", "stale/orphaned plan … after BullMQ lock loss" ×3 |
| J2 | H | Long CPU-bound/network stages hold the BullMQ lock with no explicit `lockDuration`; event-loop stalls (local transformers.js embed) → lock loss → job re-delivered as stalled and runs twice. No job timeout: an embed chain stuck in rung fallback ran 30+ min ("infinite rung-fallback loop"). | `worker/src/index.ts:88-91`; live error rows |
| J3 | M | Orphaned `job_runs` rows: a killed worker leaves `status='running'` forever; there is no reaper / heartbeat. API job lists show phantom running jobs. | `worker/src/jobRun.ts` |
| J4 | M | Crawl `attempts: 2` with no backoff and no idempotency: a partially-completed crawl is re-run immediately. | `shared/src/queue.ts:26` |
| J5 | M | Redis AOF off + anonymous volume: queued (not yet started) jobs are lost on Redis restart/recreate; schedulers are rebuilt only when a worker boots. | `docker-compose.yml`; `CONFIG GET appendonly → no` |
| J6 | L | `syncSchedules()` runs on both replicas concurrently (upsert is idempotent; prune can race with an API upsert happening at the same moment → a just-created schedule deleted until next boot). | `worker/src/scheduler.ts:345-399` |

### 6.4 Bottlenecks
| ID | Sev | Finding | Where |
|---|---|---|---|
| P1 | H | Crawl re-chunks **every** post of the site sequentially after each crawl, even unchanged ones (2,621 posts / 112k paragraphs on site36). | `worker/src/jobs/crawl.ts:17-24`; `jobs/audit.ts:104-110` |
| P2 | H | `/sync` and `/csv/import` do 2 queries per post sequentially, no transaction/batching; 50-post 1.6 MB batches → 100 round trips per request. | `routes/sync.ts:63-117`; `routes/csv.ts:113-150` |
| P3 | M | Unbounded responses: `/suggestions` (all approved; 12k on one site), `/reports/links` (all posts), link-health CSV (46k rows), history CSV. | `routes/suggestions.ts:23-41`; `routes/reports.ts:18-101` |
| P4 | M | Correlated count subqueries per row: `GET /sites` (2 per site × 83), `GET /posts` (2 per post, and ORDER BY a correlated count forces full evaluation before LIMIT), `/silo/clusters`, `/usage` (paragraph count over 429k rows via nested IN). | `routes/sites.ts:42-55`; `routes/posts.ts:60-69`; `routes/usage.ts:22-30` |
| P5 | M | `pg.Pool max: 10` per process while dashboard fires 10 and link-health report 9 parallel queries → two concurrent dashboard loads exhaust the pool. | `shared/src/db.ts:8`; `routes/dashboard.ts:62-149` |
| P6 | M | Planner statistics absent (`n_live_tup = 0` for all tables) — run `ANALYZE`; check autovacuum. paragraphs heap 644 MB with 97k unembedded rows. | live `pg_stat_user_tables` |
| P7 | L | `/health` creates and tears down a Redis connection on every call (compose polls every 10 s). | `routes/health.ts:17-27` |
| P8 | L | `model_call_log` and `job_runs` grow unbounded (no pruning job despite migration comment). Redundant indexes: `posts_site_idx`, `paragraphs_post_idx`, `model_chain_rungs_chain_idx` duplicate unique-index prefixes (extra write cost on 429k-row table). | `008_model_providers.sql`; `001_init.sql` |

### 6.5 Ghost issues (configured/claimed but not real)
| ID | Finding | Where |
|---|---|---|
| G1 | **Embed chains in Settings > Models are never used.** `embedTexts()` (crawl/plan/intent) uses only the env chain; `runEmbed` has no production caller. Operators can configure/activate embed chains with zero effect. | `engine/src/embed/embedder.ts:265`; `engine/src/providers/chain.ts:298` |
| G2 | **Site-scoped chat chains are never used**: `llm.ts` calls `runChat` without `siteId`. The API lets admins create per-site chains. | `engine/src/pipeline/llm.ts:24-27` |
| G3 | 1,688 `applied` plan edges have `snapshot_id IS NULL` (only 1 snapshot row exists) — applied by plugin pull, which keeps its own revert metadata. `POST /bulk/undo` will enqueue them, but the server has nothing to restore from; verify `undoEdges` behaviour before promising undo in the Rust port. | live DB; `routes/bulk.ts:61-87` |
| G4 | `ADMIN_API_KEY` still required-with-default in config but unused; `GSC_CREDENTIALS_JSON` env + per-site creds coexist; `gsc.ts` route comment says engine ignores per-site creds but engine now reads them (stale comment). | `shared/src/config.ts:29`; `routes/gsc.ts:5-10` |
| G5 | Dashboard `http_links`/`outbound_domains` come from `link_health`, so they read **0** for every site never link-health-checked (e.g. site04, site66, site74) — looks like "no http links" rather than "unknown". | `routes/dashboard.ts:24-46` |
| G6 | `insert_modules` rephrase/context never reach the plugin: `/suggestions` always returns `replacement_html: null`, so LLM modules only work in WP-REST push mode. | `routes/suggestions.ts:11-14,59` |
| G7 | Plugin caches the site JWT 25 days; token lifetime 30 days — fine, but if the Rust port shortens JWT TTL below 25 d the plugin will only recover via its single 401-retry path (it does handle it). | `connect/.../siloforge-connect.php:22` |

### 6.6 Contract drift (implementation is authoritative for the port)
- `/history/:siteId` returns `{rows,total,limit,offset}` (contract: array).
- `/reports/metrics/:siteId` takes `?trend=` (contract: `?n=`).
- Rung bodies use `model_row_id` (contract: `model_id`); `POST /model-chains/:id/rungs` exists but is undocumented.
- `GET /model-chains/:id`, `GET /models/:id` documented but absent.
- Undocumented routes: `/auth/change-password`, `/posts/:siteId`, `/usage`, `/sites/:siteId/gsc` (GET/POST/DELETE).
- Mutating enqueue routes return **202**, register/creates **201**; handshake **200** (plugin requires exactly 200).
- Graph and link-health responses carry extra fields beyond the contract (`total_*`, `truncated`, `unverified`, `blocked`, `server_errors`, …) that the UI uses.

---

## 7. Environment variables (names only)

API/worker (`shared/src/config.ts` + direct `process.env`): `DATABASE_URL`, `REDIS_URL`, `QDRANT_URL`, `API_PORT`, `JWT_SECRET`, `ADMIN_API_KEY` (dead), `ADMIN_EMAIL`, `ADMIN_PASSWORD`, `DARIO_URL`, `DARIO_MODEL_PRIMARY`, `DARIO_MODEL_FALLBACK`, `EMBED_MODEL`, `EMBED_PROVIDER`, `EMBED_CHAIN`, `EMBED_REMOTE_MODEL`, `EMBED_DIM`, `EMBED_BATCH_SIZE`, `EMBED_MAX_CHARS`, `EMBED_TIMEOUT_MS`, `EMBED_RETRIES`, `EMBED_RETRY_DELAY_MS`, `EMBED_DEBUG`, `OPENROUTER_URL`, `OPENROUTER_API_KEY`, `OPENROUTER_API_KEY_T2`, `GSC_CREDENTIALS_JSON`, `SILOFORGE_ENCRYPTION_KEY` (not in .env.example, not set live), `CORS_ORIGINS`, `CRAWL_ALLOW_PRIVATE_HOSTS`, `MIGRATIONS_DIR`, `TRANSFORMERS_CACHE`, `SILOFORGE_RUST_ENGINE`, `SILOFORGE_RUST_ENGINE_PATH`, `NODE_ENV`. Compose: `POSTGRES_DB`, `POSTGRES_USER`, `POSTGRES_PASSWORD`.
MCP: `SILOFORGE_API_URL`, `SILOFORGE_EMAIL`, `SILOFORGE_PASSWORD`, `SILOFORGE_TOKEN`.
WP plugin options: `siloforge_api_url`, `siloforge_api_key`, `sfc_site_id`, transient `sfc_jwt`, `sfc_last_sync`; optional constants `SFC_API_URL`, `SFC_API_KEY`.

---

## 8. Port checklist (must-match items)

1. HS256 JWT with existing `JWT_SECRET`, claims `{role,user_id|site_id,iat,exp}`, 30-day expiry → existing UI sessions, MCP static tokens and plugin transients keep working.
2. bcrypt `$2a$10$` verify; dummy-hash timing path.
3. AES-256-GCM `v1:` blobs keyed by scrypt(N=16384,r=8,p=1,32) of `SILOFORGE_ENCRYPTION_KEY ?? JWT_SECRET`, salt `siloforge.provider.keys.v1`.
4. Exact status codes (201/202/200/404-not-403 ownership/403 admin) and `{error}` bodies; Fastify-style 400 validation body.
5. Empty-JSON-body tolerance; 25 MiB body limit; raw text/csv bodies on the CSV import route.
6. BullMQ wire compatibility if Rust workers must coexist with Node producers (queue `siloforge`, job name = kind, JSON payload camelCase `siteId/jobRunId/edgeIds/topology/archive/skipCrawl`, scheduler ids `sf:<siteId>:<kind>`) — or replace both sides at once.
7. `schema_migrations` ledger compatibility.
8. Decide (and document) whether to keep or fix S1/S2/S3 response leaks — the UI only displays `api_key` from the `POST /sites` 201 response (`ui/src/pages/Sites.tsx:254`), so `api_key` can be dropped from list/get/dashboard payloads (add an explicit reveal/rotate endpoint) without breaking the UI; MCP only needs `id`/`domain`. Never return `wp_app_password` or GSC credentials.
