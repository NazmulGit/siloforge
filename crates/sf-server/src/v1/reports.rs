//! `/reports/links`, `/reports/graph`, `/reports/anchors`, `/history` (JSON + CSV).

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use rusqlite::params;
use serde_json::{Value, json};
use sf_store::err;

use super::common::{
    Params, count, csv_line, csv_response, get_site, guard_user_site, int_param, num, query_json,
    safe_filename_part, str_param, v1_collation,
};
use crate::auth::User;
use crate::{ApiResult, AppState};

/// Max rows in one CSV export (bounded memory; the largest v1 site has ~15k history rows).
pub const CSV_MAX_ROWS: i64 = 250_000;

/// Per-post link report. Paged with `?limit` (default 5000, max 20000) and `?offset`; v1 was unbounded.
pub async fn links(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
    q: Params,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &site_id).await?;
    let limit = int_param(&q, "limit", 5000, 1, 20_000);
    let offset = int_param(&q, "offset", 0, 0, i64::MAX / 2);
    let rows = st
        .db
        .read(move |c| {
            v1_collation(c)?;
            let max_pr: f64 = c
                .query_row(
                    "SELECT coalesce(max(pagerank), 0) FROM posts WHERE site_id=?1",
                    [&site_id],
                    |r| r.get(0),
                )
                .map_err(err)?;
            let mut stmt = c
                .prepare(
                    "WITH live AS MATERIALIZED (SELECT source_post_id, target_post_id FROM link_edges
                                    WHERE site_id=?1 AND (status IN ('applied','approved') OR origin='existing')),
                          inb AS (SELECT target_post_id AS pid, count(*) AS n FROM live GROUP BY target_post_id),
                          outb AS (SELECT source_post_id AS pid, count(*) AS n FROM live GROUP BY source_post_id),
                          dup AS (SELECT source_post_id, target_post_id, count(*) - 1 AS extras FROM link_edges
                                   WHERE site_id=?1 AND status IN ('applied','approved','suggested')
                                   GROUP BY source_post_id, target_post_id HAVING count(*) > 1),
                          dupc AS (SELECT pid, sum(extras) AS n FROM (
                                     SELECT source_post_id AS pid, extras FROM dup
                                     UNION ALL SELECT target_post_id AS pid, extras FROM dup) GROUP BY pid)
                     SELECT p.id, p.url, p.title, p.pagerank, p.click_depth,
                            coalesce(inb.n, 0), coalesce(outb.n, 0), coalesce(dupc.n, 0)
                       FROM posts p
                       LEFT JOIN inb ON inb.pid = p.id
                       LEFT JOIN outb ON outb.pid = p.id
                       LEFT JOIN dupc ON dupc.pid = p.id
                      WHERE p.site_id=?1
                      ORDER BY p.pagerank DESC, p.url COLLATE V1 ASC
                      LIMIT ?2 OFFSET ?3",
                )
                .map_err(err)?;
            let rows = stmt
                .query_map(params![site_id, limit, offset], |r| {
                    let pr: f64 = r.get(3)?;
                    let inbound: i64 = r.get(5)?;
                    let outbound: i64 = r.get(6)?;
                    let pr_norm = if max_pr > 0.0 { pr / max_pr } else { 0.0 };
                    let flow = (100.0
                        * (0.5 * pr_norm
                            + 0.3 * (inbound as f64 / 5.0).min(1.0)
                            + 0.2 * (outbound as f64 / 5.0).min(1.0)))
                        .round();
                    Ok(json!({
                        "post_id": r.get::<_, String>(0)?,
                        "url": r.get::<_, String>(1)?,
                        "title": r.get::<_, String>(2)?,
                        "inbound": inbound,
                        "outbound": outbound,
                        "pagerank": num(pr),
                        "click_depth": r.get::<_, i64>(4)?,
                        "link_flow_score": num(flow),
                        "duplicate_count": r.get::<_, i64>(7)?,
                    }))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?;
    Ok(Json(Value::Array(rows)))
}

pub async fn graph(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
    q: Params,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &site_id).await?;
    let node_limit = int_param(&q, "max_nodes", 500, 1, 750);
    let edge_limit = int_param(&q, "max_edges", 1500, 0, 2500);
    // Nodes and edges are independent reads: run them on two pooled connections.
    let nodes = {
        let sid = site_id.clone();
        st.db.read(move |c| {
            let nodes = query_json(
                c,
                "SELECT id, url, title, cluster_id, is_orphan, pagerank FROM posts
                  WHERE site_id=?1 ORDER BY pagerank DESC, id ASC LIMIT ?2",
                &[&sid, &node_limit],
                &[],
                &["is_orphan"],
            )?;
            let total = count(c, "SELECT count(*) FROM posts WHERE site_id=?1", &[&sid])?;
            Ok((nodes, total))
        })
    };
    let edges = st.db.read(move |c| {
        let edges = query_json(
            c,
            "WITH sel AS (SELECT id FROM posts WHERE site_id=?1 ORDER BY pagerank DESC, id ASC LIMIT ?2)
             SELECT e.source_post_id AS source, e.target_post_id AS target, e.status
               FROM link_edges e
               JOIN sel s1 ON s1.id = e.source_post_id
               JOIN sel s2 ON s2.id = e.target_post_id
              WHERE e.site_id=?1 AND (e.status IN ('applied','approved','suggested') OR e.origin='existing')
              ORDER BY CASE e.status WHEN 'applied' THEN 0 WHEN 'approved' THEN 1 WHEN 'suggested' THEN 2 ELSE 3 END,
                       e.composite_score DESC, e.id ASC
              LIMIT ?3",
            &[&site_id, &node_limit, &edge_limit],
            &[],
            &[],
        )?;
        let total = count(
            c,
            "SELECT count(*) FROM link_edges WHERE site_id=?1
               AND (status IN ('applied','approved','suggested') OR origin='existing')",
            &[&site_id],
        )?;
        Ok((edges, total))
    });
    let ((nodes, total_nodes), (edges, total_edges)) = tokio::try_join!(nodes, edges)?;
    let truncated = (nodes.len() as i64) < total_nodes || (edges.len() as i64) < total_edges;
    let v = json!({
        "nodes": nodes, "edges": edges,
        "total_nodes": total_nodes, "total_edges": total_edges,
        "node_limit": node_limit, "edge_limit": edge_limit,
        "truncated": truncated,
    });
    Ok(Json(v))
}

pub async fn anchors(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &site_id).await?;
    let v = st
        .db
        .read(move |c| {
            let mut by_type: BTreeMap<String, i64> = BTreeMap::new();
            let mut stmt = c
                .prepare(
                    "SELECT anchor_type, count(*) FROM link_edges
                      WHERE site_id=?1 AND anchor<>'' AND status IN ('suggested','approved','applied')
                      GROUP BY anchor_type",
                )
                .map_err(err)?;
            let rows = stmt
                .query_map([&site_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(err)?;
            for row in rows {
                let (t, n) = row.map_err(err)?;
                by_type.insert(t, n);
            }
            let by_anchor = query_json(
                c,
                "SELECT anchor, count(*) AS count FROM link_edges
                  WHERE site_id=?1 AND anchor<>'' AND status IN ('suggested','approved','applied')
                  GROUP BY anchor ORDER BY count(*) DESC, anchor ASC LIMIT 50",
                &[&site_id],
                &[],
                &[],
            )?;
            let total: i64 = by_type.values().sum();
            let mut warnings: Vec<String> = Vec::new();
            if total > 0 {
                let share = |k: &str| by_type.get(k).copied().unwrap_or(0) as f64 / total as f64;
                let exact = share("exact");
                let generic = share("generic");
                if exact > 0.4 {
                    warnings.push(format!(
                        "Exact-match anchors are {}% (over 40% — risk of over-optimization)",
                        (exact * 100.0).round()
                    ));
                }
                if generic > 0.3 {
                    warnings.push(format!(
                        "Generic anchors are {}% (over 30%)",
                        (generic * 100.0).round()
                    ));
                }
            }
            Ok(json!({ "total": total, "byType": by_type, "byAnchor": by_anchor, "warnings": warnings }))
        })
        .await?;
    Ok(Json(v))
}

const HISTORY_SELECT: &str =
    "SELECT e.id AS edge_id, sp.url AS source_url, sp.title AS source_title,
        tp.url AS target_url, tp.title AS target_title, e.anchor, e.anchor_type, e.origin AS tool,
        e.status, e.applied_at, e.decided_at
   FROM link_edges e
   JOIN posts sp ON sp.id = e.source_post_id
   JOIN posts tp ON tp.id = e.target_post_id
  WHERE e.site_id=?1 AND e.status IN ('applied','reverted')
  ORDER BY e.applied_at DESC NULLS LAST, e.decided_at DESC NULLS LAST, e.id ASC";

/// JSON `{rows,total,limit,offset}` or `?format=csv` (ISO timestamps, fixes v1 B7).
pub async fn history(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
    q: Params,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &site_id).await?;
    if str_param(&q, "format") == Some("csv") {
        let site = get_site(&st, &site_id).await?;
        let domain = site.domain.clone();
        let body = st
            .db
            .read(move |c| {
                let mut stmt = c
                    .prepare(&format!("{HISTORY_SELECT} LIMIT ?2"))
                    .map_err(err)?;
                let mut lines =
                    vec!["Domain,Source,Target,Tool,Anchor,Status,Applied At".to_string()];
                let rows = stmt
                    .query_map(params![site_id, CSV_MAX_ROWS], |r| {
                        Ok(csv_line(&[
                            &domain,
                            &r.get::<_, String>("source_url")?,
                            &r.get::<_, String>("target_url")?,
                            &r.get::<_, String>("tool")?,
                            &r.get::<_, String>("anchor")?,
                            &r.get::<_, String>("status")?,
                            &r.get::<_, Option<String>>("applied_at")?
                                .unwrap_or_default(),
                        ]))
                    })
                    .map_err(err)?;
                for line in rows {
                    lines.push(line.map_err(err)?);
                }
                Ok(lines.join("\r\n"))
            })
            .await?;
        return Ok(csv_response(
            &format!("link-history-{}.csv", safe_filename_part(&site.domain)),
            body,
        ));
    }
    let limit = int_param(&q, "limit", 50, 1, 100);
    let offset = int_param(&q, "offset", 0, 0, i64::MAX / 2);
    let v = st
        .db
        .read(move |c| {
            let rows = query_json(
                c,
                &format!("{HISTORY_SELECT} LIMIT ?2 OFFSET ?3"),
                &[&site_id, &limit, &offset],
                &[],
                &[],
            )?;
            let total = count(
                c,
                "SELECT count(*) FROM link_edges WHERE site_id=?1 AND status IN ('applied','reverted')",
                &[&site_id],
            )?;
            Ok(json!({ "rows": rows, "total": total, "limit": limit, "offset": offset }))
        })
        .await?;
    Ok(Json(v).into_response())
}
