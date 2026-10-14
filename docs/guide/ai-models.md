---
title: AI models
section: Automation
order: 2
---

# AI models

SiloForge works without any AI chat model. AI is optional and is used for three things:

- placing `rephrase`, `context` and `content_gap` links (writing or rewriting a sentence),
- **AI anchor refinement** and **relevance checks** (see [Anchors](anchors)),
- optionally, remote embeddings (see [Resources and performance](resources-and-performance)).

Every AI reply is validated before it is used. A bad or missing reply never breaks a job; SiloForge falls back to its
own result.

## Providers, models and chains

Open **Settings → AI Models**. The page has the **Always use AI** switch on top and five tabs: **Chains by tier**,
**Task routing**, **Budgets & cache**, **Usage** and **Providers & models**. Providers and models are on the last tab.

1. **Add a provider**: a kind, a base URL and an API key (stored encrypted, never shown again).

   | Kind | Base URL example | Note |
   |---|---|---|
   | `openai-compatible` | `https://api.openai.com/v1` | Include the version segment. |
   | `anthropic` | `https://api.anthropic.com` | The host only, **without** `/v1`. |
   | `openrouter` | `https://openrouter.ai/api/v1` | |
   | `ollama` | `http://localhost:11434` | Local models, no key. |
   | `custom` | your endpoint | |

2. **Add models** under the provider. Each model has a **role**: `chat` (writing, anchors) or `embed` (vectors).
   The role cannot change later.
3. **Probe**: click **Probe** on a provider or model to send one tiny live request. The result shows latency and a
   sample reply; for embed models, the vector width (SiloForge needs 384).
4. **Build a chain**: under **Chains by tier** (chat), or under **Fallback chains** on the Providers & models tab,
   create a chain, add models as rungs, and order them by dragging or with the arrows. The first rung is tried first; if it fails, the next one takes over.
5. **Activate** the chain. A chain can be global or for one site; a site chain wins over the global one.

## Always use AI for perfect linking

**Settings → AI Models → Always use AI model for perfect linking** (`ai.always_on`, off by default) makes every plan
and every Link Run use the AI where it helps most, without ticking anything per run:

- **AI anchor refinement and a relevance check** for every *borderline* candidate (low tier, batched, cached). Edges
  where the heuristic is already confident are not sent at all (see *Skip rules* below).
- **AI insertion** (a written or rewritten sentence) for contextual items, on the main tier.
- **Money-page targets** on the high tier.

It respects the daily budgets. When the AI chain is down or a budget is used up, linking carries on with the
heuristics automatically, and the run stats and a notification say so. The Link Run wizard shows the estimated tokens
before each run starts.

## Tiers: low, main and high

Every AI call belongs to a **task**, and every task runs on a **tier**. Each tier has its own fallback chain, so cheap
work goes to a cheap model and only important work goes to a strong one.

| Tier | Meant for | Example chain |
|---|---|---|
| **Low** | Many small, batched decisions | a small, fast model → a small fallback |
| **Main** | Writing one sentence, repairs | your everyday model → a fallback |
| **High** | Money pages, high-priority runs | the strongest model → fallbacks |

**Settings → AI Models → Chains by tier** has one editor per tier. Create a chain in the tier, add rungs, drag them
(or use ↑ ↓) into the fallback order and **Activate** it. One chain per tier can be active globally, plus one per tier
for each site.

A tier uses, in order: the site's chain for that tier → the global chain for that tier → the **main** chain (site, then
global) → the fallback endpoint. The box above the editor shows which chain each tier really uses right now.

## Task routing

**Task routing** maps each task to a tier:

| Task | Default tier | What it does |
|---|---|---|
| `anchor_pick` | low | Choose or propose the anchor among candidates (batched) |
| `relevance_check` | low | Rate how useful the target is to the paragraph, 0–1 (batched) |
| `link_verify_judge` | low | Ambiguous verification decisions |
| `rephrase` | main | Rewrite a sentence so it carries the link |
| `context_sentence` | main | Write a new bridging sentence |
| `repair_insert` | main | The AI step of **Apply properly** |
| `money_page` | high | Any task whose target is a money page |
| `priority` | high | Any task of a run marked **High priority** (or a site with `ai_high_priority`) |

The same tab sets:

- **Reasoning effort** per tier (`none`, `low`, `medium`, `high`). It is sent only to reasoning models (Codex, GPT-5 and
  newer, o-series). Lower effort means fewer hidden reasoning tokens.
- **Batch size**: how many edges go into one low-tier request (default 20).
- **Skip thresholds**: no AI call when the heuristic anchor confidence is at least 0.85, or when the embedding relevance
  is at least 0.80 (defaults). Only borderline items go to the AI.

## Budgets and cache

**Budgets & cache** shows today's use per tier and per site as bars, and sets:

- **Daily tokens** per tier (input + output tokens per UTC day; 0 = unlimited).
- **Daily tokens per site**, all tiers together.
- **Cache answers for** N days (default 30; 0 = off). The same task, model family and inputs (paragraph, target,
  candidates) reuse the stored answer. Cache hits cost no tokens and are counted as cache hits.

When a budget runs out, SiloForge uses heuristics only until 00:00 UTC, notes `budget exhausted` in the run stats and
sends one **AI budget exhausted** notification per day.

## Usage

**Usage** shows the AI calls for the last 24 hours, 7 days or 30 days: calls, tokens in and out, cost, cache hit rate
and errors, plus charts per day, and bars by tier, model, task and top sites.

Tokens come from the provider's reply. When a provider sends none, SiloForge estimates them as characters ÷ 4 and
marks the row as estimated. Cost uses the model's **price in / price out** (per 1 million tokens); local proxies cost
$0 unless you set prices on the model.

The same numbers are on the [Logs](logs) page (source **AI**) and available to agents with the MCP tools
`siloforge_ai_usage`, `siloforge_ai_settings_get` and `siloforge_ai_settings_update`.

## Token saving, in short

- Batching: low-tier tasks send up to 20 edges per request with a compact JSON format.
- Caching: repeated questions are free for 30 days.
- Skip rules: confident heuristic results are never sent.
- Compact prompts: HTML is stripped and the paragraph is cut to about 600 characters around the candidate anchor.
- Tight output limits per task (anchor 60 tokens per item, relevance 80, a sentence 220).

## Fallback and retries

- Each rung retries short-lived errors (timeouts, 429, 5xx), then hands over to the next rung.
- A rung that keeps failing is skipped for a while (circuit breaker), so a dead provider does not slow every job.
- If no chain is active, SiloForge uses the fallback endpoint in **Settings → AI Models**
  (`llm.fallback_url`, default `http://127.0.0.1:3460/v1`, with its fallback models and a 120 s timeout).
- Every call is logged with model, latency and outcome.

> **Tip:** Changing models in the UI takes effect immediately; no restart is needed.

> **Warning:** Deleting a provider deletes its models, and chains lose those rungs. Check your chains afterwards.

## Do I need AI?

| Without AI | With AI |
|---|---|
| Native links only (the safest kind) | Also rephrase and context links |
| `content_gap` suggestions are reported, not written | Some `content_gap` links can be placed |
| Heuristic anchors | Heuristic anchors, optionally reviewed by AI (or always, with **Always use AI**) |
