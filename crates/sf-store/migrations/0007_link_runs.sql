-- v2.2 Link Runs (docs/specs/v2.2-link-runs.md §1) + Auto-Linker schedules (§5).

CREATE TABLE IF NOT EXISTS link_runs (
  id             TEXT PRIMARY KEY,
  site_id        TEXT NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  name           TEXT NOT NULL DEFAULT '',
  status         TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft','crawling','planning','ready',
                   'deploying','deployed','partially_deployed','failed','cancelled','undone')),
  options        TEXT NOT NULL DEFAULT '{}',   -- §2 options (validated, defaults filled in)
  stats          TEXT NOT NULL DEFAULT '{}',   -- {opportunities, selected, applied, failed, not_present, repaired, skipped, by_strategy}
  error          TEXT,
  crawl_job_id   TEXT,
  plan_job_id    TEXT,
  deploy_job_id  TEXT,
  repair_job_id  TEXT,
  verify_job_id  TEXT,
  undo_job_id    TEXT,
  current_job_id TEXT,                         -- the link_run job driving the run right now
  created_by     TEXT,
  created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  deployed_at    TEXT,
  auto           INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS link_runs_site_idx ON link_runs(site_id, created_at);

-- Edges produced (or adopted) by a run. run_selected = ticked for deploy; verified_at /
-- verify_present = last live-page check of an applied link (NULL = never / inconclusive).
ALTER TABLE link_edges ADD COLUMN run_id TEXT REFERENCES link_runs(id) ON DELETE SET NULL;
ALTER TABLE link_edges ADD COLUMN run_selected INTEGER NOT NULL DEFAULT 0;
ALTER TABLE link_edges ADD COLUMN verified_at TEXT;
ALTER TABLE link_edges ADD COLUMN verify_present INTEGER;
CREATE INDEX IF NOT EXISTS link_edges_run_idx ON link_edges(run_id, status);

-- site_schedules.job_kind gains 'link_run' (Auto-Linker). SQLite cannot alter a CHECK, so the
-- table is rebuilt; nothing references site_schedules, and the migration runs in one transaction.
CREATE TABLE site_schedules_v7 (
  id              TEXT PRIMARY KEY,
  site_id         TEXT NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  job_kind        TEXT NOT NULL CHECK (job_kind IN ('crawl','plan','link_health','audit','link_run')),
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
INSERT INTO site_schedules_v7(id, site_id, job_kind, cron, tz, active, payload, last_run_at,
    last_job_run_id, next_run_at, created_at, updated_at)
  SELECT id, site_id, job_kind, cron, tz, active, payload, last_run_at, last_job_run_id,
    next_run_at, created_at, updated_at FROM site_schedules;
DROP TABLE site_schedules;
ALTER TABLE site_schedules_v7 RENAME TO site_schedules;
CREATE INDEX IF NOT EXISTS site_schedules_last_job_idx ON site_schedules(last_job_run_id);
