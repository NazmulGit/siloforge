//! Global job views (all sites for admins, own sites for users), logs and cancel.

use axum::Json;
use axum::extract::{Path, Query, State};
use rusqlite::OptionalExtension;
use rusqlite::types::Value as SqlValue;
use serde::Deserialize;
use serde_json::{Value, json};
use sf_jobs::queue::{API_STATUS_SQL, JOB_JSON_COLS, job_cols};
use sf_store::{err, row_to_json};

use super::clamp_limit;
use crate::auth::{Principal, User, audit, ensure_site_access};
use crate::{ApiError, ApiResult, AppState};

#[derive(Deserialize)]
pub struct ListQ {
    status: Option<String>,
    kind: Option<String>,
    site_id: Option<String>,
    limit: Option<i64>,
    /// A2: paging (MCP siloforge_list_jobs sends it).
    offset: Option<i64>,
}

fn prefixed_cols() -> String {
    job_cols("j")
}

/// `GET /jobs`: newest first, with `site_domain`.
pub async fn list(
    State(st): State<AppState>,
    u: User,
    Query(q): Query<ListQ>,
) -> ApiResult<Json<Value>> {
    let limit = clamp_limit(q.limit, 100, 500);
    let mut sql = format!(
        "SELECT {}, s.domain AS site_domain FROM job_runs j LEFT JOIN sites s ON s.id = j.site_id WHERE 1=1",
        prefixed_cols()
    );
    let mut args: Vec<SqlValue> = Vec::new();
    // R3: the status filter matches the API-visible status (a stopping job counts as cancelled)
    let status_col = "(CASE WHEN j.status='running' AND j.cancel_requested_at IS NOT NULL THEN 'cancelled' ELSE j.status END)";
    for (col, v) in [
        (status_col, &q.status),
        ("j.kind", &q.kind),
        ("j.site_id", &q.site_id),
    ] {
        if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            if v.len() > 100 {
                return Err(ApiError::bad("filter value too long"));
            }
            args.push(SqlValue::Text(v.to_string()));
            sql.push_str(&format!(" AND {col} = ?{}", args.len()));
        }
    }
    // v2.4: workspace scope (admins without a workspace header also see site-less jobs).
    let scope = crate::tenancy::site_scope(&st, &u, "s").await?;
    if scope != "1=1" {
        sql.push_str(&format!(" AND s.id IS NOT NULL AND {scope}"));
    }
    args.push(SqlValue::Integer(limit));
    let lim = args.len();
    args.push(SqlValue::Integer(q.offset.unwrap_or(0).clamp(0, 1_000_000)));
    sql.push_str(&format!(
        " ORDER BY j.created_at DESC, j.id LIMIT ?{lim} OFFSET ?{}",
        args.len()
    ));
    let rows = st
        .db
        .read(move |c| {
            let mut stmt = c.prepare(&sql).map_err(err)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                    Ok(row_to_json(r, JOB_JSON_COLS, &[]))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?;
    Ok(Json(json!({ "jobs": rows, "count": rows.len() })))
}

fn principal(u: &User) -> Principal {
    u.principal()
}

/// 404 unless the caller may see this job (site-less jobs are admin-only).
async fn ensure_job_access(st: &AppState, u: &User, id: &str) -> ApiResult<()> {
    let id2 = id.to_string();
    let site: Option<Option<String>> = st
        .db
        .read(move |c| {
            c.query_row("SELECT site_id FROM job_runs WHERE id=?1", [&id2], |r| {
                r.get(0)
            })
            .optional()
            .map_err(err)
        })
        .await?;
    match site {
        None => Err(ApiError::not_found("job not found")),
        Some(_) if u.is_admin() => Ok(()),
        Some(None) => Err(ApiError::not_found("job not found")),
        Some(Some(s)) => ensure_site_access(st, &principal(u), &s)
            .await
            .map_err(|_| ApiError::not_found("job not found")),
    }
}

/// `GET /jobs/{id}`.
pub async fn get_one(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    ensure_job_access(&st, &u, &id).await?;
    Ok(Json(st.jobs.get(&id).await?))
}

#[derive(Deserialize)]
pub struct LogsQ {
    after_id: Option<i64>,
    after: Option<i64>,
    limit: Option<i64>,
}

/// `GET /jobs/{id}/logs?after_id=`: `{logs:[{id, seq, ts, level, message, msg, data}], done, status, last_id}`.
pub async fn logs(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
    Query(q): Query<LogsQ>,
) -> ApiResult<Json<Value>> {
    ensure_job_access(&st, &u, &id).await?;
    let after = q.after_id.or(q.after).unwrap_or(0).max(0);
    let limit = clamp_limit(q.limit, 500, 2000);
    let (lines, status) = st
        .db
        .read(move |c| {
            let mut stmt = c
                .prepare(
                    "SELECT id, ts, level, message, data FROM job_logs WHERE job_run_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
                )
                .map_err(err)?;
            let lines = stmt
                .query_map(rusqlite::params![id, after, limit], |r| {
                    let mut v = row_to_json(r, &["data"], &[]);
                    if let Some(o) = v.as_object_mut() {
                        if let Some(i) = o.get("id").cloned() {
                            o.insert("seq".into(), i);
                        }
                        if let Some(m) = o.get("message").cloned() {
                            o.insert("msg".into(), m);
                        }
                    }
                    Ok(v)
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            let status: String = c
                .query_row(&format!("SELECT {API_STATUS_SQL} FROM job_runs WHERE id=?1"), [&id], |r| r.get(0))
                .map_err(err)?;
            Ok((lines, status))
        })
        .await?;
    let last_id = lines
        .last()
        .and_then(|l| l.get("id"))
        .and_then(Value::as_i64)
        .unwrap_or(after);
    let done = matches!(status.as_str(), "done" | "failed" | "cancelled");
    Ok(Json(
        json!({ "logs": lines, "done": done, "status": status, "last_id": last_id }),
    ))
}

/// `POST /jobs/{id}/cancel`.
pub async fn cancel(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    ensure_job_access(&st, &u, &id).await?;
    let status = st.jobs.cancel(&id).await?;
    audit(
        &st,
        &u.actor(),
        u.via(),
        "job.cancel",
        Some(&id),
        json!({ "status": status.as_str() }),
    )
    .await;
    Ok(Json(json!({ "id": id, "status": status.as_str() })))
}
