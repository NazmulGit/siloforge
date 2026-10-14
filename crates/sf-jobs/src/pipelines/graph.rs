//! Link graph: nodes = every post (ORDER BY id), body edges = committed links (status applied OR
//! origin existing, kept in sync with the live content by the chunker), nav provenance from
//! `link_health`. Metrics (PageRank, click depth with fixed roots, orphan/dead-end/contextual
//! flags, inbound counts) are computed on the rayon pool and persisted in batched updates (rows
//! whose metrics did not change are skipped: a `posts` UPDATE rewrites the whole wide row).

use std::collections::HashMap;

use rusqlite::params;
use serde_json::Value;
use sf_engine::graph::{GraphInput, GraphNode, GraphOutput, GraphSummary, NodeMetric};
use sf_store::err;

use super::common::SiteInfo;
use crate::ctx::{Job, JobContext, JobError, JobResult, compute};

/// Graph metrics plus the distinct committed body edges they were computed from.
pub struct GraphData {
    pub metrics: HashMap<String, NodeMetric>,
    /// Distinct (source, target) committed body links (self-loops dropped).
    pub edges: Vec<(String, String)>,
    pub summary: GraphSummary,
}

pub async fn build_graph(
    ctx: &JobContext,
    job: &Job,
    site: &SiteInfo,
    persist: bool,
) -> JobResult<GraphData> {
    let sid = site.id.clone();
    let (nodes, edges, nav): (Vec<GraphNode>, Vec<(String, String)>, HashMap<String, usize>) = ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare("SELECT id, url FROM posts WHERE site_id=?1 ORDER BY id")
                .map_err(err)?;
            let nodes = st
                .query_map([&sid], |r| Ok(GraphNode { id: r.get(0)?, url: r.get(1)? }))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            let mut st = c
                .prepare(
                    "SELECT DISTINCT source_post_id, target_post_id FROM link_edges
                     WHERE site_id=?1 AND (status='applied' OR origin='existing') AND source_post_id <> target_post_id
                     ORDER BY source_post_id, target_post_id",
                )
                .map_err(err)?;
            let edges = st
                .query_map([&sid], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            let mut st = c
                .prepare(
                    "SELECT target_post_id, count(DISTINCT source_post_id) FROM link_health
                     WHERE site_id=?1 AND target_post_id IS NOT NULL AND placement IN ('nav','header','footer','aside')
                     GROUP BY target_post_id",
                )
                .map_err(err)?;
            let nav = st
                .query_map([&sid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(err)?
                .filter_map(|r| r.ok())
                .map(|(t, n)| (t, usize::try_from(n).unwrap_or(0)))
                .collect();
            Ok((nodes, edges, nav))
        })
        .await?;
    job.checkpoint().await?;
    let configured_roots = configured_roots(site, &nodes);
    let input = GraphInput {
        nodes,
        edges: edges.clone(),
        configured_roots,
        nav_inbound: nav,
        params: Default::default(),
    };
    let out: GraphOutput = compute(move || sf_engine::graph::compute(&input))
        .await?
        .map_err(JobError::msg)?;
    if persist {
        for chunk in out.nodes.chunks(1000) {
            job.checkpoint().await?;
            let rows: Vec<NodeMetric> = chunk.to_vec();
            ctx.db
                .write(move |t| {
                    let mut st = t
                        .prepare(
                            "UPDATE posts SET pagerank=?2, click_depth=?3, is_orphan=?4, is_dead_end=?5,
                               is_contextual_orphan=?6, is_nav_only=?7, body_inbound_count=?8, nav_inbound_count=?9
                             WHERE id=?1 AND (pagerank IS NOT ?2 OR click_depth IS NOT ?3 OR is_orphan IS NOT ?4
                               OR is_dead_end IS NOT ?5 OR is_contextual_orphan IS NOT ?6 OR is_nav_only IS NOT ?7
                               OR body_inbound_count IS NOT ?8 OR nav_inbound_count IS NOT ?9)",
                        )
                        .map_err(err)?;
                    for m in &rows {
                        let pr = if m.pagerank.is_finite() { m.pagerank } else { 0.0 };
                        st.execute(params![
                            m.id,
                            pr,
                            m.click_depth,
                            m.is_orphan,
                            m.is_dead_end,
                            m.is_contextual_orphan,
                            m.is_nav_only,
                            m.body_inbound_count as i64,
                            m.nav_inbound_count as i64
                        ])
                        .map_err(err)?;
                    }
                    Ok(())
                })
                .await?;
        }
    }
    let summary = out.summary.clone();
    let metrics = out.nodes.into_iter().map(|m| (m.id.clone(), m)).collect();
    Ok(GraphData {
        metrics,
        edges,
        summary,
    })
}

/// `settings.root_urls` (array of URLs) mapped to post ids.
fn configured_roots(site: &SiteInfo, nodes: &[GraphNode]) -> Vec<String> {
    let Some(urls) = site.settings.get("root_urls").and_then(Value::as_array) else {
        return Vec::new();
    };
    let want: Vec<String> = urls
        .iter()
        .filter_map(Value::as_str)
        .map(sf_engine::html::normalize_url)
        .collect();
    nodes
        .iter()
        .filter(|n| want.contains(&sf_engine::html::normalize_url(&n.url)))
        .map(|n| n.id.clone())
        .collect()
}
