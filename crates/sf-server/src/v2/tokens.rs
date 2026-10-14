//! API tokens (`sfk_...`): users manage their own; the plaintext is returned once at creation.
//!
//! M-04: an API token may mint tokens only when it carries the `*` or `admin` scope; the child
//! never gets broader scopes or a later expiry than its parent, records `parent_id`, and revoking
//! a token revokes all of its descendants.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sf_store::{err, row_to_json};

use crate::auth::{User, audit};
use crate::{ApiError, ApiResult, AppState};

pub const SCOPES: &[&str] = &["*", "read", "write", "admin"];
const COLS: &str = "id, user_id, name, prefix, scopes, created_at, last_used_at, expires_at, revoked_at, parent_id";

#[derive(Deserialize)]
pub struct ListQ {
    all: Option<String>,
    include_revoked: Option<String>,
}

fn truthy(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("1" | "true" | "yes"))
}

pub async fn list(
    State(st): State<AppState>,
    u: User,
    Query(q): Query<ListQ>,
) -> ApiResult<Json<Value>> {
    let all = truthy(&q.all) && u.is_admin();
    let revoked = truthy(&q.include_revoked);
    let uid = u.user_id.clone();
    let rows = st
        .db
        .read(move |c| {
            let mut sql = format!("SELECT {COLS} FROM api_tokens WHERE (?1 OR user_id = ?2)");
            if !revoked {
                sql.push_str(" AND revoked_at IS NULL");
            }
            sql.push_str(" ORDER BY created_at DESC, id");
            let mut stmt = c.prepare(&sql).map_err(err)?;
            let rows = stmt
                .query_map(params![all, uid], |r| Ok(row_to_json(r, &["scopes"], &[])))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?;
    Ok(Json(json!({ "tokens": rows })))
}

#[derive(Deserialize)]
pub struct CreateBody {
    name: String,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default)]
    expires_in_days: Option<i64>,
}

/// May the caller grant `scope`? Session users: any (admin scope needs the admin role, checked
/// separately). Token callers can only grant what their own token covers.
fn caller_covers(u: &User, scope: &str) -> bool {
    if u.token.is_none() || u.scopes.iter().any(|s| s == "*") {
        return true;
    }
    u.scopes.iter().any(|s| s == scope)
        || (scope == "read" && u.scopes.iter().any(|s| s == "write" || s == "admin"))
}

/// The calling API token's row (id, scopes, expires_at), looked up by the bearer's hash.
async fn caller_token(
    st: &AppState,
    headers: &HeaderMap,
) -> ApiResult<(String, Vec<String>, Option<String>)> {
    let cred = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.split_once(' '))
        .filter(|(s, _)| s.eq_ignore_ascii_case("bearer"))
        .map(|(_, v)| v.trim().to_string())
        .ok_or_else(|| ApiError::unauthorized("invalid API token"))?;
    let hash = sf_core::crypto::sha256_hex(cred.as_bytes());
    let row = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT id, scopes, expires_at FROM api_tokens WHERE token_hash=?1 AND revoked_at IS NULL",
                [&hash],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(err)
        })
        .await?;
    let (id, scopes, exp) = row.ok_or_else(|| ApiError::unauthorized("invalid API token"))?;
    let scopes: Vec<String> = serde_json::from_str(&scopes).unwrap_or_default();
    Ok((id, scopes, exp))
}

/// `POST /tokens` -> the row plus `token` (plaintext, shown once).
pub async fn create(
    State(st): State<AppState>,
    u: User,
    headers: HeaderMap,
    body: Result<Json<CreateBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<Value>> {
    let Json(b) = body?;
    // M-04: only a full (`*`) or `admin` token may mint tokens; record it as the parent.
    let parent = if u.token.is_some() {
        let (pid, pscopes, pexp) = caller_token(&st, &headers).await?;
        if !pscopes.iter().any(|s| s == "*" || s == "admin") {
            return Err(ApiError::forbidden(
                "API tokens can only create tokens when they have the * or admin scope; \
                 use a signed-in session instead",
            ));
        }
        Some((pid, pexp))
    } else {
        None
    };
    let name = b.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(ApiError::bad("name is required (max 100 chars)"));
    }
    let mut scopes = b.scopes.unwrap_or_else(|| vec!["read".into()]);
    scopes.sort();
    scopes.dedup();
    if scopes.is_empty() || scopes.len() > SCOPES.len() {
        return Err(ApiError::bad(
            "scopes must be a non-empty subset of *, read, write, admin",
        ));
    }
    for s in &scopes {
        if !SCOPES.contains(&s.as_str()) {
            return Err(ApiError::bad(format!(
                "unknown scope '{s}' (allowed: *, read, write, admin)"
            )));
        }
        // "*" on a non-admin acts as that user (role still caps it); "admin" needs the admin role.
        if s == "admin" && !u.is_admin() {
            return Err(ApiError::forbidden(
                "only admins can create admin-scope tokens",
            ));
        }
        if !caller_covers(&u, s) {
            return Err(ApiError::forbidden(format!(
                "your token cannot grant the '{s}' scope"
            )));
        }
    }
    let expires_at = match b.expires_in_days {
        None => None,
        Some(d) if (1..=3650).contains(&d) => Some(
            (chrono::Utc::now() + chrono::Duration::days(d))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        ),
        Some(_) => return Err(ApiError::bad("expires_in_days must be 1..3650")),
    };
    // A child token never outlives its parent: inherit the parent's expiry, refuse a later one.
    let expires_at = match (&parent, expires_at) {
        (Some((_, Some(pexp))), None) => Some(pexp.clone()),
        (Some((_, Some(pexp))), Some(e)) if e.as_str() > pexp.as_str() => {
            return Err(ApiError::bad(
                "expires_in_days would outlive the API token used to create this token",
            ));
        }
        (_, e) => e,
    };
    let parent_id = parent.map(|(pid, _)| pid);
    let (token, hash, prefix) = sf_core::crypto::new_api_token();
    let id = sf_core::new_id();
    let scopes_json = serde_json::to_string(&scopes).unwrap_or_else(|_| "[\"read\"]".into());
    let (id2, uid, name2, prefix2, exp2, parent2) = (
        id.clone(),
        u.user_id.clone(),
        name.clone(),
        prefix.clone(),
        expires_at.clone(),
        parent_id.clone(),
    );
    let created_at = st
        .db
        .write(move |t| {
            t.execute(
                "INSERT INTO api_tokens(id, user_id, name, token_hash, prefix, scopes, expires_at, parent_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![id2, uid, name2, hash, prefix2, scopes_json, exp2, parent2],
            )
            .map_err(err)?;
            t.query_row("SELECT created_at FROM api_tokens WHERE id=?1", [&id2], |r| {
                r.get::<_, String>(0)
            })
            .map_err(err)
        })
        .await?;
    audit(
        &st,
        &u.actor(),
        u.via(),
        "token.create",
        Some(&id),
        json!({"name": name, "scopes": scopes, "parent_id": parent_id}),
    )
    .await;
    Ok(Json(json!({
        "token": token,
        "id": id,
        "user_id": u.user_id,
        "name": name,
        "prefix": prefix,
        "scopes": scopes,
        "created_at": created_at,
        "last_used_at": Value::Null,
        "expires_at": expires_at,
        "revoked_at": Value::Null,
        "parent_id": parent_id,
    })))
}

/// `DELETE /tokens/{id}`: revokes (own token; admins any) together with every token it minted,
/// recursively (M-04).
pub async fn revoke(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let (id2, uid, admin) = (id.clone(), u.user_id.clone(), u.is_admin());
    let (n, cascaded) = st
        .db
        .write(move |t| {
            let now = sf_core::now_rfc3339();
            let n = t
                .execute(
                    "UPDATE api_tokens SET revoked_at=?3 WHERE id=?1 AND revoked_at IS NULL AND (?4 OR user_id=?2)",
                    params![id2, uid, now, admin],
                )
                .map_err(err)?;
            if n == 0 {
                return Ok((0, 0));
            }
            let cascaded = t
                .execute(
                    "WITH RECURSIVE d(id) AS (
                         SELECT id FROM api_tokens WHERE parent_id=?1
                         UNION SELECT t.id FROM api_tokens t JOIN d ON t.parent_id = d.id
                     )
                     UPDATE api_tokens SET revoked_at=?2 WHERE id IN (SELECT id FROM d) AND revoked_at IS NULL",
                    params![id2, now],
                )
                .map_err(err)?;
            Ok((n, cascaded))
        })
        .await?;
    if n == 0 {
        return Err(ApiError::not_found("token not found"));
    }
    st.auth_cache.clear();
    audit(
        &st,
        &u.actor(),
        u.via(),
        "token.revoke",
        Some(&id),
        json!({ "descendants_revoked": cascaded }),
    )
    .await;
    Ok(Json(json!({ "deleted": true, "id": id })))
}

#[derive(Deserialize, Default)]
pub struct LogoutAllBody {
    /// Also revoke every API token of the user (default true). `false` ends sessions only.
    #[serde(default)]
    revoke_api_tokens: Option<bool>,
}

/// I-02: `POST /api/v2/auth/logout-all` ends every session of the caller (bumps
/// `users.token_version`) and, unless `revoke_api_tokens: false`, revokes all of the caller's API
/// tokens. Interactive sessions only. Answers `{ok, token, api_tokens_revoked}` with a fresh
/// session token for the caller. A password change does NOT revoke API tokens (they are separate,
/// individually revocable credentials, e.g. the Hermes MCP token): use this endpoint for that.
pub async fn logout_all(
    State(st): State<AppState>,
    u: User,
    crate::v1::common::JsonBody(body): crate::v1::common::JsonBody<LogoutAllBody>,
) -> ApiResult<Json<Value>> {
    if u.token.is_some() {
        return Err(ApiError::forbidden(
            "API tokens cannot log out everywhere; use an interactive session",
        ));
    }
    let revoke_tokens = body.revoke_api_tokens.unwrap_or(true);
    let uid = u.user_id.clone();
    let (role, tv, revoked) = st
        .db
        .write(move |t| {
            t.execute(
                "UPDATE users SET token_version=token_version+1 WHERE id=?1",
                [&uid],
            )
            .map_err(err)?;
            let revoked = if revoke_tokens {
                t.execute(
                    "UPDATE api_tokens SET revoked_at=?2 WHERE user_id=?1 AND revoked_at IS NULL",
                    params![uid, sf_core::now_rfc3339()],
                )
                .map_err(err)?
            } else {
                0
            };
            let (role, tv) = t
                .query_row(
                    "SELECT role, token_version FROM users WHERE id=?1",
                    [&uid],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                )
                .optional()
                .map_err(err)?
                .ok_or_else(|| sf_core::Error::not_found("user not found"))?;
            Ok((role, tv, revoked))
        })
        .await?;
    st.auth_cache.clear();
    audit(
        &st,
        &u.actor(),
        u.via(),
        "auth.logout_all",
        Some(&u.user_id),
        json!({ "api_tokens_revoked": revoked }),
    )
    .await;
    let token = sf_core::crypto::sign_jwt(
        &st.cfg.auth.jwt_secret,
        &sf_core::crypto::user_claims(&role, &u.user_id, tv),
    )?;
    Ok(Json(
        json!({ "ok": true, "token": token, "api_tokens_revoked": revoked }),
    ))
}
