---
title: Anchors
section: Linking
order: 5
---

# Anchors

The anchor is the clickable text of a link. Good anchors read naturally and tell the reader (and search engines)
what the target page is about.

## How SiloForge picks an anchor

1. It looks at the 5 source paragraphs closest in meaning to the target post.
2. In each, it finds **noun phrases** of 2-6 words ("stainless steel water bottle"). Verbs, adverbs, possessives,
   model numbers and unit fragments ("48V", "34L") end or reject a phrase, and so do brand names the target page
   does not mention.
3. It scores each phrase against the target: word overlap with the target's title and keywords (common words on your
   site count less), and the semantic similarity of the phrase to the target post.
4. It prefers phrases early in a paragraph and in sentences that do not already hold a link.
5. It applies the guards below, then saves the anchor with its **type**.

If no phrase fits, the link is a `content_gap`: it needs an AI model to write a sentence, or it stays a report item.

Measured on 109 hand-checked real cases, every chosen anchor passed the naturalness check, and about 80 % matched an
anchor a person had marked as good.

## Anchor types

| Type | Example (target: "Best insulated water bottles") |
|---|---|
| `exact` | the target's top keyword, word for word: "insulated water bottles" |
| `partial` | contains or is contained in the keyword: "insulated bottles for hiking" |
| `branded` | contains your brand (from the domain): "example bottle range" |
| `lsi` | a related phrase: "double-walled steel flask" |
| `generic` | "click here", "read more" |
| `naked` | a bare URL: "example.com/bottles" |
| `cta` | a call to action: "compare insulated bottles" |

## Guards

- No link inside headings, code, buttons, figure captions or an existing link.
- The same anchor is not reused within one source post.
- **Over-optimisation guard**: site-wide, exact-match anchors are kept at or below 40 % and generic at or below
  30 %. Per target, exact anchors are capped at 35 %.
- **Diversity guard**: if one type makes up more than 70 % of the anchors pointing at a target, some are swapped to
  alternatives.
- **Ignore list**: phrases in **Tools → Ignore Keywords** are never used.

## Edit an anchor

1. Open **Tools → Anchors** (it also shows the anchor-type distribution chart).
2. Click the anchor text or type and change it. The new text must appear in the source paragraph for a native link.
3. Save. Anchors of applied links cannot be edited; undo the link first.

## AI anchor refinement (optional)

An AI model can review the top suggestions and pick a better phrase. Its answer is checked strictly: the anchor must
appear word for word in the paragraph, be 1-6 words, not generic, and pass every guard. If the AI fails or times out,
the heuristic anchor is kept and the plan still completes. When the AI judges a link weakly relevant, its score is
lowered.

Turn it on per site in **Settings → Linking Rules → Anchor text → AI anchor refinement** (off by default), or per run
with **AI anchors: on/off** on the Dashboard or in the Bulk wizard. Budget settings:

| Setting | Default | Range | Meaning |
|---|---|---|---|
| Edges per plan | 200 | 0-5000 | Top new suggestions (by score) sent to the AI |
| Parallel calls | 4 | 1-16 | AI requests at once |
| Time budget (s) | 300 | 5-3600 | After this the plan finishes with heuristic anchors |

**Semantic phrase scoring** (on by default) adds about a second per thousand links. Turn it off only to compare.

Since 2.3 the anchors are sent in **batches** (20 per request by default) on the **low** tier, answers are **cached**
for 30 days, and edges where the heuristic is already confident are **not sent at all** (anchor confidence ≥ 0.85 and
relevance ≥ 0.80 by default). **Settings → AI Models → Always use AI** turns refinement on for every plan and run.
Thresholds, batch size, tiers and daily budgets are in **Settings → AI Models**.

AI refinement needs a chat model; see [AI models](ai-models).

> **Tip:** Brands written in lowercase on your site and phrases that are natural but off-topic can still slip through.
> Always skim anchors before injecting.
