//! `/edges/*` and `/bulk/*`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rusqlite::types::ToSql;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sf_core::types::{AnchorType, EdgeStatus, JobKind};
use sf_store::{err, placeholders};

use super::common::{
    EDGE_JSON, JsonBody, MAX_LIMIT, Params, audit_user, conflict_as, count, enqueue_raw,
    guard_child, guard_user_site, int_param, is_uuid, owner_scope, query_json, query_one_json,
    str_param,
};
use crate::auth::User;
use crate::{ApiError, ApiResult, AppState};

const MAX_BULK_IDS: usize = 20_000;

/// `limit` is clamped to 1..=5000 instead of rejected (v1 answered 400 above 1000, which broke the
/// Reports "Send to Bulk" button that asks for 2000).
pub async fn list(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
    q: Params,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &site_id).await?;
    let status = str_param(&q, "status").map(str::to_string);
    if let Some(s) = &status
        && EdgeStatus::parse_known(s).is_none()
    {
        return Err(ApiError::bad("invalid status filter"));
    }
    let tool = str_param(&q, "tool").map(str::to_string);
    let limit = int_param(&q, "limit", 100, 1, MAX_LIMIT);
    let offset = int_param(&q, "offset", 0, 0, i64::MAX / 2);
    let v = st
        .db
        .read(move |c| {
            let wh = "e.site_id=?1 AND (?2 IS NULL OR e.status=?2) AND (?3 IS NULL OR e.origin=?3)";
            let p: [&dyn ToSql; 3] = [&site_id, &status, &tool];
            let edges = query_json(
                c,
                &format!(
                    "SELECT e.*, sp.url AS source_url, sp.title AS source_title,
                            tp.url AS target_url, tp.title AS target_title
                       FROM link_edges e
                       JOIN posts sp ON sp.id = e.source_post_id
                       JOIN posts tp ON tp.id = e.target_post_id
                      WHERE {wh}
                      ORDER BY e.composite_score DESC, e.created_at ASC, e.id ASC
                      LIMIT ?4 OFFSET ?5"
                ),
                &[p[0], p[1], p[2], &limit, &offset],
                EDGE_JSON,
                &[],
            )?;
            let total = count(
                c,
                &format!("SELECT count(*) FROM link_edges e WHERE {wh}"),
                &p,
            )?;
            Ok(json!({ "edges": edges, "total": total }))
        })
        .await?;
    Ok(Json(v))
}

async fn edge_site(st: &AppState, edge_id: &str) -> ApiResult<(String, String)> {
    if !is_uuid(edge_id) {
        return Err(ApiError::not_found("edge not found"));
    }
    let id = edge_id.to_string();
    st.db
        .read(move |c| {
            c.query_row(
                "SELECT site_id, status FROM link_edges WHERE id=?1",
                [&id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(err)
        })
        .await?
        .ok_or_else(|| ApiError::not_found("edge not found"))
}

fn check_decision(d: &str) -> ApiResult<()> {
    if d == "approved" || d == "rejected" {
        Ok(())
    } else {
        Err(ApiError::bad("decision must be 'approved' or 'rejected'"))
    }
}

#[derive(Deserialize)]
pub struct DecideBody {
    decision: String,
}

pub async fn decide(
    State(st): State<AppState>,
    u: User,
    Path(edge_id): Path<String>,
    JsonBody(b): JsonBody<DecideBody>,
) -> ApiResult<Json<Value>> {
    check_decision(&b.decision)?;
    let (site_id, _) = edge_site(&st, &edge_id).await?;
    guard_child(&st, &u, &site_id, "edge not found").await?;
    let (eid, decision) = (edge_id.clone(), b.decision.clone());
    let res: Result<Value, ApiError> = st
        .db
        .write(move |t| {
            let n = t
                .execute(
                    "UPDATE link_edges SET status=?2, decided_at=?3
                      WHERE id=?1 AND status IN ('suggested','reverted')",
                    params![eid, decision, sf_core::now_rfc3339()],
                )
                .map_err(err)?;
            if n == 0 {
                let status: Option<String> = t
                    .query_row("SELECT status FROM link_edges WHERE id=?1", [&eid], |r| {
                        r.get(0)
                    })
                    .optional()
                    .map_err(err)?;
                return Ok(Err(match status {
                    None => ApiError::not_found("edge not found"),
                    Some(s) => ApiError::bad(format!(
                        "edge is '{s}', only 'suggested' or 'reverted' edges can be decided"
                    )),
                }));
            }
            Ok(query_one_json(
                t,
                "SELECT * FROM link_edges WHERE id=?1",
                &[&eid],
                EDGE_JSON,
                &[],
            )?
            .ok_or_else(|| ApiError::not_found("edge not found")))
        })
        .await?;
    let row = res?;
    audit_user(
        &st,
        &u,
        "edge.decide",
        Some(&edge_id),
        json!({ "decision": b.decision }),
    )
    .await;
    Ok(Json(row))
}

#[derive(Deserialize)]
pub struct AnchorBody {
    anchor: String,
    #[serde(default)]
    anchor_type: Option<String>,
}

pub async fn anchor(
    State(st): State<AppState>,
    u: User,
    Path(edge_id): Path<String>,
    JsonBody(b): JsonBody<AnchorBody>,
) -> ApiResult<Json<Value>> {
    let anchor: String = b.anchor.trim().chars().take(255).collect();
    if anchor.is_empty() {
        return Err(ApiError::bad("anchor cannot be empty"));
    }
    if let Some(t) = &b.anchor_type
        && AnchorType::parse_known(t).is_none()
    {
        return Err(ApiError::bad("invalid anchor_type"));
    }
    let (site_id, status) = edge_site(&st, &edge_id).await?;
    guard_child(&st, &u, &site_id, "edge not found").await?;
    if status == "applied" {
        return Err(ApiError::bad(
            "cannot edit anchor of an applied edge; undo it first",
        ));
    }
    let (eid, a2, at) = (edge_id.clone(), anchor.clone(), b.anchor_type.clone());
    let row = st
        .db
        .write(move |t| {
            t.execute(
                "UPDATE link_edges SET anchor=?2, anchor_type=coalesce(?3, anchor_type) WHERE id=?1",
                params![eid, a2, at],
            )
            .map_err(err)?;
            query_one_json(
                t,
                "SELECT * FROM link_edges WHERE id=?1",
                &[&eid],
                EDGE_JSON,
                &[],
            )
        })
        .await
        .map_err(|e| conflict_as(e, "another edge already uses this anchor for the same link"))?
        .ok_or_else(|| ApiError::not_found("edge not found"))?;
    audit_user(
        &st,
        &u,
        "edge.anchor",
        Some(&edge_id),
        json!({ "anchor": anchor, "anchor_type": b.anchor_type }),
    )
    .await;
    Ok(Json(row))
}

#[derive(Deserialize)]
pub struct BulkDecideBody {
    edge_ids: Vec<String>,
    decision: String,
}

/// Non-uuid ids are dropped silently (v1); non-admins only touch edges of sites they own.
pub async fn bulk_decide(
    State(st): State<AppState>,
    u: User,
    JsonBody(b): JsonBody<BulkDecideBody>,
) -> ApiResult<Json<Value>> {
    if b.edge_ids.is_empty() {
        return Err(ApiError::bad(
            "body/edge_ids must NOT have fewer than 1 items",
        ));
    }
    if b.edge_ids.len() > MAX_BULK_IDS {
        return Err(ApiError::bad(format!(
            "too many edge_ids (max {MAX_BULK_IDS})"
        )));
    }
    check_decision(&b.decision)?;
    let ids: Vec<String> = b.edge_ids.into_iter().filter(|i| is_uuid(i)).collect();
    if ids.is_empty() {
        return Ok(Json(json!({ "updated": 0 })));
    }
    let scope = owner_scope(&u);
    let decision = b.decision.clone();
    let updated = st
        .db
        .write(move |t| {
            let now = sf_core::now_rfc3339();
            let mut total = 0usize;
            for chunk in ids.chunks(500) {
                let sql = format!(
                    "UPDATE link_edges SET status=?1, decided_at=?2
                      WHERE id IN ({}) AND status IN ('suggested','reverted')
                        AND (?3 IS NULL OR site_id IN (SELECT id FROM sites WHERE user_id=?3))",
                    placeholders_from(4, chunk.len())
                );
                let mut p: Vec<&dyn ToSql> = vec![&decision, &now, &scope];
                for id in chunk {
                    p.push(id);
                }
                total += t.execute(&sql, p.as_slice()).map_err(err)?;
            }
            Ok(total)
        })
        .await?;
    audit_user(
        &st,
        &u,
        "edge.bulk_decide",
        None,
        json!({ "decision": b.decision, "updated": updated }),
    )
    .await;
    Ok(Json(json!({ "updated": updated })))
}

/// `?N,?N+1,...` numbered placeholders starting at `start`.
fn placeholders_from(start: usize, n: usize) -> String {
    (start..start + n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(Deserialize)]
pub struct BulkBody {
    site_id: String,
    #[serde(default)]
    edge_ids: Option<Vec<String>>,
}

/// Extra filter for undo: only edges SiloForge itself injected. `origin='existing'` rows are links
/// that were already in the post (crawled with status applied); undo cannot remove them, and
/// counting them made `/bulk/undo` report e.g. 157 edges while 11 were reverted.
const UNDOABLE: &str = "AND origin <> 'existing'
     AND (applied_at IS NOT NULL OR snapshot_id IS NOT NULL OR link_start IS NOT NULL)";

async fn collect_edges(
    st: &AppState,
    site_id: &str,
    status: &'static str,
    edge_ids: Option<Vec<String>>,
    extra: &'static str,
) -> ApiResult<Vec<String>> {
    let filter: Option<Vec<String>> =
        edge_ids.map(|v| v.into_iter().filter(|i| is_uuid(i)).collect());
    if let Some(f) = &filter
        && f.len() > MAX_BULK_IDS
    {
        return Err(ApiError::bad(format!(
            "too many edge_ids (max {MAX_BULK_IDS})"
        )));
    }
    let sid = site_id.to_string();
    Ok(st
        .db
        .read(move |c| {
            let mut out: Vec<(f64, String)> = Vec::new();
            match filter {
                None => {
                    let mut stmt = c
                        .prepare(&format!(
                            "SELECT composite_score, id FROM link_edges WHERE site_id=?1 AND status=?2 {extra}"
                        ))
                        .map_err(err)?;
                    let rows = stmt
                        .query_map(params![sid, status], |r| Ok((r.get(0)?, r.get(1)?)))
                        .map_err(err)?;
                    for r in rows {
                        out.push(r.map_err(err)?);
                    }
                }
                Some(ids) => {
                    for chunk in ids.chunks(500) {
                        let sql = format!(
                            "SELECT composite_score, id FROM link_edges
                              WHERE site_id=? AND status=? AND id IN ({}) {extra}",
                            placeholders(chunk.len())
                        );
                        let mut p: Vec<&dyn ToSql> = vec![&sid, &status];
                        for id in chunk {
                            p.push(id);
                        }
                        let mut stmt = c.prepare(&sql).map_err(err)?;
                        let rows = stmt
                            .query_map(p.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))
                            .map_err(err)?;
                        for r in rows {
                            out.push(r.map_err(err)?);
                        }
                    }
                }
            }
            out.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            out.dedup_by(|a, b| a.1 == b.1);
            Ok(out.into_iter().map(|(_, id)| id).collect())
        })
        .await?)
}

pub async fn bulk_run(
    State(st): State<AppState>,
    u: User,
    JsonBody(b): JsonBody<BulkBody>,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &b.site_id).await?;
    let ids = collect_edges(&st, &b.site_id, "approved", b.edge_ids, "").await?;
    if ids.is_empty() {
        return Err(ApiError::bad("no approved edges to inject"));
    }
    let n = ids.len();
    let out = enqueue_raw(
        &st,
        &u.actor(),
        JobKind::Inject,
        &b.site_id,
        json!({ "edge_ids": ids }),
    )
    .await?;
    audit_user(
        &st,
        &u,
        "job.enqueue.inject",
        Some(&b.site_id),
        json!({ "job_run_id": out.job_run_id, "count": n, "deduplicated": out.deduplicated }),
    )
    .await;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "job_run_id": out.job_run_id, "count": n, "deduplicated": out.deduplicated })),
    )
        .into_response())
}

pub async fn bulk_undo(
    State(st): State<AppState>,
    u: User,
    JsonBody(b): JsonBody<BulkBody>,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &b.site_id).await?;
    let ids = collect_edges(&st, &b.site_id, "applied", b.edge_ids, UNDOABLE).await?;
    if ids.is_empty() {
        return Ok(Json(json!({ "job_run_id": Value::Null, "count": 0 })).into_response());
    }
    let n = ids.len();
    let out = enqueue_raw(
        &st,
        &u.actor(),
        JobKind::Undo,
        &b.site_id,
        json!({ "edge_ids": ids }),
    )
    .await?;
    audit_user(
        &st,
        &u,
        "job.enqueue.undo",
        Some(&b.site_id),
        json!({ "job_run_id": out.job_run_id, "count": n, "deduplicated": out.deduplicated }),
    )
    .await;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "job_run_id": out.job_run_id, "count": n, "deduplicated": out.deduplicated })),
    )
        .into_response())
}
