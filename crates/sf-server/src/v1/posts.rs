//! `/usage` and `/posts/:siteId`.

use axum::Json;
use axum::extract::{Path, State};
use rusqlite::types::ToSql;
use serde_json::{Value, json};
use sf_store::err;

use super::common::{
    MAX_LIMIT, Params, count, guard_user_site, int_param, is_uuid, owner_scope, query_json,
    str_param, v1_collation,
};
use crate::auth::User;
use crate::{ApiError, ApiResult, AppState};

/// Footprint counters, scoped like `GET /sites` (admin = global). Joins instead of nested IN lists.
pub async fn usage(State(st): State<AppState>, u: User) -> ApiResult<Json<Value>> {
    let scope = owner_scope(&u);
    let v = st
        .db
        .read(move |c| {
            let s = "(?1 IS NULL OR s.user_id = ?1)";
            let p: &[&dyn ToSql] = &[&scope];
            let sites = count(c, &format!("SELECT count(*) FROM sites s WHERE {s}"), p)?;
            let posts = count(
                c,
                &format!("SELECT count(*) FROM posts p JOIN sites s ON s.id=p.site_id WHERE {s}"),
                p,
            )?;
            let paragraphs = count(
                c,
                &format!(
                    "SELECT count(*) FROM paragraphs g JOIN posts p ON p.id=g.post_id
                     JOIN sites s ON s.id=p.site_id WHERE {s}"
                ),
                p,
            )?;
            let mut stmt = c
                .prepare(&format!(
                    "SELECT sum(e.status='suggested'), sum(e.status='approved'), sum(e.status='applied')
                     FROM link_edges e JOIN sites s ON s.id=e.site_id WHERE {s}"
                ))
                .map_err(err)?;
            let (suggested, approved, applied) = stmt
                .query_row(p, |r| {
                    Ok((
                        r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                        r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    ))
                })
                .map_err(err)?;
            let clusters = count(
                c,
                &format!("SELECT count(*) FROM clusters k JOIN sites s ON s.id=k.site_id WHERE {s}"),
                p,
            )?;
            Ok(json!({
                "sites": sites, "posts": posts, "paragraphs": paragraphs,
                "suggested": suggested, "approved": approved, "applied": applied,
                "applied_links": applied, "clusters": clusters,
            }))
        })
        .await?;
    Ok(Json(v))
}

fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('%');
    for ch in s.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

/// Bulk-wizard post list. Suggested in/out counts are index-only range counts per post
/// (`link_edges_site_pair_idx` / `link_edges_site_target_idx`): cheaper than v1's per-row scans
/// (P4) and than grouping every suggested edge of the site when a filter keeps few posts.
pub async fn list(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
    q: Params,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &site_id).await?;
    let limit = int_param(&q, "limit", 100, 1, MAX_LIMIT);
    let offset = int_param(&q, "offset", 0, 0, i64::MAX / 2);
    let search =
        str_param(&q, "search").map(|s| like_escape(&s.chars().take(200).collect::<String>()));
    let post_type = str_param(&q, "post_type").map(str::to_string);
    let cluster_id = str_param(&q, "cluster_id").map(str::to_string);
    if let Some(c) = &cluster_id
        && !is_uuid(c)
    {
        return Err(ApiError::bad("invalid cluster_id filter"));
    }
    let is_orphan: Option<i64> = match str_param(&q, "is_orphan") {
        None => None,
        Some("1" | "true") => Some(1),
        Some("0" | "false") => Some(0),
        Some(_) => return Err(ApiError::bad("is_orphan must be a boolean")),
    };
    let v = st
        .db
        .read(move |c| {
            v1_collation(c)?;
            let wh = "p.site_id = ?1
                AND (?2 IS NULL OR p.title LIKE ?2 ESCAPE '\\' OR p.url LIKE ?2 ESCAPE '\\')
                AND (?3 IS NULL OR p.post_type = ?3)
                AND (?4 IS NULL OR p.cluster_id = ?4)
                AND (?5 IS NULL OR p.is_orphan = ?5)";
            let fp: [&dyn ToSql; 5] = [&site_id, &search, &post_type, &cluster_id, &is_orphan];
            let posts = query_json(
                c,
                &format!(
                    "SELECT p.id, p.url, p.title, p.post_type, p.cluster_id, k.label AS cluster_label,
                            p.is_orphan, p.is_dead_end,
                            (SELECT count(*) FROM link_edges
                              WHERE site_id=?1 AND source_post_id=p.id AND status='suggested') AS suggested_out,
                            (SELECT count(*) FROM link_edges
                              WHERE site_id=?1 AND target_post_id=p.id AND status='suggested') AS suggested_in
                       FROM posts p
                       LEFT JOIN clusters k ON k.id = p.cluster_id
                      WHERE {wh}
                      ORDER BY suggested_out DESC, p.title COLLATE V1 ASC, p.id ASC
                      LIMIT ?6 OFFSET ?7"
                ),
                &[fp[0], fp[1], fp[2], fp[3], fp[4], &limit, &offset],
                &[],
                &["is_orphan", "is_dead_end"],
            )?;
            let total = count(c, &format!("SELECT count(*) FROM posts p WHERE {wh}"), &fp)?;
            let mut stmt = c
                .prepare("SELECT DISTINCT post_type FROM posts WHERE site_id=?1 ORDER BY post_type")
                .map_err(err)?;
            let types = stmt
                .query_map([&site_id], |r| r.get::<_, String>(0))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            let clusters = query_json(
                c,
                "SELECT k.id, k.label, count(p.id) AS count FROM clusters k
                   LEFT JOIN posts p ON p.cluster_id = k.id
                  WHERE k.site_id=?1 GROUP BY k.id ORDER BY k.label COLLATE V1 ASC, k.id ASC",
                &[&site_id],
                &[],
                &[],
            )?;
            Ok(json!({ "posts": posts, "total": total, "post_types": types, "clusters": clusters }))
        })
        .await?;
    Ok(Json(v))
}
