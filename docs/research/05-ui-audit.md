# 05 — SiloForge UI audit (for the Rust backend)

Source audited: `D:\Code\SiloForge-Suite\app\packages\ui` (`@siloforge/ui` v1.4.0 — React 18, react-router-dom 6, reactflow 11, Vite 6).
About 10.7k lines of TS/TSX plus 2.4k lines of CSS. Audit date: 2026-10-13. I only read code. The one exception is `npm run build`, which wrote to `packages/ui/dist` only (see section 4).

**Bottom line:** the UI is a pure same-origin SPA. It talks only to `/api/v1/*` with JSON and a `Bearer` JWT, and it has no SSE or websockets (it polls). It calls **64 distinct method+path endpoints**. Another 5 helpers sit in `api.ts` but nothing calls them, so 69 are defined. Every one of the 64 exists in today's Node API (`packages/api/src`). A Rust server that copies their paths, status codes and JSON shapes will run the UI unchanged.

The UI's own mirror of every wire shape is `src/types.ts` plus the interfaces in `src/api.ts`. **Treat those two files as the JSON contract.** Section 1 lists the subset of fields the UI actually reads.

---

## 0. Global wire rules the Rust server MUST honour

| Rule | Why (UI code) |
|---|---|
| All UI calls go to `/api/v1<path>`. They are relative and same-origin. | `api.ts` `request()` does `fetch('/api/v1'+path)` |
| Every request carries `Content-Type: application/json`, GETs included, and `Authorization: Bearer <jwt>` once a token exists. | `request()` |
| **Every 2xx response must have a JSON body.** No `204 No Content` and no empty body. DELETEs return e.g. `{"deleted":true}`. Action triggers return `{"job_run_id":"…"}`. | `request()` always does `await res.json()` on success, so an empty body throws. |
| Error bodies are JSON with a `message` and/or `error` string. The UI shows `body.message ?? body.error ?? "Request failed (N)"`. Validation messages are shown verbatim, for example the cron/timezone errors in SchedulesPanel. | `request()`, SchedulesPanel |
| `401` on any path except `/auth/login` and `/auth/register` makes the UI clear its token and do a hard redirect to `/login`. Login/register failures should return 401 or 400 with a `message`, such as "Invalid credentials". | `request()` |
| `403` is the "admin only" signal for the Models panel, which shows "Model settings are restricted to admins." | `ModelsPanel.tsx` |
| Action endpoints get a POST body of `{}` (or `{topology}`). The server must accept an empty object. | crawl/plan/probe/run calls |
| `PATCH /sites/:id {settings}` must **shallow-merge** into the stored settings, like Node's `settings || $2::jsonb`. The UI sends partial objects: `{insert_modules, preset}` from Tools and the 4 limits + `silo_topology` + `skip_inject_verify` from Settings → Linking Rules. Replacing the whole object would wipe the other keys. | `Tools.tsx`, `LinkRulesPanel.tsx` |
| IDs are opaque strings (UUIDs today). Timestamps are ISO-8601 strings or `null`. Counts are JSON numbers, not strings. Postgres `count()` gave strings until Node cast them with `::int`. | `types.ts` |
| CSV endpoints: the UI fetches them as a Blob with the Bearer header and names the file itself. `Content-Disposition` is ignored but harmless. | `downloadHistoryCsv`, `downloadEdgesCsv`, `downloadLinkHealthCsv` |
| CSV import sends the **raw CSV text** with `Content-Type: text/csv`. It is not multipart. | `importCsv` |

---

## 1. Every API call, grouped by screen

Notation: `:site` = site id. "Reads" = response fields the UI actually touches. Fields listed under "Unread" can be omitted, but sending them does no harm.

### 1.1 Auth / shell (all screens)
| # | Method + path | Request | Response the UI reads | Used by |
|---|---|---|---|---|
| 1 | `POST /auth/login` | `{email, password}` | `{token, user:{id,email,name,role}}`. It reads `token`, `user.email`, `user.role` (`'admin'|'user'`). | Login |
| 2 | `POST /auth/register` | `{email, password, name?}` | same as login | Register |
| 3 | `GET /auth/me` | — | `AuthUser`. Only `email` and `role` are read (Topbar, Settings → Account, admin gating). | `user.tsx` on boot |
| 4 | `GET /sites` | — | `SiteListItem[]`: `id, domain, mode, last_sync_at`, a post count (`posts_count`, or `post_count`/`posts`), and edge counts by status (`edge_counts` or `edges_by_status`, shaped `{suggested:n,…}`). Node sends `posts_count` and `edge_counts`. | `sites.tsx` provider (Topbar site select, Sites page, Settings site pickers) |

### 1.2 Sites page (`/sites`)
| # | Call | Request | Reads |
|---|---|---|---|
| 5 | `POST /sites` | `{domain, wp_url?, mode?: 'wp_rest'|'sitemap'}` | `Site`. It reads `domain` and **`api_key` (plaintext, shown once)**. |
| 6 | `POST /sites/:site/crawl` | `{}` | `{job_run_id}` |
| 7 | `POST /sites/:site/plan` | `{}` or `{topology}`. Topology is one of `reverse|serial|priority|circle|bidirectional`. | `{job_run_id}` |
| — | then polls #13 (job tracker, below) | | |

### 1.3 Dashboard (`/site/:site/dashboard`)
| # | Call | Reads |
|---|---|---|
| 8 | `GET /dashboard/:site` | `site.domain, health_score` (0..1 **or** 0..100, which the UI tolerates), `posts, orphans, dead_ends, avg_links_per_post` (number; it calls `.toFixed`), `clusters, edges_by_status{status:n}, opportunities, http_links, outbound_domains, link_duplication`, `cannibalization_warnings[]` (string or object with any of `keyword|anchor|message|posts[]|urls[]|target_a_url|target_a_title|target_b_url|target_b_title`), `recent_jobs[]` (`id,kind,status,started_at,created_at,finished_at`), `orphan_posts[]` (`post_id,url,title`). **All fields are required.** Missing arrays crash the page because it calls `.length` and `.map` on them. |
| 9 | `GET /reports/link-health/:site?limit=1` | `summary.broken, nofollow, chains_over_two`. A failure here is tolerated (the UI uses `allSettled`). |
| 10 | `GET /reports/metrics/:site?trend=1` | `latest.metrics.{broken_links,nofollow_links,redirect_chains_over_2,contextual_orphans,nav_only_orphans}`. A failure is tolerated. |
| 6, 7 | crawl / plan (with topology) | `{job_run_id}` |

### 1.4 Tools (`/site/:site/tools?tab=inbound|outbound|silo|archive|bulk|anchors|keyword_linker|ignore&bulk=1`)
| # | Call | Request / query | Reads |
|---|---|---|---|
| 11 | `GET /edges/:site` | `status=suggested` (or `approved` on the bulk tab), `limit=500`, `tool=archive` on the archive tab | `{edges: EdgeView[], total}`. EdgeView = LinkEdge + `source_url, source_title, target_url, target_title`. It reads `id, source_post_id, target_post_id, anchor, anchor_type, composite_score (0..1), score_breakdown{semantic,orphan,depth,tier,cluster,velocity,graph,money,gsc,authority,placement}` (or `{}`), `silo_topology` (null means non-silo), `insert_module, status, error, created_at, decided_at, applied_at`. Unread: `paragraph_id, origin, snapshot_id, score_breakdown.weights`. |
| 12 | `GET /sites/:site` | — | `Site.settings` (`preset, insert_modules, …`) |
| 14 | `PATCH /sites/:site` | `{settings:{insert_modules, preset}}` | `Site` (the return value is ignored) |
| 15 | `POST /bulk/run` | `{site_id, edge_ids?}` | `{job_run_id, count}` |
| 16 | `POST /bulk/undo` | `{site_id, edge_ids?}` | `{job_run_id, count}` |
| 17 | `POST /sites/:site/archive-plan` | `{}` or `{topology}` | `{job_run_id}` |
| 18 | `POST /edges/:edge/decide` | `{decision:'approved'|'rejected'}` | `LinkEdge` (ignored) |
| 19 | `PATCH /edges/:edge/anchor` | `{anchor, anchor_type?}` | `LinkEdge` (ignored) |
| 20 | `POST /bulk/decide` | `{edge_ids:[], decision}` | `{updated}` |
| *Keyword linker tab* | | | |
| 21 | `GET /rules/keywords/:site` | — | `KeywordRule[]`: `id, keyword, target_url, anchor_type, max_links, active, created_at` |
| 22 | `GET /reports/links/:site` | — | the target-URL picker reads `url, title` |
| 23 | `POST /rules/keywords/:site` | `{keyword, target_url, anchor_type?, max_links?}` | `KeywordRule` |
| 24 | `PATCH /rules/keywords/:ruleId` | `{active?, max_links?, anchor_type?}`. The UI only sends `{active}`. | `KeywordRule` |
| 25 | `DELETE /rules/keywords/:ruleId` | — | `{deleted}` |
| 26 | `POST /rules/keywords/:site/run` | `{}` | `{job_run_id}` |
| *Ignore tab* | | | |
| 27 | `GET /rules/ignore/:site` | — | `IgnoreKeyword[]`: `id, phrase, created_at` |
| 28 | `POST /rules/ignore/:site` | `{phrase}` | `IgnoreKeyword` |
| 29 | `DELETE /rules/ignore/:phraseId` | — | `{deleted}` |
| *Silo tab — Custom clusters* | | | |
| 30 | `GET /silo/:site/clusters` | — | `ClusterInfo[]`: `id, label, source('auto'|'custom'), pillar_url, members` |
| 31 | `GET /silo/:site/clusters/export` | — | `{clusters:[{label, pillar_url, member_urls[]}]}`. The UI downloads it as JSON. |
| 32 | `POST /silo/:site/clusters/import` | the same `ClusterExport` JSON | `{clusters, members, skipped}` |
| *Bulk Wizard (modal, `?bulk=1`)* | | | |
| 33 | `GET /posts/:site` | `limit=500`, plus optional `search`, `post_type`, `cluster_id`, `is_orphan=1|0`. `offset` is never sent. | `{posts[], total, post_types[], clusters[{id,label,count}]}`. Each post: `id, url, title, post_type, cluster_id, cluster_label, is_orphan, suggested_out`. Unread: `is_dead_end, suggested_in`. |
| 11 | edges `status=suggested&limit=500` | filtered client-side by `source_post_id` | |
| 7 | plan (optional topology) | | |
| 20 / 15 | bulk decide / bulk run `{site_id, edge_ids}` | | |
| 13 | jobs poll **every 3 s** | reads `stats.progress{phase:string,pct:number}` and, for inject, `stats.applied:number` | |

### 1.5 Reports (`/site/:site/reports`)
| # | Call | Reads |
|---|---|---|
| 22 | `GET /reports/links/:site` | `LinkReportRow[]`: `post_id, url, title, inbound, outbound, pagerank, click_depth, link_flow_score, duplicate_count`. `?duplicates=1` exists in `api.ts` but no caller passes it. |
| 34 | `GET /reports/graph/:site` | No `max_nodes`/`max_edges` are sent, so the server defaults apply. Reads `nodes[{id,url,title,cluster_id,is_orphan,pagerank}]`, `edges[{source,target,status}]`, `truncated, total_nodes, total_edges`. Unread: `node_limit, edge_limit`. |
| 11 | `GET /edges/:site?status=suggested&limit=2000` | **broken today**, see §6 |
| 20 | `POST /bulk/decide` | `{updated}` |

### 1.6 Link Health (`/site/:site/link-health`)
| # | Call | Reads |
|---|---|---|
| 9 | `GET /reports/link-health/:site?limit=250` | `summary{total_links,internal,external,broken,redirected,chains_over_two,loops,nofollow,sponsored,ugc,last_checked_at}`. Then `broken[]`, `redirects[]` and `nofollow[]` rows: `id, source_post_id, source_url, source_title, href, requested_url, resolved_url, scope, status_code, redirect_hops, redirect_chain` (JSON array of `{url,status,order}`, read defensively), `has_redirect_loop, has_temp_redirect, rel, is_nofollow, is_sponsored, is_ugc, anchor, placement, error`. Then `breadcrumbs{posts,checked,with_visible,with_schema,mismatched,missing[{post_id,url,title,has_breadcrumbs,has_breadcrumb_schema}]}`. Unread: `summary.temp_redirects`, `row.is_broken`, `row.checked_at`, `offset`. |
| 35 | `GET /reports/link-health/:site?format=csv` | Blob (`text/csv`) |
| 36 | `POST /sites/:site/link-health` | `{}` → `{job_run_id}` |

### 1.7 History (`/site/:site/history`)
| # | Call | Reads |
|---|---|---|
| 37 | `GET /history/:site?limit=50&offset=N` | `{rows[], total, limit}`. Rows: `edge_id, source_url, source_title, target_url, target_title, anchor, tool, status, applied_at, decided_at`. Unread: `anchor_type`, `offset`. |
| 38 | `GET /history/:site?format=csv` | Blob |

### 1.8 Integrations (`/site/:site/integrations`)
| # | Call | Reads |
|---|---|---|
| 39 | `POST /csv/import/:site`, body raw `text/csv` (`url,title,content_html`) | `{received, upserted, skipped}` |
| 40 | `GET /csv/export/:site` | Blob. Columns: `source_url,target_url,anchor,anchor_type,status,insert_module`. |

### 1.9 Jobs (`/site/:site/jobs?tab=runs|schedules`)
| # | Call | Request | Reads |
|---|---|---|---|
| 13 | `GET /jobs/:site` | — | `JobRun[]`, newest first. Node caps it at 25. Reads `id, kind, status('queued'|'running'|'done'|'failed'), stats` (object; scalar values are rendered as `k: v · …`), `error, created_at, started_at, finished_at`. |
| 41 | `GET /schedules/:site` | — | `SiteSchedule[]`: `id, job_kind('crawl'|'plan'|'link_health'|'audit'), cron, tz, active, payload, last_run_at, next_run_at, created_at`. Unread: `last_job_run_id, updated_at`. |
| 42 | `POST /schedules/:site` | `{job_kind, cron, tz, active, payload:{topology?}}`. This is an **upsert per job_kind**; toggling active re-posts the whole row. | `SavedSchedule`. Reads optional `warning` (toast). `scheduled` is unread. |
| 43 | `DELETE /schedules/:scheduleId` | — | `{deleted}` |
| 44 | `POST /schedules/:site/run-now` | `{topology?}` (`skip_crawl` exists in the type but is never sent) | `{job_run_id}` |
| 45 | `GET /reports/metrics/:site?trend=30` | — | `{latest: {metrics, captured_at, source…}|null, deltas[{key,current,previous,delta,improved,pct}], trend[{captured_at, metrics{…}}]}`. **`improved` is authoritative**, so the UI never derives it from the sign. |

### 1.10 Settings (`/settings?section=general|account|cms|linking|gsc|models|plans|activity|kb`)
| # | Call | Request | Reads |
|---|---|---|---|
| 46 | `POST /auth/change-password` | `{current_password, new_password}` (the UI enforces at least 8 characters) | ignored (`{ok}`) |
| 12/14 | Linking Rules: `GET /sites/:site`, `PATCH /sites/:site {settings:{min_links_per_post,max_links_per_post,max_new_links_per_pass,link_overload_threshold,silo_topology?,skip_inject_verify}}` | | `settings.*` |
| 47 | `GET /sites/:site/gsc` | — | `{connected, client_email, saved_at}` |
| 48 | `POST /sites/:site/gsc` | `{credentials_json: "<service-account JSON string>"}` | same as #47 |
| 49 | `DELETE /sites/:site/gsc` | — | `{connected}` (ignored) |
| 50 | `GET /usage` | — | `{sites, posts, paragraphs, suggested, approved, applied, clusters}` |
| 13, 37 | Activity: jobs (polled) + `GET /history/:site?limit=50&offset=0` (first 10 rows shown) | | |
| *Models (admin only; all of these return 403 for non-admins)* | | | |
| 51 | `GET /providers/health` | — | `{providers:[ModelProvider + models: ModelRow[]], index_dim}`. ModelProvider fields: `id,name,label,kind,base_url,api_key_masked,has_api_key,enabled,last_probe_at,last_probe_ok,last_probe_error,last_probe_ms,created_at`. Unread: `headers, updated_at`. ModelRow fields: `id,provider_id,model_id,label,role('chat'|'embed'),context_tokens,dim,enabled,price_in,price_out,notes,last_probe_*`. |
| 52 | `POST /providers` | `{name,label?,kind,base_url,api_key?,enabled?}`, where kind is one of `openai-compatible|anthropic|openrouter|ollama|custom` | `ModelProvider` |
| 53 | `PATCH /providers/:id` | partial. **Omitting `api_key` keeps the stored key; `""` clears it.** | `ModelProvider` |
| 54 | `DELETE /providers/:id` | — | `{deleted}` |
| 55 | `POST /providers/:id/probe` | `{}` | `ProbeResult{ok, latency_ms, status_code, error, dim?, sample?, index_dim?}` |
| 56 | `POST /models` | `{provider_id, model_id, label?, role, context_tokens?, dim?, enabled?, price_in?, price_out?, notes?}` | `ModelRow` |
| 57 | `PATCH /models/:id` | the same minus `provider_id`/`role` | `ModelRow` |
| 58 | `DELETE /models/:id` | — | `{deleted}` |
| 59 | `POST /models/:id/probe` | `{}` | `ProbeResult` |
| 60 | `GET /model-chains` | no query sent | `ModelChainWithRungs[]`: `id,name,role,site_id,active,notes,created_at, rungs[{id, model_id (models row id), position, enabled, wire_model_id, provider_name, model_role, dim}]` |
| 61 | `POST /model-chains` | `{name, role}` | `ModelChain` |
| 62 | `DELETE /model-chains/:id` | — | `{deleted}` |
| 63 | `POST /model-chains/:id/activate` | `{}` | `ModelChain` |
| 64 | `POST /model-chains/:id/deactivate` | `{}` | `ModelChain` |
| 65 | `POST /model-chains/:id/rungs` | `{model_row_id}` | `ModelChainRung` |
| 66 | `POST /model-chains/:id/rungs/reorder` | `{rung_ids:[…all, in order]}` | `ModelChainRung[]` |
| 67 | `DELETE /model-chain-rungs/:rungId` | — | `{deleted}` |

(The row numbers above count *uses*. Rows #9/#35 and #37/#38 are the same method+path with a different query string. Distinct method+path endpoints come to **64**, broken down below.)

### 1.11 Distinct endpoint inventory (method + path template)

auth 4 · sites 7 (`GET/POST /sites`, `GET/PATCH /sites/:id`, `POST …/crawl`, `…/plan`, `…/archive-plan`) · gsc 3 · usage 1 · dashboard 1 · reports 4 (`links`, `graph`, `link-health`, `metrics`) · `POST /sites/:id/link-health` 1 · history 1 (JSON + `?format=csv`) · jobs 1 · edges 3 (`GET /edges/:site`, `POST …/decide`, `PATCH …/anchor`) · posts 1 · bulk 3 · rules 8 · silo clusters 3 · csv 2 · schedules 4 · providers 5 (`/health`, POST, PATCH, DELETE, probe) · models 4 · model-chains 7 (GET, POST, DELETE, activate, deactivate, rungs POST, rungs/reorder) · rung DELETE 1
= **64**.

Defined in `api.ts` but never called (dead helpers): `GET /reports/anchors/:site`, `GET /providers`, `GET /models`, `PATCH /model-chains/:id`, `PUT /model-chains/:id/rungs`. The Rust server can skip these for the UI, although external clients may want them.

Node routes the UI does **not** use: `/auth/handshake`, `/suggestions/:site` (+ `/ack`, `/revert-ack`), `/sync`, `GET /providers/:id`, `PATCH /model-chain-rungs/:id`, `GET /model-chains/:id/rungs`, `/health`. The WordPress plugin uses handshake, suggestions and sync, so Rust still needs them, but not for the UI.

### 1.12 Polling / realtime
No `EventSource`, no `WebSocket`. Everything is `setInterval`/`setTimeout` polling of `GET /jobs/:site`:

| Where | Interval | Stop condition |
|---|---|---|
| `hooks.ts useJobTracker` (Sites, Dashboard, Tools, LinkHealth, Rules, Schedules run-now) | 4 s | the tracked `job_run_id` reaches `done`/`failed`. **No timeout**: if the id never shows up in the list, it polls until unmount. |
| `Jobs.tsx` Runs tab | 5 s | nothing is `queued`/`running`. On error it retries every 5 s forever. |
| `Settings.tsx` Activity | 4 s | same as Jobs |
| `BulkWizard.tsx` | 3 s | the job is done/failed. Reads `stats.progress` and `stats.applied`. |
| `toast.tsx` | 5 s | UI-only auto-dismiss |

So the Rust job runner must (a) keep `GET /jobs/:site` cheap, since it is hit every 3-5 s, (b) return the newest first and include a just-enqueued run immediately, so the row must exist before `{job_run_id}` is returned, and (c) write `stats.progress = {phase, pct}` while plan/inject run and `stats.applied` when inject finishes.

---

## 2. Auth handling in the UI

- **Storage:** JWT in `localStorage['sf_token']`. Other localStorage keys are UI-only: `sf_active_site`, `sf_help_hints`, and the BulkGuidance safety-dismiss key.
- **Header:** `Authorization: Bearer <token>` on every `request()` and on the CSV/Blob helpers.
- **Login/Register:** the response's `{token, user}` → `setToken`, `setUser`, navigate to `/sites`.
- **Boot:** `UserProvider` calls `GET /auth/me` if a token exists. `RequireAuth` only checks that a token is *present*; it never checks expiry client-side.
- **Refresh:** none. The Node API issues 30-day JWTs (`expiresIn: '30d'`) with payload `{role, user_id}`. When it expires, the next call gets a 401, the token is cleared, and the page hard-redirects (`window.location.assign('/login')`).
- **Logout:** client-only (`clearToken`). There is no server endpoint, so Rust needs no revoke route unless you want one.
- **Roles:** `user.role === 'admin'` controls whether Settings → Models appears. The server must still enforce it (403).
- **Gaps:** the Blob helpers (CSV downloads, `importCsv`) do *not* do the 401→/login redirect; they just show "Download failed (401)". The token sits in localStorage, so any XSS can read it. This is acceptable for a LAN tool.
- **Rust implication:** keep the JWT bearer scheme. `role` and `user_id` claims are optional; the UI never decodes the token. Lifetime can be anything; there is no refresh flow to implement.

---

## 3. How the UI is built and served — what Rust must do

**Today:** Docker 2-stage build (`node:22-slim` → `vite build`, then `nginx:1.27-alpine`). The nginx config:
- `location /api/` → `proxy_pass http://api:8600/api/` (300 s read timeout)
- `location /health-api` → `http://api:8600/health` (**the UI never calls this**)
- `location /` → `try_files $uri $uri/ /index.html` (SPA fallback)
- No cache headers are configured.

Dev: the Vite server runs on :5173 and proxies `/api` to `http://localhost:8600`.

**Build facts:** `base` is the default `/`, so asset URLs are absolute (`/assets/index-<hash>.js`). `outDir` is `dist`. Routes use `BrowserRouter`, i.e. history API. The SPA hardcodes `/login` in the 401 redirect. **The UI must therefore be served at the domain root.** Mounting it under a sub-path needs `base` in `vite.config.ts`, a `basename` on the router, and the hardcoded `/login` fixed.

**Rust static serving (e.g. axum + `tower_http::services::ServeDir` with `.fallback(ServeFile::new("dist/index.html"))`, or rust-embed):**
1. Mount `/api/v1/*` and `/health` first. **Unknown `/api/*` paths must return JSON 404, never `index.html`.** Otherwise the UI's `res.json()` gets HTML and shows a confusing error.
2. `/assets/*` holds content-hashed files. Send `Cache-Control: public, max-age=31536000, immutable`.
3. `index.html` (and the fallback) → `Cache-Control: no-cache`, so new deploys load at once.
4. SPA fallback: any other `GET` that doesn't match a file → `dist/index.html` with status 200. Client routes are `/`, `/login`, `/register`, `/sites`, `/settings`, `/site/:id/{dashboard,tools,reports,link-health,history,integrations,jobs}`; anything unknown redirects client-side to `/sites`.
5. MIME types: `.js` → `text/javascript`, `.css`, `.html`. Gzip/br compression is optional (`tower_http::compression`). The largest chunks are Reports at 155 kB raw / 51 kB gz and index at 189 kB / 62 kB.
6. Same origin means **no CORS is needed**. If the Vite dev server is used against Rust, keep the dev proxy on `localhost:8600` or change the target in `vite.config.ts`. That is the only file to touch.
7. The request body limit must allow CSV imports and cluster JSON imports. Node's limit is the Fastify default of 1 MB unless raised; pick at least 10 MB for CSV.
8. Long-running calls: nginx allowed 300 s. All heavy work is queued (it returns `job_run_id`), so normal timeouts are fine. The exception is probe calls, which wait on upstream LLMs, so allow 30-60 s there.

---

## 4. Does the build work today?

- `node_modules` exists (root workspace + `packages/ui`). `vite.config.ts` has `outDir: 'dist'` relative to `packages/ui`, so the build writes only to `packages/ui/dist`.
- `npm run build -w packages/ui` → **succeeded in 4.5 s**: 239 modules, 22 output files. The output hashes are identical to the `dist/` already there (dated Aug 13), so the committed `dist` is current with `src`.
- `npm run typecheck -w packages/ui` (`tsc --noEmit`, writes nothing) → **clean, 0 errors**.
- So the Rust server can serve the existing `dist/` as-is or embed it at compile time.

---

## 5. UI gaps for the planned features

Code-style conventions to follow: pages live in `src/pages/<Name>.tsx` and export `function <Name>Page()`, lazy-loaded in `App.tsx`. Settings sub-panels are components in `src/components/<Name>Panel.tsx`, registered in the `SECTIONS` array in `Settings.tsx` (with `adminOnly: true` where needed) plus one `{section === 'x' && …}` line. API helpers go in `api.ts` as `fetchX/createX/patchX/deleteX`, and wire types go in `types.ts`. Use the existing `Skeleton`/`EmptyState`, `StatCard`, `StatusChip`, `Modal`, `Toggle`, `useToast`, and `data-hint` tooltips. Polling follows the `setTimeout`-tick pattern from `Jobs.tsx`.

| Feature | Exists? | Minimal additions (proposed names) | New endpoints |
|---|---|---|---|
| **System / resource monitor** (CPU, RAM, jobs) | **No.** Only the per-site Jobs page and Settings → Activity (per site). Nothing is global or system-level. | `src/pages/System.tsx` (`SystemPage`), route `/system` (global, in Layout). A Sidebar NavLink "System" next to Sites/Settings, shown to admins. `src/components/ResourceGauges.tsx` (CPU %, RAM used/total, disk, uptime, version) built from `StatCard`. A global jobs table that reuses the Jobs RunsTab markup, ideally extracted into `src/components/JobRunsTable.tsx`. Poll every 5 s. Types `SystemStats`, `QueueStats` in `types.ts`. | `GET /system/stats` → `{cpu_pct, mem_used_bytes, mem_total_bytes, disk_free_bytes?, uptime_s, version, queue:{queued,running,failed_24h}, workers?:[…]}`; `GET /jobs?status=&kind=&limit=` (all sites, includes `site_id`, plus `site_domain` for display). |
| **Task logs viewer** | **No.** `JobRun` only has `error` + `stats`. | `src/components/JobLogDrawer.tsx` (a `Modal`), opened by clicking a row in Jobs → Runs (and in the System jobs table). Tail by polling every 2 s while the job is running. | `GET /jobs/run/:jobId` → `JobRun` (this also fixes the 25-row tracker limit, §6); `GET /jobs/run/:jobId/logs?after=<seq>` → `{lines:[{seq,ts,level,msg}], done:boolean}`. Keep it polling to match the UI. SSE is optional later. |
| **Notifications settings** | **No.** | `src/components/NotificationsPanel.tsx`, Settings section `notifications` ("Notifications"). Each channel is a row with `Toggle` + target + event checkboxes, plus a "Send test" button. | `GET /settings/notifications` → `{channels:[{id,kind:'webhook'|'telegram'|'email',target,enabled,events:['job_failed','job_done','broken_links_found','schedule_missed']}]}`; `PUT /settings/notifications` (same body); `POST /settings/notifications/test {channel_id}` → `{ok,error}`. |
| **Backup / restore** | **No.** Only cluster JSON export/import and the edges/history/link-health CSVs. | `src/components/BackupPanel.tsx`, Settings section `backup`, `adminOnly`. It holds a list table, "Create backup", Download (Blob helper like `downloadEdgesCsv`), and Restore (file input like Integrations → raw body upload) behind a `Modal` confirmation. | `GET /backups` → `[{id,created_at,size_bytes,kind:'manual'|'scheduled',note}]`; `POST /backups {note?}` → `{job_run_id}` or the row; `GET /backups/:id/download` (binary); `POST /backups/restore` (raw body `application/octet-stream`) → `{job_run_id}`; `DELETE /backups/:id` → `{deleted}`. |
| **AI model management** | **Yes, complete.** Settings → **Models** (admin): `ModelsPanel.tsx` (providers + models + probe) and `ChainsPanel.tsx` (per-role fallback chains, reorder, activate). It uses 17 endpoints (#51-#67). There is no page literally titled "Model Providers"; it is this panel. | Nothing is needed for parity. Optional: rung timeout/retries editing (the type has `timeout_ms`/`max_retries` but the UI never sends them), a site-scoped chain picker (`createChain` accepts `site_id`, the UI never sends it), and per-model usage/cost (`price_in/out` are stored but there is no usage view). | Optional: `GET /models/usage?since=` → `[{model_row_id, calls, tokens_in, tokens_out, cost}]`. |
| **API keys / tokens for external control (Hermes)** | **No.** The only "API key" is the per-site WordPress plugin key, shown once in Sites → Add Site. | `src/components/ApiTokensPanel.tsx`, Settings section `tokens` ("API Tokens"), `adminOnly`. A table of name, scopes, created, last used and expiry, with a Revoke button. The create `Modal` shows the plaintext once, reusing the `apikey-box` + Copy pattern from `Sites.tsx`. The backend must accept these tokens in the same `Authorization: Bearer` header. | `GET /tokens` → `[{id,name,prefix,scopes[],created_at,last_used_at,expires_at}]`; `POST /tokens {name, scopes:['read','write','admin'], expires_in_days?}` → `{token, …row}`; `DELETE /tokens/:id` → `{deleted}`. |
| **MCP info** | **No.** | `src/components/McpInfoPanel.tsx`, Settings section `mcp` ("MCP / Agents"). Read-only: endpoint URL, transport, auth note ("use an API token"), a copyable client-config JSON snippet, and the tool list. It can sit next to ApiTokensPanel. | `GET /mcp/info` → `{enabled, transport:'streamable-http'|'sse'|'stdio', url, tools:[{name, description}]}`. If the endpoint 404s, the panel can render static text. |

Sidebar: only System needs a new top-level nav entry; everything else lives under Settings. The KB (`Knowledgebase.tsx`, static content) could get a "System & integrations" entry later.

---

## 6. UI bugs, dead code and broken calls

**Broken calls**
1. **Reports → "Send to Bulk" / "Export filtered to Bulk" are broken against today's API.** `Reports.tsx:98` requests `GET /edges/:site?status=suggested&limit=2000`, but the Node route schema caps `limit` at 1000 (`edges.ts:23`), so Fastify answers 400 and the user sees an error toast. Fix one side: either the UI uses `limit: 1000` with paging, or the Rust route accepts or clamps up to 2000. **Recommendation: the Rust side should clamp silently (`min(limit, MAX)`) instead of rejecting.** That fixes it with no UI change.
2. Dashboard "Add link" on an orphan navigates to `/site/:id/tools?tab=inbound&target=<post_id>`, but `Tools.tsx` never reads `target`, so the per-orphan pre-filter does nothing (dead param).

**Latent bugs**
3. `useJobTracker` polls forever with no timeout if the job id isn't in `GET /jobs/:site`. Node caps that list at the newest 25, so a busy site can push the tracked job out of it. Fix: a `GET /jobs/run/:id` endpoint (§5) plus the UI using it, or have Rust return the 25 newest *plus* any still-running runs.
4. `HealthDial` treats `score <= 1` as a 0..1 fraction, so a real score of 1/100 renders as 100. Rust should pick one scale. **Recommendation: always 0..100.**
5. Tools tabs fetch at most 500 suggested edges (`limit: 500`, and `EDGE_FETCH_LIMIT = 500` in BulkWizard) with no paging, so larger sites silently show a partial list. `total` is returned but only displayed.
6. The CSV/Blob helpers and `importCsv` skip the global 401 handling (no redirect to /login).
7. `Jobs.tsx`/Settings Activity retry forever every 4-5 s on persistent errors. That is harmless but noisy against a down backend.
8. `GET /sites` (Node) returns `s.*`, so the **per-site `api_key` and full settings go to every list call**. The UI only needs `api_key` from `POST /sites`. Rust should drop `api_key` from the list and from `GET /sites/:id` (the UI reads only `settings` there).

**Dead code / unused contract**
9. Unused api helpers: `fetchAnchorReport`, `fetchProviders`, `fetchModels`, `patchChain`, `putChainRungs`. The `fetchLinksReport(…, duplicates=true)` flag is never used. Unused params: `runScheduleNow.skip_crawl`, `fetchGraphReport` limits, `offset` on `fetchEdges`/`fetchPosts`.
10. Unused type: `AnchorReport` (the anchors tab computes from edges client-side). Unread fields are listed per endpoint in §1 (e.g. `LinkEdge.paragraph_id/origin/snapshot_id`, `SiteSchedule.last_job_run_id/updated_at`, `ModelProvider.headers`, `SiteSettings.money_page_urls` — there is no UI to edit money pages).
11. The nginx `/health-api` location is never used by the UI.
12. Settings → CMS has a "Shopify — Connect" button that is a disabled placeholder (roadmap).
13. `types.ts` tolerates three spellings of the site post count and two of the edge counts. Rust should emit **`posts_count` and `edge_counts`** (what Node sends).

**Method note:** I found "unread fields" by searching all non-type source files for each `types.ts`/`api.ts` interface field name. Common names (`id`, `name`, `status`) always match, so treat this list as "definitely unused", not "everything unused".
