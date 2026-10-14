-- v2 reliability fixes (docs/testing/reliability-audit.md).

-- R5: every foreign-key child column gets an index. Without one, deleting a parent row makes SQLite
-- full-scan the child table to apply ON DELETE SET NULL / CASCADE (23 ms per paragraph delete on
-- 150k edges, so a 10k-paragraph re-chunk held the single writer for minutes).
CREATE INDEX IF NOT EXISTS link_edges_paragraph_idx ON link_edges(paragraph_id);
CREATE INDEX IF NOT EXISTS link_edges_snapshot_idx ON link_edges(snapshot_id);
CREATE INDEX IF NOT EXISTS link_health_target_post_idx ON link_health(target_post_id);
CREATE INDEX IF NOT EXISTS clusters_pillar_idx ON clusters(pillar_post_id);
CREATE INDEX IF NOT EXISTS site_schedules_last_job_idx ON site_schedules(last_job_run_id);
CREATE INDEX IF NOT EXISTS site_metrics_job_idx ON site_metrics_history(job_run_id);
CREATE INDEX IF NOT EXISTS model_chains_site_idx ON model_chains(site_id);
CREATE INDEX IF NOT EXISTS model_chain_rungs_model_idx ON model_chain_rungs(model_id);
CREATE INDEX IF NOT EXISTS model_call_log_chain_idx ON model_call_log(chain_id);
CREATE INDEX IF NOT EXISTS api_tokens_user_idx ON api_tokens(user_id);

-- R3: cancelling a running job only flags it; the row stays 'running' (so the site stays busy for
-- claim_next) until the runner sees the pipeline actually stop and writes 'cancelled'. The API
-- reports a flagged row as 'cancelled' (v1 shape).
ALTER TABLE job_runs ADD COLUMN cancel_requested_at TEXT;

-- R4: write-ahead intent for WordPress writes (inject, verify rollback, precise undo). A row is
-- written before the PUT and deleted in the same transaction as the bookkeeping that follows it.
-- A row that survives (abort, crash, transport timeout) is reconciled against the live raw content
-- by the next inject/undo of the site (and a reconcile job enqueued at boot).
CREATE TABLE IF NOT EXISTS inject_journal (
  id           TEXT PRIMARY KEY,
  site_id      TEXT NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  post_id      TEXT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
  job_run_id   TEXT,
  op           TEXT NOT NULL CHECK (op IN ('inject','rollback','undo')),
  snapshot_id  TEXT REFERENCES snapshots(id) ON DELETE SET NULL,
  wp_post_id   INTEGER NOT NULL,
  post_type    TEXT NOT NULL,
  edges        TEXT NOT NULL DEFAULT '[]',   -- JSON [{id, link_start, module, text, result}]
  expected     TEXT NOT NULL,                -- raw content WordPress holds if the write landed
  previous     TEXT NOT NULL,                -- raw content before the write
  created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS inject_journal_site_idx ON inject_journal(site_id);
CREATE INDEX IF NOT EXISTS inject_journal_post_idx ON inject_journal(post_id);
CREATE INDEX IF NOT EXISTS inject_journal_snapshot_idx ON inject_journal(snapshot_id);

-- R9: modified_gmt WordPress returned for SiloForge's last write to the post. Snapshot-mode undo
-- refuses (unless forced) when the live post was modified after it.
ALTER TABLE posts ADD COLUMN sf_written_gmt TEXT;

ANALYZE;
