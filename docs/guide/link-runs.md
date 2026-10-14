---
title: Link Runs
section: Linking
order: 4
---

# Link Runs

A **Link Run** is the guided way to link a whole site in one go: crawl, see the opportunities, tick the strategies you
want, pick a linking mode, preview every link, deploy them, check each one on the live page, and fix the ones that did
not land. Every run is stored with its options, its links and its results, so you can look back at it or undo it as a
whole.

Open it from the site menu (**Site → Link Run**), or with **New Link Run** on the Dashboard or the Tools page.

> **Note:** Nothing reaches your site until you click **Deploy N links** in step 6 and confirm. Steps 1-5 only read
> your site and build a preview.

## The nine steps

| Step | What you do | Changes your site? |
|---|---|---|
| **1 Crawl** | See when the site was last crawled. Click **Crawl now** or **Use last crawl**. | No |
| **2 Opportunities** | Cards with counts: orphans, under-linked posts, semantic pairs, keyword-rule hits, broken internal links, silo gaps. | No |
| **3 Strategies** | Tick where links may come from, and set the filters. | No |
| **4 Linking mode** | Pick the shape of the links (link opportunity or a silo topology). | No |
| **5 Run** | **Start run**, then a progress bar through the phases (Crawl, Plan and score, Ready). The run ends as **Ready to deploy**. | No |
| **6 Review** | All proposed links grouped by post. Untick, edit anchors, pick an alternate anchor. Then **Deploy N links**. | No (until you click deploy) |
| **7 Deploy** | Progress while the links are written and checked. | **Yes** |
| **8 Results** | Per post: each deployed link with its status, anchor, target and **Open ↗**; **Verify again** and **Undo run** at the top. | No |
| **9 Fix** | **Apply properly to N items** retries everything that failed or is missing. | **Yes** |

### Step 1: Crawl

- **Crawl now** reads the site again. Choose it if you published or edited posts since the last crawl.
- **Use last crawl** skips the crawl. Fine if the last crawl is recent (the page shows its age).
- **When the run starts** decides what the run itself does: **Re-crawl if stale** (crawl first when the last crawl is
  older than 24 hours), **Always crawl** or **Use last crawl**. Via the API the option is
  `crawl: if_stale | always | skip`.

### Step 2: Opportunities

A quick overview of what is worth doing on this site. Use it to decide which strategies to tick: lots of orphans →
tick **Orphan rescue**; broken internal links → tick **Fix broken links**; many keyword-rule hits → keep **Keyword rules** on.
**Semantic pairs** needs embeddings, which the first plan creates: on a site that was only crawled it shows 0 until the
first run has planned.

### Step 3: Strategies

Tick at least one. Each strategy is a source of candidate links; the run merges them, scores them and keeps the best.

| Strategy | What it means | Default |
|---|---|---|
| **In-text** | Links a phrase that already exists in the paragraph. Your text is not changed apart from the `<a>` tag (the `native` module). The safest choice. | On |
| **Contextual** | An AI model writes or rewrites one sentence that carries the link (the `context` / `rephrase` modules). Needs an AI chat model (see [AI models](ai-models)). Use it to reach targets that have no natural anchor. | Off |
| **Semantic** | Finds candidates by meaning: paragraphs that are close to another post's topic (embedding similarity). The main source of good ideas. | On |
| **Keyword rules** | Uses the site's keyword → URL rules (see [Keyword rules](keyword-rules)). | On |
| **Silo** | Adds the links your chosen silo topology requires (for example every post → pillar). Has no effect in the *Link opportunity* mode. | On |
| **Orphan rescue** | Puts posts with no or very few contextual inbound links first, so they get links before the budget runs out. | On |
| **Money pages** | Boosts targets listed as money pages in **Settings → Linking Rules → Targets**. | Off |
| **Fix broken links** | Replaces links that point to broken internal URLs with the best live target (uses the last [link health](link-health) check). Run a link-health check first. | Off |

> **Tip:** A good first run: **In-text + Semantic + Keyword rules + Orphan rescue**. Add **Contextual** later, once you
> trust the AI output on a few posts.

### Step 3: Filters

| Filter | Meaning | Recommended start |
|---|---|---|
| **Minimum score** | Drop candidates below this composite score (0-1). | `0.55`; `0.65` on a site you care a lot about |
| **Max links per post** | Total contextual links a post may have after the run. | `5` (raise to 8-10 on long posts) |
| **Max new per post** | New links this run may add to one post. | `3` (first run: `2`) |
| **Max total links** | Upper limit for the whole run. | `500`; first run on a site: `50-100` |
| **Anchor types** | Allowed types: exact, partial, LSI, branded. | All four |
| **Exact-match share** | Share of exact-match anchors allowed. | `35%` (lower to `25%` on sensitive sites) |

Under **Scope: clusters, post types and exclusions**:

| Filter | Meaning | Recommended start |
|---|---|---|
| **Clusters** | Limit the run to some silos (empty = all). | Empty (whole site) |
| **Post types** | Which types take part. | `post`, `page` |
| **Only these post IDs** / **Skip these post IDs** | Limit the run to some posts, or leave some out. | Empty |
| **Skip URLs matching** | One pattern per line; never link from or to these URLs, for example `/privacy`, `/cart`. | Legal and checkout pages |

The filters decide which links are **ticked** for deploy; the run still lists the others in Review, unticked, so you
can add them by hand.

### Step 4: Linking mode

The mode decides the *shape* of the links.

| Mode | Link pattern | Choose it when |
|---|---|---|
| **Link opportunity** | The best-scoring links anywhere on the site, no silo shape | You want the most relevant links and do not run strict silos. Good for blogs and first runs. |
| **Reverse silo** | Every post → its pillar | You want link equity to flow up to hubs or money pages. The safe silo default. |
| **Serial** | Post 1 → post 2 → ... → pillar | Clusters are series or step-by-step guides. |
| **Priority** | Every post → pillar, and pillar → its top 3 posts | You want a strong hub that also promotes its best supporting posts. |
| **Circle** | A ring through the cluster, back to the pillar | Every post should get exactly one link from a sibling. |
| **Bidirectional** | Pillar ↔ every post | Small, tight clusters. Produces the most links; keep **Max new per post** low. |

The silo modes are the same topologies as in [Plan and silos](plan-and-silos). If in doubt: **Link opportunity** for a
general blog, **Reverse silo** for a site built around pillar or money pages. The **Silo** strategy needs a silo mode;
with **Link opportunity** untick it.

Three switches sit under the modes: **Refine anchors with AI** (the AI polishes the top anchors; slower, optional),
**High priority (stronger AI model)** (every AI task of this run uses the high tier; costs more tokens, use it for
money pages and important launches) and **Check every link on the live page** (verify after deploy; leave it **on**).

With **Settings → AI Models → Always use AI** turned on, AI refinement is used for every run even when **Refine anchors
with AI** is off. See [AI models](ai-models).

### Step 5: Run

Check the summary, give the run a **Name (optional)** and click **Start run**. The summary shows an **AI estimate**:
the tokens (in and out), the number of AI calls and the cost the run is expected to use, the budget left today per tier
and for this site, and whether **Always use AI** is on. The estimate comes from your recent average use per task (or
default averages on a new install) multiplied by the number of links. When the run may need more than the remaining
budget, a warning says so: the run still works, and whatever the budget cannot cover is linked with heuristics only.
With no AI switched on the estimate says "No AI calls planned: heuristics only".

Check the summary, give the run a **Name (optional)** and click **Start run**. The run crawls (if chosen), plans and
ticks the links that pass your filters; when it is ready the page moves on to Review by itself. You can leave the page;
the run keeps going and shows up in the **History** tab. **Cancel run** stops it while it is crawling or planning.

### Step 6: Review

Links are grouped by source post. For each link you see the anchor in its sentence, the target, the strategy that
found it and the score.

- Posts are collapsed; click a post title or **Expand all**. The tick box next to a post ticks all its links.
- **Untick** links you do not want. By default every link that passed the filters is selected; **Select all** /
  **Select none** change all of them.
- **Edit anchor** (type, then Enter) or pick one from **Alternates…** if the chosen one reads badly. In a run without
  **Contextual** the anchor must be a phrase from the same paragraph; only such alternates are offered.
- The counts at the top update as you go. When happy, click **Deploy N links** and confirm.

Review tips:

- Read the anchor *in its sentence*: it must read naturally and describe the target page.
- Look out for one target that appears in many posts; lower **Max new per post** or untick some.
- Check a few **Contextual** links closely: AI-written sentences must sound like the rest of the post.
- Untick links from legal, contact or checkout pages if you forgot to exclude them.
- On a first run, deploy a small batch (20-50 links), check them in step 8, then run again with a larger **Max total**.

### Step 7: Deploy

Deploy uses the same safety as a normal [inject](inject-and-undo):

1. **Raw content.** Each post is read again just before writing, in raw form, so Gutenberg blocks and HTML survive.
2. **Exact position.** The link goes into the exact paragraph (checked by hash). If the paragraph changed, that link
   is not written.
3. **Your edits win.** If someone edited the post in WordPress meanwhile, SiloForge does not overwrite it.
4. **Snapshots.** Every post is snapshotted before it is written.
5. **Verify.** After deploy, each link is checked on the live page.
6. **Undo run.** **Undo run** (on Results, or next to the run in the **History** tab) removes every link this run
   added, exactly, and nothing else.

A deploy can end as **Deployed** (all applied) or **Partly deployed** (some items need step 9). **Stop deploy** stops a
deploy in progress.

### Step 8: Results

Each post lists the links this run deployed (unticked suggestions stay in Review) with a status badge:

| Badge | Meaning |
|---|---|
| **Applied** | Written and found on the live page. |
| **Failed** | Not written; the row shows why (for example the anchor is no longer in the post). |
| **Not on live page** / **Not present** | Written, but the live check did not find it (edited away, a cache or a page builder). |
| **Skipped** | Given up by step 9, with the reason. |
| **Reverted** | Removed by **Undo run**. |

An **AI summary** above the list shows what the AI did for this run: calls, tokens in and out, cost, cache hits, how
many edges were skipped because the heuristic was already confident, errors, and, if it happened, why the run fell
back to heuristics (for example `budget exhausted` or `ai unavailable`). Every call is also on the [Logs](logs) page
(source **AI**).

Filter with **All / Applied / Failed / Not present / Skipped**. **Verify again** re-checks the live pages (useful after
you clear a cache or edit a post). When something needs work, **Fix them** opens step 9.

**Open ↗** opens the live post in a new tab and jumps straight to the link, highlighted. It uses a *text fragment*
(the post URL plus `#:~:text=` and the anchor, for example
`https://example.com/best-water-bottles/#:~:text=an-,insulated%20steel%20bottle,-keeps%20drinks`). Up to three words
before and after the anchor are added, so the link is found even when the same words appear earlier on the page (title,
table of contents, product tables). This works in **Chrome, Brave and Edge**. In other browsers the page opens at the
top; use Ctrl+F with the anchor text.

### Step 9: Fix ("Apply properly")

**Apply properly to N items** takes every item that is failed, not present or unverified and tries, in order:

1. **Re-check.** Reads the raw content again. If the link is actually there (the earlier check was wrong or
   cached), the item becomes `applied`.
2. **Find the paragraph again.** Looks for the paragraph by its hash, then by searching the anchor text across the
   post, and inserts the link there (in-text).
3. **Alternate anchors.** Tries the link's alternate anchors in the original or a nearby paragraph.
4. **Contextual.** If **Contextual** was ticked for the run and an AI model is available, writes a sentence that
   carries the link (validated before writing).
5. **Skip with a reason.** If nothing works, the item becomes `skipped`.

Every attempt is snapshotted, logged in **Workspace → Logs**, and verified afterwards.

Reasons an item can still be skipped:

- **Anchor text no longer in post**: the post was edited and the phrase is gone, and no alternate fits. Re-crawl and
  run again.
- **Post locked or changed**: someone is editing it in WordPress, or it changed during the repair. Try later.
- **No WordPress credentials**: the site has no application password. Add one in [Sites](sites).
- **Live check still fails**: usually a cache or a page builder that stores text elsewhere. Clear the cache and click
  **Verify again**.
- **Contextual not available**: the run did not tick Contextual, or no AI model is configured.

## Runs history

The **History** tab of the Link Run page lists every run of the site: status, counts (selected, applied, failed,
repaired), when, and who started it (**Auto** for the [Auto-Linker](auto-linker)). **Open** a run to see its results again, or
click **Undo run** to remove all links it added.

A run that is **Ready to deploy** keeps its links: a newer run does not take them over. **Discard** a ready run you will
not deploy (for example an old Auto-Linker dry run) to free its links for the next run.
