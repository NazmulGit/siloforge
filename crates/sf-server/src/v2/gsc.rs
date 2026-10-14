//! `POST /api/v2/sites/{id}/gsc/sync`: enqueue a Search Console pull (`gsc` job, G2-2).
//! User (own sites; admin all). Answers `202 {job_run_id, deduplicated}` like the other enqueue
//! routes; `409` when the site has no Search Console connection and no install-wide
//! `GSC_CREDENTIALS_JSON` fallback is configured (G2-5).

use axum::extract::{Path, State};
use axum::response::Response;
use serde_json::{Value, json};
use sf_core::types::JobKind;

use crate::auth::User;
use crate::v1::common::{enqueue, get_site, guard_user_site};
use crate::{ApiError, ApiResult, AppState};

pub async fn sync(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &id).await?;
    let site = get_site(&st, &id).await?;
    let connected = site
        .settings
        .get("gsc")
        .and_then(|g| g.get("connected"))
        .and_then(Value::as_bool)
        == Some(true);
    if !connected && st.cfg.gsc.fallback_credentials().is_none() {
        return Err(ApiError::conflict(
            "Search Console is not connected for this site",
        ));
    }
    enqueue(&st, &u, JobKind::Gsc, &id, json!({})).await
}
