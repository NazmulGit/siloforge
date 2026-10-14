---
title: Crawl
section: Linking
order: 1
---

# Crawl

A crawl reads your site's posts and pages into SiloForge. It **never changes** anything in WordPress.

## Run a crawl

1. Open the site's **Dashboard**.
2. Click **Run Crawl**. (Or click **Crawl** on the site's row in **Sites**.)
3. Watch progress in **Recent jobs** on the Dashboard, or under **Jobs**. Click a job to see its log.
4. When it finishes, the Dashboard's post count is up to date.

## What the crawler does

- In `wp_rest` mode it reads every published post and page through the WordPress REST API. With an application
  password it reads the **raw** content (`context=edit`), so Gutenberg block comments and your exact HTML are kept.
  Without credentials it falls back to the rendered content.
- In `sitemap` mode it reads `sitemap.xml` (or `sitemap_index.xml`) and fetches each page on your domain.
- It splits each post into paragraphs. Headings are boundaries; very short blocks (under 40 characters) are skipped;
  a list counts as one block.
- It records the links already in your content, so SiloForge knows the current link graph.
- It is **incremental**: posts whose content has not changed are skipped, and only changed posts are re-split.
- It respects `robots.txt`, uses a few parallel requests per site (default 4), and backs off when the server answers
  429 or 503.

Real numbers: a 360-post site crawls in about 3 seconds on a local network.

## When to crawl again

- After you publish or edit posts in WordPress.
- Before a plan, if the site changed since the last crawl. An **audit** schedule does both for you
  (see [Schedules and audits](schedules-and-audits)).

> **Tip:** A post edited in WordPress after a plan is detected at inject time anyway: SiloForge never overwrites your
> edit. But a fresh crawl and plan give better suggestions for it.

## Common problems

| Message | What to do |
|---|---|
| 401 from WordPress | The application password is missing, wrong, or disabled on plain `http`. See [Sites](sites). |
| Blocked: private host | The site is on `localhost` or your LAN. An admin must allow private hosts for that site. |
| Few posts found | Check the WordPress URL, and that the REST API (`/wp-json/`) is not blocked by a security plugin. |
