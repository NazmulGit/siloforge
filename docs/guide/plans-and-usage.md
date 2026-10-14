---
title: Plans and usage
section: Accounts & SaaS
order: 3
---

# Plans and usage

Every workspace has a **plan**. The plan sets how much the workspace may use: sites, posts, Link Runs, AI tokens,
members, API tokens and schedules. The Default workspace of a single-user install is on **Unlimited**, so nothing is
ever limited there.

## Plans

| Limit | Free | Pro | Agency | Unlimited |
|---|---|---|---|---|
| Sites | 1 | 10 | 100 | unlimited |
| Posts per site | 500 | 5,000 | 50,000 | unlimited |
| Link Runs per month | 5 | 100 | unlimited | unlimited |
| AI tokens per month | 100,000 | 2,000,000 | 20,000,000 | unlimited |
| Members | 1 | 5 | 25 | unlimited |
| API tokens | 1 | 10 | 50 | unlimited |
| Schedules | 1 | 20 | 200 | unlimited |
| Auto-Linker | no | yes | yes | yes |
| MCP (AI agents) | no | yes | yes | yes |

Unlimited is an internal plan (used by the Default workspace) and is not offered to new sign-ups. Platform admins
can change these limits and add plans (see [Admin console](admin-console)), so your installation may differ: the
**Plan & usage** page always shows the real numbers. Months are calendar months in UTC.

## See your usage

**Workspace → Settings → Plan & usage** shows each limit with a bar: sites, total posts and the largest site, Link
Runs this month, AI tokens this month, members (including pending invites), API tokens and schedules. Earlier months
are listed below the bars.

## What a "plan limit" message means

When an action would go over a limit, SiloForge refuses it with a **plan limit** message and an upgrade prompt.
Nothing is half-done: the action simply does not start. Scripts and agents receive HTTP status **402**.

| You tried to | Limit checked |
|---|---|
| Add a site | Sites |
| Crawl, plan, sync from the WordPress plugin or import a CSV | Posts per site (refused once the site already has that many posts) |
| Start a Link Run | Link Runs per month (and AI tokens, if the run asks for AI) |
| Invite someone | Members (members + pending invites) |
| Create an API token | API tokens |
| Create a schedule | Schedules |
| Turn on the Auto-Linker | Auto-Linker |
| Let an agent use the workspace's MCP tools | MCP (on a plan without MCP the agent sees no tools) |

**AI tokens** work like the 2.3 AI budgets (see [AI models](ai-models)): once the month's quota is used up,
SiloForge keeps working with its built-in heuristics instead of calling an AI model, so plans and anchors are still
produced, just without AI. A Link Run that explicitly asks for AI is refused until the next month.

What you can do:

- Make room: delete a site, token or schedule you no longer use, or cancel a pending invite.
- Wait for the next month (Link Runs and AI tokens).
- Ask for a bigger plan.

## Upgrade

SiloForge 2.4 does not take payments itself. **Plan & usage** shows how to upgrade (a contact address or a note from
the platform). A platform admin then assigns the new plan, and it applies immediately.

For scripts, the 402 body tells you exactly what was hit:

```json
{"error": "plan limit", "message": "Your plan allows 1 site.", "statusCode": 402,
 "key": "sites", "limit": 1, "used": 1, "plan": "free", "workspace_id": "ws-example"}
```

`key` is the limit name (`sites`, `posts_per_site`, `link_runs_per_month`, `ai_tokens_per_month`, `members`,
`api_tokens`, `schedules`, `auto_linker`, `mcp`).
