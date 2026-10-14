# Link Runs E2E: v2.2 UI + backend against a local WordPress (2026-10-14)

The first real end-to-end run of v2.2 **Link Runs** and the **Auto-Linker**: the real UI (Playwright driving
headless Chromium) against the real backend and the local test WordPress, plus the MCP tools. No public site was
contacted. Production (127.0.0.1:8600/8601, `D:\Final_Apps\SiloForge`) and `wh-wp`/`wh-db` were not touched.

## Setup

| Item | Value |
|---|---|
| WordPress | `_archive/v1/app/dev/site69-local` (docker compose), http://127.0.0.1:8391, canonical `http://site01.example:8391`; 353 posts + 7 pages + 1 draft |
| Reset point | `D:\tmp\sf-e2e22\wp-before.sql` (mariadb-dump, md5 `49e7a30a...`) taken before anything else; restored at the end. `wp-config.php` saved and restored too (`WP_ENVIRONMENT_TYPE=local` was needed for application passwords on plain http) |
| Build | current tree (`npm ci` + `npm run build`, `cargo build --release -p siloforge`), target `D:\tmp\cargo-target\agent-e2e22`; rebuilt after each fix |
| Home | `D:\tmp\sf-e2e22-home`: `siloforge init`, admin via `user set-password --create`, ORT DirectML runtime + MiniLM model copied from production, `embed.chain = ["onnx"]`, `[crawl] allow_private_hosts = true`, `llm.fallback_models = []`, backups off |
| Server | `SILOFORGE_BINDS=127.0.0.1:8680,127.0.0.1:8681`, stopped by PID |
| Site | added in the UI (**Sites → + Add Site**: `127.0.0.1:8391`, WordPress URL `http://127.0.0.1:8391`), credentials via the **Connection** dialog (WordPress username + application password; never printed); admin site setting `allow_private_host: true` via `PATCH /api/v1/sites/{id}`; 3 keyword rules (`chain lube`, `insulated water bottle`, `road bike helmet`) via the API |
| AI | provider `dario` (`http://127.0.0.1:3460/v1`, no key), model `claude-haiku-4-5`, one active chat chain. 9 AI calls in total (cap 20) |
| Browser | Chromium 1243 (`D:\Programs\ms-playwright`), headless, CDP, `--host-resolver-rules="MAP site01.example 127.0.0.1"` so the canonical post URLs open; `playwright-core` in `D:\tmp\sf-e2e22\pw`; step scripts in `D:\tmp\sf-e2e22\steps` |
| MCP | `siloforge mcp` over stdio with an API token (`token create --scopes '*'`), client `D:\tmp\sf-e2e22\mcp.py` |
| Content checks | `wp eval` dump of every `post_content` (`snap.sh`) before each deploy and after each undo, plus `wp post get <id> --field=post_content` spot checks |

## Runs

| # | Run | What happened |
|---|---|---|
| 0 | Login, add site, Connection dialog, **Crawl now** | 360 posts in 6.5 s. Opportunities: 0 orphans, 360 under-linked (bug 1; after the fix 289 orphans, 340 under-linked), 19 keyword-rule hits |
| 1 | **In-text + Semantic + Keyword rules + Orphan rescue**, **Link opportunity**, Max new per post 2, Max total links 40 | Plan 33 s (14k paragraphs embedded on DirectML), 629 items, 40 ticked. Review grouped by post: unticked 3, **Edit anchor** "MIPS Urban Bike Helmet" → "Urban Bike Helmet", **Alternates…** "road helmets" → "Budget Road Bike Helmets". **Deploy 37 links** into 30 posts in 18.6 s: 36 applied, 1 not written (the picked alternate was a title phrase, not in the paragraph: bugs 5, 6). Live verify: all 36 inconclusive (bug 7); after the fix **Verify again** = 36/36 present |
| 1 | Open ↗ | 5, then 12 links opened from Results: every opened URL carries `#:~:text=` with the anchor and the anchor is an `<a href=target>` in the live HTML. With the original fragments only 1 of 5 landed at the link (bug 9). With prefix + suffix: 10 of 12 in the viewport, 1 still on an earlier product-table occurrence ("…Light Set’s" had no usable suffix), 1 just below the fold. After the glued-suffix fix (run 2 links, 8 checked): 8 of 8 on the right occurrence, 6 in the viewport, 2 ~50 px below the fold because lazy images load after the scroll |
| 1 | Edit WP | post 7207: deleted the anchor text "Signal Cycling Helmet" (with its link); post 6963: unwrapped the inserted link. **Verify again**: 34 applied, 2 Not present, 1 Failed; **Fix them** → **Apply properly to 3 items**: 2 repaired (7207: the phrase also occurs in another paragraph and was linked there; 6963: re-linked in place), 1 skipped `anchor text no longer in post` |
| 1 | **History → Undo run** | 36 reverted in 30 posts. Content vs the pre-deploy dump: identical in 360 posts; 7207 differs only by the manual deletion of "Signal Cycling Helmet". `wp post get` spot checks (6963, 7207, 28, 31, 7351) agree |
| 2 | **+ Contextual + Silo**, **Reverse silo**, Max new per post 1, Max total links 12 | Strategy cards now show counts (6,572 semantic pairs, 19, 342 silo gaps, 289; bug 3). 341 items, 12 ticked, 1 unticked, one anchor replaced by free text (accepted: Contextual is on). Deploy 11: 8 applied (4 by AI context sentences), 3 failed: 1 AI reply rejected (`block element <p> in a context sentence`, validator working), 2 native items of a contextual run ended as "no AI insertion model" content gaps (bug 13). **Apply properly**: 3/3 applied (AI). 11/11 verified. 7 AI calls |
| 2 | Undo run | 11 reverted, but 5 posts kept the AI-written sentences (bug 14) and two sentences had been placed between `</li>` and `</ul>` (bug 15). Those posts were put back by hand from the dump |
| 3 | Same as run 2 after the fixes, Max total links 8 | 4 applied (native), 2 failed because dario now answers HTTP 403 `oauth_not_allowed_for_organization` for `claude-haiku-4-5` (provider side, not SiloForge), 2 failed `paragraph changed` (my out-of-band restore of run 2's posts made the local copy stale; correct behaviour). Undo: **byte-identical** to the pre-deploy dump (0 of 361 posts differ) |
| 4 | **Auto-Linker** card (Settings → Linking Rules, opened with **Auto-Linker** on the Link Run page): **Auto-Linker on**, **Custom cron…** `27 12 * * *` (2 min ahead), `Europe/London`, **Daily cap (links)** 20, dry-run, **Save Auto-Linker** | The scheduler started the run at 11:27:13Z; status **Ready to deploy** with 20 ticked, marked **Auto** in History; notification `link_run.ready` "Auto-Linker: links ready / 20 links ready to deploy" in the bell; schedule `last_run_at` and `next_run_at` (tomorrow 11:27Z) set |
| 5 | MCP | 132 tools listed (133 minus `siloforge_api_request`, off by default). `siloforge_link_run_create` → ready (281 items, 3 ticked), `_status`, `_items` (open_url with context), `_deploy` without `confirm` refused ("nothing was sent"), with `confirm: true` → deployed 3/3 verified, `_undo` (confirm) → undone, content byte-identical |
| 5 | After MCP | The ready Auto-Linker run had lost all 20 links to the MCP run (bug 16) |

## Bugs fixed (all with regression tests unless UI-only)

| # | Where | Bug | Fix | Test |
|---|---|---|---|---|
| 1 | sf-jobs `link_run/store.rs` | Opportunities right after the first crawl: 0 orphans, every post under-linked (read from graph columns that only a plan fills) | Orphan / contextual-orphan / under-linked counts computed live from committed body links + nav links | `e2e22_opportunities_alternates_anchor_edits_and_content_gaps` |
| 2 | sf-server `v2/link_runs.rs` | `threshold: 0.550000011920929` | rounded to 3 decimals | `link_runs_e2e` asserts `0.55` |
| 3 | ui `api2.ts` | Strategy count badges never showed (the server sends objects per strategy) | headline number per strategy (pairs, hits, gaps, contextual orphans, money pages, broken links) | UI (screenshot 10-r2) |
| 4 | ui `LinkRun.tsx` | Review said "40 selected of 345" (posts, not links); Load more paged posts with an item offset | `total_items` for counts, post offset for paging | UI |
| 5 | sf-jobs store | Alternates that are not in the paragraph were offered in in-text runs; picking one ended as a silent content gap | in-text runs offer only in-paragraph alternates; `PATCH item {anchor}` refuses (400) an anchor that is not in the paragraph unless the run has Contextual | same test + `link_runs_e2e` |
| 6 | sf-jobs store | An item that deploy could not place (approved + content_gap) stayed "selected": in no Results filter, not offered by Fix, run "partly deployed" with 0 failed | reported as `failed` (failure_kind `content_gap`) | same test |
| 7 | sf-jobs `common.rs` | Live verify always inconclusive when the canonical post host (`site01.example`) differs from the configured domain: the alias applied only to configured hosts, so the name resolved to the LAN IP and the SSRF guard blocked it | post-URL hosts that have a configured `[crawl.host_aliases]` entry count as the site's own (other hosts stay guarded) | `content_hosts_count_only_with_an_alias` |
| 8 | sf-jobs `link_run/mod.rs` | The reason next to a link kept the deploy-time `verify_skipped: blocked by ssrf guard` after a later verify | verify sets `link not found on the live page` / clears verify errors | round-trip test asserts the reason |
| 9 | sf-jobs `fragment.rs` | Open ↗ landed on the first occurrence on the page (title, TOC, product table): context was added only when the anchor repeated inside the paragraph | always `prefix-,anchor,-suffix` (≤ 3 words each) when safe; punctuation / typographic possessives glued to the anchor are part of the term; straight quotes and dashes (rewritten by wptexturize) end the context | `fragment::tests` (2 tests, 15 cases) |
| 10 | ui `LinkRun.tsx` | Results listed the 592 unticked suggestions too ("All 629") | Results show deployed items only | UI |
| 11 | ui `LinkRun.tsx` | Step 9 Fix was disabled on the rail after a deploy | reachable after a deploy | UI |
| 12 | ui `LinkRun.tsx` | **Apply properly** jumped to the Deploy step (the run passes through `deploying`), the Fix step unmounted and "Repair finished" never came; Undo on Results did the same | Results/Fix actions keep the step | UI (run 2 repair shows "Repair finished") |
| 13 | sf-jobs `inject.rs` | In a run with Contextual, native items whose anchor could not be placed were not handed to the AI (only the site's insert modules counted) | `InjectOpts.llm_fallback` (deploy + repair of contextual runs) | `contextual_run_falls_back_to_ai_for_native_items` |
| 14 | sf-jobs `inject.rs` + `undo.rs` | Undo of an AI context link removed only the `<a>`; the AI sentence stayed (rephrase: the rewritten paragraph stayed) | the AI edit is recorded (`score_breakdown.llm_edit`) and undo removes the inserted sentence / restores the paragraph exactly | same test: posts byte-identical after undo |
| 15 | sf-engine `insert/splice.rs` | A context sentence for a plain list was appended between the last `</li>` and `</ul>` | inserted inside the last `<li>` | `append_to_a_list_goes_inside_the_last_item` |
| 16 | sf-jobs `link_run/mod.rs` | A new run adopted the links of a `ready` run (the Auto-Linker's "20 links ready" run was left with 0) | `ready` runs hold their links; a ready run can now be cancelled (**History → Discard**) to release them | `ready_run_keeps_its_links_until_cancelled` |
| 17 | sf-server PATCH item | The response looked the item up in the first 1000 items only | `ItemQuery.edge_id` | `link_runs_e2e` |
| 18 | ui History | The list did not refresh after Undo run (stayed "Deploying"); "By" showed `user:<uuid>` | polls while a run is busy; `created_by_name` (user name/e-mail or `token <name>`) | `link_runs_e2e` (`created_by_name`) |
| 19 | docs/guide | Labels that differ from the UI (Deploy links, Fix broken, Min score, Max total, Runs history, Enabled, Live, Max per day, Run now, AI refine/Verify placement, "Switch module") | `link-runs.md` and `auto-linker.md` use the real labels; Discard and the ready-run hold documented | n/a |

Verification: `cargo xtask ci` green (529 passed, 0 failed, 10 ignored; was 523), `npm run typecheck && npm run build` green.

## Screenshots (`D:\tmp\sf-e2e22\shots`)

`01-login-page`, `02-after-login`, `03-sites-empty`, `04-add-site`, `05-connection-dialog`, `06-linkrun-step1`,
`07-linkrun-crawled`, `08-opportunities`, `10/11/12/13-r{1,2,3}-*` (strategies, mode, run summary, running),
`15-r1-review`, `16-r1-review-edited`, `17/18/19-r{1,2,3}-*` (deploy confirm, deploying, deployed),
`20-r*-results-applied`, `21-r*-open-{1,2}` (live pages opened with Open ↗), `22-r1-verified`, `23-r1-after-edit-verified`,
`24-r{1,2}-fix-list`, `25-r2-fix-done`, `26-r2-results-after-fix`, `27/28/29-r{1,2,3}-*` (History, undo confirm, undone),
`30-r2-review-free-anchor-contextual`, `31/32/33-autolinker-*`, `34-history-auto-run`, `35-notification-bell`,
`36-auto-run-review-refused-edit` (in-text run refuses an anchor that is not in the paragraph),
`37-history-discard-ready-run` (Discard on a ready run; "By" shows names). Runs r1/r1b/r1c and r2/r2b/r2c
are the same Results before and after the fragment fixes.

## Open issues

- **dario 403** (`oauth_not_allowed_for_organization`) for `claude-haiku-4-5` since ~11:23Z: the AI-undo fix (bug 14) and the
  list placement (bug 15) are proven by stub-AI tests, not yet against real AI output on WordPress.
- Pages with lazy-loaded images shift after Chrome scrolls to the text fragment; the highlighted link can end up just below
  the fold. A fragment cannot help when the same context words occur earlier on the page (rare).
- Auto-Linker: `schedule.next_run_at` is null right after saving until the first scheduler tick; the card's **Run now**
  starts a normal (manual) run, not an Auto-Linker run (documented). `link_run.done` is titled "deployed" also for a partly
  deployed run.
- In a Contextual run any free-text anchor is accepted (the AI places it); review AI sentences before deploying.
- `Re-crawl if stale` does not notice edits made in WordPress within 24 h; such posts fail with `paragraph changed` until
  the next crawl (safe, but a re-crawl of just the failed posts in repair would help).
- The opportunities card "under-linked" counts posts with ≤ 1 body inbound link (340 of 360 here) and overlaps "orphans"
  (289); the two cards read like separate problems.
- The test helper `/tmp/sfedit.php`, `/tmp/sfrestore.php` and `/tmp/restore-posts.json` were removed from the cli container.
