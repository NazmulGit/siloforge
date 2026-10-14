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
const COLS: &str = "id, user_id, name, prefix, scopes, created_at, last_used_at, expires_at, revoked_at, parent_id, workspace_id";

#[derive(Deserialize)]
pub struct ListQ {
    all: Option<String>,
    include_revoked: Option<String>,
    /// v2.4: tokens of one workspace (its admins see every member's tokens there).
    workspace_id: Option<String>,
}

fn truthy(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("1" | "true" | "yes"))
}

pub async fn list(
    State(st): State<AppState>,
    u: User,
    Query(q): Query<ListQ>,
) -> ApiResult<Json<Value>> {
    let mut all = truthy(&q.all) && u.is_admin();
    let revoked = truthy(&q.include_revoked);
    let uid = u.user_id.clone();
    let ws = match q
        .workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(w) => {
            let (ws, role) =
                crate::tenancy::ws_guard(&st, &u, w, crate::tenancy::Role::Viewer).await?;
            if role >= crate::tenancy::Role::Admin {
                all = true;
            }
            Some(ws.id)
        }
        None => None,
    };
    let rows = st
        .db
        .read(move |c| {
            let mut sql = format!(
                "SELECT {COLS} FROM api_tokens WHERE (?1 OR user_id = ?2) AND (?3 IS NULL OR workspace_id = ?3)"
            );
            if !revoked {
                sql.push_str(" AND revoked_at IS NULL");
            }
            sql.push_str(" ORDER BY created_at DESC, id");
            let mut stmt = c.prepare(&sql).map_err(err)?;
            let rows = stmt
                .query_map(params![all, uid, ws], |r| Ok(row_to_json(r, &["scopes"], &[])))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?;
    // v2.4: per-token rules live in the token's workspace policy.
    let mut policies: std::collections::HashMap<String, super::mcp::McpPolicy> =
        std::collections::HashMap::new();
    let mut out: Vec<Value> = Vec::with_capacity(rows.len());
    for mut r in rows {
        let id = r
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let ws = r
            .get("workspace_id")
            .and_then(Value::as_str)
            .unwrap_or(crate::tenancy::DEFAULT_WS)
            .to_string();
        if !policies.contains_key(&ws) {
            let p = super::mcp::load_ws_policy(&st, &ws)
                .await
                .unwrap_or_default();
            policies.insert(ws.clone(), p);
        }
        let rule = policies
            .get(&ws)
            .and_then(|p| p.per_token.get(&id))
            .map_or(Value::Null, |p| json!({"allow": p.allow}));
        if let Some(o) = r.as_object_mut() {
            o.insert("tool_policy".into(), rule);
        }
        out.push(r);
    }
    let rows = out;
    Ok(Json(json!({ "tokens": rows })))
}

#[derive(Deserialize)]
pub struct CreateBody {
    name: String,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default)]
    expires_in_days: Option<i64>,
    /// v2.4: workspace the token belongs to (default: the current / default workspace).
    #[serde(default)]
    workspace_id: Option<String>,
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
pub(crate) async fn caller_token(
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
    // v2.4: the token's workspace (admin role there) and the plan's `api_tokens` quota.
    let ws_id = match b
        .workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(w) => Some(
            crate::tenancy::ws_guard(&st, &u, w, crate::tenancy::Role::Admin)
                .await?
                .0
                .id,
        ),
        None => match crate::tenancy::target_ws(&st, &u).await? {
            Some(ws) => Some(
                crate::tenancy::ws_guard(&st, &u, &ws.id, crate::tenancy::Role::Admin)
                    .await?
                    .0
                    .id,
            ),
            None => None,
        },
    };
    crate::tenancy::check_quota(&st, ws_id.as_deref(), "api_tokens", 1).await?;
    let (token, hash, prefix) = sf_core::crypto::new_api_token();
    let id = sf_core::new_id();
    let scopes_json = serde_json::to_string(&scopes).unwrap_or_else(|_| "[\"read\"]".into());
    let (id2, uid, name2, prefix2, exp2, parent2, ws2) = (
        id.clone(),
        u.user_id.clone(),
        name.clone(),
        prefix.clone(),
        expires_at.clone(),
        parent_id.clone(),
        ws_id.clone(),
    );
    let created_at = st
        .db
        .write(move |t| {
            t.execute(
                "INSERT INTO api_tokens(id, user_id, name, token_hash, prefix, scopes, expires_at, parent_id, workspace_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![id2, uid, name2, hash, prefix2, scopes_json, exp2, parent2, ws2],
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
    crate::eventlog::log(&st).record_auth(
        &u.actor(),
        "token create",
        "ok",
        Some(&u.user_id),
        Some(&id),
        json!({"name": name, "scopes": scopes, "parent_id": parent_id}),
    );
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
        "workspace_id": ws_id,
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
    crate::eventlog::log(&st).record_auth(
        &u.actor(),
        "token revoke",
        "ok",
        Some(&u.user_id),
        Some(&id),
        json!({ "descendants_revoked": cascaded }),
    );
    Ok(Json(json!({ "deleted": true, "id": id })))
}

/// Does a token holding `parent` scopes cover `scope`? (`*` covers all; admin/write imply read.)
fn parent_covers(parent: &[String], scope: &str) -> bool {
    parent.iter().any(|p| p == "*" || p == scope)
        || (scope == "read" && parent.iter().any(|p| p == "write" || p == "admin"))
        || (scope == "write" && parent.iter().any(|p| p == "admin"))
}

#[derive(Deserialize)]
pub struct PatchBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    /// `null` = never expires (still capped by the parent), a number = days from now.
    #[serde(default, deserialize_with = "double_option")]
    expires_in_days: Option<Option<i64>>,
    /// `{allow: [...] | null}`: MCP tools this token may use (`null` = all its scope allows).
    #[serde(default, deserialize_with = "double_option")]
    tool_policy: Option<Option<super::mcp::TokenRule>>,
}

/// Distinguishes a missing field (`None`) from an explicit `null` (`Some(None)`).
fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

type TokenRow = (String, String, Vec<String>, Option<String>, Option<String>);

async fn token_row(st: &AppState, id: &str) -> ApiResult<Option<TokenRow>> {
    let id = id.to_string();
    let row = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT id, user_id, scopes, expires_at, parent_id FROM api_tokens WHERE id=?1 AND revoked_at IS NULL",
                [&id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(err)
        })
        .await?;
    Ok(row.map(|(id, uid, scopes, exp, parent)| {
        let scopes: Vec<String> = serde_json::from_str(&scopes).unwrap_or_default();
        (id, uid, scopes, exp, parent)
    }))
}

/// `PATCH /tokens/{id}` (owner or admin): `{name?, scopes?, expires_in_days?: number|null,
/// tool_policy?: {allow: [...]|null}|null}` -> the updated row. Scopes are a subset of
/// `*, read, write, admin`; `*` and `admin` only for admins; a token is never raised above its
/// parent's scopes or expiry; descendants are capped to the new scopes (revoked when nothing
/// remains). The auth cache is cleared so the change applies to the next request.
pub async fn patch(
    State(st): State<AppState>,
    u: User,
    headers: HeaderMap,
    Path(id): Path<String>,
    crate::v1::common::JsonBody(b): crate::v1::common::JsonBody<PatchBody>,
) -> ApiResult<Json<Value>> {
    let Some((tid, owner, old_scopes, old_exp, parent_id)) = token_row(&st, &id).await? else {
        return Err(ApiError::not_found("token not found"));
    };
    if owner != u.user_id && !u.is_admin() {
        return Err(ApiError::not_found("token not found"));
    }
    // An API token may only edit tokens when it is a full/admin token (same rule as create).
    if u.token.is_some() {
        let (_, pscopes, _) = caller_token(&st, &headers).await?;
        if !pscopes.iter().any(|s| s == "*" || s == "admin") {
            return Err(ApiError::forbidden(
                "API tokens can only edit tokens when they have the * or admin scope",
            ));
        }
    }
    let parent = match &parent_id {
        Some(pid) => token_row(&st, pid).await?,
        None => None,
    };
    let name = match b.name {
        Some(n) => {
            let n = n.trim().to_string();
            if n.is_empty() || n.chars().count() > 100 {
                return Err(ApiError::bad("name is required (max 100 chars)"));
            }
            Some(n)
        }
        None => None,
    };
    let scopes = match b.scopes {
        Some(mut s) => {
            s.sort();
            s.dedup();
            if s.is_empty() || s.len() > SCOPES.len() {
                return Err(ApiError::bad(
                    "scopes must be a non-empty subset of *, read, write, admin",
                ));
            }
            for x in &s {
                if !SCOPES.contains(&x.as_str()) {
                    return Err(ApiError::bad(format!(
                        "unknown scope '{x}' (allowed: *, read, write, admin)"
                    )));
                }
                if (x == "admin" || x == "*") && !u.is_admin() {
                    return Err(ApiError::forbidden(format!(
                        "only admins can grant the '{x}' scope"
                    )));
                }
                if !caller_covers(&u, x) {
                    return Err(ApiError::forbidden(format!(
                        "your token cannot grant the '{x}' scope"
                    )));
                }
                if let Some((_, _, pscopes, _, _)) = &parent
                    && !parent_covers(pscopes, x)
                {
                    return Err(ApiError::forbidden(format!(
                        "scope '{x}' is above the parent token's scopes"
                    )));
                }
            }
            Some(s)
        }
        None => None,
    };
    let pexp = parent.as_ref().and_then(|p| p.3.clone());
    let expires_at: Option<Option<String>> = match b.expires_in_days {
        None => None,
        Some(None) => Some(pexp.clone()),
        Some(Some(d)) if (1..=3650).contains(&d) => {
            let e = (chrono::Utc::now() + chrono::Duration::days(d))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            if let Some(p) = &pexp
                && e.as_str() > p.as_str()
            {
                return Err(ApiError::bad(
                    "expires_in_days would outlive the parent token",
                ));
            }
            Some(Some(e))
        }
        Some(Some(_)) => return Err(ApiError::bad("expires_in_days must be 1..3650 or null")),
    };
    if let Some(Some(rule)) = &b.tool_policy
        && let Some(a) = &rule.allow
        && a.len() > 500
    {
        return Err(ApiError::bad("tool_policy.allow: too many tools"));
    }
    // validate tool names before writing anything
    if let Some(Some(rule)) = &b.tool_policy {
        super::mcp::check_tool_names(&st, rule.allow.as_deref().unwrap_or(&[]))?;
    }
    let (tid2, name2, scopes2, exp2) = (
        tid.clone(),
        name.clone(),
        scopes.clone(),
        expires_at.clone(),
    );
    let capped = st
        .db
        .write(move |t| {
            if let Some(n) = &name2 {
                t.execute("UPDATE api_tokens SET name=?2 WHERE id=?1", params![tid2, n])
                    .map_err(err)?;
            }
            if let Some(e) = &exp2 {
                t.execute("UPDATE api_tokens SET expires_at=?2 WHERE id=?1", params![tid2, e])
                    .map_err(err)?;
            }
            let mut capped = 0usize;
            if let Some(s) = &scopes2 {
                let js = serde_json::to_string(s).unwrap_or_else(|_| "[\"read\"]".into());
                t.execute("UPDATE api_tokens SET scopes=?2 WHERE id=?1", params![tid2, js])
                    .map_err(err)?;
                // cap descendants (breadth first, each against its own parent's new scopes)
                let mut frontier: Vec<(String, Vec<String>)> = vec![(tid2.clone(), s.clone())];
                let now = sf_core::now_rfc3339();
                let mut guard = 0usize;
                while let Some((pid, pscopes)) = frontier.pop() {
                    guard += 1;
                    if guard > 10_000 {
                        break;
                    }
                    let kids: Vec<(String, String)> = {
                        let mut stmt = t
                            .prepare("SELECT id, scopes FROM api_tokens WHERE parent_id=?1 AND revoked_at IS NULL")
                            .map_err(err)?;
                        stmt.query_map([&pid], |r| Ok((r.get(0)?, r.get(1)?)))
                            .map_err(err)?
                            .collect::<rusqlite::Result<Vec<_>>>()
                            .map_err(err)?
                    };
                    for (kid, ks) in kids {
                        let ks: Vec<String> = serde_json::from_str(&ks).unwrap_or_default();
                        let kept: Vec<String> =
                            ks.iter().filter(|x| parent_covers(&pscopes, x)).cloned().collect();
                        if kept.is_empty() {
                            t.execute(
                                "UPDATE api_tokens SET revoked_at=?2 WHERE id=?1",
                                params![kid, now],
                            )
                            .map_err(err)?;
                            capped += 1;
                            continue;
                        }
                        if kept.len() != ks.len() {
                            let js = serde_json::to_string(&kept).unwrap_or_default();
                            t.execute("UPDATE api_tokens SET scopes=?2 WHERE id=?1", params![kid, js])
                                .map_err(err)?;
                            capped += 1;
                        }
                        frontier.push((kid, kept));
                    }
                }
            }
            Ok(capped)
        })
        .await?;
    if let Some(tp) = b.tool_policy {
        super::mcp::set_token_rule(&st, &tid, tp).await?;
    }
    st.auth_cache.clear();
    audit(
        &st,
        &u.actor(),
        u.via(),
        "token.update",
        Some(&tid),
        json!({
            "name": name, "scopes": scopes, "old_scopes": old_scopes,
            "expires_changed": expires_at.is_some(), "old_expires_at": old_exp,
            "descendants_capped": capped,
        }),
    )
    .await;
    let tid3 = tid.clone();
    let row = st
        .db
        .read(move |c| {
            c.query_row(
                &format!("SELECT {COLS} FROM api_tokens WHERE id=?1"),
                [&tid3],
                |r| Ok(row_to_json(r, &["scopes"], &[])),
            )
            .map_err(err)
        })
        .await?;
    let mut row = row;
    if let Some(o) = row.as_object_mut() {
        let rule = super::mcp::token_rule(&st, &tid).await;
        o.insert(
            "tool_policy".into(),
            rule.map_or(Value::Null, |r| json!({"allow": r.allow})),
        );
        o.insert("descendants_capped".into(), json!(capped));
    }
    Ok(Json(row))
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
            crate::sessions::revoke_all_sync(t, &uid)?;
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
