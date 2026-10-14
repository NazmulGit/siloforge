//! `/sites/:id/gsc`: Google Search Console service-account connect / status / disconnect.
//! The credentials JSON is sealed into `sites.secrets_enc` (v1 kept it in plaintext settings, S3);
//! `settings.gsc` only keeps `{connected, saved_at, client_email}`.

use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::{Value, json};

use super::common::{JsonBody, audit_user, get_site, guard_user_site, mutate_site};
use crate::auth::User;
use crate::{ApiError, ApiResult, AppState};

const MAX_CREDENTIALS: usize = 64 * 1024;

#[derive(Deserialize)]
pub struct ConnectBody {
    credentials_json: String,
}

pub async fn connect(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<ConnectBody>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &id).await?;
    if b.credentials_json.len() > MAX_CREDENTIALS {
        return Err(ApiError::bad("credentials_json is too large"));
    }
    let parsed: Value = serde_json::from_str(&b.credentials_json)
        .map_err(|_| ApiError::bad("credentials_json must be valid JSON"))?;
    let Some(obj) = parsed.as_object() else {
        return Err(ApiError::bad(
            "credentials_json must be a service account object",
        ));
    };
    if !obj.contains_key("type") && !obj.contains_key("client_email") {
        return Err(ApiError::bad(
            "credentials_json must be a service account object",
        ));
    }
    let client_email = obj
        .get("client_email")
        .and_then(Value::as_str)
        .map(str::to_string);
    let saved_at = sf_core::now_rfc3339();
    let gsc = json!({ "connected": true, "saved_at": saved_at, "client_email": client_email });
    let creds = parsed.to_string();
    mutate_site(&st, &id, None, move |settings, secrets| {
        secrets.gsc_credentials = Some(creds);
        if let Some(o) = settings.as_object_mut() {
            o.insert("gsc".into(), gsc);
        }
        Ok(true)
    })
    .await?;
    audit_user(
        &st,
        &u,
        "site.gsc.connect",
        Some(&id),
        json!({ "client_email": client_email }),
    )
    .await;
    Ok(Json(
        json!({ "connected": true, "client_email": client_email, "saved_at": saved_at }),
    ))
}

pub async fn disconnect(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &id).await?;
    mutate_site(&st, &id, None, |settings, secrets| {
        let had = secrets.gsc_credentials.take().is_some();
        if let Some(o) = settings.as_object_mut() {
            o.remove("gsc");
        }
        Ok(had)
    })
    .await?;
    audit_user(&st, &u, "site.gsc.disconnect", Some(&id), json!({})).await;
    Ok(Json(json!({ "connected": false })))
}

/// Status only; never returns the credentials.
pub async fn status(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &id).await?;
    let site = get_site(&st, &id).await?;
    let gsc = site.settings.get("gsc").cloned().unwrap_or(Value::Null);
    Ok(Json(json!({
        "connected": gsc.get("connected").and_then(Value::as_bool) == Some(true),
        "client_email": gsc.get("client_email").and_then(Value::as_str),
        "saved_at": gsc.get("saved_at").and_then(Value::as_str),
    })))
}
