---
title: Plan and silos
section: Linking
order: 2
---

# Plan and silos

A **plan** turns your crawled content into scored link suggestions. It writes nothing to your site.

## Run a plan

1. Open the site's **Dashboard**.
2. Choose a **topology** in the drop-down (see below). If you leave the site default, the topology set in
   **Settings → Linking Rules** is used.
3. Optional: choose **AI anchors** (site default, on, off). See [Anchors](anchors).
4. Click **Run Plan**.
5. When the job is done, the **Opportunities** card shows how many suggestions wait for review.

What the plan does, in order: embed new text → extract keywords → build the link graph (PageRank, click depth,
orphans, dead ends) → cluster posts into silos → create candidates from the topology and from semantic similarity
(the 5 most similar posts above 0.5) → apply density limits → score → pick anchors.

## How long it takes

| Situation | Typical time |
|---|---|
| Re-plan, nothing new to embed (2,600-post site) | 3-4 seconds |
| First full plan of a 2,600-post site (115,000 texts, GPU) | about 3 minutes |
| First plan of a 360-post site (14,500 paragraphs, GPU) | under a minute |

Embedding is the slow part, and it happens only once per paragraph. Later plans reuse the stored vectors.

## Topologies

A topology decides how links flow inside each cluster. The **pillar** is the cluster's hub post.

| Topology | Link pattern | Use it when |
|---|---|---|
| `reverse` (default) | Every post → pillar | You want link equity to flow up to a hub or money page. The safe default. |
| `serial` | Post 1 → post 2 → ... → last → pillar | The cluster is a series or step-by-step guide. |
| `priority` | Every post → pillar, and pillar → its top 3 posts by PageRank | You want a strong hub that also promotes its best supporting posts. |
| `circle` | A ring: pillar → post 1 → post 2 → ... → back to pillar | Every post should get exactly one link from a sibling. |
| `bidirectional` | Pillar ↔ every post, both directions | Small, tight clusters where readers move back and forth. Produces the most links. |

> **Tip:** Start with `reverse`. Re-planning with another topology rescores the existing suggestions; you do not lose
> your earlier decisions.

## Archive silo

**Tools → Archive Silo → Build Archive Silo** links posts with their category and tag archive pages (archive ↔ its 5
closest posts). Archive pages are link targets, not sources of normal suggestions.

> **Note:** The v2 crawler does not read category and tag archives yet. Archive Silo works with archive pages that were
> imported from v1; on new sites it has nothing to link.

## Custom clusters

To hand-craft your silos:

1. **Tools → Silo → Custom clusters → Export JSON** downloads the current clusters
   (`label`, `pillar_url`, `member_urls`).
2. Edit the file: move posts between clusters, change pillars.
3. **Import** it. Posts are matched by exact URL; unknown URLs are skipped.

Custom clusters are pinned: later plans only rebuild the automatic clusters around them.

## Money pages and root URLs

In **Settings → Linking Rules → Targets**:

- **Money pages**: your most important commercial pages. The scorer favours links to them until they have about 10
  inbound links, and the density cap never drops a suggestion pointing at one.
- **Root URLs**: hub pages reachable from everywhere (main menu). Click depth is measured from the home page plus
  these.
- **Max new inbound per target**: stops one popular target from taking the whole budget of a plan.
