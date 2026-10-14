---
title: AI models
section: Automation
order: 2
---

# AI models

SiloForge works without any AI chat model. AI is optional and is used for three things:

- placing `rephrase`, `context` and `content_gap` links (writing or rewriting a sentence),
- **AI anchor refinement** (see [Anchors](anchors)),
- optionally, remote embeddings (see [Resources and performance](resources-and-performance)).

Every AI reply is validated before it is used. A bad or missing reply never breaks a job; SiloForge falls back to its
own result.

## Providers, models and chains

Open **Settings → AI Models**.

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
4. **Build a chain**: under **Fallback chains**, create a chain for `chat` or `embed`, add models as rungs, and order
   them with the arrows. The first rung is tried first; if it fails, the next one takes over.
5. **Activate** the chain. A chain can be global or for one site; a site chain wins over the global one.

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
| Heuristic anchors | Heuristic anchors, optionally reviewed by AI |
