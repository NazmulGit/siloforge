-- SiloForge v2 schema (SQLite). Mirrors the v1.4 Postgres schema (migrations 001-011) so data can be
-- migrated 1:1, then adds v2 tables (api_tokens, job_logs, notifications, system_metrics, backups, kv).
-- Conventions: ids are TEXT uuids (v1 ids are preserved), timestamps are RFC 3339 UTC TEXT,
-- json columns are TEXT holding JSON, booleans are INTEGER 0/1, vectors are f32 little-endian BLOBs.

CREATE TABLE users (
  id            TEXT PRIMARY KEY,
  email         TEXT NOT NULL UNIQUE,
  password_hash TEXT NOT NULL,
  name          TEXT NOT NULL DEFAULT '',
  role          TEXT NOT NULL DEFAULT 'user' CHECK (role IN ('admin','user')),
  token_version INTEGER NOT NULL DEFAULT 0,
  created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE TABLE sites (
  id           TEXT PRIMARY KEY,
  domain       TEXT NOT NULL,
  wp_url       TEXT,
  mode         TEXT NOT NULL DEFAULT 'wp_rest' CHECK (mode IN ('wp_rest','sitemap')),
  api_key      TEXT NOT NULL UNIQUE,
  settings     TEXT NOT NULL DEFAULT '{}',
  secrets_enc  TEXT NOT NULL DEFAULT '',          -- v2: encrypted {wp_user, wp_app_password, gsc_credentials}
  created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  last_sync_at TEXT,
  user_id      TEXT REFERENCES users(id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX sites_domain_owner_uq ON sites(domain, ifnull(user_id, ''));
CREATE INDEX sites_user_idx ON sites(user_id);

CREATE TABLE clusters (
  id             TEXT PRIMARY KEY,
  site_id        TEXT NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  label          TEXT NOT NULL DEFAULT '',
  pillar_post_id TEXT REFERENCES posts(id) ON DELETE SET NULL,
  source         TEXT NOT NULL DEFAULT 'auto' CHECK (source IN ('auto','custom')),
  created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX clusters_site_source_idx ON clusters(site_id, source);

CREATE TABLE posts (
  id                     TEXT PRIMARY KEY,
  site_id                TEXT NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  wp_post_id             INTEGER,
  url                    TEXT NOT NULL,
  slug                   TEXT,
  title                  TEXT NOT NULL DEFAULT '',
  content_html           TEXT NOT NULL DEFAULT '',
  content_text           TEXT NOT NULL DEFAULT '',
  content_hash           TEXT NOT NULL DEFAULT '',
  builder                TEXT NOT NULL DEFAULT 'gutenberg',
  post_type              TEXT NOT NULL DEFAULT 'post',
  post_status            TEXT NOT NULL DEFAULT 'publish',
  meta                   TEXT NOT NULL DEFAULT '{}',
  word_count             INTEGER NOT NULL DEFAULT 0,
  cluster_id             TEXT REFERENCES clusters(id) ON DELETE SET NULL,
  pagerank               REAL NOT NULL DEFAULT 0,
  click_depth            INTEGER NOT NULL DEFAULT -1,
  is_orphan              INTEGER NOT NULL DEFAULT 0,
  is_dead_end            INTEGER NOT NULL DEFAULT 0,
  embedding              BLOB,
  chunked_hash           TEXT NOT NULL DEFAULT '',   -- v2: content_hash at last chunking (skip unchanged posts)
  published_at           TEXT,
  modified_at            TEXT,
  crawled_at             TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  outbound_link_count    INTEGER NOT NULL DEFAULT 0,
  internal_link_count    INTEGER NOT NULL DEFAULT 0,
  broken_link_count      INTEGER NOT NULL DEFAULT 0,
  nofollow_link_count    INTEGER NOT NULL DEFAULT 0,
  redirect_link_count    INTEGER NOT NULL DEFAULT 0,
  has_breadcrumbs        INTEGER NOT NULL DEFAULT 0,
  has_breadcrumb_schema  INTEGER NOT NULL DEFAULT 0,
  breadcrumb_report      TEXT NOT NULL DEFAULT '{}',
  link_health_checked_at TEXT,
  is_contextual_orphan   INTEGER NOT NULL DEFAULT 0,
  is_nav_only            INTEGER NOT NULL DEFAULT 0,
  body_inbound_count     INTEGER NOT NULL DEFAULT 0,
  nav_inbound_count      INTEGER NOT NULL DEFAULT 0,
  UNIQUE (site_id, url)
);
CREATE INDEX posts_cluster_idx ON posts(cluster_id);
CREATE INDEX posts_site_type_idx ON posts(site_id, post_type);
CREATE INDEX posts_contextual_orphan_idx ON posts(site_id) WHERE is_contextual_orphan = 1;

CREATE TABLE paragraphs (
  id        TEXT PRIMARY KEY,
  post_id   TEXT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
  idx       INTEGER NOT NULL,
  text      TEXT NOT NULL,
  hash      TEXT NOT NULL,
  embedding BLOB,
  UNIQUE (post_id, idx)
);
CREATE INDEX paragraphs_hash_idx ON paragraphs(hash);

CREATE TABLE keywords (
  id       TEXT PRIMARY KEY,
  post_id  TEXT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
  keyword  TEXT NOT NULL,
  source   TEXT NOT NULL CHECK (source IN ('title','htag','meta','body','gsc')),
  score    REAL NOT NULL DEFAULT 0,
  volume   INTEGER,
  position REAL,
  striking INTEGER NOT NULL DEFAULT 0,
  UNIQUE (post_id, keyword, source)
);

CREATE TABLE snapshots (
  id           TEXT PRIMARY KEY,
  post_id      TEXT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
  content_html TEXT NOT NULL,
  reason       TEXT NOT NULL DEFAULT 'pre-inject',
  taken_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX snapshots_post_idx ON snapshots(post_id);

CREATE TABLE link_edges (
  id              TEXT PRIMARY KEY,
  site_id         TEXT NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  source_post_id  TEXT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
  target_post_id  TEXT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
  paragraph_id    TEXT REFERENCES paragraphs(id) ON DELETE SET NULL,
  anchor          TEXT NOT NULL DEFAULT '',
  anchor_type     TEXT NOT NULL DEFAULT 'partial' CHECK (anchor_type IN ('exact','partial','branded','generic','lsi','naked','cta')),
  composite_score REAL NOT NULL DEFAULT 0,
  score_breakdown TEXT NOT NULL DEFAULT '{}',
  silo_topology   TEXT CHECK (silo_topology IN ('reverse','serial','priority','circle','bidirectional')),
  insert_module   TEXT CHECK (insert_module IN ('native','rephrase','context','content_gap')),
  status          TEXT NOT NULL DEFAULT 'suggested' CHECK (status IN ('suggested','approved','rejected','applied','reverted','failed')),
  origin          TEXT NOT NULL DEFAULT 'plan',
  snapshot_id     TEXT REFERENCES snapshots(id) ON DELETE SET NULL,
  error           TEXT,
  failure_kind    TEXT,
  created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  decided_at      TEXT,
  applied_at      TEXT,
  UNIQUE (site_id, source_post_id, target_post_id, anchor)
);
CREATE INDEX link_edges_site_status_idx ON link_edges(site_id, status);
CREATE INDEX link_edges_source_idx ON link_edges(source_post_id);
CREATE INDEX link_edges_target_idx ON link_edges(target_post_id);

CREATE TABLE job_runs (
  id           TEXT PRIMARY KEY,
  site_id      TEXT REFERENCES sites(id) ON DELETE CASCADE,
  kind         TEXT NOT NULL,
  status       TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','running','done','failed','cancelled')),
  stats        TEXT NOT NULL DEFAULT '{}',
  error        TEXT,
  payload      TEXT NOT NULL DEFAULT '{}',           -- v2: job input (was the BullMQ job data)
  attempts     INTEGER NOT NULL DEFAULT 0,           -- v2
  max_attempts INTEGER NOT NULL DEFAULT 1,           -- v2
  priority     INTEGER NOT NULL DEFAULT 0,           -- v2: higher runs first
  dedupe_key   TEXT,                                 -- v2: identical queued jobs collapse
  requested_by TEXT,                                 -- v2: user id / token name / 'schedule' / 'mcp'
  heartbeat_at TEXT,                                 -- v2: stale running rows are reaped
  run_after    TEXT,                                 -- v2: retry backoff
  resources    TEXT NOT NULL DEFAULT '{}',           -- v2: peak cpu/ram sampled while running
  started_at   TEXT,
  finished_at  TEXT,
  created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX job_runs_site_created_idx ON job_runs(site_id, created_at DESC);
CREATE INDEX job_runs_status_idx ON job_runs(status, priority DESC, created_at);
CREATE UNIQUE INDEX job_runs_dedupe_uq ON job_runs(dedupe_key) WHERE status IN ('queued','running') AND dedupe_key IS NOT NULL;

CREATE TABLE job_logs (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  job_run_id TEXT NOT NULL REFERENCES job_runs(id) ON DELETE CASCADE,
  ts         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  level      TEXT NOT NULL DEFAULT 'info' CHECK (level IN ('trace','debug','info','warn','error')),
  message    TEXT NOT NULL,
  data       TEXT
);
CREATE INDEX job_logs_run_idx ON job_logs(job_run_id, id);

CREATE TABLE keyword_rules (
  id          TEXT PRIMARY KEY,
  site_id     TEXT NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  keyword     TEXT NOT NULL,
  target_url  TEXT NOT NULL,
  anchor_type TEXT NOT NULL DEFAULT 'exact' CHECK (anchor_type IN ('exact','partial','branded','generic','lsi','naked','cta')),
  max_links   INTEGER NOT NULL DEFAULT 3 CHECK (max_links >= 1),
  active      INTEGER NOT NULL DEFAULT 1,
  created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  UNIQUE (site_id, keyword, target_url)
);

CREATE TABLE ignore_keywords (
  id         TEXT PRIMARY KEY,
  site_id    TEXT NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  phrase     TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  UNIQUE (site_id, phrase)
);

CREATE TABLE link_health (
  id                TEXT PRIMARY KEY,
  site_id           TEXT NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  source_post_id    TEXT REFERENCES posts(id) ON DELETE CASCADE,
  href              TEXT NOT NULL,
  requested_url     TEXT NOT NULL,
  resolved_url      TEXT,
  target_post_id    TEXT REFERENCES posts(id) ON DELETE SET NULL,
  scope             TEXT NOT NULL DEFAULT 'internal' CHECK (scope IN ('internal','external')),
  status_code       INTEGER,
  ok                INTEGER NOT NULL DEFAULT 0,
  is_broken         INTEGER NOT NULL DEFAULT 0,
  redirect_hops     INTEGER NOT NULL DEFAULT 0,
  redirect_chain    TEXT NOT NULL DEFAULT '[]',
  has_redirect_loop INTEGER NOT NULL DEFAULT 0,
  has_temp_redirect INTEGER NOT NULL DEFAULT 0,
  rel               TEXT NOT NULL DEFAULT '',
  is_nofollow       INTEGER NOT NULL DEFAULT 0,
  is_sponsored      INTEGER NOT NULL DEFAULT 0,
  is_ugc            INTEGER NOT NULL DEFAULT 0,
  anchor            TEXT NOT NULL DEFAULT '',
  placement         TEXT NOT NULL DEFAULT 'body' CHECK (placement IN ('body','nav','header','footer','aside','unknown')),
  error             TEXT,
  checked_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  failure_kind      TEXT NOT NULL DEFAULT 'none',
  is_unverified     INTEGER NOT NULL DEFAULT 0,
  UNIQUE (site_id, source_post_id, href, anchor)
);
CREATE INDEX link_health_site_idx ON link_health(site_id);
CREATE INDEX link_health_broken_idx ON link_health(site_id) WHERE is_broken = 1;
CREATE INDEX link_health_source_idx ON link_health(source_post_id);

CREATE TABLE site_schedules (
  id              TEXT PRIMARY KEY,
  site_id         TEXT NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  job_kind        TEXT NOT NULL CHECK (job_kind IN ('crawl','plan','link_health','audit')),
  cron            TEXT NOT NULL,
  tz              TEXT NOT NULL DEFAULT 'UTC',
  active          INTEGER NOT NULL DEFAULT 1,
  payload         TEXT NOT NULL DEFAULT '{}',
  last_run_at     TEXT,
  last_job_run_id TEXT REFERENCES job_runs(id) ON DELETE SET NULL,
  next_run_at     TEXT,
  created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  UNIQUE (site_id, job_kind)
);

CREATE TABLE site_metrics_history (
  id          TEXT PRIMARY KEY,
  site_id     TEXT NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  job_run_id  TEXT REFERENCES job_runs(id) ON DELETE SET NULL,
  source      TEXT NOT NULL DEFAULT 'crawl' CHECK (source IN ('crawl','plan','link_health','audit','manual')),
  metrics     TEXT NOT NULL DEFAULT '{}',
  captured_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  m_posts            INTEGER GENERATED ALWAYS AS (CAST(json_extract(metrics,'$.posts') AS INTEGER)) VIRTUAL,
  m_orphans          INTEGER GENERATED ALWAYS AS (CAST(json_extract(metrics,'$.orphans') AS INTEGER)) VIRTUAL,
  m_broken_links     INTEGER GENERATED ALWAYS AS (CAST(json_extract(metrics,'$.broken_links') AS INTEGER)) VIRTUAL,
  m_internal_links   INTEGER GENERATED ALWAYS AS (CAST(json_extract(metrics,'$.internal_links') AS INTEGER)) VIRTUAL,
  m_health_score     INTEGER GENERATED ALWAYS AS (CAST(json_extract(metrics,'$.health_score') AS INTEGER)) VIRTUAL,
  m_avg_click_depth  REAL    GENERATED ALWAYS AS (CAST(json_extract(metrics,'$.avg_click_depth') AS REAL)) VIRTUAL
);
CREATE INDEX site_metrics_site_idx ON site_metrics_history(site_id, captured_at DESC);

-- AI model management (v1 migration 008 shape).
CREATE TABLE model_providers (
  id            TEXT PRIMARY KEY,
  name          TEXT NOT NULL UNIQUE,
  label         TEXT NOT NULL DEFAULT '',
  kind          TEXT NOT NULL CHECK (kind IN ('openai-compatible','anthropic','openrouter','ollama','gemini','custom')),
  base_url      TEXT NOT NULL DEFAULT '',
  api_key_enc   TEXT NOT NULL DEFAULT '',
  headers       TEXT NOT NULL DEFAULT '{}',
  enabled       INTEGER NOT NULL DEFAULT 1,
  last_probe_at TEXT,
  last_probe_ok INTEGER,
  last_probe_error TEXT,
  last_probe_ms INTEGER,
  created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE TABLE models (
  id               TEXT PRIMARY KEY,
  provider_id      TEXT NOT NULL REFERENCES model_providers(id) ON DELETE CASCADE,
  model_id         TEXT NOT NULL,
  label            TEXT NOT NULL DEFAULT '',
  role             TEXT NOT NULL CHECK (role IN ('chat','embed')),
  context_tokens   INTEGER,
  dim              INTEGER,
  enabled          INTEGER NOT NULL DEFAULT 1,
  price_in         REAL,
  price_out        REAL,
  notes            TEXT NOT NULL DEFAULT '',
  last_probe_at    TEXT,
  last_probe_ok    INTEGER,
  last_probe_error TEXT,
  last_probe_ms    INTEGER,
  created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  UNIQUE (provider_id, model_id, role)
);

CREATE TABLE model_chains (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  role       TEXT NOT NULL CHECK (role IN ('chat','embed')),
  site_id    TEXT REFERENCES sites(id) ON DELETE CASCADE,
  active     INTEGER NOT NULL DEFAULT 0,
  notes      TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE UNIQUE INDEX model_chains_active_site_uq ON model_chains(role, site_id) WHERE active = 1 AND site_id IS NOT NULL;
CREATE UNIQUE INDEX model_chains_active_global_uq ON model_chains(role) WHERE active = 1 AND site_id IS NULL;

CREATE TABLE model_chain_rungs (
  id          TEXT PRIMARY KEY,
  chain_id    TEXT NOT NULL REFERENCES model_chains(id) ON DELETE CASCADE,
  model_id    TEXT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
  position    INTEGER NOT NULL CHECK (position >= 0),
  timeout_ms  INTEGER,
  max_retries INTEGER,
  enabled     INTEGER NOT NULL DEFAULT 1,
  UNIQUE (chain_id, position)
);

CREATE TABLE model_call_log (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  model_row_id TEXT REFERENCES models(id) ON DELETE CASCADE,
  chain_id     TEXT REFERENCES model_chains(id) ON DELETE SET NULL,
  provider     TEXT,
  role         TEXT NOT NULL,
  rung         INTEGER NOT NULL DEFAULT 0,
  ok           INTEGER NOT NULL,
  status_code  INTEGER,
  latency_ms   INTEGER,
  tokens_in    INTEGER,
  tokens_out   INTEGER,
  error        TEXT,
  called_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX model_call_log_model_idx ON model_call_log(model_row_id, called_at DESC);
CREATE INDEX model_call_log_called_idx ON model_call_log(called_at);

-- v2: long-lived API tokens for external control (Hermes, MCP, scripts). Only a SHA-256 hash is stored.
CREATE TABLE api_tokens (
  id           TEXT PRIMARY KEY,
  user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name         TEXT NOT NULL,
  token_hash   TEXT NOT NULL UNIQUE,
  prefix       TEXT NOT NULL,
  scopes       TEXT NOT NULL DEFAULT '["*"]',
  created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  last_used_at TEXT,
  expires_at   TEXT,
  revoked_at   TEXT
);

-- v2: notification channels and delivery log.
CREATE TABLE notification_channels (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  kind       TEXT NOT NULL CHECK (kind IN ('webhook','ntfy','telegram','hermes','desktop','log')),
  config_enc TEXT NOT NULL DEFAULT '',
  events     TEXT NOT NULL DEFAULT '["job.failed","job.done","backup.done","backup.failed","system.alert"]',
  enabled    INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE TABLE notifications (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  event      TEXT NOT NULL,
  level      TEXT NOT NULL DEFAULT 'info',
  title      TEXT NOT NULL,
  body       TEXT NOT NULL DEFAULT '',
  data       TEXT NOT NULL DEFAULT '{}',
  site_id    TEXT,
  job_run_id TEXT,
  delivered  TEXT NOT NULL DEFAULT '{}',
  read_at    TEXT,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX notifications_created_idx ON notifications(created_at DESC);

-- v2: resource samples (1-minute aggregates; raw 5 s samples stay in memory).
CREATE TABLE system_metrics (
  ts            TEXT PRIMARY KEY,
  cpu_pct       REAL NOT NULL,
  proc_cpu_pct  REAL NOT NULL,
  mem_used_mb   INTEGER NOT NULL,
  mem_total_mb  INTEGER NOT NULL,
  proc_rss_mb   INTEGER NOT NULL,
  jobs_running  INTEGER NOT NULL DEFAULT 0,
  jobs_queued   INTEGER NOT NULL DEFAULT 0,
  disk_free_mb  INTEGER
);

-- v2: error tracking (deduplicated by fingerprint).
CREATE TABLE errors (
  fingerprint TEXT PRIMARY KEY,
  source      TEXT NOT NULL,
  message     TEXT NOT NULL,
  context     TEXT NOT NULL DEFAULT '{}',
  count       INTEGER NOT NULL DEFAULT 1,
  first_seen  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  last_seen   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  resolved_at TEXT
);

-- v2: backup catalogue.
CREATE TABLE backups (
  id         TEXT PRIMARY KEY,
  path       TEXT NOT NULL,
  kind       TEXT NOT NULL DEFAULT 'full' CHECK (kind IN ('full','db','config')),
  size_bytes INTEGER NOT NULL DEFAULT 0,
  sha256     TEXT NOT NULL DEFAULT '',
  encrypted  INTEGER NOT NULL DEFAULT 0,
  app_version TEXT NOT NULL DEFAULT '',
  status     TEXT NOT NULL DEFAULT 'ok',
  note       TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

-- v2: audit trail of mutating API calls (who did what, via UI/API/MCP).
CREATE TABLE audit_log (
  id       INTEGER PRIMARY KEY AUTOINCREMENT,
  ts       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  actor    TEXT NOT NULL,
  via      TEXT NOT NULL DEFAULT 'ui',
  action   TEXT NOT NULL,
  target   TEXT,
  detail   TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX audit_log_ts_idx ON audit_log(ts DESC);

-- v2: small key/value settings (instance id, feature flags, migration markers).
CREATE TABLE kv (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
