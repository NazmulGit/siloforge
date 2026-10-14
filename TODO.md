# TODO — SiloForge v2

Legend: [x] done · [~] in progress (agent) · [ ] open. Details: docs/PLAN.md. History: log/devlog.md.

## Phase 0–1 Research & foundation
- [x] Research: engine spec (01), backend spec (02), integrations (04), UI audit (05)
- [~] Research: OSS/upstream + crate stack (03), embedding backends incl. DirectML (06)
- [x] ADR 0001 single binary + SQLite
- [x] Workspace, lints, xtask, AGENTS.md, PLAN.md
- [x] sf-core (config, v1 crypto, enums, site secrets), sf-store (schema, writer/reader), sf-jobs queue, sf-server core

## Phase 2 Parallel packets
- [~] E1 engine-text: html/chunk, nlp keywords 1–3-grams, anchors, block-aware raw insertion, LLM validation, keyword linker
- [~] E2 engine-graph: pool, vector top-k/int8, graph (root fix), kmeans, silo topologies, drift fix, 11-dim scorer, density-after-score, metrics
- [~] C crawl: HTTP client, SSRF per hop, WP REST raw content, WP writer w/ optimistic concurrency, sitemap, link health
- [~] A ai: adapters (openai/openrouter/anthropic/ollama/gemini), chains, fallback, circuit breaker, call log, probes
- [~] S server-v1: 78 routes + security fixes
- [~] O ops: monitor, notifier, errors, backup/restore, v1 migrator, retention, /api/v2
- [~] U ui: v1 UI + System/Jobs-logs/Notifications/Backups/Tokens/MCP pages
- [~] P mcp: rmcp server, v1's 20 tools + v2 tools, stdio + HTTP
- [ ] M embed: chain remote → onnx(DirectML) → candle (waits for research/06)

## Phase 3 Integration
- [ ] J jobs: runner, scheduler, pipelines (crawl/plan/inject/undo/keyword_link/link_health/audit/embed/backup/maintenance)
- [ ] B bin: CLI serve/mcp/init/backup/restore/migrate/doctor/token/version; service install scripts
- [ ] Workspace-wide `cargo xtask ci` green

## Phase 4 Data, test, cutover
- [ ] Migrate v1 data into a test home; verify counts + embedding samples
- [ ] Side-by-side run on 8610/8611; JSON diff vs v1 for UI + MCP endpoints
- [ ] Pilot one site end to end (crawl → plan → approve → inject on a test WP → verify → undo)
- [ ] Perf: CPU/RAM v1 vs v2 on site36 (112k paragraphs) plan; report
- [ ] Linking accuracy test (golden set + spot checks)
- [ ] Security/code audit by review agents; fix findings
- [ ] Cutover to 8600/8601; v1 containers stopped (kept for rollback); autostart task; reboot test
- [ ] Hermes: ap13 MCP → siloforge.exe with API token (remove plaintext admin password); webhook notifications; skill update
- [ ] Run a SiloForge task through Hermes end to end
- [ ] Update Backup Hub, Model Control, Mission Control, teli scripts (docker exec psql users)
- [ ] Docs: README, CHANGELOG, KB, skill `siloforge-dev`, release v2.0.0 zip
- [ ] GitHub: new public repo (MIT), secret scan, push
