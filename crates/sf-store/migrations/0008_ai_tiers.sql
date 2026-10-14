-- v2.3 AI task tiers, token accounting and the AI result cache (docs/specs/v2.3-ai-tiers.md).

-- Chains get a tier; existing chains are "main". One active chain per (role, tier, scope).
ALTER TABLE model_chains ADD COLUMN tier TEXT NOT NULL DEFAULT 'main' CHECK (tier IN ('low','main','high'));
DROP INDEX IF EXISTS model_chains_active_site_uq;
DROP INDEX IF EXISTS model_chains_active_global_uq;
CREATE UNIQUE INDEX model_chains_active_site_uq ON model_chains(role, tier, site_id) WHERE active = 1 AND site_id IS NOT NULL;
CREATE UNIQUE INDEX model_chains_active_global_uq ON model_chains(role, tier) WHERE active = 1 AND site_id IS NULL;

-- Token accounting on every call (task / tier / site / run, cache hits, cost, estimated tokens).
ALTER TABLE model_call_log ADD COLUMN task TEXT;
ALTER TABLE model_call_log ADD COLUMN tier TEXT;
ALTER TABLE model_call_log ADD COLUMN site_id TEXT;
ALTER TABLE model_call_log ADD COLUMN run_id TEXT;
ALTER TABLE model_call_log ADD COLUMN cache_hit INTEGER NOT NULL DEFAULT 0;
ALTER TABLE model_call_log ADD COLUMN cost_usd REAL NOT NULL DEFAULT 0;
ALTER TABLE model_call_log ADD COLUMN estimated INTEGER NOT NULL DEFAULT 0;
ALTER TABLE model_call_log ADD COLUMN items INTEGER NOT NULL DEFAULT 1;
ALTER TABLE model_call_log ADD COLUMN model_id TEXT;
CREATE INDEX IF NOT EXISTS model_call_log_tier_idx ON model_call_log(called_at, tier);
CREATE INDEX IF NOT EXISTS model_call_log_site_idx ON model_call_log(site_id, called_at);
CREATE INDEX IF NOT EXISTS model_call_log_run_idx ON model_call_log(run_id);

-- AI result cache: sha256(task, model family, prompt-relevant inputs) -> result JSON.
CREATE TABLE IF NOT EXISTS ai_cache (
  key        TEXT PRIMARY KEY,
  task       TEXT NOT NULL,
  family     TEXT NOT NULL DEFAULT '',
  result     TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  expires_at TEXT NOT NULL,
  hits       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS ai_cache_expires_idx ON ai_cache(expires_at);
