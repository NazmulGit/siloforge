---
title: Auto-Linker
section: Automation
order: 5
---

# Auto-Linker

The Auto-Linker runs a [Link Run](link-runs) for a site on a schedule. In **dry-run** mode (the default) it prepares the
links and asks you to deploy them; in **live** mode it deploys them itself, verifies them and repairs failures once.

## Enable it

1. Pick the site, open **Settings → Linking Rules** (or click **Auto-Linker** at the top of the Link Run page) and find
   the **Auto-Linker** card.
2. Switch **Auto-Linker on**.
3. Set the **Schedule**: pick a preset or **Custom cron…**, then check **Cron** and **Timezone**, for example
   `0 3 * * 1` (Mondays 03:00) in `Europe/London`. Cron works as in [Schedules and audits](schedules-and-audits).
4. Open **Run options (same as the Link Run steps)**: the same strategies, linking mode, filters and switches as the
   Link Run page.
5. Leave **Deploy automatically (live)** off for now (the card then reads **Dry-run: stop at the preview**) and set the
   caps (below).
6. Click **Save Auto-Linker**.

## Dry-run vs live

| | Dry-run (switch off, `live: false`, default) | Live (**Deploy automatically (live)** on, `live: true`) |
|---|---|---|
| Crawl, plan, select | Yes | Yes |
| Writes to WordPress | **No.** The run stops at **ready**. | Yes: deploy, verify, and one **Apply properly** pass |
| Notification | "N links ready to deploy", with a link to the run's review step | A summary: applied, failed, repaired, skipped |
| What you do | Open the run, review, click **Deploy N links** | Check the summary; open the run if anything failed |

Both kinds show up in the **History** tab of the Link Run page marked **Auto**, and every step is in
**Workspace → Logs**. A live run can be undone like any other with **Undo run**.

## Caps

- **Daily cap (links)**: the most links the Auto-Linker may add to the site in one day (UTC), across all its runs.
- The run's own filters still apply: **Max total links**, **Max new per post**, **Max links per post**,
  **Minimum score**.

Start small. A site that gains hundreds of links overnight looks unnatural, and small batches are easier to check.

## Auto-pause

If too many links of a live run fail (failure rate above **Pause when failures exceed**, default 20 %), the
Auto-Linker switches itself off (**Auto-Linker on** goes off) and sends a notification. Common causes: expired WordPress
application password, a security plugin blocking the REST API, a new cache or page builder. Fix the cause, run one
Link Run by hand to check, then enable it again.

## Notifications

The Auto-Linker uses your normal [notification](notifications) channels:

- dry-run finished: "N links ready to deploy";
- live run finished: summary of applied, failed, repaired and skipped links;
- auto-paused: the failure rate and the run to look at.

## Recommended rollout

1. **Week 1: dry-run.** Enable with a weekly schedule and **Deploy automatically (live)** off. Each week, open the
   ready run, review the links and deploy them yourself. Note what you untick: tighten **Minimum score**,
   **Skip URLs matching** or the anchor types to match.
2. **Week 2: live with small caps.** When you hardly untick anything any more, switch **Deploy automatically (live)**
   on with **Daily cap (links)** `20-30`, **Max new per post** `2` and **Max total links** `50`. Keep **Check every
   link on the live page** on.
3. **After that:** check the summaries; raise the caps slowly (for example `50` per day) if results stay clean. Leave
   **Contextual** off in live mode unless you have reviewed many AI-written links.

> **Tip:** **Run now** on the card starts a normal Link Run with the card's options at once (it stops at Review, even
> in live mode). To trigger a real Auto-Linker run (dry-run or live, with the daily cap) use
> `POST /api/v2/sites/{id}/auto-linker/run` or the MCP tool `siloforge_auto_linker_run_now`, or let an AI agent drive
> it (see [API, MCP and AI agents](api-and-mcp)).
