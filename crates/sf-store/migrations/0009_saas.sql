-- v2.4 SaaS readiness (docs/specs/v2.4-saas.md): plans, workspaces, members, invites, sessions,
-- one-time auth tokens, 2FA recovery codes, mail outbox, monthly usage snapshots.

CREATE TABLE IF NOT EXISTS plans (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  limits      TEXT NOT NULL DEFAULT '{}',
  price_label TEXT NOT NULL DEFAULT '',
  public      INTEGER NOT NULL DEFAULT 1,
  sort        INTEGER NOT NULL DEFAULT 0,
  created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

INSERT OR IGNORE INTO plans(id, name, limits, price_label, public, sort) VALUES
 ('free', 'Free', '{"sites":1,"posts_per_site":500,"link_runs_per_month":5,"ai_tokens_per_month":100000,"members":1,"api_tokens":1,"schedules":1,"auto_linker":false,"mcp":false}', 'Free', 1, 10),
 ('pro', 'Pro', '{"sites":10,"posts_per_site":5000,"link_runs_per_month":100,"ai_tokens_per_month":2000000,"members":5,"api_tokens":10,"schedules":20,"auto_linker":true,"mcp":true}', '', 1, 20),
 ('agency', 'Agency', '{"sites":100,"posts_per_site":50000,"link_runs_per_month":null,"ai_tokens_per_month":20000000,"members":25,"api_tokens":50,"schedules":200,"auto_linker":true,"mcp":true}', '', 1, 30),
 ('unlimited', 'Unlimited', '{"sites":null,"posts_per_site":null,"link_runs_per_month":null,"ai_tokens_per_month":null,"members":null,"api_tokens":null,"schedules":null,"auto_linker":true,"mcp":true}', 'Internal', 0, 100);

CREATE TABLE IF NOT EXISTS workspaces (
  id            TEXT PRIMARY KEY,
  name          TEXT NOT NULL,
  slug          TEXT NOT NULL UNIQUE,
  plan_id       TEXT NOT NULL DEFAULT 'free' REFERENCES plans(id),
  status        TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','suspended')),
  owner_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
  settings      TEXT NOT NULL DEFAULT '{}',
  is_default    INTEGER NOT NULL DEFAULT 0,
  created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  deleted_at    TEXT
);
CREATE INDEX IF NOT EXISTS workspaces_plan_idx ON workspaces(plan_id);
CREATE INDEX IF NOT EXISTS workspaces_owner_idx ON workspaces(owner_user_id);

CREATE TABLE IF NOT EXISTS workspace_members (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  role         TEXT NOT NULL DEFAULT 'member' CHECK (role IN ('owner','admin','member','viewer')),
  invited_by   TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  PRIMARY KEY (workspace_id, user_id)
);
CREATE INDEX IF NOT EXISTS workspace_members_user_idx ON workspace_members(user_id);
CREATE INDEX IF NOT EXISTS workspace_members_invited_by_idx ON workspace_members(invited_by);

CREATE TABLE IF NOT EXISTS workspace_invites (
  id           TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  email        TEXT NOT NULL,
  role         TEXT NOT NULL DEFAULT 'member' CHECK (role IN ('admin','member','viewer')),
  token_hash   TEXT NOT NULL UNIQUE,
  invited_by   TEXT REFERENCES users(id) ON DELETE SET NULL,
  created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  expires_at   TEXT NOT NULL,
  accepted_at  TEXT,
  accepted_by  TEXT REFERENCES users(id) ON DELETE SET NULL,
  revoked_at   TEXT
);
CREATE INDEX IF NOT EXISTS workspace_invites_ws_idx ON workspace_invites(workspace_id);
CREATE INDEX IF NOT EXISTS workspace_invites_invited_by_idx ON workspace_invites(invited_by);
CREATE INDEX IF NOT EXISTS workspace_invites_accepted_by_idx ON workspace_invites(accepted_by);

CREATE TABLE IF NOT EXISTS sessions (
  id           TEXT PRIMARY KEY,
  user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  last_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  expires_at   TEXT NOT NULL,
  ip           TEXT NOT NULL DEFAULT '',
  user_agent   TEXT NOT NULL DEFAULT '',
  remember     INTEGER NOT NULL DEFAULT 1,
  revoked_at   TEXT
);
CREATE INDEX IF NOT EXISTS sessions_user_idx ON sessions(user_id, created_at);

-- One-time links: verify_email | reset_password | change_email (sha256 of the token).
CREATE TABLE IF NOT EXISTS auth_tokens (
  token_hash TEXT PRIMARY KEY,
  purpose    TEXT NOT NULL CHECK (purpose IN ('verify_email','reset_password','change_email')),
  user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  email      TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  expires_at TEXT NOT NULL,
  used_at    TEXT
);
CREATE INDEX IF NOT EXISTS auth_tokens_user_idx ON auth_tokens(user_id, purpose);

CREATE TABLE IF NOT EXISTS user_recovery_codes (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  code_hash  TEXT NOT NULL,
  used_at    TEXT,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS user_recovery_codes_user_idx ON user_recovery_codes(user_id);

-- Mail that could not be (or was not) sent through SMTP: platform admins deliver links by hand.
CREATE TABLE IF NOT EXISTS mail_outbox (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  to_email   TEXT NOT NULL,
  subject    TEXT NOT NULL,
  body       TEXT NOT NULL DEFAULT '',
  kind       TEXT NOT NULL DEFAULT '',
  link       TEXT,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  sent_at    TEXT,
  error      TEXT
);

CREATE TABLE IF NOT EXISTS usage_monthly (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  month        TEXT NOT NULL,
  sites        INTEGER NOT NULL DEFAULT 0,
  posts        INTEGER NOT NULL DEFAULT 0,
  link_runs    INTEGER NOT NULL DEFAULT 0,
  ai_tokens    INTEGER NOT NULL DEFAULT 0,
  members      INTEGER NOT NULL DEFAULT 0,
  updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  PRIMARY KEY (workspace_id, month)
);

ALTER TABLE users ADD COLUMN email_verified_at TEXT;
ALTER TABLE users ADD COLUMN pending_email TEXT;
ALTER TABLE users ADD COLUMN totp_secret_enc TEXT;
ALTER TABLE users ADD COLUMN totp_pending_enc TEXT;
ALTER TABLE users ADD COLUMN totp_last_step INTEGER NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN disabled_at TEXT;
ALTER TABLE users ADD COLUMN last_login_at TEXT;

ALTER TABLE sites ADD COLUMN workspace_id TEXT REFERENCES workspaces(id);
CREATE INDEX IF NOT EXISTS sites_workspace_idx ON sites(workspace_id);
ALTER TABLE api_tokens ADD COLUMN workspace_id TEXT REFERENCES workspaces(id) ON DELETE CASCADE;
CREATE INDEX IF NOT EXISTS api_tokens_workspace_idx ON api_tokens(workspace_id);

-- Existing accounts are trusted.
UPDATE users SET email_verified_at = created_at WHERE email_verified_at IS NULL;

-- Back-fill (spec §9.1): Default workspace (Unlimited) owned by the first admin; all admins join it.
INSERT OR IGNORE INTO workspaces(id, name, slug, plan_id, status, owner_user_id, is_default)
VALUES ('default', 'Default', 'default', 'unlimited', 'active',
        (SELECT id FROM users WHERE role = 'admin' ORDER BY created_at, id LIMIT 1), 1);
INSERT OR IGNORE INTO workspace_members(workspace_id, user_id, role)
SELECT 'default', id,
       CASE WHEN id = (SELECT owner_user_id FROM workspaces WHERE id = 'default') THEN 'owner' ELSE 'admin' END
  FROM users WHERE role = 'admin';

-- Each non-admin user keeps their own sites in a personal workspace (so nobody sees more than before).
INSERT OR IGNORE INTO workspaces(id, name, slug, plan_id, status, owner_user_id)
SELECT 'ws-' || id,
       CASE WHEN trim(name) <> '' THEN trim(name) ELSE email END || ' workspace',
       'ws-' || substr(id, 1, 8) || '-' || substr(lower(hex(randomblob(2))), 1, 4),
       'unlimited', 'active', id
  FROM users WHERE role <> 'admin';
INSERT OR IGNORE INTO workspace_members(workspace_id, user_id, role)
SELECT 'ws-' || id, id, 'owner' FROM users WHERE role <> 'admin';

UPDATE sites SET workspace_id = CASE
    WHEN user_id IS NOT NULL AND EXISTS (SELECT 1 FROM users u WHERE u.id = sites.user_id AND u.role <> 'admin')
      THEN 'ws-' || user_id
    ELSE 'default' END
 WHERE workspace_id IS NULL;

UPDATE api_tokens SET workspace_id = CASE
    WHEN EXISTS (SELECT 1 FROM users u WHERE u.id = api_tokens.user_id AND u.role <> 'admin')
      THEN 'ws-' || user_id
    ELSE 'default' END
 WHERE workspace_id IS NULL;
