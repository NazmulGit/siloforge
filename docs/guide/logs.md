---
title: Logs
section: Administration
order: 2
---

# Logs

**Workspace → Logs** is one timeline of everything that happens in SiloForge: clicks in the UI that change something,
API calls, AI-agent (MCP) tool calls, jobs, sign-ins, notifications and system events.

## What is recorded

| Source | Examples |
|---|---|
| `ui` | Changes made in the browser: a site setting saved, links approved |
| `api` | Calls from scripts with an API token |
| `mcp` | AI-agent tool calls (start and end, ok or error), with the tool name |
| `job` | Jobs queued, started, done, failed or cancelled, with a short stats summary |
| `auth` | Sign-in ok or failed, account lockout, tokens created or revoked |
| `notify` | Notifications sent |
| `ai` | Every AI call: task, model, tier, tokens in and out, cost, cache hits, fallbacks |
| `system` | Start, stop, restart, settings changed, backup done or failed, errors |

Each row shows time, source, **actor** (a user, a token by name, an MCP token, or the system), action, target, site,
status (`ok`, `error`, `denied`), HTTP status and duration. Expand a row to see its details.

By default every change, every MCP call and every error is logged. Plain page loads and reads are not, unless an
admin turns on **log reads**.

## AI token use

The **AI** chip shows only AI calls. Their rows have the action `ai <task> <model>` (for example
`ai anchor_pick gpt-example-mini`, or `ai anchor_pick cache` for answers served from the cache) and two extra columns:
**Tokens in / out** and **Cost**. A `*` after the tokens means the provider sent no usage and SiloForge estimated it
(characters ÷ 4). Expand a row to see the tier, rung, latency and whether a fallback model answered.

The stats header adds the AI totals for the chosen time range: tokens in / out, cost and number of calls. Charts per
day, tier, model and site are in **Settings → AI Models → Usage** (see [AI models](ai-models)).

## Find something

- **Source** and **status** chips: for example `mcp` + `error` to see failed agent calls.
- **Actor** and **site** selectors: everything one agent token did, or everything on one site.
- **Search** box and **time range**.
- **Live tail**: refreshes every 3 seconds, handy while an agent is working.
- Job rows link to the job's own log.
- **Export CSV** downloads the filtered rows.

Admins see all rows. Other users see only their own.

## Retention

Rows are kept for **90 days** by default (`monitor.event_log_retain_days`, in **Settings → Resources & Performance**).
Logging is done in the background and does not slow SiloForge down.

## Privacy

The log never stores request bodies, passwords, API keys, tokens or other secrets, only what happened, who did it
and the result.

## Other logs

- **Job logs**: every job's step-by-step log, in **Jobs** / **All jobs**.
- **System → errors**: repeated errors grouped by fingerprint, with first and last seen. Mark one handled; it reopens
  if it happens again.
- **Server log files**: `D:\Final_Apps\SiloForge\data\logs\siloforge.log.<date>` (one file per day), for technical
  troubleshooting.
