# SiloForge for Hermes Agent (MCP)

SiloForge ships its own MCP server. It is a thin client of the REST API, so whatever a tool does, the API enforces
the same ownership, scopes and approval rules. Two transports:

| Transport | How | Use when |
|---|---|---|
| stdio | `siloforge.exe mcp` with `SILOFORGE_API_URL` + `SILOFORGE_TOKEN` | Hermes profiles (one process per agent session) |
| streamable HTTP | `POST http://127.0.0.1:8600/mcp` with `Authorization: Bearer <token>` | clients that speak MCP over HTTP; loopback `Host` only |

The 20 v1 tool names and parameters are unchanged, so existing kanban cards and skills that name `siloforge_*` tools
keep working (research/04 risk R17).

---

## 1. Setup (profile `ap13_internallink`)

1. Create a token for the agent (printed once; store it only in the profile config):
   ```powershell
   D:\Final_Apps\SiloForge\current\siloforge.exe token create --name hermes-ap13 --scopes read,write --home D:\Final_Apps\SiloForge\data
   ```
   Add `admin` to the scopes only if the agent should create backups or read notifications/system details
   (`siloforge_create_backup`, `siloforge_list_notifications` and the full `siloforge_system_status` use admin routes).
   `--expires-days 180` makes it expire. Revoke with `siloforge token revoke <id-or-prefix>` or in the UI.
2. Back up the config: copy `D:\Agent\hermes\profiles\ap13_internallink\config.yaml` to `config.yaml.bak-pre-sf2`
   (`rollback-to-v1.ps1` restores that file).
3. Replace the `mcp_servers.siloforge` block:
   ```yaml
   mcp_servers:
     siloforge:
       command: D:\Final_Apps\SiloForge\current\siloforge.exe
       args: [mcp]
       enabled: true
       env:
         SILOFORGE_API_URL: http://127.0.0.1:8600
         SILOFORGE_TOKEN: sfk_xxxxxxxxxxxxxxxxxxxxxxxx      # the token from step 1
   ```
   Remove `SILOFORGE_EMAIL` and `SILOFORGE_PASSWORD`. They still work as a legacy fallback (login + one re-login on
   401), but a scoped, revocable token keeps the admin password out of the config and its many backups.
4. New agent sessions pick up the change; kanban workers need no gateway restart. The MCP process logs to stderr
   only (stdout is the protocol channel).
5. Check: in a session, call `siloforge_system_status`, then `siloforge_list_sites`.

The `siloforge` MCP stays disabled on LanPool nodes (`REMOTE_MCP_DISABLE` in the Hermes `_patches`), and ap13 runs on
the main PC, so `127.0.0.1:8600` is correct. If ap13 is ever offloaded, forward 8600 through LanPool first.

The UI shows a ready-made config under Settings -> Integrations (`GET /api/v2/mcp`).

---

## 2. Tools (28)

`site` accepts a site UUID or a domain (`example.com`, `www.` and scheme tolerated); lookups are cached per identity.
Enqueue tools return a `job_run_id`; the job tools accept `job_id` or `job_run_id`. Secrets (api keys, WP application
passwords, tokens) are stripped from every result, and long lists are shortened item-wise with a note.

### v1 tools (20, unchanged names)
| Tool | Params | What it does |
|---|---|---|
| `siloforge_list_sites` | | Sites the caller owns: id, domain, mode, post and edge counts |
| `siloforge_dashboard` | site | Health score, orphans, dead ends, opportunities, http links, outbound domains, duplication, cannibalization, recent jobs |
| `siloforge_list_edges` | site, status?, tool?, limit? (1-1000, 200), offset? | Link edges sorted by score with `total`, `has_more`, `next_offset` (big sites are not cut silently) |
| `siloforge_links_report` | site | Per post: inbound, outbound, pagerank, click depth, Link Flow Score, duplicates |
| `siloforge_anchor_report` | site | Anchor-type distribution, top anchors, over-optimisation warnings |
| `siloforge_link_history` | site, limit? (1-100), offset? | Applied/reverted links with time, anchor, tool |
| `siloforge_list_jobs` | site?, status?, kind?, offset? | With only `site`: the site's latest 25 runs (v1). Otherwise all visible jobs, newest first |
| `siloforge_list_clusters` | site | Silo clusters: label, source, members, pillar |
| `siloforge_add_site` | domain, wp_url?, mode? | Registers a site (the plugin `api_key` is NOT returned; copy it from the UI if needed) |
| `siloforge_crawl_site` | site | Enqueues a crawl (read-only towards WordPress) |
| `siloforge_plan_site` | site, topology? (reverse/serial/priority/circle/bidirectional), archive? | Enqueues a plan: produces **suggested** edges only |
| `siloforge_decide_edge` | edge_id, decision (approved/rejected) | Approve or reject one edge |
| `siloforge_bulk_decide` | edge_ids[], decision | Approve or reject many edges |
| `siloforge_bulk_inject` | site, edge_ids? | **Writes to the live site.** Injects approved edges via WP REST (snapshot first); returns a job |
| `siloforge_bulk_undo` | site, edge_ids? | **Writes to the live site.** Reverts applied edges; returns a job |
| `siloforge_edit_anchor` | edge_id, anchor, anchor_type? | Changes the anchor of a non-applied edge |
| `siloforge_add_keyword_rule` | site, keyword, target_url, anchor_type?, max_links? (1-100) | Keyword linker rule |
| `siloforge_run_keyword_linker` | site | Runs all active rules -> suggested edges (still need approval) |
| `siloforge_add_ignore_keyword` | site, phrase | Phrase never used as an anchor |
| `siloforge_export_edges_csv` | site | Approved + applied edges as CSV |

### v2 tools (8)
| Tool | Params | What it does |
|---|---|---|
| `siloforge_system_status` | | Version, uptime, CPU/RAM, database, queued/running jobs (admin details when the token allows) |
| `siloforge_job_status` | job_id | One job: kind, status (queued/running/done/failed/cancelled), stats, error, timestamps |
| `siloforge_job_logs` | job_id, after?, limit? (<=1000) | Log lines (seq, ts, level, msg); pass the last seq as `after` to tail |
| `siloforge_cancel_job` | job_id | Cancels a queued/running job; a running inject stops at its next checkpoint (applied links stay; use undo) |
| `siloforge_wait_for_job` | job_id, timeout_secs? (600, max 900), poll_secs? (5) | Blocks until done/failed/cancelled or timeout; returns final status + stats |
| `siloforge_run_pipeline` | site, topology?, archive?, skip_crawl?, timeout_secs? (900, max 1800) | Crawl -> wait -> plan -> wait; returns per-step stats, job ids, number of suggestions. **Never injects** |
| `siloforge_create_backup` | note? | Starts a full backup (admin scope) |
| `siloforge_list_notifications` | limit? (<=200), unread_only? | Recent notifications, newest first (admin scope) |

Error contract: a failed call returns `isError: true` with `SILOFORGE <METHOD> <path> -> <status>: <error>`.

### Result semantics the skills rely on
After `siloforge_bulk_inject` + `siloforge_wait_for_job`, the job stats count edges as:
- **applied**: the link is live in WordPress (written and verified);
- **marked**: not live. Either queued for the Connect plugin (`insert_module = content_gap` without an LLM, or no WP
  credentials), or the anchor no longer exists because the post was edited after planning. `error` says which;
- **failed**: the write or the verify failed; nothing is left half-done (auto-rollback).
A SiloForge zero is not a fleet zero: report `applied`, never `marked`, as live links.

---

## 3. Typical workflows

### 3.1 Plan, review, approve, inject (the safe default)
```
siloforge_run_pipeline(site="example.com", topology="reverse")         # crawl + plan, suggestions only
siloforge_list_edges(site="example.com", status="suggested", limit=200) # page with offset/next_offset
siloforge_anchor_report(site="example.com")                            # check over-optimisation first
siloforge_edit_anchor(edge_id=..., anchor="better phrase")             # fix weak anchors
siloforge_bulk_decide(edge_ids=[...], decision="approved")             # the approval gate
siloforge_bulk_inject(site="example.com", edge_ids=[...])              # WRITES to WordPress
siloforge_wait_for_job(job_id=<job_run_id>)                            # applied / marked / failed
siloforge_link_history(site="example.com")                             # audit trail
```
Rules: approve explicitly listed `edge_ids` (never "all approved" on a site with an old backlog: v1 left ~12k approved
edges), and inject in batches you can review.

### 3.2 Step by step with waits
```
j = siloforge_crawl_site(site=...)          -> {job_run_id}
siloforge_wait_for_job(job_id=j)            -> status done, stats {posts, upserted, skipped, ...}
p = siloforge_plan_site(site=..., archive=true)
siloforge_wait_for_job(job_id=p, timeout_secs=900)
```
If a wait times out, call it again with the same id (the job keeps running). Use `siloforge_job_logs` with `after`
to follow progress, and `siloforge_cancel_job` to stop.

### 3.3 Undo
```
siloforge_bulk_undo(site=..., edge_ids=[...])   # removes exactly the inserted links
siloforge_wait_for_job(job_id=...)
```

### 3.4 Keyword linker
```
siloforge_add_keyword_rule(site=..., keyword="water bottle", target_url="https://.../best-water-bottles/", max_links=3)
siloforge_run_keyword_linker(site=...)  -> wait -> review/approve like 3.1
```

### 3.5 Kanban gates (T8 cards)
Use `siloforge_list_jobs(site=...)` or `siloforge_job_status` as a gate ("do not plan again while a plan is
running"); SiloForge also deduplicates identical queued jobs and never runs two mutating jobs on one site at once.

---

## 4. Safety

- **The approval gate is mandatory.** Plans, pipelines and the keyword linker only create `suggested` edges.
  Nothing reaches WordPress until edges are `approved` and `siloforge_bulk_inject` runs.
- Injection reads the current raw content, never overwrites an edit made in WordPress after planning
  (`modified_gmt` precondition), keeps a snapshot per post, verifies the live page and rolls back on failure.
- Use a `read,write` token for agents. `read` alone is enough for reporting agents. Keep `admin` for operators.
- The agent never sees WordPress application passwords or site api keys (stripped from MCP output and never
  returned by the API).
- Do not let agents run `docker exec`/SQL against SiloForge: in v2 there is no container, and the SQLite file must
  only be written by the server.

---

## 5. Notifications to Hermes (Telegram via the gateway)

SiloForge can push events to a Hermes gateway webhook route, so the gateway delivers them to Telegram and SiloForge
never holds the bot token (research/04 sections 2.5 and 7).

1. Pick a random secret (store it in the two configs below, nowhere else):
   `-join ((1..32) | % { '{0:x2}' -f (Get-Random -Max 256) })`
2. Add a `deliver_only` route to the teli gateway config (`D:\Agent\hermes\profiles\teli\config.yaml`; back it up first):
   ```yaml
   platforms:
     webhook:
       extra:
         routes:
           siloforge-events:
             secret: "<the secret>"
             deliver: telegram
             deliver_only: true          # the rendered prompt IS the message, no LLM turn
             prompt: "SiloForge {level}: {title}\n{body}"
   ```
   The gateway restart takes 15-18 min; do it only via the hub (`POST :9120/api/gateway-restart`) with
   `active_agents = 0`.
3. Add a `hermes` channel in SiloForge (UI Settings -> Notifications, or the API):
   ```powershell
   Invoke-RestMethod -Method Post http://127.0.0.1:8600/api/v2/notifications/channels -Headers $h -ContentType application/json `
     -Body '{"name":"hermes-telegram","kind":"hermes","config":{"url":"http://127.0.0.1:8644/webhooks/siloforge-events","secret":"<the secret>"},"events":["job.failed","inject.done","backup.failed","system.alert"]}'
   ```
   SiloForge signs each POST with `X-Webhook-Timestamp` and `X-Webhook-Signature-V2` (hex HMAC-SHA256 of
   `"<ts>.<body>"`), the gateway's generic V2 scheme with a 300 s replay window. The body has `event`, `level`,
   `title`, `body`, `text`, `data`, `site_id`, `job_run_id`.
4. `POST /api/v2/notifications/channels/{id}/test` sends a `test` event; it should arrive in Telegram.

Events: `job.done`, `job.failed`, `plan.ready`, `inject.done`, `backup.done`, `backup.failed`, `system.alert`.
Each event type is rate limited per minute. Use `cron_job` instead of `deliver_only` on the route if an agent should
react (for example re-plan after a failure); the two options are mutually exclusive.
