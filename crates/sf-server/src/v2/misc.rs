//! Errors, audit log and MCP connection info.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};
use sf_ops::errors::ErrorTracker;
use sf_store::{err, row_to_json};

use super::clamp_limit;
use crate::auth::{Admin, User, audit as audit_write};
use crate::{ApiError, ApiResult, AppState};

#[derive(Deserialize)]
pub struct ErrorsQ {
    resolved: Option<String>,
    limit: Option<i64>,
}

pub async fn errors(
    State(st): State<AppState>,
    _a: Admin,
    Query(q): Query<ErrorsQ>,
) -> ApiResult<Json<Value>> {
    let resolved = match q.resolved.as_deref() {
        None | Some("" | "all") => None,
        Some("1" | "true" | "yes") => Some(true),
        Some(_) => Some(false),
    };
    let limit = clamp_limit(q.limit, 100, 1000) as usize;
    let rows = ErrorTracker::new(st.db.clone())
        .list(resolved, limit)
        .await?;
    Ok(Json(json!({ "errors": rows })))
}

pub async fn resolve_error(
    State(st): State<AppState>,
    Admin(u): Admin,
    Path(fp): Path<String>,
) -> ApiResult<Json<Value>> {
    if fp.len() > 64 {
        return Err(ApiError::bad("invalid fingerprint"));
    }
    if !ErrorTracker::new(st.db.clone()).resolve(&fp).await? {
        return Err(ApiError::not_found("error not found"));
    }
    audit_write(
        &st,
        &u.actor(),
        u.via(),
        "error.resolve",
        Some(&fp),
        json!({}),
    )
    .await;
    Ok(Json(json!({ "resolved": true, "fingerprint": fp })))
}

#[derive(Deserialize)]
pub struct AuditQ {
    limit: Option<i64>,
    before_id: Option<i64>,
}

pub async fn audit(
    State(st): State<AppState>,
    _a: Admin,
    Query(q): Query<AuditQ>,
) -> ApiResult<Json<Value>> {
    let limit = clamp_limit(q.limit, 100, 1000);
    let before = q.before_id.unwrap_or(i64::MAX);
    let rows = st
        .db
        .read(move |c| {
            let mut stmt = c
                .prepare(
                    "SELECT id, ts, actor, via, action, target, detail FROM audit_log WHERE id < ?1 ORDER BY id DESC LIMIT ?2",
                )
                .map_err(err)?;
            let rows = stmt
                .query_map(rusqlite::params![before, limit], |r| {
                    Ok(row_to_json(r, &["detail"], &[]))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?;
    Ok(Json(json!({ "entries": rows })))
}

/// `GET /mcp`: how to connect an MCP client (no secrets; auth is an API token from `/api/v2/tokens`).
pub async fn mcp_info(State(st): State<AppState>, _u: User) -> ApiResult<Json<Value>> {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "siloforge".into());
    let bind = st
        .cfg
        .server
        .binds
        .first()
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:8600".into());
    let url = format!("http://{bind}/mcp");
    // G2-7: `siloforge mcp` talks to the API over HTTP and ignores --home, so none is advertised.
    let args = vec!["mcp".to_string()];
    // A1: SILOFORGE_API_URL is the server origin; the MCP client appends /api/v1/... itself.
    let api_url = format!("http://{bind}");
    let tools: Vec<Value> = st
        .services
        .get::<crate::state::McpTools>()
        .map(|t| t.0.clone())
        .unwrap_or_default();
    Ok(Json(json!({
        "enabled": true,
        "stdio_command": exe,
        "stdio_args": args,
        "http_url": url,
        "api_url": api_url,
        "transport": "streamable-http",
        "url": url,
        "transports": {
            "stdio": { "command": exe, "args": args },
            "http": { "url": url, "auth": "Authorization: Bearer <API token>" },
        },
        "auth": "Create an API token in Settings -> API Tokens (POST /api/v2/tokens) and pass it as a Bearer token.",
        "client_config": {
            "mcpServers": {
                "siloforge": { "command": exe, "args": args, "env": { "SILOFORGE_API_URL": api_url, "SILOFORGE_TOKEN": "<API token>" } }
            }
        },
        "tools": tools,
    })))
}
