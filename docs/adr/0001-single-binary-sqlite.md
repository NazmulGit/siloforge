# ADR 0001: One Rust binary with embedded SQLite replaces the 7-container stack

Date: 2026-10-13. Status: accepted.

## Context
SiloForge v1.4 runs as 7 Docker containers: Postgres+pgvector, Qdrant, Redis, Fastify API, 2 BullMQ
workers (Node, 1.88 GB image, transformers.js), nginx UI. Idle RAM is about 840 MB. On top of that,
the Docker Desktop/WSL VM is capped at 7.75 GB. Live data: 83 sites, 15.8k posts, 429k paragraphs
(332k with 384-d embeddings), 663k keywords, 85k edges, 169k link-health rows, about 1.4 GB in Postgres.

Problems we have seen (see docs/research/02-backend-spec.md section 6): no per-site job lock (duplicate runs),
lost BullMQ locks while the event loop is busy embedding, phantom `running` jobs, Redis queue loss, and
migrating to another PC means moving Docker volumes. Postgres has no vector indexes; ANN goes through Qdrant,
which uses brute force at this size anyway (`indexed_vectors_count 0`).

## Decision
1. **One process, `siloforge.exe`** (Windows GNU build; Linux build for Docker): axum HTTP API
   (contract-compatible `/api/v1` plus new `/api/v2`), the embedded React UI, the in-process job runner,
   the scheduler, the MCP server (stdio subcommand plus streamable HTTP at `/mcp`), and the backup/restore/migrate CLI.
2. **Storage: SQLite (rusqlite, bundled) in WAL mode**, one file `siloforge.db` under `SILOFORGE_HOME`.
   One writer connection on a dedicated thread plus a small pool of reader connections. JSON columns are
   stored as TEXT. UUIDs are TEXT, so the existing ids are kept and the plugin, MCP and UI links stay valid.
   Timestamps are RFC 3339 TEXT in UTC.
3. **Vectors: f32 little-endian BLOBs** on posts and paragraphs. Search is brute-force dot product over
   the site's vectors, loaded on demand into a per-site in-memory cache with LRU eviction and a RAM budget.
   The biggest site (112k paragraphs) is 172 MB at f32. An int8-quantised cache variant would keep that at 43 MB (implemented in
   `sf_engine::vector`, not wired: f32 fits the default budget, G2-10).
   No Qdrant.
4. **Queue: a `jobs` table** (the existing `job_runs` table, extended). Its guarantees:
   - a per-site mutex for mutating kinds;
   - dedupe of identical queued jobs;
   - a heartbeat column, with stale `running` rows re-queued or failed at boot;
   - a global timeout per kind;
   - a bounded worker pool.
   No Redis.
5. **CPU budget:** all CPU-heavy work runs on one dedicated rayon pool. Its size is set by
   `[resources] cpu_threads` (default = physical cores / 2). On Windows the process can drop to
   BELOW_NORMAL priority during bulk jobs, and the scheduler checks the system load monitor before it
   starts heavy jobs.

## Consequences
- Migration to another PC = copy `SILOFORGE_HOME` (or `siloforge backup create` + `restore`). No Docker needed.
- Existing data comes over once with `siloforge migrate from-postgres`, which streams `COPY ... TO STDOUT (FORMAT csv)`
  through `docker exec siloforge_pg psql`, so Postgres needs no host port. The old containers are stopped,
  not deleted, which keeps rollback a single command.
- SQLite has a single writer. Writes are batched in transactions (thousands of rows per commit), and the
  workload (a few jobs at a time) fits well within it.
- Multi-PC "install and run" works with the single exe. Several PCs sharing one live DB is out of scope;
  each PC has its own home, and backups move data between them.
