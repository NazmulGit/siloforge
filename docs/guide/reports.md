---
title: Reports and dashboard
section: Analysis
order: 1
---

# Reports and dashboard

## Dashboard

The site Dashboard shows the state of your internal linking at a glance.

| Card | Meaning | What to do |
|---|---|---|
| **Link Health Score** (0-100) | Combines the orphan rate, dead-end rate and average links per post. Higher is better. | Work on the cards below. |
| **Orphans** | Pages with no inbound links. Readers and crawlers barely find them. | **Quick Fix Orphans**, or **Tools → Inbound**. |
| **Dead ends** | Pages with no outbound internal links. | **Quick Fix Deadends**, or **Tools → Outbound**. |
| **Opportunities** | Suggestions waiting for review. | Click to open the approval queue. |
| **Broken links** | Links whose target errors or cannot be reached. | See [Link health](link-health). |
| **Nofollow internal** | Internal links with `rel=nofollow`, `sponsored` or `ugc`. | Usually a mistake; remove the attribute in WordPress. |
| **Redirect chains** | Links that take more than two hops. | Point the link at the final URL. |
| **HTTP links** | Internal links still on `http://`. | Update them to `https://`. |
| **Edges by status** | Count of suggestions per status. | |
| **Cannibalization warnings** | One exact anchor pointing at several different pages. | Pick one target per term. |
| **Recent jobs** | The latest crawl, plan and inject runs. | Click for details. |

The link-health cards show "unknown" until a link-health check has run.

## Reports → Links

A sortable table with one row per post: inbound links, outbound links, PageRank, click depth, Link Flow score and
duplicates.

- Type a minimum or maximum under any column to filter.
- **Duplicates only** shows posts touched by repeated links.
- Tick posts and **Send selection to Bulk**, or use **Export filtered to Bulk**. This approves their suggested links
  and sends them to **Tools → Bulk**.

## Reports → Visualizer

An interactive graph of your highest-PageRank posts. Colour is the cluster, size is PageRank, orphans have a red
ring, and links are styled by status. Use it to see whether your silos hang together.

## History

**History** is the audit trail of every link actually applied or reverted: source, target, anchor, tool, status and
time. **Download CSV** for your records.

## Jobs and trends

The site's **Jobs** page shows each run with its duration, CPU time and peak memory, plus the site's schedules and a
metrics trend (health score, orphans and links over time). Every audit stores a metrics snapshot, so you can see
progress.

## Anchor report

The anchor-type distribution lives in **Tools → Anchors**. Aim for a mix led by partial and LSI anchors, with exact
anchors well under 40 %.
