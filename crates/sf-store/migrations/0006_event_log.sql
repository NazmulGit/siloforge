-- v2.1 unified event log (contract §9). Append-only: rows are only inserted (batched by the
-- sf-ops EventLog writer) and deleted by retention (`monitor.event_log_retain_days`).
CREATE TABLE IF NOT EXISTS event_log (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  ts           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  source       TEXT NOT NULL,              -- ui | api | mcp | job | system | auth | notify
  actor        TEXT NOT NULL,              -- user:<id> | token:<name> | mcp:<token> | site:<id> | system | anonymous
  action       TEXT NOT NULL,              -- "PATCH /api/v1/sites/{id}", "job plan done", "login failed", ...
  target       TEXT,                       -- site / job / edge / token id
  site_id      TEXT,
  status       TEXT NOT NULL DEFAULT 'ok', -- ok | error | denied
  http_status  INTEGER,
  duration_ms  INTEGER,
  detail       TEXT,                       -- JSON object, never bodies or secrets
  user_id      TEXT                        -- owning user (non-admins see only their own rows)
);
CREATE INDEX IF NOT EXISTS event_log_ts_idx ON event_log(ts);
CREATE INDEX IF NOT EXISTS event_log_source_ts_idx ON event_log(source, ts);
CREATE INDEX IF NOT EXISTS event_log_actor_ts_idx ON event_log(actor, ts);
CREATE INDEX IF NOT EXISTS event_log_site_ts_idx ON event_log(site_id, ts);
CREATE INDEX IF NOT EXISTS event_log_user_ts_idx ON event_log(user_id, ts);
