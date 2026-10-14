//! Backup catalogue: create (background), download (streamed), verify, delete.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use sf_ops::backup::{self, BackupKind, BackupOptions};
use sf_ops::notify::Notifier;

use super::clamp_limit;
use crate::auth::{Admin, audit};
use crate::{ApiError, ApiResult, AppState};

#[derive(Deserialize)]
pub struct ListQ {
    limit: Option<i64>,
}

pub async fn list(
    State(st): State<AppState>,
    _a: Admin,
    Query(q): Query<ListQ>,
) -> ApiResult<Json<Value>> {
    let rows = backup::list(&st.db, clamp_limit(q.limit, 100, 1000) as usize).await?;
    Ok(Json(
        json!({ "backups": rows, "dir": st.cfg.backups_dir() }),
    ))
}

pub async fn get_one(
    State(st): State<AppState>,
    _a: Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    backup::get(&st.db, &id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("backup not found"))
}

#[derive(Deserialize, Default)]
pub struct CreateBody {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    note: Option<String>,
    /// Explicit passphrase; when absent the configured one (`backup.passphrase_env`) is used.
    #[serde(default)]
    passphrase: Option<String>,
    /// Set true to force an unencrypted archive even when a passphrase is configured.
    #[serde(default)]
    unencrypted: Option<bool>,
    /// Put `secrets.env` (master keys) into an unencrypted archive anyway. Off by default (M-02).
    #[serde(default)]
    include_secrets: Option<bool>,
}

/// `POST /backups` -> `{id, status:"running"}`; the archive is built in the background.
pub async fn create(
    State(st): State<AppState>,
    Admin(u): Admin,
    body: Option<Json<CreateBody>>,
) -> ApiResult<Json<Value>> {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let kind = match b.kind.as_deref() {
        None | Some("") => BackupKind::Full,
        Some(k) => {
            BackupKind::parse(k).ok_or_else(|| ApiError::bad("kind must be full, db or config"))?
        }
    };
    let passphrase = if b.unencrypted == Some(true) {
        None
    } else {
        b.passphrase
            .filter(|p| !p.is_empty())
            .or_else(|| backup::configured_passphrase(&st.cfg))
    };
    if passphrase.as_ref().is_some_and(|p| p.len() > 1024) {
        return Err(ApiError::bad("passphrase too long"));
    }
    let encrypted = passphrase.is_some();
    let note: String = b.note.unwrap_or_default().chars().take(500).collect();
    let include_secrets = b.include_secrets == Some(true);
    let secrets_included = kind != BackupKind::Db && (encrypted || include_secrets);
    let opts = BackupOptions {
        kind,
        passphrase,
        note,
        include_secrets,
    };
    let notifier = st.services.get::<Notifier>().map(|n| (*n).clone());
    let id = backup::start(st.cfg.clone(), st.db.clone(), opts, notifier).await?;
    audit(
        &st,
        &u.actor(),
        u.via(),
        "backup.create",
        Some(&id),
        json!({"kind": kind.as_str(), "encrypted": encrypted, "secrets_included": secrets_included}),
    )
    .await;
    Ok(Json(
        json!({ "id": id, "status": "running", "kind": kind.as_str(), "encrypted": encrypted,
                "secrets_included": secrets_included }),
    ))
}

async fn ready_path(st: &AppState, id: &str) -> ApiResult<std::path::PathBuf> {
    let row = backup::get(&st.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("backup not found"))?;
    if row.get("status").and_then(Value::as_str) != Some("ok") {
        return Err(ApiError::conflict("backup is not ready"));
    }
    let p = std::path::PathBuf::from(row.get("path").and_then(Value::as_str).unwrap_or(""));
    if !p.is_file() {
        return Err(ApiError::not_found("backup file is missing on disk"));
    }
    Ok(p)
}

/// `GET /backups/{id}/download`: streams the archive.
pub async fn download(
    State(st): State<AppState>,
    _a: Admin,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let p = ready_path(&st, &id).await?;
    let file = tokio::fs::File::open(&p)
        .await
        .map_err(|e| ApiError::internal(format!("open backup: {e}")))?;
    let len = file.metadata().await.map(|m| m.len()).ok();
    let name: String = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "siloforge-backup.zip".into())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    let ctype = if name.ends_with(".zip") {
        "application/zip"
    } else {
        "application/octet-stream"
    };
    let stream = tokio_util::io::ReaderStream::with_capacity(file, 256 * 1024);
    let mut res = (
        [
            (header::CONTENT_TYPE, ctype.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        Body::from_stream(stream),
    )
        .into_response();
    if let Some(len) = len
        && let Ok(v) = header::HeaderValue::from_str(&len.to_string())
    {
        res.headers_mut().insert(header::CONTENT_LENGTH, v);
    }
    Ok(res)
}

#[derive(Deserialize, Default)]
pub struct VerifyBody {
    #[serde(default)]
    passphrase: Option<String>,
}

/// `POST /backups/{id}/verify` -> `{ok, manifest?, error?}`.
pub async fn verify(
    State(st): State<AppState>,
    _a: Admin,
    Path(id): Path<String>,
    body: Option<Json<VerifyBody>>,
) -> ApiResult<Json<Value>> {
    let p = ready_path(&st, &id).await?;
    let pass = body
        .and_then(|Json(b)| b.passphrase)
        .filter(|p| !p.is_empty())
        .or_else(|| backup::configured_passphrase(&st.cfg));
    let res = tokio::task::spawn_blocking(move || backup::verify(&p, pass.as_deref()))
        .await
        .map_err(|e| ApiError::internal(format!("verify task: {e}")))?;
    Ok(Json(match res {
        Ok(m) => json!({ "ok": true, "manifest": m }),
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }))
}

pub async fn delete(
    State(st): State<AppState>,
    Admin(u): Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !backup::delete(&st.db, &id).await? {
        return Err(ApiError::not_found("backup not found"));
    }
    audit(
        &st,
        &u.actor(),
        u.via(),
        "backup.delete",
        Some(&id),
        json!({}),
    )
    .await;
    Ok(Json(json!({ "deleted": true })))
}
