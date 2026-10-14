//! Notification channels (secrets masked) and the notification inbox.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};
use sf_ops::notify::{self, ChannelPatch, NewChannel, Notifier};

use super::clamp_limit;
use crate::auth::{Admin, audit};
use crate::{ApiError, ApiResult, AppState};

pub async fn list_channels(State(st): State<AppState>, _a: Admin) -> ApiResult<Json<Value>> {
    let ch = notify::list_channels(&st.db, &st.secret_key).await?;
    Ok(Json(json!({
        "channels": ch.iter().map(|c| c.public_json()).collect::<Vec<_>>(),
        "kinds": notify::CHANNEL_KINDS,
        "events": notify::events::ALL,
    })))
}

pub async fn create_channel(
    State(st): State<AppState>,
    Admin(u): Admin,
    body: Result<Json<NewChannel>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<Value>> {
    let Json(body) = body?;
    let ch = notify::create_channel(&st.db, &st.secret_key, body).await?;
    audit(
        &st,
        &u.actor(),
        u.via(),
        "notification_channel.create",
        Some(&ch.id),
        json!({"kind": ch.kind, "name": ch.name}),
    )
    .await;
    Ok(Json(ch.public_json()))
}

pub async fn update_channel(
    State(st): State<AppState>,
    Admin(u): Admin,
    Path(id): Path<String>,
    body: Result<Json<ChannelPatch>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<Value>> {
    let Json(body) = body?;
    let ch = notify::update_channel(&st.db, &st.secret_key, &id, body).await?;
    audit(
        &st,
        &u.actor(),
        u.via(),
        "notification_channel.update",
        Some(&id),
        json!({}),
    )
    .await;
    Ok(Json(ch.public_json()))
}

pub async fn delete_channel(
    State(st): State<AppState>,
    Admin(u): Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !notify::delete_channel(&st.db, &id).await? {
        return Err(ApiError::not_found("channel not found"));
    }
    audit(
        &st,
        &u.actor(),
        u.via(),
        "notification_channel.delete",
        Some(&id),
        json!({}),
    )
    .await;
    Ok(Json(json!({ "deleted": true })))
}

/// `POST /notifications/channels/{id}/test` -> `{ok, error, attempts}`.
pub async fn test_channel(
    State(st): State<AppState>,
    _a: Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let n = st.service::<Notifier>()?;
    let d = n.test_channel(&id).await?;
    Ok(Json(
        json!({ "ok": d.ok, "error": d.error, "attempts": d.attempts }),
    ))
}

#[derive(Deserialize)]
pub struct ListQ {
    unread: Option<String>,
    limit: Option<i64>,
}

pub async fn list(
    State(st): State<AppState>,
    _a: Admin,
    Query(q): Query<ListQ>,
) -> ApiResult<Json<Value>> {
    let unread = matches!(q.unread.as_deref(), Some("1" | "true" | "yes"));
    let limit = clamp_limit(q.limit, 100, 1000) as usize;
    let items = notify::list_notifications(&st.db, unread, limit).await?;
    let unread_count = notify::unread_count(&st.db).await?;
    Ok(Json(
        json!({ "notifications": items, "unread": unread_count }),
    ))
}

#[derive(Deserialize, Default)]
pub struct ReadBody {
    #[serde(default)]
    ids: Option<Vec<i64>>,
    #[serde(default)]
    all: Option<bool>,
}

/// `POST /notifications/read` with `{ids:[..]}` or `{all:true}` (empty body = all).
pub async fn mark_read(
    State(st): State<AppState>,
    _a: Admin,
    body: Option<Json<ReadBody>>,
) -> ApiResult<Json<Value>> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let ids = if body.all == Some(true) {
        None
    } else {
        body.ids
    };
    let updated = notify::mark_read(&st.db, ids).await?;
    Ok(Json(json!({ "updated": updated })))
}
