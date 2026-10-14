---
title: Getting started
section: Start here
order: 1
---

# Getting started

SiloForge finds good internal links for your WordPress site, lets you review them, writes the approved ones into your
posts, and can take every one of them out again. It runs on your own PC as one program (`siloforge.exe`) with one
data folder. Nothing is sent to a cloud service unless you configure an AI or embedding provider.

## What you need

- SiloForge installed (default: `D:\Final_Apps\SiloForge`) and running. It starts by itself at logon and is checked
  every 5 minutes by the Scheduled Task `siloforge-keepalive`.
- The web UI: open `http://127.0.0.1:8600/` (or `:8601`) in your browser.
- A WordPress site you manage, with an **application password** for a user that may edit posts (see [Sites](sites)).

## First run in ten minutes

1. **Sign in.** Use the admin account created at install time. Self-registration is off; only the very first user
   can register, and only from the PC itself (`http://127.0.0.1:8601`).
2. **Add your site.** Go to **Sites → + Add Site**, enter the domain (for example `example.com`), the WordPress URL
   and the crawl mode `wp_rest`. Copy the one-time API key if you plan to use the WordPress plugin.
3. **Give SiloForge write access.** Store the WordPress user and application password for the site
   (see [Sites](sites)). Without them SiloForge can still analyse the site, but it cannot write links.
4. **Crawl.** Open the site's **Dashboard** and click **Run Crawl**. A crawl only reads; nothing on the site changes.
5. **Plan.** Pick a topology (start with `reverse`) and click **Run Plan**. SiloForge scores every candidate link
   and proposes anchor text. These are *suggestions* only.
6. **Review.** Open **Tools → Inbound** (or click **Opportunities** on the Dashboard). Approve the links you like,
   reject the rest, and fix weak anchors in **Tools → Anchors**.
7. **Inject a few.** In **Tools → Bulk**, inject a small batch (5-10 links). SiloForge snapshots each post, writes
   the link and checks the live page.
8. **Check and keep going.** Look at the changed pages, then **History**. If anything looks wrong, undo it from
   **Tools → Bulk**. Once you trust the results, work in larger batches.

> **Tip:** Every page has a **?** button that opens the matching part of this guide in a side drawer.
> The full guide is under **Workspace → Docs**.

## Finding your way around

The menu has three groups:

| Group | Items | What they are for |
|---|---|---|
| **Site** | Dashboard, Tools, Reports, Link Health, History, Integrations, Jobs | Work on one site. If no site is chosen yet, clicking an item opens a site picker first. The last site you used is remembered. |
| **Workspace** | Sites, All jobs, System, Logs, Downloads, Docs | Everything across sites: add sites, watch jobs, server health, the activity log, installers, this guide. |
| **Settings** | General, Linking Rules, Resources & Performance, WordPress, AI Models, API & MCP, Notifications, Backups, Activity, Account | How SiloForge behaves. Most sections are for admins. |

Hover over buttons and labels to see short hints. You can turn hints off in **Settings → General**.

## The golden rule

Nothing reaches your live site until **you approve it and run an inject**. Plans, schedules, the keyword linker and
AI agents only create suggestions. Every write is snapshotted and can be undone.

Next: read [How SiloForge works](concepts) for the ideas behind scores, silos and anchors.
