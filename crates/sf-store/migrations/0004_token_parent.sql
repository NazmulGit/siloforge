-- v2 security (M-04): API tokens minted by another API token record their parent, so revoking
-- (or expiring) a token can take every descendant with it.
ALTER TABLE api_tokens ADD COLUMN parent_id TEXT REFERENCES api_tokens(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS api_tokens_parent_idx ON api_tokens(parent_id);
