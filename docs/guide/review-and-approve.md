---
title: Review and approve
section: Linking
order: 3
---

# Review and approve

Approval is the gate between a suggestion and your live site. Approving only marks a link as ready; nothing is
written until you [inject](inject-and-undo).

## The Tools tabs

Open the site's **Tools** page. Each tab is a different view of the same suggestions.

| Tab | Shows | Use it to |
|---|---|---|
| **Inbound** | Suggestions grouped by the page they point **to** | Give orphan and weak pages more incoming links |
| **Outbound** | Suggestions grouped by the page they come **from** | Give dead-end pages outgoing links |
| **Silo** | Suggestions that follow the cluster topology | Strengthen your topic silos |
| **Archive Silo** | Links between posts and category/tag archives | Connect archives to their posts |
| **Bulk** | The approved queue, insertion settings and the **+ New Bulk** wizard | Inject links |
| **Anchors** | Anchor text and type of each suggestion | Fix weak or repetitive anchors |
| **Keyword Linker** | Keyword → URL rules | See [Keyword rules](keyword-rules) |
| **Ignore Keywords** | Phrases never used as anchors | Brand safety |

Use the filter box to narrow a list by source or target title or URL.

## Approve one by one

1. Read the row: source post, target post, anchor, score.
2. Click the **score** to open the breakdown: the 11 signals, their weights, and the anchor details
   (how the anchor was chosen, its similarity to the target, and the AI's verdict if AI refinement ran).
3. Click **Approve** or **Reject**. Tick several rows to decide them together.

> **Tip:** Good suggestions have an anchor that reads naturally in the sentence *and* describes the target page.
> If only the anchor is weak, edit it in **Tools → Anchors** instead of rejecting the link.

## The Bulk wizard

**Tools → Bulk → + New Bulk** walks you through a larger batch in five steps:

1. **Fetch**: choose the posts to link **from**. Scope by all posts, cluster, post type or search; tick
   **Orphans only** if wanted; or import a list of URLs.
2. **Match**: click **Start matching** to score targets for those posts (or **View results** if they are scored already).
   You can choose AI anchors here too.
3. **Progress**: watch the matching job.
4. **Results**: review each source → target with its score. Use the **Min relevancy** slider, then **Approve** rows,
   **Approve selected** or **Approve all ≥ threshold**. Export as CSV or JSON if you want a record.
5. **Link**: click **Start Linking** to inject the approved links.

## Shortcuts

- **Dashboard → Quick Fix Orphans / Quick Fix Deadends** create suggestions for those pages (running a plan first if
  needed) and open the matching approval queue.
- **Reports → Links**: tick posts and **Send selection to Bulk**, or **Export filtered to Bulk**. This approves their
  suggested links and sends them to the Bulk queue.

## Good habits

- Approve explicit lists of links, in batches you can check. Do not blindly approve everything on a site that has an
  old backlog of approved links.
- Look at **Tools → Anchors** for the anchor-type mix before a large inject.
- Reverted links can be approved again later.
