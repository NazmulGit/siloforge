//! `GET /jobs/:siteId` (v1) and `GET /jobs/run/:jobId` (v2 addition for the UI job tracker).

use axum::Json;
use axum::extract::{Path, State};
use serde_json::Value;

use super::common::{
    JOB_API_COLS, JOB_API_JSON, Params, guard_user_site, int_param, is_uuid, principal, query_json,
    query_one_json,
};
use crate::auth::{User, ensure_site_access};
use crate::{ApiError, ApiResult, AppState};

/// Newest first, 25 by default (`?limit` up to 200). Rows: `job_runs` minus the (large) `payload`.
pub async fn list(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
    q: Params,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &site_id).await?;
    let limit = int_param(&q, "limit", 25, 1, 200);
    let rows = st
        .db
        .read(move |c| {
            query_json(
                c,
                &format!(
                    "SELECT {JOB_API_COLS} FROM job_runs WHERE site_id=?1
                      ORDER BY created_at DESC, id DESC LIMIT ?2"
                ),
                &[&site_id, &limit],
                JOB_API_JSON,
                &[],
            )
        })
        .await?;
    Ok(Json(Value::Array(rows)))
}

/// One job run by id. Site jobs follow the site ownership rule; site-less jobs are admin-only.
pub async fn get_run(
    State(st): State<AppState>,
    u: User,
    Path(job_id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&job_id) {
        return Err(ApiError::not_found("job not found"));
    }
    let row = st
        .db
        .read(move |c| {
            query_one_json(
                c,
                &format!("SELECT {JOB_API_COLS} FROM job_runs WHERE id=?1"),
                &[&job_id],
                JOB_API_JSON,
                &[],
            )
        })
        .await?
        .ok_or_else(|| ApiError::not_found("job not found"))?;
    match row.get("site_id").and_then(Value::as_str) {
        Some(site) => ensure_site_access(&st, &principal(&u), site)
            .await
            .map_err(|_| ApiError::not_found("job not found"))?,
        None if u.is_admin() => {}
        None => return Err(ApiError::not_found("job not found")),
    }
    Ok(Json(row))
}
