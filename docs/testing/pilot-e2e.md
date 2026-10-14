# Pilot E2E: SiloForge v2 against a local WordPress (2026-10-13)

The first end-to-end run of v2 against a real (local) WordPress site: crawl, plan, review, inject, verify,
link health, undo, plus the operational paths (cancel, scheduler, re-crawl, jobs/logs/resources,
notifications, backup/verify/restore). No public site was used as a target. See the note on external
link checks under "Safety" below.

## Setup

| Item | Value |
|---|---|
| WordPress | `app/dev/site69-local` (WP 7.0.1, PHP 8.2, MariaDB 11) on http://127.0.0.1:8391, siteurl `http://site01.example:8391`; 353 published posts + 7 pages (+1 draft page); 11 posts are Gutenberg (block comments), the rest are classic HTML with Amazon comparison tables and review lists; 176 internal links, ~15.4k affiliate links |
| Reset point | `D:\tmp\sf-pilot\wp-before.sql` (mariadb-dump). Restored twice; the md5 of a fresh dump matched the original both times (`6557ef20...`) |
| WP changes for the test | `WP_ENVIRONMENT_TYPE=local` in wp-config (application passwords are disabled on plain http otherwise), plus one application password for `admin`. Both were removed at the end: the constant was deleted, and the DB restore removed the password |
| v2 home | `D:\tmp\sf-pilot-home`: fresh `siloforge init`, admin via `user set-password --create`, an API token (`token create --scopes '*'`) for MCP, ORT DirectML 1.24.4 DLLs in `runtime\`, Xenova MiniLM in `models\all-MiniLM-L6-v2\` |
| Config | binds `127.0.0.1:8620`, `embed.chain = ["onnx"]` (no OpenRouter), `crawl.allow_private_hosts = true`, `backup.schedule = ""`, `llm.fallback_models = []` (no dario/LLM calls) |
| Doctor | all ok; embeddings via onnx (DirectML), cosine 1.000000 |
| Drivers | REST via curl, MCP via a minimal stdio JSON-RPC client (`D:\tmp\sf-pilot\mcp.py`) that spawns `siloforge mcp` with `SILOFORGE_API_URL` + token |

## Runs

There were three passes. Pass 1 used the original build and surfaced the bugs. Passes 2 and 3 used
fixed builds, each starting from a fresh DB and a restored WordPress. The numbers below are from
pass 3 (final build) unless they are labelled pass 1.

| Step | Path | Result |
|---|---|---|
| add site, set creds | MCP `siloforge_add_site`, REST `PATCH /sites/:id` | `has_wp_credentials: true`; the app password is never returned (grep of GET output: 0 hits) |
| crawl | MCP `crawl_site` + `wait_for_job` (`job_run_id` alias) | 360 posts, raw content, 3.1 s, 146 existing links synced |
| plan reverse | MCP `run_pipeline` | 14,573 paragraphs embedded (DirectML), 1,733 suggestions, 48.8 s wall (embed 27 s), peak RSS 279 MB |
| cancel plan | MCP `cancel_job` during embed | `cancelled` within one checkpoint; `resources` recorded (fixed); the next plan resumed embedding from the persisted batches |
| plan bidirectional | MCP `plan_site` (was refused, fixed) | 1,725 rescored, 29 bidirectional edges, 476 dropped by density, 0 duplicate pairs, 0 shared native paragraphs (fixed) |
| approve 13 | MCP `bulk_decide` | 11 native (4 Gutenberg incl. a link inside a `wp:list-item`, 1 reverse-topology), 1 anchor edited away in WP, 1 content_gap |
| edit WP between plan and inject | raw DB edit of 2 posts (anchor text changed in 7864; unrelated text in 7351) | 7864: not written, edge stays approved with reason (fixed). 7351: link added **and** the WP edit kept |
| inject | MCP `bulk_inject` | 11 applied, 11 verified, 2 marked (with reasons), 0 failed, 4.8 s |
| verify in WP | `wp post list ... post_content` diff | exactly 11 posts changed; each diff is the anchor wrapped in `<a href=...>`; block comment counts unchanged (post 28: 82→82, 31: 11→11, 30: 2→2, 8433: 12→12) |
| link health | scheduler run with payload `{"internal_only": true}` (new) | 12,521 links; 178 internal rows checked (2 real 404s: `/terms-of-service/`, `/disclaimer/`; 2 redirects), 12,343 external rows listed, unchecked |
| undo | MCP `bulk_undo` | 11 reverted, 0 not present; WP content **byte-identical** to the pre-inject state (diff: 0 posts) |
| re-crawl (pass 1) | REST + scheduler cron `*/1` | 359 skipped, 1 upserted (the post edited in WP), 2 re-chunked; the scheduled crawl stamped `last_run_at`/`last_job_run_id`/`next_run_at`, `requested_by=schedule`, 0 upserted |
| jobs/logs | `GET /api/v2/jobs`, `/jobs/:id/logs` | every run has resources (wall, cpu s, avg cpu, peak RSS), logs per phase incl. timings |
| notifications | `GET /api/v2/notifications` | rows for job.done, plan.ready, inject.done (+ a cpu system.alert in pass 1) |
| backup | MCP `create_backup`, REST create + verify, CLI `backup verify` | ok, 3 files (db, toml, secrets), sha256 manifest |
| restore | CLI `restore` into `D:\tmp\sf-pilot-restore` | counts identical (sites 1, posts 360, paragraphs 14,573, edges 1,879); a second restore kept `.pre-restore-<ts>` copies. `runtime/` and `models/` are not part of a backup (doctor in the restored home fails on embeddings until they are copied) |

### Before/after excerpts (pass 3)

```
28 (Gutenberg list item):
  <!-- wp:list-item -->
  <li><a href="http://site01.example:8391/home/">Identify common bike</a> problems and questions</li>
  <!-- /wp:list-item -->
31  @680: -[Cycling tech changes] +[<a href=".../about-us/">Cycling tech changes</a>]   blocks 11->11
7351 @42: -[rechargeable bike lights] +[<a href=".../best-bike-with-lights/">rechargeable bike lights</a>]
          (+ the operator's edit "ride with total confidence" made after planning is preserved)
7982 @996: -[Dry Bike Chain Wax] +[<a href=".../best-dry-chain-lube-for-bikes/">Dry Bike Chain Wax</a>]
after undo: changed posts vs pre-inject = []
```

Target URLs use the site's canonical `site01.example:8391` permalinks even though the site was
added as `127.0.0.1:8391`, which is correct. The host alias resolves the crawl.

## Suggestion quality

Structural checks over all 1,733 suggestions (pass 3): 0 self-links, 0 duplicate pairs, at most 5 per
source (density), 0 paragraphs with two native suggestions, and every native anchor is verbatim in its
paragraph (1,255 exact case plus 9 differing only in case). 469 edges (27%) are `content_gap`: their anchor
is the target's cleaned title and needs an AI rephrase/context model to be inserted. With no model
configured, they are reported (see fix 6), not written.

Sample of 15 (random, seed 11, pass 3). Relevance is judged by hand.

| Source → target | Anchor [type, module] | Score | Verdict |
|---|---|---|---|
| Quad Lock Phone Mounts → Mountain Bike Phone Mounts | "Quad Lock Motorcycle Handlebar Mount" [lsi, native] | 0.70 | weak: product name → category roundup |
| Bike Lights → Bikes With Lights | "2026’s top bike lights" [lsi, native] | 0.75 | ok target, clumsy anchor (year possessive) |
| Brake Pads → Disc Brake Pads | "AGPTEK Bike Brake Pads" [lsi, native] | 0.73 | plausible |
| Cycle Shoes → Cycling Shoes for Women | "Peloton Cycling Shoes featuring Delta cleats" [partial, native] | 0.76 | weak: 6-word clause |
| Quad Lock Phone Mounts → Bike Mounts for Mobile Phones | "phone mount withstand daily abuse" [lsi, native] | 0.71 | relevant target, bad anchor (contains a verb) |
| Pedal Power Bikes and Skis → E-Bikes Without Pedals | "Folding Electric Bike 48V" [lsi, native] | 0.72 | poor relevance |
| Cycling Shoes Wide Feet → Cycling Shoes (Pedal Perfection) | "choosing cycling shoes" [lsi, native] | 0.68 | good |
| Downhill Full Face Helmets → Enduro Full Face Helmets | "full-face safety" [lsi, native] | 0.75 | good |
| MTB SPD Pedals and Shoes → Cycling Shoes for SPD Pedals | "Cycling Shoes for SPD Pedals" [partial, content_gap] | 0.76 | good (needs AI to place) |
| Balance and Pedal Bikes → Balance Bikes for Toddlers | "balance bike" [partial, native] | 0.66 | good |
| Water Bottles → Bike Bottles and Cages | "Bike Water Bottle" [lsi, native] | 0.74 | good |
| Bike Chain Tools → Chain Guards | "need dependable bike chain" [lsi, native] | 0.68 | poor: weak relevance and a verb-led anchor |
| Bike Pedals and Straps → Bike Pedals | "aluminum alloy bodies" [lsi, native] | 0.70 | weak anchor |
| Phone Mounts for Bike → Bike Mounts for Phones | "Bike Mounts for Phones" [partial, content_gap] | 0.77 | good, near-duplicate pages (cannibalization) |
| Bikes That Haul Trailers → Bike Trailers | "match heavy-duty trailers" [lsi, native] | 0.77 | relevant target, verb-led anchor |

Honest summary: target selection is mostly sensible within this single-niche site (semantic 0.65 to 0.9),
with about 2 in 15 off-topic. Anchor text is the weak spot. About half of the native anchors are
natural noun phrases. The rest contain verbs ("withstand", "need", "match"), are long clauses, or are
product names. Before production use, an operator should review anchors or edit them in the UI. The
heuristic noun-phrase chunker (no POS model) is the root cause. See open issues.

Pass 1 was worse and is now fixed. Title-fallback anchors read like "Bike Locks With Alarms for (Loud",
"Comfort Saddles for (Ride in Total" and "Bike Pedal Clips and Shoes for". 625 of 7,813 paragraphs were
glued table/review text ("Most Fun FeatureWheel Size", "Road PedalBest for BeginnersView Latest
Price"). Anchors crossed `<li>` boundaries ("information Identify common bike"), ended in possessives
("Bike Handlebar Bag’s"), and 5 paragraphs had two native suggestions.

## Bugs found and fixed (with regression tests)

| # | Crate | Bug | Fix | Test |
|---|---|---|---|---|
| 1 | sf-engine anchor | `clean_title` removed years after the clause cut and trimmed stop words before the word cap, giving title anchors like "Bike Locks With Alarms for (Loud" | Strip a leading `[..]`; cut at the first clause word, year, `(`/`[` or separator; cap; then trim dangling stop words and edge punctuation | `clean_title_pilot_regressions` |
| 2 | sf-engine html | Prose splitter kept `<table>` as one paragraph (glued cell text, anchors across cells) | Tables skipped in Prose mode. Splitting per cell added about 3,500 spec lines ("Material: ...") instead | `prose_skips_tables` |
| 3 | sf-engine html | Review lists (`<ol><li><h2/><div/><p/>…</li>`) were one giant glued paragraph per item | Lists whose items hold block children are walked as sections (each `<p>` is its own paragraph). Plain lists are unchanged | `prose_lists_of_sections_split_into_their_paragraphs` |
| 4 | sf-engine html | Anchors spanned `<li>` boundaries | List-item separators are emitted as blocked 1-char spans (text and hash unchanged) | `prose_list_item_boundaries_are_blocked` |
| 5 | sf-engine nlp | Phrases ended in a possessive ("Bag’s") | Possessive tail stripped from candidate phrases | `possessive_tail_is_stripped` |
| 6 | sf-jobs inject | An edge whose anchor is gone (WP edited after planning), or a content_gap edge with no AI model, was counted as "marked" with no error and stayed approved silently. MCP docs say "marked = plugin pull" | `error` now holds the reason, plus a job warning with the count | `inject_anchor_edited_away_is_marked_with_a_reason` |
| 7 | sf-jobs anchors | Guard swaps (diversity/over-optimisation) and re-located existing suggestions put two native links in one paragraph (5 in pass 1, 31 after a bidirectional re-plan) | Post-guard claim per source. Existing suggestions are re-located only in free paragraphs. A swapped anchor that collides becomes content_gap | assertions in `plan_suggests_valid_unique_edges_and_respects_rejections` (incl. bidirectional re-plan) |
| 8 | sf-jobs link_health | Every external URL was checked, with no opt-out (15k affiliate links on this site) | Payload `internal_only` / `internalOnly`: external rows listed, never requested (usable from schedules) | `link_health_internal_only_skips_external_checks` |
| 9 | sf-jobs runner/queue | A job cancelled while running kept `resources: {}` | `Queue::record_cancelled_resources`, called on the cancel and abort paths | `cancel_mid_plan_...` asserts `wall_ms` |
| 10 | sf-mcp | `siloforge_plan_site` refused `topology: bidirectional` (the server accepts all 5) | Enum extended | `each_tool_maps_to_method_path_body` |
| 11 | sf-mcp | Enqueue tools return `job_run_id`, but the job tools required `job_id`, and agents echo the former | `job_run_id` accepted as an alias of `job_id` | same test |

Verification: `cargo test -p sf-engine -p sf-jobs -p sf-mcp` passes (146 + 6/14 + 8/11), and
`cargo clippy -p sf-engine -p sf-jobs -p sf-mcp --all-targets -D warnings` is clean.

## v1-route issues (not fixed here: `crates/sf-server/src/v1/**` is owned by another agent)

- `POST /api/v1/bulk/undo` without `edge_ids` reports `count` over all `applied` edges, including the
  146 `origin='existing'` body links (157 reported vs 11 reverted). The undo pipeline correctly skips them.
  The count (and the MCP output) should exclude `origin='existing'`.
- `POST /api/v1/schedules/:id/run-now` builds the payload only for audit/plan (`job_payload`) and drops the
  stored schedule payload for other kinds. A `link_health` schedule with `{"internal_only": true}` ran
  *with* external checks on run-now. The cron path (sf-jobs scheduler) passes the payload correctly.
- `POST /api/v1/sites/:id/link-health` cannot pass `internal_only`. Suggest accepting an optional body or a
  site setting (`link_health_internal_only`) in the settings whitelist.

## Safety note

In pass 1, a REST `link-health` run (v1 route, payload `{}`) started checking external links. It was
cancelled after about 10 s at 4 req/s, so at most about 40 HEAD/GET requests reached third-party hosts
from the page's links (affiliate/shop URLs; which hosts was not recorded). The later `run-now` attempt was cancelled after
4 s, still in the extraction phase. All further link-health runs used `internal_only` through the
scheduler. Nothing was written to any site other than the local test WordPress, and the containers
`wh-wp`/`wh-db` and the v1 stack were not touched.

## Open issues

- Anchor quality: the noun-phrase chunker lets verbs in ("phone mount withstand daily abuse", "need
  dependable bike chain") and allows repeated tokens ("MIPS feature MIPS"). A leading/inner-verb filter or a
  small POS model would help most. Product-name anchors pointing to category roundups are frequent on
  affiliate sites.
- 27% of suggestions are content_gap and need an AI model. Without one they are reported but never
  written.
- Splitter changes do not re-chunk existing sites: `chunked_hash` keys on the content hash only. Consider a
  splitter version in the key (this would re-chunk every site once; paragraph ids/embeddings survive by
  text hash).
- Approved/applied edges do not reserve their paragraph against new suggestions. Two approvals in
  separate plan runs can put two links in one paragraph.
- The scheduler ticks every 60 s, so a `*/1` cron fires up to 59 s late.
- MCP has no link-health tool. `restore` does not bring back `runtime/`/`models/` (document it or offer
  `fetch-runtime`/`fetch-model` in the restore output).
- "compute pool ready" is logged twice at startup (cosmetic).
- `/tmp/edit.php` (the test helper) remains in the stopped `site69-local-cli-1` container's own filesystem
  (not in a volume, root-owned). Removing the container drops it.

## Artifacts (D:\tmp\sf-pilot)

`wp-before.sql`, `content-*.json` (raw post content snapshots), `diffc.py`, `analyze.py`, `sample.py`,
`mcp.py`, `run1/` (pass 1 db/log/edges), `sample-final.txt`. Pilot home: `D:\tmp\sf-pilot-home`; restore
test: `D:\tmp\sf-pilot-restore`.
