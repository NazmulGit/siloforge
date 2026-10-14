# v1 vs v2 contract diff on real migrated data

Date: 2026-10-13. v1 = the live stack (`http://127.0.0.1:8600`, Node/Fastify + Postgres + Redis + Qdrant, 7 containers).
v2 = `siloforge serve --home D:\tmp\sf-test-home` on `127.0.0.1:8610` (a migrated copy of all v1 data: 83 sites,
85k link edges, 46k link-health rows on the biggest site). One v1 JWT (from `POST /api/v1/auth/login` on v1) was used
on both servers; the two homes share `JWT_SECRET`.

Harness: `D:\tmp\sf-contract\diff.py` (Python stdlib, `python -I diff.py [--time 5] --label X`), helper `common.py`,
ordering check `verify_order.py`, tables `report.py`. Against v1 it only sends GETs plus `POST /auth/login`;
credentials are read from the test home's `secrets.env` and never printed, and secret-looking values (api_key,
passwords, tokens, credentials) are redacted in its output.

## What is compared

- Sites: **big** site21.example (2,621 posts, ~14.7k edges, 46k link-health rows), **medium** site55.example
  (152 posts), **tiny** site23.example (4 posts), **empty** site65.example (0 posts).
- Endpoints: every GET the UI (05-ui-audit) and the MCP tools (04-integrations section 3) call, plus the plugin's
  `GET /suggestions/:site`, the admin model endpoints and `/health`: sites list/get, gsc status, dashboard, edges
  (status / tool / limit / offset variants, incl. the MCP default `limit=200` and the UI's `limit=2000`), posts
  (limit/offset/search/post_type/cluster_id/is_orphan), reports links / graph (default + small limits) / anchors /
  link-health (limit 1/250, offset, CSV) / metrics (trend 1/30/default), history (JSON pages + CSV), jobs, silo
  clusters + export, keyword/ignore rules, schedules, csv export, usage, auth/me, providers (+health, +:id), models
  (+role, +provider filter), model-chains (+role, +site_id=global) and rungs; negative cases (no token -> 401,
  unknown site -> 404, non-uuid -> 404, unknown route -> JSON 404).
- Per request: status code, JSON type, recursive key sets (missing in v2 / extra in v2 / type changes), list
  lengths and values. Lists of objects are matched by `id` / `edge_id` / `post_id`; ordering is checked against
  v1's `ORDER BY` keys, so a different order is accepted only when the sort-key sequences are identical (a tie).
  Numbers compare with 1e-6 relative tolerance; timestamps compare as instants. CSVs compare header, row count,
  row set (timestamps normalised) and order.

## Mismatches found and fixed

First run: 49 of 187 requests had real mismatches. Apart from the intentional ones below they had one root cause.

1. **Text ordering (collation).** v1's Postgres sorted every `ORDER BY <text>` with glibc `en_US.UTF-8`
   (punctuation/spaces ignored, case- and accent-insensitive, lowercase before uppercase); SQLite used BINARY
   (`Zebra` < `apple`, `tag/bottle-art/` < `tag/bottle/`, `water bottle many` < `water bottled brands`).
   Effects on real data: `/posts` returned a different set of posts inside large `suggested_out` ties (61 of 500
   posts differed on site21 at `limit=500`; page 2 at `limit=100&offset=100` shared 0 of 100 posts with v1),
   and the order differed in `/posts` `clusters[]`, dashboard `orphan_posts` and `cannibalization_warnings` (which
   also changed *which* 20 warnings are shown), `/reports/links` pagerank ties, `/silo/:site/clusters`,
   `/silo/:site/clusters/export` (cluster order and `member_urls` order), every link-health list, breadcrumbs
   `missing[]` and the link-health CSV.
   Fix: `crates/sf-server/src/v1/common.rs` `v1_collate` (allocation-free approximation of the glibc order:
   level 1 alnum folded + lowercased, level 2 accents, level 3 lowercase first, level 4 bytes), registered per read as
   SQLite collation `V1` via `v1_collation(c)`, and `COLLATE V1` on those ORDER BYs in `posts.rs`, `dashboard.rs`,
   `reports.rs`, `silo.rs`, `linkhealth.rs`. Needs the rusqlite `collation` feature (added in
   `crates/sf-server/Cargo.toml`). Checked against real v1 output: 0 order violations over 2,621 links-report rows,
   2,596 cluster member URLs and all post / cluster / orphan lists. Cost: about +10-30 ms on the 2.6k-row sorts
   (measured by swapping in a BINARY comparator). Regression tests: unit
   `v1::common::tests::v1_collate_matches_postgres_en_us` (pairs taken from v1 output + hostile input) and
   integration `text_order_matches_v1_collation` in `tests/v1_routes.rs` (posts order + page boundary, posts
   clusters, orphan_posts, cannibalization, links ties, silo list/export, link-health).

After the fix: **0 real mismatches in 203 requests**, on both the debug and the release build.

## Intentional differences (not fixed)

- `api_key` absent from `GET /sites`, `GET /sites/:id` and the dashboard `site` (S1; only on create/rotate).
- `settings.wp_app_password` / `settings.wp_user` / GSC credentials absent (S2/S3); v2 adds top-level
  `has_wp_credentials` and `wp_user`.
- `GET /edges/:site?...&limit=2000`: v1 400 (schema max 1000), v2 200 with the limit clamped (fixes the UI's
  Reports "Send to Bulk", UI audit 6.1).
- `GET /posts/:site?is_orphan=1|0`: v1 400 `querystring/is_orphan must be boolean`, but the UI sends exactly `1`/`0`
  (`ui/src/api.ts:291`), so the Bulk Wizard's orphans-only filter is broken on v1; v2 accepts `1/0/true/false`.
  `is_orphan=true/false` returns identical rows on both.
- History CSV `Applied At`: v1 writes JS `Date.toString()` (`Tue Jul 28 2026 09:48:16 GMT+0000 (Coordinated
  Universal Time)`), v2 ISO-8601 (`2026-07-28T09:48:16.876Z`). Same instants; the UI only downloads the blob.
- Extra v2 fields: `/health` (`version`, `uptime_secs`, `jobs`, `services.queue`), job rows (`attempts`,
  `max_attempts`, `priority`, `requested_by`, `heartbeat_at`, `resources`), edges (`failure_kind`, `link_start`).
- JSON timestamps: v2 returns the same `YYYY-MM-DDTHH:MM:SS.mmmZ` strings as Node; no format difference was seen.

## Remaining order differences that are ties (not bugs)

The migration stores timestamps with millisecond precision (v1 Postgres has microseconds), so rows that v1 ordered
by microseconds now tie and v2 breaks the tie by `id`. This shows up in `/edges` (composite_score, created_at),
`/history` JSON + CSV (applied_at, decided_at), `/csv/export` and `/suggestions` (created_at). The harness checks
that the v1 and v2 sort-key sequences are identical; for csv export and suggestions (keys not in the payload)
`verify_order.py` checks v1's order against the timestamps stored in v2: 0 violations (e.g. 165 adjacent ms ties in
site21 suggestions). `/reports/anchors` `byAnchor` ties on `count` (v1 has no tie-break; v2 adds `anchor`).
Link-health nofollow rows that are identical on (scope, source_url, href) differ only in other columns.

## Performance

Caveats: sequential requests from one Python client over loopback, 5 runs each; both servers on the same PC while
v1 was otherwise idle. Windows loopback shows a ~15 ms quantisation on small responses (delayed ACK), visible on both
sides as values of 14-16 ms vs 1-4 ms. The v2 `dev` profile is optimized (+debuginfo), so debug and release are close.

**Memory.** v1 containers (`docker stats --no-stream` after the timing run): api 488.6 MiB, worker-1 143.6 MiB,
worker-2 114.6 MiB, ui 19.8 MiB, postgres 1.293 GiB (includes its page cache), redis 29.9 MiB, qdrant 226.3 MiB =
**about 2.29 GiB**. v2 release process (job runner not wired yet): **24 MiB working set idle, 39 MiB after the
full diff + timing run, 200 MiB peak working set** (peak during the 46k-row link-health CSV; SQLite also maps up to
256 MiB of the DB file per connection, which is page cache, not private memory; private bytes were 17-30 MiB).
Debug build: 33 MiB after the run, 200 MiB peak.

**Where v2 is slower** (pre-existing, not caused by the fix; also measured with the collation disabled):
`/reports/link-health` on site21 (~330-360 ms vs ~40 ms: summary + six sorted lists run sequentially in one
read, each 30-50 ms over 46k rows, while v1 runs its 9 queries in parallel), `/reports/graph` (~165 ms vs ~21 ms),
`/reports/links` (~220 ms vs ~72 ms), `/posts` on the big site (~120-190 ms vs ~30-60 ms) and `/dashboard`
(~180 ms vs ~76 ms). Suggested follow-ups (performance, not contract): run the link-health lists as parallel
`Db::read`s or add partial indexes such as `(site_id, is_nofollow)` / `(site_id, redirect_hops)`, and profile the
graph/links/posts CTEs. v2 is faster on `/sites` (23 vs 161 ms), `/usage` (123 vs 311 ms), edge pages, csv
exports, medium/small sites and every small endpoint (median p50 3.2 ms vs 8.9 ms; v2 release faster on 145/203).

## Raw results

Requests compared per run: **203** (66 distinct endpoint/query templates). Real mismatches in the final run: **0**.

### Result per endpoint

Status columns are v1/v2 for big / medium / tiny / empty (global and negative rows have one site). `=` means identical after normalisation; notes are listed below the table.

| Endpoint | Statuses (v1/v2) | Verdict |
|---|---|---|
| `health` | 200/200 | = (notes) |
| `/auth/me` | 200/200 | = |
| `/sites` | 200/200 | = (intentional diff) |
| `/usage` | 200/200 | = |
| `/providers` | 200/200 | = |
| `/providers/health` | 200/200 | = |
| `/models` | 200/200 | = |
| `/models?role=chat` | 200/200 | = |
| `/models?role=embed` | 200/200 | = |
| `/model-chains` | 200/200 | = |
| `/model-chains?role=embed` | 200/200 | = |
| `/model-chains?site_id=global` | 200/200 | = |
| `!/auth/me` | 401/401 | = |
| `!/sites` | 401/401 | = |
| `!/dashboard/07ca7287-1424-4a7d-b4d7-54c87ee755c6` | 401/401 | = |
| `/sites/00000000-0000-4000-8000-000000000000` | 404/404 | = |
| `/dashboard/00000000-0000-4000-8000-000000000000` | 404/404 | = |
| `/edges/not-a-uuid` | 404/404 | = |
| `/posts/00000000-0000-4000-8000-000000000000` | 404/404 | = |
| `/no-such-route` | 404/404 | = |
| `/sites/:site` | 200/200 200/200 200/200 200/200 | = (intentional diff) |
| `/sites/:site/gsc` | 200/200 200/200 200/200 200/200 | = |
| `/dashboard/:site` | 200/200 200/200 200/200 200/200 | = (intentional diff) |
| `/edges/:site` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/edges/:site?status=suggested&limit=500` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/edges/:site?status=approved&limit=500` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/edges/:site?status=applied&limit=50&offset=20` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/edges/:site?tool=archive&limit=500` | 200/200 200/200 200/200 200/200 | = |
| `/edges/:site?tool=plan&limit=200` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/edges/:site?status=rejected` | 200/200 200/200 200/200 200/200 | = |
| `/edges/:site?status=suggested&limit=2000` | 400/200 400/200 400/200 400/200 | = (intentional diff) |
| `/edges/:site?status=bogus` | 400/400 400/400 400/400 400/400 | = |
| `/edges/:site?limit=1000&offset=1000` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/posts/:site?limit=500` | 200/200 200/200 200/200 200/200 | = |
| `/posts/:site` | 200/200 200/200 200/200 200/200 | = |
| `/posts/:site?limit=100&offset=100` | 200/200 200/200 200/200 200/200 | = |
| `/posts/:site?limit=500&is_orphan=1` | 400/200 400/200 400/200 400/200 | = (intentional diff) |
| `/posts/:site?limit=500&is_orphan=0` | 400/200 400/200 400/200 400/200 | = (intentional diff) |
| `/posts/:site?limit=500&is_orphan=true` | 200/200 200/200 200/200 200/200 | = |
| `/posts/:site?limit=500&is_orphan=false` | 200/200 200/200 200/200 200/200 | = |
| `/posts/:site?limit=500&post_type=post` | 200/200 200/200 200/200 200/200 | = |
| `/posts/:site?limit=500&search=best` | 200/200 200/200 200/200 200/200 | = |
| `/reports/links/:site` | 200/200 200/200 200/200 200/200 | = |
| `/reports/graph/:site` | 200/200 200/200 200/200 200/200 | = |
| `/reports/graph/:site?max_nodes=50&max_edges=100` | 200/200 200/200 200/200 200/200 | = |
| `/reports/anchors/:site` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/reports/link-health/:site?limit=1` | 200/200 200/200 200/200 200/200 | = |
| `/reports/link-health/:site?limit=250` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/reports/link-health/:site?limit=10&offset=5` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/reports/link-health/:site?format=csv` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/reports/metrics/:site?trend=1` | 200/200 200/200 200/200 200/200 | = |
| `/reports/metrics/:site?trend=30` | 200/200 200/200 200/200 200/200 | = |
| `/reports/metrics/:site` | 200/200 200/200 200/200 200/200 | = |
| `/history/:site` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/history/:site?limit=50&offset=0` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/history/:site?limit=50&offset=50` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/history/:site?format=csv` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/jobs/:site` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/silo/:site/clusters` | 200/200 200/200 200/200 200/200 | = |
| `/silo/:site/clusters/export` | 200/200 200/200 200/200 200/200 | = |
| `/rules/keywords/:site` | 200/200 200/200 200/200 200/200 | = |
| `/rules/ignore/:site` | 200/200 200/200 200/200 200/200 | = |
| `/schedules/:site` | 200/200 200/200 200/200 200/200 | = |
| `/csv/export/:site` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/suggestions/:site` | 200/200 200/200 200/200 200/200 | = (notes) |
| `/posts/:site?cluster_id=` | 200/200 200/200 200/200 | = |

### Intentional differences seen (final run)

- `/sites`: missing in v2: `[].api_key` -- api_key only on create/rotate in v2 (S1) (x1)
- `/sites`: missing in v2: `[].settings.wp_user` -- WP user moved to top-level wp_user (S2) (x1)
- `/sites`: missing in v2: `[].settings.wp_app_password` -- WP credentials sealed in v2 (S2) (x1)
- `/sites/:site`: missing in v2: `api_key` -- api_key only on create/rotate in v2 (S1) (x4)
- `/sites/:site`: missing in v2: `settings.wp_user` -- WP user moved to top-level wp_user (S2) (x3)
- `/sites/:site`: missing in v2: `settings.wp_app_password` -- WP credentials sealed in v2 (S2) (x3)
- `/dashboard/:site`: missing in v2: `site.api_key` -- api_key only on create/rotate in v2 (S1) (x4)
- `/dashboard/:site`: missing in v2: `site.settings.wp_user` -- WP user moved to top-level wp_user (S2) (x3)
- `/dashboard/:site`: missing in v2: `site.settings.wp_app_password` -- WP credentials sealed in v2 (S2) (x3)
- `/edges/:site?status=suggested&limit=2000`: status v1=400 v2=200 -- v2 clamps limit (max 5000) instead of 400; fixes UI Reports 'Send to Bulk' (UI audit 6.1) -- v2 clamps limit (max 5000) instead of 400; fixes UI Reports 'Send to Bulk' (UI audit 6.1) (x4)
- `/posts/:site?limit=500&is_orphan=1`: status v1=400 v2=200 -- v1 schema rejects is_orphan=1/0 although the UI sends exactly that (orphans-only filter broken in v1); v2 accepts 1/0/true/false -- v1 schema rejects is_orphan=1/0 although the UI sends exactly that (orphans-only filter broken in v1); v2 accepts 1/0/true/false (x4)
- `/posts/:site?limit=500&is_orphan=0`: status v1=400 v2=200 -- v1 schema rejects is_orphan=1/0 although the UI sends exactly that (orphans-only filter broken in v1); v2 accepts 1/0/true/false -- v1 schema rejects is_orphan=1/0 although the UI sends exactly that (orphans-only filter broken in v1); v2 accepts 1/0/true/false (x4)

### Notes (not contract bugs)

- `/csv/export/:site`: csv NOTE same rows, order differs only inside ms-timestamp ties (v1 order agrees with v2 (applied_at, decided_at); 5132 adjacent ms ties) (x2)
- `/dashboard/:site`: extra in v2: `recent_jobs[].attempts` (x4)
- `/dashboard/:site`: extra in v2: `recent_jobs[].heartbeat_at` (x4)
- `/dashboard/:site`: extra in v2: `recent_jobs[].max_attempts` (x4)
- `/dashboard/:site`: extra in v2: `recent_jobs[].priority` (x4)
- `/dashboard/:site`: extra in v2: `recent_jobs[].requested_by` (x4)
- `/dashboard/:site`: extra in v2: `recent_jobs[].resources` (x4)
- `/dashboard/:site`: extra in v2: `site.has_wp_credentials` -- v2 addition (x4)
- `/dashboard/:site`: extra in v2: `site.wp_user` -- v2 addition (x4)
- `/edges/:site`: extra in v2: `edges[].failure_kind` (x3)
- `/edges/:site`: extra in v2: `edges[].link_start` (x3)
- `/edges/:site?limit=1000&offset=1000`: extra in v2: `edges[].failure_kind` (x1)
- `/edges/:site?limit=1000&offset=1000`: extra in v2: `edges[].link_start` (x1)
- `/edges/:site?limit=1000&offset=1000`: tie order `edges`: order differs only inside ties of v1 ORDER BY ['composite_score', 'created_at'] (first at 506) (x1)
- `/edges/:site?status=applied&limit=50&offset=20`: extra in v2: `edges[].failure_kind` (x2)
- `/edges/:site?status=applied&limit=50&offset=20`: extra in v2: `edges[].link_start` (x2)
- `/edges/:site?status=applied&limit=50&offset=20`: tie order `edges`: order differs only inside ties of v1 ORDER BY ['composite_score', 'created_at'] (first at 14) (x1)
- `/edges/:site?status=approved&limit=500`: extra in v2: `edges[].failure_kind` (x3)
- `/edges/:site?status=approved&limit=500`: extra in v2: `edges[].link_start` (x3)
- `/edges/:site?status=suggested&limit=500`: extra in v2: `edges[].failure_kind` (x3)
- `/edges/:site?status=suggested&limit=500`: extra in v2: `edges[].link_start` (x3)
- `/edges/:site?tool=plan&limit=200`: extra in v2: `edges[].failure_kind` (x3)
- `/edges/:site?tool=plan&limit=200`: extra in v2: `edges[].link_start` (x3)
- `/history/:site`: tie order `rows`: order differs only inside ties of v1 ORDER BY ['applied_at', 'decided_at'] (first at 9) (x1)
- `/history/:site?format=csv`: csv NOTE same rows, order differs only inside ties of v1 ORDER BY (x2)
- `/history/:site?format=csv`: csv NOTE v1 writes JS Date.toString() timestamps, v2 ISO-8601 (compared as instants) (x2)
- `/history/:site?limit=50&offset=0`: tie order `rows`: order differs only inside ties of v1 ORDER BY ['applied_at', 'decided_at'] (first at 9) (x1)
- `/history/:site?limit=50&offset=50`: tie order `rows`: order differs only inside ties of v1 ORDER BY ['applied_at', 'decided_at'] (first at 3) (x1)
- `/jobs/:site`: extra in v2: `[].attempts` (x4)
- `/jobs/:site`: extra in v2: `[].heartbeat_at` (x4)
- `/jobs/:site`: extra in v2: `[].max_attempts` (x4)
- `/jobs/:site`: extra in v2: `[].priority` (x4)
- `/jobs/:site`: extra in v2: `[].requested_by` (x4)
- `/jobs/:site`: extra in v2: `[].resources` (x4)
- `/reports/anchors/:site`: tie order `byAnchor`: rows differ only inside ties of v1 ORDER BY ['count'] (x3)
- `/reports/link-health/:site?format=csv`: csv NOTE same rows, order differs only inside ties of v1 ORDER BY (x2)
- `/reports/link-health/:site?limit=10&offset=5`: page boundary inside ties `nofollow[]<id set>`: ('1 only in v1', '1 only in v2') (x1)
- `/reports/link-health/:site?limit=10&offset=5`: page boundary inside ties `nofollow[]<id set>`: ('2 only in v1', '2 only in v2') (x1)
- `/reports/link-health/:site?limit=10&offset=5`: tie order `nofollow`: order differs only inside ties of v1 ORDER BY ['scope', 'source_url', 'href'] (first at 0) (x2)
- `/reports/link-health/:site?limit=250`: page boundary inside ties `nofollow[]<id set>`: ('2 only in v1', '2 only in v2') (x1)
- `/reports/link-health/:site?limit=250`: tie order `nofollow`: order differs only inside ties of v1 ORDER BY ['scope', 'source_url', 'href'] (first at 0) (x2)
- `/reports/link-health/:site?limit=250`: tie order `redirects`: order differs only inside ties of v1 ORDER BY ['redirect_hops', 'source_url', 'href'] (first at 2) (x1)
- `/sites`: extra in v2: `[].has_wp_credentials` -- v2 addition (x1)
- `/sites`: extra in v2: `[].wp_user` -- v2 addition (x1)
- `/sites/:site`: extra in v2: `has_wp_credentials` -- v2 addition (x4)
- `/sites/:site`: extra in v2: `wp_user` -- v2 addition (x4)
- `/suggestions/:site`: tie order `<root>`: order differs only inside ms-timestamp ties (v1 order agrees with v2 created_at; 165 adjacent ms ties) (x1)
- `health`: extra in v2: `jobs` (x1)
- `health`: extra in v2: `services.queue` (x1)
- `health`: extra in v2: `uptime_secs` (x1)
- `health`: extra in v2: `version` (x1)

### Latency before the perf pass (ms, 5 runs each, sequential, loopback)

v1 = live Node/Fastify + Postgres (Docker). v2 debug = `dev` profile (opt-level per workspace profile, debuginfo); v2 release = `--release`. Same JWT, same queries, same data.

| Site | Endpoint | v1 p50 | v1 p95 | v2 debug p50 | v2 debug p95 | v2 release p50 | v2 release p95 |
|---|---|---|---|---|---|---|---|
| global | `health` | 8.9 | 11.5 | 1.5 | 1.6 | 1.6 | 14.5 |
| global | `/auth/me` | 3.2 | 3.3 | 1.5 | 1.7 | 1.3 | 23.4 |
| global | `/sites` | 160.7 | 535.2 | 23.9 | 60.3 | 23.4 | 47.5 |
| global | `/usage` | 310.7 | 330.2 | 117.0 | 169.7 | 123.3 | 151.5 |
| global | `/providers` | 3.8 | 13.8 | 1.6 | 20.3 | 1.1 | 15.9 |
| global | `/providers/health` | 4.0 | 26.2 | 1.3 | 17.7 | 1.7 | 24.3 |
| global | `/models` | 14.6 | 27.6 | 1.4 | 13.3 | 1.4 | 14.8 |
| global | `/models?role=chat` | 14.8 | 26.0 | 1.5 | 13.9 | 1.4 | 21.0 |
| global | `/models?role=embed` | 16.0 | 24.6 | 1.3 | 13.7 | 1.1 | 13.4 |
| global | `/model-chains` | 4.5 | 27.3 | 1.5 | 14.0 | 1.4 | 17.1 |
| global | `/model-chains?role=embed` | 15.0 | 27.7 | 13.9 | 25.4 | 1.2 | 12.6 |
| global | `/model-chains?site_id=global` | 4.5 | 27.2 | 14.1 | 22.0 | 1.6 | 18.8 |
| big | `/sites/:site` | 15.2 | 17.6 | 1.6 | 23.2 | 12.6 | 15.5 |
| big | `/sites/:site/gsc` | 3.8 | 28.4 | 1.6 | 23.4 | 1.2 | 25.1 |
| big | `/dashboard/:site` | 76.3 | 77.7 | 155.3 | 172.1 | 183.0 | 283.3 |
| big | `/edges/:site` | 52.2 | 60.9 | 48.0 | 55.1 | 49.4 | 66.9 |
| big | `/edges/:site?status=suggested&limit=500` | 55.4 | 66.2 | 40.0 | 57.2 | 47.2 | 47.7 |
| big | `/edges/:site?status=approved&limit=500` | 60.2 | 63.9 | 36.9 | 48.4 | 50.9 | 54.9 |
| big | `/edges/:site?status=applied&limit=50&offset=20` | 28.8 | 49.1 | 13.3 | 35.9 | 17.7 | 39.1 |
| big | `/edges/:site?tool=archive&limit=500` | 35.1 | 82.4 | 13.7 | 30.9 | 21.4 | 45.8 |
| big | `/edges/:site?tool=plan&limit=200` | 57.8 | 71.1 | 68.2 | 82.2 | 35.1 | 55.6 |
| big | `/edges/:site?status=rejected` | 7.4 | 20.7 | 5.8 | 22.2 | 4.7 | 26.6 |
| big | `/edges/:site?status=suggested&limit=2000` | 4.8 | 16.2 | 183.1 | 202.5 | 109.7 | 116.3 |
| big | `/edges/:site?status=bogus` | 4.2 | 4.6 | 1.8 | 2.1 | 12.8 | 15.5 |
| big | `/edges/:site?limit=1000&offset=1000` | 68.7 | 92.9 | 105.6 | 110.2 | 95.0 | 103.3 |
| big | `/posts/:site?limit=500` | 61.5 | 64.9 | 145.7 | 155.5 | 128.3 | 138.7 |
| big | `/posts/:site` | 29.8 | 51.8 | 116.7 | 136.6 | 122.4 | 129.2 |
| big | `/posts/:site?limit=100&offset=100` | 29.1 | 48.7 | 125.1 | 131.9 | 110.3 | 126.1 |
| big | `/posts/:site?limit=500&is_orphan=1` | 15.3 | 22.3 | 27.8 | 43.3 | 48.0 | 75.9 |
| big | `/posts/:site?limit=500&is_orphan=0` | 3.0 | 22.4 | 137.9 | 138.4 | 130.5 | 140.3 |
| big | `/posts/:site?limit=500&is_orphan=true` | 30.6 | 33.0 | 47.4 | 50.5 | 39.1 | 46.4 |
| big | `/posts/:site?limit=500&is_orphan=false` | 45.3 | 51.0 | 141.4 | 145.5 | 143.7 | 159.3 |
| big | `/posts/:site?limit=500&post_type=post` | 45.3 | 49.3 | 150.1 | 157.9 | 137.6 | 145.6 |
| big | `/posts/:site?limit=500&search=best` | 24.7 | 41.7 | 58.7 | 68.3 | 64.1 | 77.7 |
| big | `/reports/links/:site` | 72.0 | 115.9 | 342.8 | 427.2 | 218.6 | 226.8 |
| big | `/reports/graph/:site` | 21.4 | 23.7 | 189.1 | 304.6 | 163.8 | 183.7 |
| big | `/reports/graph/:site?max_nodes=50&max_edges=100` | 10.9 | 11.9 | 159.4 | 177.3 | 162.3 | 173.5 |
| big | `/reports/anchors/:site` | 15.1 | 31.9 | 25.4 | 34.3 | 45.4 | 53.9 |
| big | `/reports/link-health/:site?limit=1` | 38.6 | 46.2 | 454.3 | 500.0 | 331.4 | 347.2 |
| big | `/reports/link-health/:site?limit=250` | 44.4 | 64.3 | 370.1 | 374.9 | 356.9 | 391.2 |
| big | `/reports/link-health/:site?limit=10&offset=5` | 42.6 | 54.2 | 388.2 | 404.1 | 378.6 | 427.3 |
| big | `/reports/link-health/:site?format=csv` | 1124.5 | 1230.8 | 1501.2 | 1646.8 | 965.8 | 1003.1 |
| big | `/reports/metrics/:site?trend=1` | 6.0 | 32.9 | 1.8 | 27.2 | 1.3 | 26.1 |
| big | `/reports/metrics/:site?trend=30` | 6.9 | 10.7 | 2.3 | 2.6 | 1.2 | 1.5 |
| big | `/reports/metrics/:site` | 6.4 | 7.1 | 1.4 | 2.0 | 1.8 | 21.9 |
| big | `/history/:site` | 20.3 | 24.8 | 26.9 | 33.2 | 29.3 | 37.9 |
| big | `/history/:site?limit=50&offset=0` | 14.5 | 14.7 | 18.5 | 34.4 | 14.2 | 36.3 |
| big | `/history/:site?limit=50&offset=50` | 13.0 | 31.4 | 16.2 | 44.7 | 29.9 | 37.0 |
| big | `/history/:site?format=csv` | 95.3 | 104.0 | 48.4 | 66.6 | 29.7 | 50.8 |
| big | `/jobs/:site` | 20.5 | 26.6 | 1.8 | 15.2 | 1.9 | 26.5 |
| big | `/silo/:site/clusters` | 14.4 | 30.4 | 15.6 | 27.8 | 4.3 | 8.2 |
| big | `/silo/:site/clusters/export` | 43.7 | 47.4 | 47.0 | 57.5 | 30.5 | 34.8 |
| big | `/rules/keywords/:site` | 15.8 | 25.9 | 15.2 | 23.3 | 1.1 | 1.6 |
| big | `/rules/ignore/:site` | 15.0 | 26.0 | 1.4 | 23.9 | 1.1 | 25.3 |
| big | `/schedules/:site` | 26.2 | 28.1 | 2.2 | 26.8 | 1.5 | 1.6 |
| big | `/csv/export/:site` | 68.4 | 85.0 | 80.3 | 97.2 | 60.5 | 69.3 |
| big | `/suggestions/:site` | 97.6 | 115.9 | 83.2 | 93.2 | 52.8 | 71.1 |
| big | `/posts/:site?cluster_id=` | 26.2 | 30.7 | 188.3 | 204.3 | 189.0 | 193.8 |
| medium | `/sites/:site` | 4.5 | 27.2 | 14.8 | 15.6 | 15.5 | 24.3 |
| medium | `/sites/:site/gsc` | 4.3 | 26.2 | 1.6 | 16.2 | 1.5 | 23.1 |
| medium | `/dashboard/:site` | 21.6 | 37.0 | 9.5 | 13.0 | 8.9 | 27.4 |
| medium | `/edges/:site` | 38.8 | 45.2 | 10.8 | 30.7 | 6.4 | 27.1 |
| medium | `/edges/:site?status=suggested&limit=500` | 22.0 | 31.7 | 9.7 | 30.4 | 7.2 | 21.9 |
| medium | `/edges/:site?status=approved&limit=500` | 14.2 | 35.0 | 29.1 | 30.8 | 29.3 | 37.6 |
| medium | `/edges/:site?status=applied&limit=50&offset=20` | 24.9 | 30.8 | 15.0 | 28.1 | 3.0 | 28.4 |
| medium | `/edges/:site?tool=archive&limit=500` | 5.2 | 23.6 | 2.4 | 24.4 | 2.2 | 26.6 |
| medium | `/edges/:site?tool=plan&limit=200` | 10.8 | 21.4 | 10.7 | 24.5 | 8.5 | 30.5 |
| medium | `/edges/:site?status=rejected` | 15.1 | 15.6 | 15.0 | 21.0 | 15.6 | 19.8 |
| medium | `/edges/:site?status=suggested&limit=2000` | 14.6 | 27.3 | 18.3 | 30.7 | 7.0 | 28.7 |
| medium | `/edges/:site?status=bogus` | 15.7 | 27.3 | 13.8 | 16.0 | 1.4 | 1.5 |
| medium | `/edges/:site?limit=1000&offset=1000` | 7.7 | 30.4 | 4.6 | 4.9 | 15.6 | 27.8 |
| medium | `/posts/:site?limit=500` | 21.8 | 30.4 | 4.5 | 29.8 | 5.8 | 27.7 |
| medium | `/posts/:site` | 30.6 | 31.3 | 15.6 | 27.5 | 4.0 | 27.1 |
| medium | `/posts/:site?limit=100&offset=100` | 7.9 | 26.2 | 15.2 | 27.2 | 4.4 | 15.9 |
| medium | `/posts/:site?limit=500&is_orphan=1` | 2.9 | 16.5 | 4.1 | 29.0 | 16.0 | 27.9 |
| medium | `/posts/:site?limit=500&is_orphan=0` | 15.5 | 25.2 | 3.5 | 18.5 | 3.8 | 24.2 |
| medium | `/posts/:site?limit=500&is_orphan=true` | 6.7 | 27.4 | 4.3 | 27.1 | 3.7 | 28.4 |
| medium | `/posts/:site?limit=500&is_orphan=false` | 24.9 | 30.3 | 3.7 | 28.2 | 3.3 | 3.6 |
| medium | `/posts/:site?limit=500&post_type=post` | 5.9 | 25.2 | 16.1 | 28.6 | 13.2 | 24.9 |
| medium | `/posts/:site?limit=500&search=best` | 23.4 | 31.0 | 3.7 | 3.9 | 5.6 | 35.0 |
| medium | `/reports/links/:site` | 23.9 | 30.5 | 5.0 | 5.4 | 4.9 | 26.5 |
| medium | `/reports/graph/:site` | 9.0 | 22.8 | 7.6 | 31.8 | 29.3 | 32.0 |
| medium | `/reports/graph/:site?max_nodes=50&max_edges=100` | 6.6 | 30.6 | 3.4 | 24.0 | 3.2 | 3.5 |
| medium | `/reports/anchors/:site` | 5.2 | 27.4 | 3.2 | 25.0 | 16.0 | 24.0 |
| medium | `/reports/link-health/:site?limit=1` | 7.1 | 24.6 | 7.7 | 32.2 | 6.9 | 27.4 |
| medium | `/reports/link-health/:site?limit=250` | 16.8 | 37.2 | 46.2 | 46.7 | 30.8 | 40.4 |
| medium | `/reports/link-health/:site?limit=10&offset=5` | 26.8 | 30.9 | 8.8 | 31.3 | 7.5 | 24.7 |
| medium | `/reports/link-health/:site?format=csv` | 49.0 | 57.4 | 45.0 | 56.9 | 32.3 | 38.2 |
| medium | `/reports/metrics/:site?trend=1` | 14.6 | 30.1 | 14.3 | 18.7 | 1.3 | 14.8 |
| medium | `/reports/metrics/:site?trend=30` | 4.1 | 29.4 | 1.7 | 14.8 | 1.4 | 26.5 |
| medium | `/reports/metrics/:site` | 3.9 | 26.7 | 15.6 | 20.9 | 15.6 | 22.6 |
| medium | `/history/:site` | 30.2 | 31.7 | 3.4 | 20.9 | 3.0 | 16.2 |
| medium | `/history/:site?limit=50&offset=0` | 27.3 | 31.3 | 3.3 | 3.4 | 3.0 | 13.7 |
| medium | `/history/:site?limit=50&offset=50` | 6.8 | 32.2 | 3.2 | 26.9 | 3.6 | 16.5 |
| medium | `/history/:site?format=csv` | 22.6 | 31.1 | 6.6 | 28.8 | 16.2 | 27.9 |
| medium | `/jobs/:site` | 4.3 | 25.8 | 1.9 | 17.1 | 1.7 | 22.3 |
| medium | `/silo/:site/clusters` | 14.8 | 24.5 | 15.3 | 15.6 | 2.0 | 19.5 |
| medium | `/silo/:site/clusters/export` | 5.4 | 17.9 | 2.6 | 20.6 | 15.6 | 17.9 |
| medium | `/rules/keywords/:site` | 14.8 | 26.5 | 1.5 | 24.3 | 15.6 | 24.2 |
| medium | `/rules/ignore/:site` | 3.9 | 22.5 | 1.4 | 25.9 | 15.7 | 24.5 |
| medium | `/schedules/:site` | 13.2 | 27.5 | 1.4 | 13.9 | 1.6 | 26.0 |
| medium | `/csv/export/:site` | 8.4 | 30.1 | 5.2 | 25.0 | 5.0 | 25.7 |
| medium | `/suggestions/:site` | 6.2 | 26.4 | 2.2 | 28.4 | 12.4 | 25.6 |
| medium | `/posts/:site?cluster_id=` | 14.8 | 26.8 | 3.4 | 14.9 | 3.4 | 25.9 |

Tiny/empty-site and negative rows are omitted from the table (all < 15 ms on both) but included in the totals.

| Aggregate over all timed requests | v1 | v2 debug | v2 release |
|---|---|---|---|
| median of p50 | 8.9 | 2.3 | 3.2 |
| sum of p50 (one pass over every endpoint) | 4612 | 6594 | 5727 |
| requests where v2 release p50 < v1 p50 | | | 145/203 |

Largest v2-release/v1 p50 ratios (same-status requests only): `big:site21.example /reports/graph/:site?max_nodes=50&max_edges=100` x14.86; `big:site21.example /reports/link-health/:site?limit=10&offset=5` x8.88; `big:site21.example /reports/link-health/:site?limit=1` x8.58; `big:site21.example /reports/link-health/:site?limit=250` x8.04; `big:site21.example /reports/graph/:site` x7.66; `big:site21.example /posts/:site?cluster_id=` x7.22

### Perf pass (2026-10-13): heavy reads on site21.example, before/after

The table above was measured before this pass. Changes: migration `0003_perf` (covering indexes
for the per-site scans: `posts` rows carry 40 KB of HTML in front of the counters, so every
`pagerank`/`is_orphan`/breadcrumb scan walked overflow pages; partial indexes for the link-health
lists; an `/edges` score-order index; `ANALYZE`), plus handler rewrites in `sf-server/src/v1`:
link-health runs 4 concurrent reads (summary scanned in two index halves and tallied from
`GROUP BY` buckets in Rust, list pages picked by rowid from the index before reading the wide rows),
dashboard runs 3 concurrent reads (outbound hosts parsed in Rust from distinct urls), graph runs
nodes/edges concurrently, posts counts are index-only correlated counts, link-health CSV is
ordered in Rust (post-url ranks + primary-key presort, no posts table lookup per link), CSV lines
render without per-cell allocations. Contract re-verified with `diff.py`: 203 requests,
**0 real mismatches**, identical notes to the earlier release run.

Method: `curl` with a fresh connection per request, 15 sequential runs per server after one warm-up,
same JWT, v1 live on :8600, v2 release on a copy of the migrated data. "Before" = the previous v2
release binary on the same copy without the 0003 indexes. (Python `urllib`/`http.client` timings on
this Windows host are bimodal at ~15.6 ms timer steps for both servers when other jobs load the
machine, so curl medians are used here.)

| Endpoint (site21.example) | v1 p50 | v2 before p50 | v2 after p50 | v2 after p95 |
|---|---|---|---|---|
| `/reports/link-health/:site?limit=1` | 40.9 | 400.7 | 15.4 | 19.5 |
| `/reports/link-health/:site?limit=250` | 43.0 | 332.3 | 27.2 | 30.2 |
| `/reports/link-health/:site?limit=10&offset=5` | 27.9 | 306.1 | 15.0 | 15.4 |
| `/reports/link-health/:site?format=csv` | 1037.9 | 1003.2 | 203.4 | 232.3 |
| `/reports/links/:site` | 56.1 | 204.4 | 41.6 | 43.6 |
| `/reports/graph/:site` | 14.1 | 177.2 | 12.4 | 13.6 |
| `/reports/graph/:site?max_nodes=50&max_edges=100` | 10.6 | 152.9 | 5.6 | 5.9 |
| `/reports/anchors/:site` | 14.0 | 25.2 | 13.6 | 15.5 |
| `/dashboard/:site` | 81.0 | 158.7 | 14.3 | 17.1 |
| `/posts/:site` | 24.5 | 110.3 | 13.7 | 14.2 |
| `/posts/:site?limit=500` | 44.5 | 110.5 | 20.5 | 21.0 |
| `/posts/:site?limit=100&offset=100` | 26.9 | 106.1 | 15.3 | 16.3 |
| `/posts/:site?limit=500&is_orphan=true` | 18.6 | 25.5 | 12.3 | 12.6 |
| `/posts/:site?limit=500&is_orphan=false` | 26.1 | 114.5 | 17.0 | 18.0 |
| `/posts/:site?limit=500&post_type=post` | 26.4 | 115.4 | 16.4 | 16.7 |
| `/posts/:site?limit=500&search=best` | 17.0 | 54.3 | 10.1 | 10.8 |
| `/posts/:site?limit=500&cluster_id=<id>` | 13.8 | 237.4 | 8.2 | 8.4 |
| `/edges/:site?limit=1000&offset=1000` | 87.0 | 124.6 | 33.6 | 35.9 |
| `/edges/:site` | 55.6 | 32.0 | 5.8 | 6.0 |

v1 p50 is from the "before" run; the "after" run measured v1 again within noise (27.9 / 42.4 / 26.9 /
799 / 56.7 / 13.5 / 10.6 / 14.5 / 63.2 / 26.1 / 43.0 / 27.9 / 17.9 / 27.8 / 26.9 / 16.8 / 10.3 / 63.9 / 35.2).

Notes:
- `0003_perf` applies in a few seconds on the 1.8 GB migrated DB (first start 8.7 s incl. migration)
  and ends with `ANALYZE` (sqlite_stat1 refreshed). The maintenance pipeline (sf-jobs) should run
  `PRAGMA optimize` after large crawls/plans so the stats keep up with growth (not changed here).
- Ties inside the old `ORDER BY`s now break by rowid (link-health lists and CSV); v1 left them
  unspecified, the harness's tie-aware comparison is unchanged.
- `/posts/:site?cluster_id=` (empty value) answers 200 with the unfiltered list in v2; v1 answers 400.
  The harness only sends a real cluster id, so this is not in the table above.
