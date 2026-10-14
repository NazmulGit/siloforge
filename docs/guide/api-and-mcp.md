---
title: API, MCP and AI agents (Hermes)
section: Automation
order: 3
---

# API, MCP and AI agents (Hermes)

SiloForge can be run entirely by an AI agent such as Hermes, through its **MCP server** (Model Context Protocol). In
2.1 an agent can do through MCP everything an admin can do in the UI, limited by the **scopes** of its token and by
the **MCP policy** you set. Scripts can use the same REST API with the same tokens.

## 1. API tokens

Agents and scripts sign in with an **API token** (`sfk_...`), never with your password. Manage them in
**Settings → API & MCP → API tokens**.

| Scope | Allows | Give it to |
|---|---|---|
| `read` | Reading sites, suggestions, reports, jobs, logs, docs | Reporting agents |
| `write` | Plus crawl, plan, approve, edit anchors, inject, undo, rules, schedules | Linking agents (the usual choice: `read,write`) |
| `admin` | Plus settings, backups, tokens, MCP policy, notifications, system, restart | Operator agents only |
| `*` | Everything | Avoid for agents |

Create a token:

1. Click **New token**, give it a name (for example `hermes-linking`), pick scopes and an optional expiry
   (1-3650 days).
2. Copy the token. It is shown **once** and stored only as a hash.

Edit a token: you can change its **name**, **scopes** and **expiry** later. A change applies immediately. A token can
never get more rights than the token or user that created it, and only admins can grant `admin` or `*`. Revoking a
token also revokes the tokens it created. The table shows **Last used** so you can spot unused tokens.

> **Warning:** If a token leaks, revoke it at once. To end every session as well, an admin can call
> `POST /api/v2/auth/logout-all`; it also revokes your API tokens.

## 2. Connect Hermes

**Settings → API & MCP → Connection** shows a ready-made config. Two transports:

| Transport | How | Use when |
|---|---|---|
| stdio | `siloforge.exe mcp` with `SILOFORGE_API_URL` and `SILOFORGE_TOKEN` | Hermes profiles (one process per session) |
| HTTP | `POST http://127.0.0.1:8600/mcp` with `Authorization: Bearer <token>` | MCP clients that speak HTTP; loopback only |

Setup for a Hermes profile:

1. Create a `read,write` token named after the agent.
2. Back up the profile's `config.yaml`.
3. Set the `siloforge` MCP server:

   ```yaml
   mcp_servers:
     siloforge:
       command: D:\Final_Apps\SiloForge\current\siloforge.exe
       args: [mcp]
       enabled: true
       env:
         SILOFORGE_API_URL: http://127.0.0.1:8600
         SILOFORGE_TOKEN: sfk_xxxxxxxxxxxxxxxx
   ```

4. Start a new agent session and ask it to call `siloforge_capabilities`, then `siloforge_list_sites`.

## 3. What the agent can do

Ask the agent to start with **`siloforge_capabilities`**: it returns the tool catalogue grouped by area, with usage
notes and the standard workflow (crawl → plan → review → approve → inject → verify → undo). The agent can also read
this guide with `siloforge_docs_list` and `siloforge_docs_get`.

| Group | Examples | Min scope |
|---|---|---|
| Sites | list, get, add, update, rotate key, set WordPress credentials (`siloforge_site_set_wp_credentials`), delete | read / write; delete needs confirm |
| Crawl, plan, audit | crawl, plan (topology, AI anchors), `siloforge_run_pipeline` (crawl + plan, never injects), audit, link health, Search Console | write |
| Suggestions | list, get, score breakdown, approve/reject, bulk decide, edit anchor | read / write |
| Inject and undo | `siloforge_bulk_inject`, `siloforge_bulk_undo` | write + confirm |
| Rules | keyword rules and ignore list | write |
| Schedules | create, edit, delete, run now | write |
| Reports | dashboard, links, anchors, orphans, cannibalization, metric trends, link-health summary, CSV export, history | read |
| Silos and posts | clusters list/import/export, posts list/search/get | read / write |
| AI models | providers, models, chains, rungs, probes | admin |
| Settings and system | `siloforge_settings_get/update` (incl. resources and CPU/GPU), status, metrics, restart, health checks | admin |
| Jobs | list, status, logs, cancel, `siloforge_wait_for_job` | read / write |
| Backups and notifications | create, list, verify, delete backups; channels, test, read notifications | admin |
| Tokens and MCP policy | list, create, change scopes, revoke; get and update the policy | admin |
| Logs, docs, downloads | `siloforge_logs_query`, docs, download list | read |

The 28 tool names from 2.0 are unchanged, so existing skills and kanban cards keep working.

### Safety built into the tools

- Every tool is marked read-only or destructive and has a minimum scope.
- **Destructive tools require `confirm: true`**: bulk inject, undo, delete site, token revoke and similar. Without it
  the tool refuses and explains what it would do. Tell your agent to ask you before passing `confirm: true`.
- Plans, pipelines and the keyword linker only create suggestions. The approval gate applies to agents too.
- Agents never see WordPress passwords, site API keys or provider keys.
- Inject results count `applied` (live and verified), `marked` (not live) and `failed`. Only `applied` is a live link.

## 4. The MCP tool manager

**Settings → API & MCP → MCP tools** (admin) controls what agents may use. Changes reach running agents within about
60 seconds.

- **Enable or disable** any tool. Disabled tools disappear from the agent's tool list, and calls to them fail.
- **Minimum scope** per tool (read, write, admin): raise it to keep a tool for operator tokens only.
- **Per-token allow list**: restrict one token to a named set of tools (for example a reporting agent gets only
  report tools). Empty means "all tools its scopes allow".
- **HTTP endpoint on/off**: turn off `/mcp` over HTTP if you only use stdio.
- **Stats**: calls and errors per tool in the last 24 hours, and when each was last called.

### The raw API tool

`siloforge_api_request` lets an agent call any `/api/v1` or `/api/v2` route directly. It needs `admin` scope and is
**disabled by default**. Enable it only for a trusted operator agent and only when a dedicated tool is missing; turn it
off again afterwards.

## 5. Example agent workflows

Give your agent instructions like these. The tool calls it should make are shown under each.

**New site onboarding**
> "Add example.com, store its WordPress application password, crawl it and run a reverse plan. Report the dashboard
> numbers and the 20 best suggestions. Do not inject."

`siloforge_add_site` → `siloforge_site_set_wp_credentials` → `siloforge_run_pipeline(topology="reverse")` →
`siloforge_dashboard` → `siloforge_list_edges(status="suggested", limit=20)`

**Weekly audit**
> "Every Monday, run an audit on all my sites and send me a summary of changes in health score, orphans and broken
> links."

Create an `audit` schedule per site (or run one), wait with `siloforge_wait_for_job`, then compare
`siloforge_dashboard` and the metrics trend; post the summary to your channel.

**Approve and inject with review**
> "List the suggestions for example.com with a score above 0.6. Show me the anchors. After I confirm, approve those
> I accept and inject them in batches of 10, and tell me how many were applied."

`siloforge_list_edges` → `siloforge_anchor_report` → (you reply) → `siloforge_edit_anchor` as needed →
`siloforge_bulk_decide(decision="approved")` → `siloforge_bulk_inject(edge_ids=[...], confirm=true)` →
`siloforge_wait_for_job` → `siloforge_link_history`

**Fix broken links**
> "Run an internal link-health check on example.com and give me a table of broken links and redirect chains, with the
> post to edit and the URL to use instead."

Link-health run → wait → link-health summary → the agent formats the list. Fixing the links themselves is done in
WordPress (or with undo for links SiloForge added).

**Change the AI model**
> "Add an OpenRouter provider, add a chat model, probe it, and put it first in the global chat chain. Keep the old
> model as fallback."

Needs an `admin` token: providers → models → probe → chains / rungs → activate. Ask the agent to report the probe
result before it changes the chain.

> **Tip:** Every agent call is recorded in **Workspace → Logs** with the token name, so you can see exactly what an
> agent did.
