-- v2 (WP J): raw WordPress source for block-aware injection, optimistic-concurrency stamp, and the
-- byte offset of each injected link (precise undo). Plus partial indexes for the incremental pipeline.

-- `content.raw` from WP REST `context=edit` (NULL when crawled without credentials / sitemap mode).
ALTER TABLE posts ADD COLUMN content_raw TEXT;
-- WP `modified_gmt` as last seen (the precondition for writes).
ALTER TABLE posts ADD COLUMN modified_gmt TEXT;
-- Byte offset of the inserted `<a` in the raw content right after injection (undo hint).
ALTER TABLE link_edges ADD COLUMN link_start INTEGER;

-- Posts whose content changed since the last chunking (incremental chunk scan).
CREATE INDEX posts_unchunked_idx ON posts(site_id) WHERE content_hash <> chunked_hash;
-- Rows still waiting for an embedding (incremental embed scan).
CREATE INDEX posts_unembedded_idx ON posts(site_id) WHERE embedding IS NULL;
CREATE INDEX paragraphs_unembedded_idx ON paragraphs(post_id) WHERE embedding IS NULL;
-- Inbound lookups by target + status (density, scoring, anchors).
CREATE INDEX link_edges_site_target_idx ON link_edges(site_id, target_post_id, status);
