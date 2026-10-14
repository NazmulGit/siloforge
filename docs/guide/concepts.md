---
title: How SiloForge works
section: Start here
order: 2
---

# How SiloForge works

## The pipeline

Every link goes through the same six steps. Only the last two touch WordPress.

| Step | What happens | Changes your site? |
|---|---|---|
| 1. **Crawl** | Reads posts and pages (raw content, so Gutenberg blocks survive) and splits them into paragraphs. | No |
| 2. **Embed** | Turns each post and paragraph into a vector, a numeric "meaning fingerprint" (384 numbers, model all-MiniLM-L6-v2). Only new or changed text is embedded. | No |
| 3. **Plan** | Builds the link graph, groups posts into topic clusters (silos), creates candidate links, scores them and picks anchor text. | No |
| 4. **Review** | You approve or reject suggestions and edit anchors. | No |
| 5. **Inject** | Writes approved links into the posts through the WordPress REST API, after a snapshot, then checks the live page. | **Yes** |
| 6. **Undo** | Removes exactly the inserted link, when you ask for it. | **Yes** |

Steps 2 and 3 run together as one **plan** job. Each step is a job you can watch under **Jobs** or **All jobs**.

A **Link Run** (2.2) bundles steps 1-5 for one site into a single guided batch with its own options (strategies,
linking mode, filters), results per post, a repair step and **Undo run**. See [Link Runs](link-runs); the
[Auto-Linker](auto-linker) starts Link Runs on a schedule.

## Link statuses

| Status | Meaning |
|---|---|
| `suggested` | Created by a plan or a rule. Waiting for you. |
| `approved` | You said yes. It will be written on the next inject. |
| `rejected` | You said no. |
| `applied` | Live in WordPress and verified. |
| `reverted` | Undone. You can approve and inject it again. |
| `failed` | The write or the live check failed. The post was rolled back; the row shows the error. |

## Silos and clusters

A **silo** is a group of posts on one topic that link to each other, with one **pillar** post as the hub. SiloForge
builds clusters automatically from the post vectors (k-means; roughly the square root of half the post count, between
2 and 25 clusters), picks as pillar the post closest to the cluster's centre, and labels the cluster with its most
common title words. You can define your own clusters too (**Tools → Silo → Custom clusters**); they survive re-plans.
How links flow inside a cluster is the **topology**, explained in [Plan and silos](plan-and-silos).

## Scores

Every candidate gets a **composite score** between 0 and 1, built from 11 signals. Click a score in the Tools tables
to see the breakdown.

| Signal | Weight | Higher when... |
|---|---|---|
| semantic | 0.22 | the two posts are about the same thing |
| graph | 0.10 | the target is weak in the link graph (orphan, low PageRank) |
| money | 0.10 | the target is a money page that still needs links |
| authority | 0.10 | the target already earns links in its cluster and has rich keywords |
| placement | 0.10 | a closely matching paragraph sits early in the source post |
| orphan | 0.08 | the target has few or no inbound links |
| gsc | 0.08 | the target ranks in "striking distance" in Search Console |
| depth | 0.06 | the target is many clicks from the home page |
| tier | 0.06 | the link goes from a broader page to a deeper one |
| cluster | 0.06 | both posts are in the same silo (best: the target is the pillar) |
| velocity | 0.04 | the target is old but has gained few links |

If a site has no Search Console data, no money pages, or a flat URL structure, those signals are switched off and
their weight is shared over the others, so scores stay comparable between sites.

> **Tip:** The score ranks candidates; it is not a guarantee. Read the anchor and the paragraph before you approve.

## Anchors

The **anchor** is the clickable text. SiloForge picks a natural noun phrase that already exists in the source
paragraph, such as "insulated steel bottle", and checks that it matches the target page. It never links inside a
heading, code, a button or an existing link, avoids reusing an anchor in the same post, and keeps exact-match anchors
from dominating. See [Anchors](anchors).

## Density limits

Defaults per post: at most **10** contextual links, at most **5** new links in one pass, and a post with fewer than
**3** is flagged as under-linked. Change them in **Settings → Linking Rules**. Limits are applied after scoring, so the
best candidates are the ones kept. Suggestions pointing at a money page are never dropped by the cap.

## Insert modules

How a link is placed in the text:

| Module | What it does | Needs AI? |
|---|---|---|
| `native` | Wraps an existing phrase in the paragraph. Your text is unchanged apart from the `<a>` tag. | No |
| `rephrase` | Rewrites one sentence so it carries the link. | Yes |
| `context` | Adds a short bridging sentence with the link. | Yes |
| `content_gap` | No natural anchor exists. Reported; written only when an AI model can place it. | Yes |

The default is native only, the safest choice. About a quarter of suggestions on a typical site are `content_gap`.
