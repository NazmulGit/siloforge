# Anchor quality: POS-filtered noun phrases, semantic scoring, AI refinement (2026-10-14)

The pilot (`docs/testing/pilot-e2e.md`) found that about half of the native anchors were natural.
The rest contained verbs ("phone mount withstand daily abuse"), were product names ("Quad Lock
Motorcycle Handlebar Mount"), had model numbers ("Folding Electric Bike 48V") or possessives
("2026’s top bike lights"). This pass fixes the heuristic and adds an optional AI step. It also
closes two items from `performance.md`: v1 post-vector parity, and the idle vector cache.

## What changed

| Area | Change |
|---|---|
| `sf-engine nlp/pos.rs` (new) | A light POS tagger. It uses closed-class lists (function words, contractions, number words, adverbs), suffix rules (`-ly`, `-ed`, `-ing`, `-s`, adjective suffixes), a compact verb lexicon (about 190 verbs that are rarely nouns) and 40 noun/verb homographs ("lock", "fit", "mount", "support"), plus context rules. A homograph is a verb after a pronoun, modal or `to`. A base form right after a plural noun is a verb ("stands support", "wheels support"). An `-s` form followed by an object or a modifier is a verb ("set keeps the", "helmet fits right"). An `-ing` form after a plural noun is a verb ("Shoes featuring"). The tagger also marks "suspect" tokens: model numbers and unit specs (`72V`, `34L`, `Oj9010`), ALL-CAPS words of 5+ letters (`AGPTEK`) and camelCase spellings (`iPhone`). |
| `sf-engine nlp/terms.rs` (new) | Site-wide term statistics, built from `posts.content_text`. Document frequency per stem gives an IDF factor in [0.35, 1] (C.3: "bike" on a bike site counts about a third as much as a specific word). Capitalisation per word gives proper nouns: words capitalised mid-sentence at least twice and written in lowercase at most twice ("Peloton", "Quad", "Schwinn"). |
| `sf-engine nlp/phrases.rs` | Runs are built from the tags. Verbs, adverbs and function words end a run. A possessive ends its run ("shoe’s cleat type" gives "cleat type"). A candidate must end on a noun, never start on a number or a year, never start mid product name (after a site proper noun: "Quad [Lock Mount]") and never split a compound modifier ("Water [Resistant ...]", "Heavy [Duty ...]"). Each candidate carries its sentence index and suspects. `NLP_VERSION` is now 2. |
| `sf-engine anchor/pick.rs` | Candidates with a model number or brand the target title does not name are rejected, and so are candidates with a repeated word ("Lock Bike Lock Cable"). Length preference is 2-5 words (6 costs -3). A salesy lead word costs -4. Semantic scoring (C.1) adds `30 * (cos(phrase, target post) - 0.35)`. Sentence placement (C.8) costs -0.5 per sentence index and -6 for a sentence that already holds a link or other blocked span. Across the top-5 paragraphs the best phrase wins, with a cost of 3 per cosine rank (v1 took the first paragraph). New: `top_phrases`, `is_natural_anchor` (the strict noun-phrase check), and `TargetProfile::apply_idf`. |
| `sf-engine anchor/refine.rs` (new) | The AI prompt (paragraph, target title and keywords, heuristic candidates; the paragraph is marked as data) and strict validation of the reply. The anchor must be verbatim and word-bounded outside blocked spans, 1-6 words (one word only if the title uses it), not generic, allowed by the pick constraints (ignore list, used in source, reserved exact) and natural. `relevance_factor`: below 0.3 the composite score is multiplied by `0.5 + relevance`. |
| `sf-jobs pipelines/anchors.rs` | Builds site terms (cached per content fingerprint), then the IDF profiles. For each batch it collects the top-3 lexical candidates per new edge, embeds the missing ones in one call (`ctx.phrases` cache, lowercase), and picks with cosine to the target post vector. Exact-match share is capped per target at 35% (min 2 anchors) as well as per site. AI refinement runs over the top-N new edges by composite score: bounded concurrency, a total timeout, a circuit after 3 failures before any success, one warning on failure, and plan completion in every case. |
| `sf-jobs pipelines/plan.rs` | `score_breakdown` gains `anchor: {source, semantic, guard_swapped?}`, `ai: {anchor, relevance, reason, model, fallback_used, reject?/error?}` and `ai_factor` (only when not 1). When a suggestion is rescored, its earlier `ai` and factor are kept. Settings: `ai_anchor_refine` (site, default false), with the payload `ai_refine`/`aiRefine` overriding it, plus `ai_anchor_refine_max` (200), `ai_anchor_refine_concurrency` (4) and `ai_anchor_refine_timeout_s` (300). Semantic scoring is on unless `anchor_semantic_off` is set. No schema change. |
| `sf-jobs ctx.rs` | `VectorCache` entries, and the new `TermsCache` and `PhraseCache` (at most 50k phrases), are dropped after 10 minutes unused (`CACHE_IDLE_TTL`). A sweeper runs every 60 s and holds only weak references. |
| `sf-embed` + `sf-jobs embed.rs` | Post vectors now match v1 (see below). New `MAX_TOKENS_LONG = 512`, `Embedder::embed_with_cap` (a default method, so remote and fake rungs are unchanged), and `EmbedChain::embed_cap`/`embed_batched_cap`. |

## v1 post-vector parity (performance.md open item)

v1's text was exactly what v2 uses: `title + "\n" + content_text.slice(0, 2000)` (UTF-16 units,
`embed/qdrantIndex.ts:83`). The difference is the token cap of the rung that served each site.
OpenRouter truncates at 256 word-pieces. v1's local transformers.js rung truncates at 512.
Measured on `D:\tmp\sf-test-home` (scratch tool `D:\tmp\sf-postvec`, 150 posts per site, cos ≥ 0.9999):

| Site | match at 512 | match at 256 |
|---|---|---|
| site24.example | 148/150 (worst 0.99988) | 49/150 |
| site65.example | 150/150 | 96/150 |
| site05 / site80.example / site45 / site18 / site46 / site74 | 58-103/150 | 150/150 |

Paragraph vectors were always 256 (40/40 long paragraphs per site match at 256). So before it
re-embeds posts, `embed_site` re-embeds up to 6 stored long posts at both caps and uses the cap
that reproduces them. It logs the choice when the cap is 512. A new site with no stored vectors
uses 256. Test: `post_vectors_reuse_the_sites_token_cap`.

## Golden set (method)

`crates/sf-engine/src/anchor/golden_anchors.json` has 61 real cases from the site65 pilot DB: the
source paragraph, target title, top-10 keywords, the anchor the old picker chose, and hand-labelled
acceptable anchors (51 cases have at least one; 10 have no good anchor in the paragraph). Cases 1-11 are
the pilot's known-bad anchors; 12-61 are random suggestions (seed 7, 42 native + 8 content_gap). The
fixture also stores capitalisation counts from the full pilot corpus, so proper-noun detection
works as in production. Test: `anchor::golden::golden_anchor_naturalness` (run with
`--nocapture` for the per-case table).

"Natural" means the anchor is in the hand-labelled set, or it passes `is_natural_anchor`: 2-6
words, noun-phrase shape, no verbs, adverbs, pronouns or possessives, no model number or brand
the target title does not name, not generic, no listicle lead. "Before" judges the old picker's
anchor (`old_anchor`); "after" runs the new picker on the same paragraph. That run has no IDF or
semantics, because the fixture has no vectors.

| Metric | Before | After |
|---|---|---|
| Natural, all 61 | **49.2%** | **100%** (assert ≥ 85%) |
| Natural, random 50 | 54.0% | 100% |
| Exactly a hand-labelled anchor (51 labelled cases) | 35.3% | **78.4%** |
| In-paragraph (native) | 90.2% | 95.1% |

Caveat: "after" is judged partly by the same tagger the picker uses, so 100% overstates it. The
independent measures are the hand-labelled match (35% to 78%) and my own reading of all 61 "after"
anchors. I count 59/61 as natural text. The two weak ones are "riding mountain trails" (a gerund
phrase) and "e-bike Material" (spec-table text). Relevance is a separate problem: 8 of the 61
pick a natural but off-topic phrase because the paragraph has nothing better ("tight bike bolts"
→ Torque Wrenches, "dual drum brakes" → E-Bikes Without Pedals). Semantic scoring and AI relevance
target exactly these cases.

Examples (before → after): "phone mount withstand daily abuse" → "phone mount"; "Quad Lock
Motorcycle Handlebar Mount" → "Motorcycle Handlebar Mount"; "Peloton Cycling Shoes featuring Delta
cleats" → "cycling shoes"; "2026’s top bike lights" → "bike lights"; "KEMIMOTO 34L Waterproof Bike
Panniers" → "Waterproof Bike Panniers"; "Training wheels come" → "Training wheels"; "bike stands
support" → "bike stands"; "Lock Bike Lock Cable" → "Bike Lock Cable".

## Pilot re-run (D:\tmp\sf-pilot-home, DB only)

Local data only: no crawl, no inject, `embed.chain = ["onnx"]` (DirectML). The 1,725 pending
suggestions were deleted so that every edge got a fresh anchor, then one plan (reverse) was run
through `/api/v1/sites/:id/plan` on 127.0.0.1:8650. The DB before this pass is backed up at
`D:\tmp\sf-anchor\pilot-backup\`. Release build, same machine and load as the pilot.

| | Before (pilot pass 3, first plan) | After (final build) |
|---|---|---|
| New suggestions | 1,733 | 1,726 |
| Native (anchor in paragraph) | 1,411 (81.4%) | 1,478 (85.6%) |
| Anchors phase | 466 ms | 2,144 ms (terms 172, split+nlp+pick 600, semantic 1,228 for 2,120 phrases, guards 2) |
| Plan wall (no embedding needed) | n/a (that plan also embedded 14.9k texts: 46.6 s) | 2.4 s, 4.3 CPU-s, peak RSS 292 MB |
| Natural anchors, 30 random suggestions judged by hand | **14/30 (47%)** (seed 30) | **26/30 (87%)** on the build before the last two lexicon fixes (seed 30); **30/30** on the final build (seed 7, 3 of them are brand-free product-name fragments, e.g. "Pro-Grade Spin Bike Pedals") |

The after-sample misses on the intermediate build were "Squirt Chain Lube" (a brand written in
lowercase elsewhere on the site), "water close" (fixed: "close" is an adjective now), "AIII
Lightweight Electric Dirt Bike" (a 4-letter ALL-CAPS brand is treated as an acronym), and
"helmets fuse breathability" (fixed: "fuse" added to the verb lexicon).

Cost: the anchors phase is about 1.7 s slower on 1,732 edges, mostly phrase embedding (2,120 short
texts on DirectML, including a model warm-up). The phrase cache makes re-plans cheaper. On a
CPU-only box, expect roughly 2-4 s more for this many edges.

### Live AI check (dario, `claude-haiku-4-5`, top 30 edges)

Settings: `ai_anchor_refine: true` and `ai_anchor_refine_max: 30`, with
`llm.fallback_models = ["claude-haiku-4-5"]` set temporarily. Both were restored afterwards
(`siloforge.toml` is byte-identical to the backup, and the token used was revoked). Result: 29
edges got `ai` metadata. 23 were asked: 9 anchors changed, 9 agreed with the heuristic, and 5 kept
the heuristic because the model's anchor was not verbatim (for example it proposed the target
title, which is not in the paragraph). The other 6 were not sent because no free paragraph was
left. There were 0 errors, it took 13.2 s at concurrency 4, and the plan took 15.6 s. All
relevance ratings were 0.65-0.95, so none was down-ranked: the model is generous. Changed anchors
were mostly good ("alarm-equipped bike locks", "waterproof handlebar bags", "dry lube"). Two let
weak phrases through, "helmet fits right" and "Best Bike Helmets". The validator now rejects both
(a verb before a modifier, and a listicle lead). Treat the AI step as an optional refinement, not a
quality gate.

## Tests

- sf-engine (165): `nlp::pos::*` (pilot verbs, homographs, suspects, possessives, sorted lists),
  `nlp::terms::*` (proper nouns, IDF, merge = sequential, hostile input),
  `nlp::phrases::{pilot_anchors_lose_their_verbs, possessives_never_inside,
  suspects_and_brand_fragments, compound_modifiers_are_not_split, sentences_are_indexed}`,
  `anchor::golden::*`, and `anchor::refine::*` (verbatim/natural/allowed validation, lenient
  reply parsing, prompt bounds). Insert byte-exactness tests are unchanged and green.
- sf-jobs (8 unit + 14 + 5 + 1 integration): new `tests/anchors_ai.rs`. A stub AI picks a
  candidate: anchors are replaced only by verbatim, natural ones, `ai_factor` 0.6 for relevance
  0.1, and both are kept on re-plan. Invented anchors are rejected (`not_verbatim`). With an
  unreachable AI the plan completes, the circuit trips and one warning is logged. With no AI
  configured there is one warning. Idle cache eviction is checked. The site's post-vector token
  cap is reused.
- Clippy `-D warnings` is clean for sf-engine, sf-jobs and sf-embed.

## Open

- POS limits: a brand that is also written in lowercase on the site ("Squirt", "Bell") is not
  caught. A 4-letter ALL-CAPS brand ("AIII") passes as an acronym. Out of context, a leading
  noun/verb homograph passes the strict check ("match heavy-duty trailers"); the picker itself
  sees the context.
- Relevance: the remaining weak suggestions are relevance problems (a target picked by post cosine
  with no good phrase in the paragraph), not anchor-text problems. Consider a minimum semantic
  cosine for native picks, or routing such edges to content_gap.
- The settings keys `ai_anchor_refine*` and `anchor_semantic_off` are read from `sites.settings`.
  The v1 settings whitelist (`sf-server`) may need them added so the UI/API can set them.
- `sf_embed::MAX_TOKENS_LONG` reproduces v1-local post vectors only on local rungs (onnx). A remote
  rung ignores the cap.

## v2.0.1 fragment filter (2026-10-14)

Hermes acceptance on site46.example reported the anchors "V kits perfect" ('12 Best Drill Kits with
Accessory Sets' → 'Drill Driver Kits Under $200') and "inch cordless Impact Wrenches". Both are **v1
suggestions** carried over by the migration (created 2026-07-27, v1 split "20V" into "20" + "V"): the
v2 plan does not re-pick anchors of existing pending edges. The v2 picker did produce the second kind
itself, though: "1/2 Inch Cordless Impact Wrenches" tokenizes as `1 / 2 Inch ...`, and a phrase could
start on "Inch".

| Rule (`nlp/pos.rs`, `nlp/phrases.rs`, `anchor/pick.rs`) | Example rejected | Kept |
|---|---|---|
| Never start on a unit word (`UNIT_WORDS`: inch, oz, ml, mm, gal, lbs, ft-lbs, mAh, hp, rpm, ...) or a lone letter | "Inch Cordless Impact Wrenches", "V kits", "Oz Water Bottles", "Gal Air Compressor" | "cordless Impact Wrenches", "20V kits" (one token, allowed when the target names 20V) |
| Never start inside a glued spec (token directly after a number, or after `/ . - × ⁄` glued to a number) | "3/[8-inch drive sockets]" | "drive sockets"; "2-inch spindle travel" (whole spec) |
| The head (last word) is a content noun (`is_bad_head`: adjectives, `BAD_HEADS` perfect/best/great/ideal/ready/available/compatible/options/things/..., units, tokens with digits) | "kits perfect", "12V options", "water bottle easy", "dual 4.0Ah" | "4.0Ah batteries" |
| Postpositive adjectives (available, compatible, ideal, perfect, ready, suitable) no longer turn a preceding -s noun into a verb | "jug caps available" tagged `caps` as a verb | `caps` = noun |
| `balance`, `strike` are noun/verb homographs | "saws balance performance", "water bottle strikes" | "bevel adjustments", "water bottle" |
| Picker skips listicle leads (same rule as `is_natural_anchor`) | "Best Drill Kits" | "Drill Kits" |
| Keyword fallback skips keywords led by a unit and matches that continue a spec | "inch impact wrench" in "1/2-inch impact wrench" | |
| `clean_title` keeps a number run before a unit | "Inch Cordless Impact Wrenches" | "1/2 Inch Cordless Impact Wrenches", "32 Oz Water Bottles With Straw" |
| `is_natural_anchor` accepts a leading spec only whole (number(s) + unit) | "Inch ...", "1/2 Cordless ..." | "1/2 Inch Cordless Impact Wrenches" |

Golden set: cases 62-96 are new (35 cases): the two Hermes anchors plus 33 real v1 suggestions from
site46.example and site24.example (`D:\tmp\sf-test-home`, read-only) whose anchor starts on a
unit/letter fragment or ends on an adjective; paragraphs trimmed to the sentences around the anchor,
target keywords from the DB, acceptable anchors labelled by hand.

| Metric | Before (v1 anchor) | After |
|---|---|---|
| Fragment cases 62-96, natural | 2.9% | **100%** (assert ≥ 85%, and no pick may start on a fragment or end on a bad head) |
| Fragment cases, exactly a hand-labelled anchor (34 labelled) | n/a | 79.4% |
| All 96, natural | 32.3% | 100% |
| Cases 1-61, hand-labelled match | 35.3% | 80.4% (was 78.4% before this pass) |

My reading of the 35 new picks: 33 natural. Weak: "crafting wine bottle" (a gerund after "Try"; the
-ing lead is a known POS limit) and "Starbucks glass bottles" (a brand the fixture has no capitalisation
counts for). Tests: `nlp::phrases::{unit_fragments_and_trailing_adjectives,
spec_fragments_hostile_input_never_panics}`, `anchor::pick::{fragments_are_never_natural_or_picked,
keyword_fallback_skips_spec_fragments, clean_title_keeps_whole_specs}`, extended
`golden::strict_check_examples`. `NLP_VERSION` stays 2 (keywords do not use the phrase chunker).

Pending suggestions from older pickers: `sf_engine::anchor::is_fragment_anchor` (first word a unit or
lone letter, or last word a bad head) runs over every pending `origin=plan/archive` suggestion when a plan
loads its edges (`sf-jobs pipelines/plan.rs load_edges`). A fragment anchor is treated like a blank one:
the edge gets a fresh anchor from the current picker, or is dropped by the blank-anchor safety net when
its paragraph offers nothing natural. Natural anchors are never re-picked. The plan log says
"N pending suggestions rescored (M get a new anchor)". Test
`anchors_ai::fragment_anchors_of_pending_suggestions_are_repicked`. So the v1 "V kits perfect" edges on
site46.example are replaced on its next plan, with no data migration.

## v2.0.2 brands, generic heads, semantic floor (2026-10-14)

Closes the open items above: lowercase / 4-letter ALL-CAPS brands, the -ing lead and the
"Starbucks glass bottles" generic head from the v2.0.1 golden pass, and the relevance gap
(a minimum semantic cosine for native picks).

| Rule | Where | Example rejected | Kept |
|---|---|---|---|
| Site-wide capitalisation ratio: a word seen capitalised mid-sentence ≥ 8 times and ≥ 8× more often than in lowercase is a proper noun (`PROPER_RATIO_MIN_CAPS`, `PROPER_RATIO`), in addition to the strict rule (≥ 2 caps, ≤ 2 lowercase). The statistics are per site (`TermsBuilder`, once per plan, cached by content fingerprint), so the brand is caught where a paragraph writes it in lowercase | `nlp/terms.rs` | "Squirt Chain Lube", "squirt lube" (pilot: 26 caps / 3 lowercase) | "Bell" (313 / 176), "Pro" (275 / 125), "lube" stay ordinary words |
| 4-letter ALL-CAPS words are brand suspects unless a known acronym (`KNOWN_ACRONYMS_4`: MIPS, HEPA, HDMI, OLED, ...), a plural of a 3-letter one (LEDS, ATVS), or a shouted common word (stopwords, lexicon verbs/adjectives, `COMMON_CAPS_WORDS`). 5+ letters were already suspects | `nlp/pos.rs suspect_token` | "AIII Lightweight Electric Dirt Bike", "WORX Nitro Cordless Impact Driver" | "MIPS bike helmets", "HEPA filter bags" |
| Gerund objects: an -ing word right after a gerund-taking verb (`GERUND_VERBS`: try, avoid, enjoy, keep, love, start, ...) is a verb, so it never leads an anchor (`ING_NOUNS` such as lighting/clothing stay nouns) | `nlp/pos.rs tag_tokens` | "crafting wine bottle", "painting glass bottle vases" | "glass bottle vases", "folding bike" |
| Generic head noun: when the head's stem is in ≥ 50% of the site's posts (`GENERIC_HEAD_DF`, sites with ≥ 20 posts) and no other word of the phrase is a target word, the phrase costs `GENERIC_HEAD_PENALTY` (12) | `nlp/phrases.rs` (`NounPhrase.generic_head`), `anchor/pick.rs score_phrase` | "Clear glass bottles", "cheap souvenir bottles" on site24.example (bottle stem in 68% of 2,621 posts) | "wine bottles" (target names wine), "vacuum-insulated water bottle" |
| Semantic floor for native phrase picks: a phrase whose cosine to the target post is below `MIN_PHRASE_SEMANTIC` (0.15; site setting `anchor_semantic_min`, 0 = off) is skipped unless every descriptive word of it is a target word. The pick then falls back to a target keyword / title found in the paragraph, else to the cleaned title (not in paragraph, so the insert step writes a sentence) | `anchor/pick.rs pick_anchor`, `sf-jobs anchors.rs`/`plan.rs` | pilot "open road" → Phone Mounts (cos 0.014), "safety questions" (0.046), "perfect match" (0.05), "long ride" (0.122), "Bell Air" (0.102) | exact target phrases, which embed poorly against a whole post ("Commuter Bike Helmets" 0.17) |

Threshold evidence: the pilot DB (`D:\tmp\sf-pilot-home`, read-only) holds 1,438 suggestions with a
recorded phrase cosine; the 5th percentile is 0.28, and the 30 lowest (0.005-0.21) are about half
off-topic phrases and half exact target phrases. Hence a low floor plus the "fully named by the
target" exemption. Only the top-3 lexical candidates per edge are embedded; phrases without a
cosine are never dropped by the floor.

Golden set: cases 97-109 are new (13 cases). 97-100 brands (pilot counts for squirt/aiii, site46
counts for worx/nitro), 101-104 site24.example, which now has its own site statistics in the
fixture (`sites.site24`: 2,621 docs, df and capitalisation counts for the 402 words of its
cases, from `D:\tmp\sf-test-home`, read-only; site24 cases 64-83 use them too, with IDF
applied to the target as the plan does), 105-109 the five real low-cosine pilot suggestions with
their recorded cosine (`semantic` per case). Every new case must not pick its old weak anchor.

| Metric | v2.0.1 | v2.0.2 |
|---|---|---|
| All cases, natural (checker) | 100% (n=96) | **100%** (n=109; assert raised to ≥ 90%) |
| v2.0.2 cases 97-109, natural (checker) | 0% (old anchors) | 100% (assert ≥ 90%) |
| v2.0.2 cases, hand-labelled match (8 labelled) | n/a | 87.5% |
| Hand-labelled match, all labelled cases | 80.0% (n=85) | 80.6% (n=93) |
| Native (anchor in paragraph) | 94.8% | 91.7% (the 4 off-topic pilot cases now fall back to the cleaned title on purpose) |

Changes in the old cases: #65 "Starbucks glass bottles" → "coffee-loving crafters", #74 "crafting
wine bottle" → "edgy cut glass bottle crafts" (labelled); #76 "label materials" → "strong adhesive" (IDF now applied on
the site24 cases; still natural, not a labelled pick). My reading of the 13 new picks: 12
natural and on topic; weak: #109 "Bell Air Attack" (a pump model name for an inner-tube target:
"Bell" is a brand that the bike site also writes in lowercase 176 times, so neither rule can tell
it apart, and only the two-word "Bell Air" had a recorded cosine).

Tests: `nlp::terms::tests::mostly_capitalised_brands_and_generic_heads`,
`nlp::pos::tests::four_letter_caps_brands_and_gerund_objects` (+ sorted-list check for the new
lists), `anchor::pick::tests::{semantic_floor_falls_back_to_keyword_then_title,
generic_head_needs_a_named_modifier}`, extended `golden::{golden_anchor_naturalness,
strict_check_examples}`.

Still open (by design / data): a brand that the site writes in lowercase about as often as
capitalised ("Bell": also an ordinary word, "bike bell") cannot be told from a common noun by
statistics; the AI refinement step or the ignore list covers it. The semantic floor needs phrase
cosines, so it is inactive when semantic scoring is off (`anchor_semantic_off`) or the embed
chain fails for a plan (lexical anchors, logged).
