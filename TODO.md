# TODO — SiloForge v2

Legend: [x] done · [~] in progress (agent) · [ ] open. Details: docs/PLAN.md. History: log/devlog.md.

## Phase 0–1 Research & foundation
- [x] Research: engine spec (01), backend spec (02), integrations (04), UI audit (05)
- [x] Research: OSS/upstream + crate stack (03), embedding backends incl. DirectML (06)
- [x] ADR 0001 single binary + SQLite
- [x] Workspace, lints, xtask, AGENTS.md, PLAN.md
- [x] sf-core (config, v1 crypto, enums, site secrets), sf-store (schema, writer/reader), sf-jobs queue, sf-server core

## Phase 2 Parallel packets
- [x] E1 engine-text: html/chunk, nlp keywords 1–3-grams, anchors, block-aware raw insertion, LLM validation, keyword linker
- [x] E2 engine-graph: pool, vector top-k/int8, graph (root fix), kmeans, silo topologies, drift fix, 11-dim scorer, density-after-score, metrics
- [x] C crawl: HTTP client, SSRF per hop, WP REST raw content, WP writer w/ optimistic concurrency, sitemap, link health
- [x] A ai: adapters (openai/openrouter/anthropic/ollama/gemini), chains, fallback, circuit breaker, call log, probes
- [x] S server-v1: 78 routes + security fixes
- [x] O ops: monitor, notifier, errors, backup/restore, v1 migrator, retention, /api/v2
- [x] U ui: v1 UI + System/Jobs-logs/Notifications/Backups/Tokens/MCP pages
- [x] P mcp: rmcp server, v1's 20 tools + v2 tools, stdio + HTTP
- [x] M embed: chain remote → onnx(DirectML) → candle (waits for research/06)

## Phase 3 Integration
- [x] J jobs: runner, scheduler, pipelines (crawl/plan/inject/undo/keyword_link/link_health/audit/embed/backup/maintenance)
- [x] B bin: CLI serve/mcp/init/backup/restore/migrate/doctor/token/version; service install scripts
- [x] Workspace-wide `cargo xtask ci` green

## Phase 4 Data, test, cutover
- [x] Migrate v1 data into a test home; verify counts + embedding samples
- [x] Side-by-side run on 8610/8611; JSON diff vs v1 for UI + MCP endpoints
- [x] Pilot one site end to end (crawl → plan → approve → inject on a test WP → verify → undo)
- [x] Perf: CPU/RAM v1 vs v2 on site24 (112k paragraphs) plan; report
- [x] Linking accuracy test (golden set + spot checks)
- [x] Security/code audit by review agents; fix findings
- [x] Cutover to 8600/8601; v1 containers stopped (kept for rollback); autostart task
- [x] Autostart verified without reboot: headless conhost keepalive task (VBS keeper retired; the PowerShell hang is the ALLUSERSPROFILE/SYSTEMDRIVE issue, not Defender)
- [x] Hermes: ap13 MCP → siloforge.exe with API token (remove plaintext admin password); skill update
- [x] Hermes gateway webhook route for SiloForge notifications (gateway restarted via hub, channel test ok)
- [x] Run a SiloForge task through Hermes end to end
- [x] Update Backup Hub, Model Control, Mission Control, teli scripts (docker exec psql users) — docs/INTEGRATIONS-v2.md
- [x] Docs: README, CHANGELOG, KB, skill `siloforge-dev`, release v2.0.0 zip
- [x] GitHub: new public repo (MIT), secret scan, push

## Follow-ups (v2.0.1)
- [x] PowerShell hang worked around (env vars); no reboot needed; temporary task siloforge-direct deleted
- [x] Anchor misses like "V kits perfect" (model-number fragments) — tighten noun-phrase filter (+ pending v1 fragment anchors re-picked on the next plan)
- [x] UI controls for `ai_anchor_refine`; R11 embedding CPU budget; R14/R16/R21/R22; A3–A13 doc mismatches
- [x] Backup Hub / Model Control / Mission Control: repoint SiloForge entries to v2 (integrations agent, docs/INTEGRATIONS-v2.md)
- [x] Retire v1 containers + volumes (archived to D:\Backups\SiloForge\v1-archive; rollback: scripts/rollback-to-v1.ps1)

## v2.0.2 (code; the ops items above stay open: reboot test, Hermes gateway restart)
- [x] G2-5 `GSC_CREDENTIALS_JSON` install-wide fallback for the gsc job (v1 parity, never logged)
- [x] Security Low/Info items: fixed or justified in docs/testing/security-audit.md (Resolution column)
- [x] Reliability open/partial items: fixed or justified in docs/testing/reliability-audit.md section 7
- [x] Anchor quality: site-wide brand capitalisation, min target similarity for in-paragraph picks, generic head nouns (anchor-quality.md)
- [x] UI: Linking Rules -> Targets (money pages, root URLs, inbound cap); dashboard "unknown" link-health cards
- [x] CLI `siloforge vacuum` (incremental auto-vacuum for pre-2.0.2 databases)

## Later (optional)
- [ ] Delete v1 Docker volumes/images after a long stable period (see RESTORE-v1.md, "Deleting v1 for good")
- [ ] PHP plugin: verify `paragraph_hash` before splicing (plugin mode is unused by the fleet today)
- [ ] Anchor edge case "Bell Air Attack" (brand also used as a common word)
