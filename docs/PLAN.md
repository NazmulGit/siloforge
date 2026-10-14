# SiloForge v2 — Rebuild plan

Owner: lead agent (Claude Code), started 2026-10-13. Status lives in `TODO.md`; history in `log/devlog.md`.

## 1. Goal
Rebuild SiloForge v1.4 (TypeScript, 7 Docker containers) as **one Rust binary** that:
- serves the **same `/api/v1` contract**, so the existing React UI, the MCP tools and the WP plugin work unchanged;
- uses much less CPU and RAM in bulk linking (one bounded rayon pool, streaming, no event-loop stalls, no duplicate jobs);
- links more accurately, fixing the 45 engine findings plus 17 accuracy ideas in `research/01`;
- adds a unified AI provider system with automatic fallback, an API + MCP for Hermes Agent, backup/restore/migration,
  monitoring/logs/notifications, and a portable install that runs on any PC;
- imports all v1 data (83 sites, 15.8k posts, 429k paragraphs incl. 332k embeddings) without re-embedding.

## 2. Research summary
| Doc | Key result |
|---|---|
| research/01 engine spec | Full algorithm spec. Top bugs: rendered HTML pushed back to WP (destroys blocks), injection overwrites live edits, unchecked LLM output, density cap before scoring, click depth always 0, one-word anchors, drift always 0, O(n²) anchor NLP pinning the CPU. |
| research/02 backend spec | 78 routes; HS256 JWT / bcrypt $2a$10 / AES-GCM `v1:` blobs are portable; no per-site job lock (duplicate runs seen live); WP app passwords stored in plaintext and leaked to the LLM via MCP. |
| research/03 OSS + crates | (pending) upstream verdict + crate stack. |
| research/04 integrations | Keep ports 8600/8601 and the contract, and Hermes/MCP/Mission Control need no edits. Backup Hub, Model Control and the docker-exec agent scripts do need edits. Admin password sits in plaintext in the Hermes config: replace it with a scoped API token. |
| research/05 UI audit | 64 endpoints used; JSON shapes documented; small server-side fixes (clamp limits, `/jobs/run/:id`). |
| research/06 embeddings | candle (pure Rust) matches stored vectors (cos 0.9999998) at ~11 texts/s CPU; ORT CPU/DirectML spike pending. |

**Upstream verdict:** no OSS project covers WordPress internal linking with silos, scoring and safe injection at this level
(see research/03), so we build our own and reuse crates (candle, tokenizers, axum, rusqlite, lol_html, scraper, rmcp).

## 3. Architecture (ADR 0001)
One process `siloforge` = axum server (`/api/v1`, `/api/v2`, `/mcp`, `/health`, UI) + job runner + scheduler + monitor.
SQLite WAL file `siloforge.db` in `SILOFORGE_HOME` (one writer thread, reader pool). Vectors are f32 BLOBs, searched by
SIMD brute force over a per-site cache. No Postgres, Qdrant or Redis.

### Request/compute flow
```
UI / Hermes (MCP, API token) / WP plugin ──HTTP──▶ sf-server ──▶ Db (reads) ─┐
                                              │                              │
                                              └─ enqueue ─▶ job_runs ◀───────┤
sf-jobs runner (bounded, per-site mutex) ─▶ pipelines ─▶ sf-crawl / sf-embed / sf-ai (IO, tokio)
                                                     └─▶ sf-engine on rayon pool (CPU, budgeted)
sf-ops: monitor (sysinfo) · notifier (webhook/ntfy/telegram/hermes/desktop) · backup/restore · v1 migrator
```

### Performance design (requirements 1.x)
- **CPU:** a single rayon pool sized by `resources.cpu_threads` (default = logical/4, about physical/2). Jobs run at
  BELOW_NORMAL priority. The scheduler can defer heavy jobs while the machine is busy. Per-edge NLP is cached by paragraph hash.
- **RAM:** stream posts and paragraphs per batch, no whole-site arrays of HTML. Vector cache LRU with a MB budget. int8 quantised
  candidate search (exact f32 re-rank of the top-k).
- **Incremental:** chunk only posts whose `content_hash` changed (`posts.chunked_hash`), embed only rows without a vector,
  keep paragraph ids stable via the hash.
- **GPU:** optional `onnx` rung uses ONNX Runtime DirectML (AMD/NVIDIA/Intel on Windows); auto-detected, falls back to CPU.

## 4. Feature map (user checklist → where)
| Area | Where |
|---|---|
| 1 Core/perf | ADR 0001, sf-engine pool, incremental pipeline, sf-embed backends, `resources.*` config |
| 2 Linking engine | sf-crawl (full-site crawl, raw content), sf-engine (anchors, silos, contextual scoring, density, cannibalization), sf-jobs pipelines |
| 3 AI | sf-ai (providers openai-compatible/openrouter/anthropic/gemini/ollama, chains, fallback, circuit breaker, probes), sf-embed chain, AI anchor refinement (optional) |
| 4 API/MCP/Hermes | `/api/v1` + `/api/v2`, API tokens with scopes, sf-mcp (stdio + HTTP `/mcp`), Hermes webhook notifier, Hermes skill |
| 5 Backup/migration | sf-ops backup (zip: db snapshot + config + secrets, optional passphrase), restore, `migrate from-postgres`, portable home |
| 6 Monitoring | sf-ops monitor, job_logs, errors table, notifications, `/api/v2/system/*`, UI System page |
| 7 Database | SQLite schema (sf-store), indexes per access path, batched writes, ANALYZE/optimize job, retention |
| 8 Testing | unit + golden + hostile input, integration test against a stub WP, load test on the site36 copy (112k paragraphs), MCP smoke, Hermes run |

## 5. Work packets
Phase 1 (lead, done): workspace, sf-core, sf-store schema, AGENTS.md, PLAN, ADR.

Phase 2 (parallel agents; each owns only its listed paths):
| WP | Owner paths | Deliverable |
|---|---|---|
| E1 engine-text | `crates/sf-engine/src/{html,nlp,anchor,insert,linker}/**` | HTML analyze/chunk (one splitter, byte offsets), keywords (1–3-grams), intent, anchor pick/classify/guards/cannibalization, block-aware raw-content injection + verify + LLM output validation, keyword linker |
| E2 engine-graph | `crates/sf-engine/src/{graph,cluster,silo,score,density,vector,metrics,pool}/**` | graph metrics (fixed roots), kmeans + labels + pillars, 5 topologies, drift (fixed), 11-dim scorer + renorm, density after scoring, SIMD vector top-k + int8, site health metrics, rayon pool |
| C crawl | `crates/sf-crawl/**` | WP REST (raw `context=edit` with app password), sitemap, SSRF guard per hop, robots, host aliases, link-health checker, WP writer with modified-check |
| A ai | `crates/sf-ai/**` | provider adapters, chains (site → global → env), retries, circuit breaker, call log, probe, embed via chain |
| M embed | `crates/sf-embed/**` | Embedder chain remote → onnx (feature) → candle; batching by length; model download/verify |
| S server-v1 | `crates/sf-server/src/v1/**` | all 78 v1 routes with exact contract + security fixes that don't break the UI |
| O ops | `crates/sf-ops/**`, `crates/sf-server/src/v2/**` | monitor, notifier, errors, backup/restore, v1 Postgres migrator, v2 routes (system, logs, notifications, backups, tokens, errors) |

Phase 3 (after phase 2 APIs exist):
| WP | Deliverable |
|---|---|
| J jobs | runner + scheduler + pipelines (crawl, plan, inject, undo, keyword_link, link_health, audit, embed, backup, maintenance) |
| P mcp | rmcp tools (v1's 20 tool names kept + new ones), stdio + HTTP |
| U ui | copy v1 UI, add System/Logs/Notifications/Backups/API tokens/MCP pages, build into `ui/dist` |
| B bin | CLI (serve, mcp, init, backup, restore, migrate, doctor, token, version), Windows task install scripts |

Phase 4: migrate a copy of the v1 data → side-by-side run on 8610/8611 → JSON diff against v1 for UI/MCP endpoints → pilot one
site end to end → perf comparison → audit → cutover on 8600/8601 (v1 containers stopped, not removed) → Hermes wiring →
docs/KB/skill → GitHub repo (public, MIT, scrubbed).

## 6. Risks
| Risk | Mitigation |
|---|---|
| Contract drift breaks UI/MCP | route-by-route spec + JSON diff harness against live v1 before cutover |
| Data loss in migration | read-only from v1 (COPY via docker exec), count/sample verification, v1 untouched for rollback |
| Injection damages WP posts | raw content only, snapshot per post, modified-time precondition, verify, dry-run mode, pilot on test site first |
| Embedding speed | remote rung first (as v1), ORT/DirectML rung, candle fallback, incremental embedding |
| Secrets exposure | encrypt WP/GSC creds at migration, never returned, API tokens instead of admin password, scrubbed public repo |
