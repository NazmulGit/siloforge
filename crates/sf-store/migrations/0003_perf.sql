-- v2 perf: covering indexes for the heavy v1 read endpoints (reports, dashboard, posts list).
-- `posts` rows carry content_html/content_text (tens of KB on overflow pages) in front of the
-- counters, so every per-site scan of pagerank/is_orphan/... walked the overflow chains. These
-- indexes hold the narrow columns the read endpoints need, so those scans never touch the table.

-- /reports/graph nodes (ORDER BY pagerank DESC, id), /reports/links, /dashboard counters,
-- /posts list, orphan lists, max(pagerank).
CREATE INDEX IF NOT EXISTS posts_site_rank_idx ON posts(
  site_id, pagerank DESC, id, url, title, post_type, cluster_id, is_orphan, is_dead_end, click_depth);
-- /reports/link-health breadcrumb counters.
CREATE INDEX IF NOT EXISTS posts_site_crumbs_idx ON posts(
  site_id, link_health_checked_at, has_breadcrumbs, has_breadcrumb_schema);

-- Per-pair edge aggregates: inbound/outbound/duplicates (links report, dashboard, posts list)
-- and the graph report edges (status/score/id order) without touching the wide edge rows.
CREATE INDEX IF NOT EXISTS link_edges_site_pair_idx ON link_edges(
  site_id, source_post_id, target_post_id, status, origin, composite_score, id);
-- /edges list: walks (composite_score DESC, created_at, id) in index order with the filters
-- and join keys at hand, so LIMIT/OFFSET pages no longer sort every edge of the site.
CREATE INDEX IF NOT EXISTS link_edges_site_score_idx ON link_edges(
  site_id, composite_score DESC, created_at, id, status, origin, source_post_id, target_post_id);
-- Anchor reports and cannibalization warnings.
CREATE INDEX IF NOT EXISTS link_edges_site_anchor_idx ON link_edges(
  site_id, anchor, anchor_type, status, target_post_id);

-- /reports/link-health summary counters (one narrow scan instead of the wide rows).
CREATE INDEX IF NOT EXISTS link_health_site_summary_idx ON link_health(
  site_id, scope, is_broken, is_unverified, failure_kind, redirect_hops, has_temp_redirect,
  is_nofollow, is_sponsored, is_ugc, checked_at);
-- Dashboard outbound domains.
CREATE INDEX IF NOT EXISTS link_health_site_ext_idx ON link_health(site_id, scope, requested_url);
-- Dashboard insecure-link counter (expression must match the query exactly).
CREATE INDEX IF NOT EXISTS link_health_http_idx ON link_health(site_id) WHERE lower(href) LIKE 'http://%';
-- Link-health report lists (WHERE terms must match the queries exactly to be usable). The
-- nofollow list is large (a third of all links on big sites), so its index also carries the sort
-- keys: the page is picked from the index alone and only the page rows are read from the table.
CREATE INDEX IF NOT EXISTS link_health_redirect_idx ON link_health(site_id) WHERE redirect_hops>0;
CREATE INDEX IF NOT EXISTS link_health_unverified_idx ON link_health(site_id) WHERE is_unverified=1;
CREATE INDEX IF NOT EXISTS link_health_blocked_idx ON link_health(site_id) WHERE failure_kind='blocked';
CREATE INDEX IF NOT EXISTS link_health_server_error_idx ON link_health(site_id, status_code)
  WHERE failure_kind='server_error';
CREATE INDEX IF NOT EXISTS link_health_nofollow_idx ON link_health(site_id, scope, source_post_id, href)
  WHERE is_nofollow=1;

-- Fresh planner statistics so the new covering indexes are picked over posts_site_type_idx.
ANALYZE;
