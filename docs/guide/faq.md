---
title: FAQ
section: Help
order: 2
---

# Frequently asked questions

**Can SiloForge break my site?**
It is designed not to. Nothing is written without your approval, each post is snapshotted, the link is spliced into
the exact paragraph without touching blocks or other HTML, edits you make in WordPress are never overwritten, the live
page is checked afterwards, and a failed check rolls the post back. Undo removes exactly the inserted link.

**Does it change my existing links?**
No. SiloForge only adds links, and undo only removes links it added.

**Does it send my content to the cloud?**
Only if you configure it. Embeddings run locally on your GPU or CPU by default. AI chat models and remote embeddings
are optional and use the providers you choose.

**How many links will it add?**
By default at most 10 contextual links per post and at most 5 new ones per inject pass. Change this in
**Settings → Linking Rules**.

**Which topology should I use?**
`reverse` for most sites: supporting posts link to their pillar. See [Plan and silos](plan-and-silos).

**Why are some suggestions marked `content_gap`?**
The source paragraph has no natural phrase that fits the target. An AI model can write a bridging sentence, or you
can skip them.

**Why does a suggestion say "anchor not found" at inject time?**
The post was edited after the plan and the phrase is gone. Re-crawl and re-plan.

**What is the quickest way to link a whole site?**
A [Link Run](link-runs): tick strategies, pick a linking mode, review the preview, click **Deploy links**. Start with
a small **Max total** (50-100) on the first run.

**Why does "Open ↗" not jump to the link?**
It uses a text fragment (`#:~:text=`), which Chrome, Brave and Edge support. Other browsers open the page at the top.
If the link is missing on the page, use **Verify again** or **Apply properly** in the run's results.

**Can SiloForge add links on its own every week?**
Yes, with the [Auto-Linker](auto-linker). It runs as a dry-run by default (links wait for you to deploy) and pauses
itself if too many links fail.

**Can I use it with page builders (Elementor, Divi)?**
Crawling and analysis work. Writing works on the stored post content; always check a few pages first, because some
builders store text elsewhere. The live-page check catches links that do not show up.

**Do I need the WordPress plugin?**
Usually not. With an application password SiloForge writes links itself. See [WordPress plugin](wordpress-plugin).

**Can several people use it?**
Yes. Each user sees their own sites; admins see all sites. Self-registration is off; an admin creates accounts, for example with
`siloforge user set-password <email> --create`.

**Can an AI agent run it for me?**
Yes, through MCP with a scoped token. Destructive actions need an explicit confirmation, and everything an agent does
is logged. See [API, MCP and AI agents](api-and-mcp).

**Can I run SiloForge on two PCs?**
Each PC runs its own install with its own data. Never point two installs at the same WordPress sites at the same time.

**Where is my data?**
In `D:\Final_Apps\SiloForge\data`. Backups go to the folder set in **Settings → Backups**.

**How do I get help with an error?**
See [Troubleshooting](troubleshooting). The job log, **Workspace → Logs** and `siloforge doctor` usually show the cause.
