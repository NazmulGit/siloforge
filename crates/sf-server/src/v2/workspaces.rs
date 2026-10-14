//! v2.4 workspaces, members, invites, plans list and the billing stub (docs/specs/v2.4-saas.md
//! §1, §2, §5, §9.4).

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sf_store::err;

use crate::auth::{User, audit};
use crate::mail::{self, Mail};
use crate::tenancy::{self, Role, Ws};
use crate::v1::common::{JsonBody, limiter};
use crate::{ApiError, ApiResult, AppState};

pub const INVITE_TTL_DAYS: i64 = 7;

fn truthy(v: Option<&str>) -> bool {
    matches!(v, Some("1" | "true" | "yes"))
}

fn ev(st: &AppState, u: &User, action: &str, target: Option<&str>, detail: Value) {
    crate::eventlog::log(st).record_auth(
        &u.actor(),
        action,
        "ok",
        Some(&u.user_id),
        target,
        detail,
    );
}

async fn ws_json(st: &AppState, ws: &Ws, role: Option<&str>) -> ApiResult<Value> {
    let plan = tenancy::plan_of_ws(st, &ws.id).await?;
    let id = ws.id.clone();
    let sites: i64 = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT count(*) FROM sites WHERE workspace_id=?1",
                [&id],
                |r| r.get(0),
            )
            .map_err(err)
        })
        .await?;
    let mut v = ws.to_json();
    if let Some(o) = v.as_object_mut() {
        o.insert("role".into(), json!(role));
        o.insert(
            "plan".into(),
            plan.map_or(
                Value::Null,
                |p| json!({"id": p.id, "name": p.name, "price_label": p.price_label}),
            ),
        );
        o.insert("sites".into(), json!(sites));
    }
    Ok(v)
}

#[derive(Deserialize)]
pub struct ListQ {
    all: Option<String>,
}

/// `GET /workspaces[?all=1]`.
pub async fn list(
    State(st): State<AppState>,
    u: User,
    Query(q): Query<ListQ>,
) -> ApiResult<Json<Value>> {
    let all = truthy(q.all.as_deref()) && u.is_admin();
    let (uid, token_ws) = (
        u.user_id.clone(),
        u.workspace_id
            .clone()
            .filter(|_| u.token.is_some() && !u.is_admin()),
    );
    let rows: Vec<(Ws, Option<String>)> = st
        .db
        .read(move |c| {
            let cols = tenancy::WS_COLS
                .split(", ")
                .map(|x| format!("w.{x}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = if all {
                format!(
                    "SELECT {cols}, (SELECT role FROM workspace_members m WHERE m.workspace_id = w.id AND m.user_id = ?1)
                       FROM workspaces w WHERE w.deleted_at IS NULL ORDER BY w.is_default DESC, w.name COLLATE NOCASE, w.id"
                )
            } else {
                format!(
                    "SELECT {cols}, m.role FROM workspace_members m JOIN workspaces w ON w.id = m.workspace_id
                      WHERE m.user_id = ?1 AND w.deleted_at IS NULL AND (?2 IS NULL OR w.id = ?2)
                      ORDER BY w.is_default DESC, m.created_at, w.id"
                )
            };
            let mut q = c.prepare(&sql).map_err(err)?;
            let p: Vec<&dyn rusqlite::ToSql> = if all { vec![&uid] } else { vec![&uid, &token_ws] };
            let rows = q
                .query_map(p.as_slice(), |r| Ok((Ws::from_row(r)?, r.get::<_, Option<String>>(9)?)))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (ws, role) in &rows {
        let r = role.clone().unwrap_or_else(|| "platform_admin".into());
        out.push(ws_json(&st, ws, Some(&r)).await?);
    }
    let current = tenancy::target_ws(&st, &u)
        .await
        .ok()
        .flatten()
        .map(|w| w.id);
    Ok(Json(json!({ "workspaces": out, "current_id": current })))
}

#[derive(Deserialize)]
pub struct CreateBody {
    name: String,
    #[serde(default)]
    slug: Option<String>,
}

fn clean_name(n: &str) -> ApiResult<String> {
    let n: String = n.trim().chars().take(121).collect();
    if n.is_empty() || n.chars().count() > 120 {
        return Err(ApiError::bad("name is required (max 120 chars)"));
    }
    Ok(n)
}

fn clean_slug(s: Option<&str>) -> ApiResult<Option<String>> {
    match s.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) if tenancy::valid_slug(s) => Ok(Some(s.to_string())),
        Some(_) => Err(ApiError::bad(
            "slug must be 3-48 characters: lowercase letters, digits and dashes",
        )),
    }
}

/// `POST /workspaces {name, slug?}` → 201.
pub async fn create(
    State(st): State<AppState>,
    u: User,
    JsonBody(b): JsonBody<CreateBody>,
) -> ApiResult<Response> {
    if u.token.is_some() && !u.is_admin() {
        return Err(ApiError::forbidden("API tokens cannot create workspaces"));
    }
    super::auth2::may_create_workspace(&st, &u).await?;
    let name = clean_name(&b.name)?;
    let slug = clean_slug(b.slug.as_deref())?;
    let plan = crate::platform::get(&st).await.registration.default_plan_id;
    let owner = u.user_id.clone();
    let ws = st
        .db
        .write(move |t| {
            let plan: String = t
                .query_row(
                    "SELECT id FROM plans WHERE id=?1 UNION ALL SELECT 'free' LIMIT 1",
                    [&plan],
                    |r| r.get(0),
                )
                .map_err(err)?;
            tenancy::create_ws_sync(t, &name, slug.as_deref(), &plan, &owner)
        })
        .await
        .map_err(|e| crate::v1::common::conflict_as(e, "slug already taken"))?;
    audit(
        &st,
        &u.actor(),
        u.via(),
        "workspace.create",
        Some(&ws.id),
        json!({"name": ws.name}),
    )
    .await;
    ev(
        &st,
        &u,
        "workspace create",
        Some(&ws.id),
        json!({"name": ws.name}),
    );
    let v = ws_json(&st, &ws, Some("owner")).await?;
    Ok((StatusCode::CREATED, Json(v)).into_response())
}

/// Workspace detail with limits and usage.
pub async fn detail(st: &AppState, ws: &Ws, role: Role) -> ApiResult<Value> {
    let mut v = ws_json(st, ws, Some(role.as_str())).await?;
    let plan = tenancy::plan_of_ws(st, &ws.id).await?;
    let usage = tenancy::usage(st, &ws.id).await?;
    if let Some(o) = v.as_object_mut() {
        o.insert("my_role".into(), json!(role.as_str()));
        o.insert("limits".into(), plan.map_or(json!({}), |p| p.limits));
        o.insert("usage".into(), usage);
        if let Some(d) = &ws.deleted_at {
            let purge = chrono::DateTime::parse_from_rfc3339(d)
                .ok()
                .map(|t| (t + chrono::Duration::days(tenancy::PURGE_AFTER_DAYS)).to_rfc3339());
            o.insert("purge_after".into(), json!(purge));
        }
    }
    Ok(v)
}

/// `GET /workspaces/{id}`.
pub async fn get_one(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let (ws, role) = tenancy::ws_guard(&st, &u, &id, Role::Viewer).await?;
    Ok(Json(detail(&st, &ws, role).await?))
}

#[derive(Deserialize)]
pub struct PatchBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    slug: Option<String>,
}

/// `PATCH /workspaces/{id} {name?, slug?}` (admin).
pub async fn patch(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<PatchBody>,
) -> ApiResult<Json<Value>> {
    let (ws, role) = tenancy::ws_guard(&st, &u, &id, Role::Admin).await?;
    let name = b.name.as_deref().map(clean_name).transpose()?;
    let slug = clean_slug(b.slug.as_deref())?;
    let wid = ws.id.clone();
    let (n2, s2) = (name.clone(), slug.clone());
    st.db
        .write(move |t| {
            if let Some(n) = n2 {
                t.execute("UPDATE workspaces SET name=?2 WHERE id=?1", params![wid, n])
                    .map_err(err)?;
            }
            if let Some(s) = s2 {
                t.execute("UPDATE workspaces SET slug=?2 WHERE id=?1", params![wid, s])
                    .map_err(err)?;
            }
            Ok(())
        })
        .await
        .map_err(|e| crate::v1::common::conflict_as(e, "slug already taken"))?;
    audit(
        &st,
        &u.actor(),
        u.via(),
        "workspace.update",
        Some(&ws.id),
        json!({"name": name, "slug": slug}),
    )
    .await;
    let ws = tenancy::load_ws(&st, &ws.id)
        .await?
        .ok_or_else(|| ApiError::not_found("workspace not found"))?;
    Ok(Json(detail(&st, &ws, role).await?))
}

#[derive(Deserialize)]
pub struct DeleteBody {
    #[serde(default)]
    confirm: Option<String>,
}

/// Soft delete (sites hidden; purged after 30 days).
pub async fn soft_delete(st: &AppState, ws: &Ws) -> ApiResult<String> {
    if ws.is_default {
        return Err(ApiError::bad("the Default workspace cannot be deleted"));
    }
    let id = ws.id.clone();
    let now = sf_core::now_rfc3339();
    let n2 = now.clone();
    st.db
        .write(move |t| {
            t.execute(
                "UPDATE workspaces SET deleted_at=?2 WHERE id=?1 AND deleted_at IS NULL",
                params![id, n2],
            )
            .map_err(err)?;
            // pause automation of the hidden sites
            t.execute(
                "UPDATE site_schedules SET active=0 WHERE site_id IN (SELECT id FROM sites WHERE workspace_id=?1)",
                [&id],
            )
            .map_err(err)?;
            t.execute(
                "UPDATE api_tokens SET revoked_at=?2 WHERE workspace_id=?1 AND revoked_at IS NULL",
                params![id, sf_core::now_rfc3339()],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    st.auth_cache.clear();
    Ok(now)
}

/// `DELETE /workspaces/{id} {confirm: <slug>}` (owner).
pub async fn delete(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<DeleteBody>,
) -> ApiResult<Json<Value>> {
    let (ws, _) = tenancy::ws_guard(&st, &u, &id, Role::Owner).await?;
    if b.confirm.as_deref() != Some(ws.slug.as_str()) {
        return Err(ApiError::bad("confirm must equal the workspace slug"));
    }
    let at = soft_delete(&st, &ws).await?;
    audit(
        &st,
        &u.actor(),
        u.via(),
        "workspace.delete",
        Some(&ws.id),
        json!({"soft": true}),
    )
    .await;
    ev(&st, &u, "workspace delete", Some(&ws.id), json!({}));
    Ok(Json(
        json!({ "deleted": true, "deleted_at": at, "purge_after_days": tenancy::PURGE_AFTER_DAYS }),
    ))
}

#[derive(Deserialize)]
pub struct TransferBody {
    user_id: String,
}

/// Moves ownership to a member (old owner becomes admin). Shared with the admin console.
pub fn transfer_sync(t: &rusqlite::Transaction<'_>, ws: &str, to: &str) -> sf_core::Result<bool> {
    let is_member: bool = t
        .query_row(
            "SELECT count(*) > 0 FROM workspace_members WHERE workspace_id=?1 AND user_id=?2",
            params![ws, to],
            |r| r.get(0),
        )
        .map_err(err)?;
    if !is_member {
        let exists: bool = t
            .query_row("SELECT count(*) > 0 FROM users WHERE id=?1", [to], |r| {
                r.get(0)
            })
            .map_err(err)?;
        if !exists {
            return Ok(false);
        }
        t.execute(
            "INSERT INTO workspace_members(workspace_id, user_id, role) VALUES(?1,?2,'owner')",
            params![ws, to],
        )
        .map_err(err)?;
    }
    t.execute(
        "UPDATE workspace_members SET role='admin' WHERE workspace_id=?1 AND role='owner' AND user_id<>?2",
        params![ws, to],
    )
    .map_err(err)?;
    t.execute(
        "UPDATE workspace_members SET role='owner' WHERE workspace_id=?1 AND user_id=?2",
        params![ws, to],
    )
    .map_err(err)?;
    t.execute(
        "UPDATE workspaces SET owner_user_id=?2 WHERE id=?1",
        params![ws, to],
    )
    .map_err(err)?;
    Ok(true)
}

/// `POST /workspaces/{id}/transfer {user_id}` (owner; target must be a member).
pub async fn transfer(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<TransferBody>,
) -> ApiResult<Json<Value>> {
    let (ws, _) = tenancy::ws_guard(&st, &u, &id, Role::Owner).await?;
    if tenancy::member_role(&st, &b.user_id, &ws.id)
        .await?
        .is_none()
    {
        return Err(ApiError::bad(
            "the new owner must be a member of the workspace",
        ));
    }
    let (w, to) = (ws.id.clone(), b.user_id.clone());
    st.db.write(move |t| transfer_sync(t, &w, &to)).await?;
    st.auth_cache.clear();
    audit(
        &st,
        &u.actor(),
        u.via(),
        "workspace.transfer",
        Some(&ws.id),
        json!({"to": b.user_id}),
    )
    .await;
    ev(
        &st,
        &u,
        "workspace transfer",
        Some(&ws.id),
        json!({"to": b.user_id}),
    );
    let ws = tenancy::load_ws(&st, &ws.id)
        .await?
        .ok_or_else(|| ApiError::not_found("workspace not found"))?;
    let role = tenancy::ws_role(&st, &u, &ws)
        .await?
        .unwrap_or(Role::Viewer);
    Ok(Json(detail(&st, &ws, role).await?))
}

// ---------------------------------------------------------------------------
// members
// ---------------------------------------------------------------------------

pub async fn members_json(st: &AppState, ws: &str) -> ApiResult<Vec<Value>> {
    let w = ws.to_string();
    Ok(st
        .db
        .read(move |c| {
            let mut q = c
                .prepare(
                    "SELECT m.user_id, u.email, u.name, m.role, m.invited_by, m.created_at, u.disabled_at IS NOT NULL
                       FROM workspace_members m JOIN users u ON u.id = m.user_id
                      WHERE m.workspace_id=?1
                      ORDER BY CASE m.role WHEN 'owner' THEN 0 WHEN 'admin' THEN 1 WHEN 'member' THEN 2 ELSE 3 END,
                               u.email",
                )
                .map_err(err)?;
            let rows = q
                .query_map([&w], |r| {
                    Ok(json!({
                        "user_id": r.get::<_, String>(0)?, "email": r.get::<_, String>(1)?,
                        "name": r.get::<_, String>(2)?, "role": r.get::<_, String>(3)?,
                        "invited_by": r.get::<_, Option<String>>(4)?, "created_at": r.get::<_, String>(5)?,
                        "disabled": r.get::<_, bool>(6)?,
                    }))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?)
}

/// `GET /workspaces/{id}/members`.
pub async fn members(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let (ws, _) = tenancy::ws_guard(&st, &u, &id, Role::Viewer).await?;
    Ok(Json(json!({ "members": members_json(&st, &ws.id).await? })))
}

#[derive(Deserialize)]
pub struct RoleBody {
    role: String,
}

/// `PATCH /workspaces/{id}/members/{user_id} {role}`.
pub async fn patch_member(
    State(st): State<AppState>,
    u: User,
    Path((id, uid)): Path<(String, String)>,
    JsonBody(b): JsonBody<RoleBody>,
) -> ApiResult<Json<Value>> {
    let (ws, my) = tenancy::ws_guard(&st, &u, &id, Role::Admin).await?;
    let new = Role::parse(&b.role)
        .filter(|r| *r != Role::Owner)
        .ok_or_else(|| {
            ApiError::bad("role must be viewer, member or admin (use transfer for owner)")
        })?;
    let cur = tenancy::member_role(&st, &uid, &ws.id)
        .await?
        .ok_or_else(|| ApiError::not_found("member not found"))?;
    if cur == Role::Owner {
        return Err(ApiError::bad(
            "the owner's role changes only through a transfer",
        ));
    }
    if (cur == Role::Admin || new == Role::Admin) && my < Role::Owner {
        return Err(ApiError::forbidden("only the owner can change admins"));
    }
    let (w, u2, r2) = (ws.id.clone(), uid.clone(), new.as_str().to_string());
    st.db
        .write(move |t| {
            t.execute(
                "UPDATE workspace_members SET role=?3 WHERE workspace_id=?1 AND user_id=?2",
                params![w, u2, r2],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    st.auth_cache.clear();
    audit(
        &st,
        &u.actor(),
        u.via(),
        "workspace.member.role",
        Some(&ws.id),
        json!({"user_id": uid, "role": new.as_str()}),
    )
    .await;
    ev(
        &st,
        &u,
        "member role",
        Some(&ws.id),
        json!({"user_id": uid, "role": new.as_str()}),
    );
    Ok(Json(json!({ "members": members_json(&st, &ws.id).await? })))
}

/// `DELETE /workspaces/{id}/members/{user_id}` (admin, or leaving yourself).
pub async fn remove_member(
    State(st): State<AppState>,
    u: User,
    Path((id, uid)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let need = if uid == u.user_id {
        Role::Viewer
    } else {
        Role::Admin
    };
    let (ws, my) = tenancy::ws_guard(&st, &u, &id, need).await?;
    let cur = tenancy::member_role(&st, &uid, &ws.id)
        .await?
        .ok_or_else(|| ApiError::not_found("member not found"))?;
    if cur == Role::Owner {
        return Err(ApiError::bad(
            "the owner cannot be removed; transfer ownership first",
        ));
    }
    if cur == Role::Admin && uid != u.user_id && my < Role::Owner {
        return Err(ApiError::forbidden("only the owner can remove admins"));
    }
    let (w, u2) = (ws.id.clone(), uid.clone());
    st.db
        .write(move |t| {
            t.execute(
                "DELETE FROM workspace_members WHERE workspace_id=?1 AND user_id=?2",
                params![w, u2],
            )
            .map_err(err)?;
            // the member's tokens for this workspace stop working
            t.execute(
                "UPDATE api_tokens SET revoked_at=?3 WHERE workspace_id=?1 AND user_id=?2 AND revoked_at IS NULL",
                params![w, u2, sf_core::now_rfc3339()],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    st.auth_cache.clear();
    audit(
        &st,
        &u.actor(),
        u.via(),
        "workspace.member.remove",
        Some(&ws.id),
        json!({"user_id": uid}),
    )
    .await;
    ev(
        &st,
        &u,
        "member remove",
        Some(&ws.id),
        json!({"user_id": uid}),
    );
    Ok(Json(json!({ "removed": true })))
}

// ---------------------------------------------------------------------------
// invites
// ---------------------------------------------------------------------------

const INVITE_COLS: &str = "i.id, i.workspace_id, i.email, i.role, i.invited_by, i.created_at, i.expires_at, i.accepted_at, i.revoked_at, w.name";

fn invite_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let exp: String = r.get(6)?;
    let accepted: Option<String> = r.get(7)?;
    let revoked: Option<String> = r.get(8)?;
    let status = if accepted.is_some() {
        "accepted"
    } else if revoked.is_some() {
        "revoked"
    } else if exp < sf_core::now_rfc3339() {
        "expired"
    } else {
        "pending"
    };
    Ok(json!({
        "id": r.get::<_, String>(0)?, "workspace_id": r.get::<_, String>(1)?,
        "email": r.get::<_, String>(2)?, "role": r.get::<_, String>(3)?,
        "invited_by": r.get::<_, Option<String>>(4)?, "created_at": r.get::<_, String>(5)?,
        "expires_at": exp, "accepted_at": accepted, "revoked_at": revoked, "status": status,
        "workspace_name": r.get::<_, String>(9)?,
    }))
}

/// Invites of one workspace (`None` = all, admin console).
pub async fn invites_json(st: &AppState, ws: Option<&str>) -> ApiResult<Vec<Value>> {
    let w = ws.map(str::to_string);
    Ok(st
        .db
        .read(move |c| {
            let mut q = c
                .prepare(&format!(
                    "SELECT {INVITE_COLS} FROM workspace_invites i JOIN workspaces w ON w.id = i.workspace_id
                      WHERE (?1 IS NULL OR i.workspace_id = ?1) ORDER BY i.created_at DESC LIMIT 500"
                ))
                .map_err(err)?;
            let rows = q
                .query_map([&w], invite_row)
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?)
}

/// `GET /workspaces/{id}/invites` (admin).
pub async fn invites(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let (ws, _) = tenancy::ws_guard(&st, &u, &id, Role::Admin).await?;
    Ok(Json(
        json!({ "invites": invites_json(&st, Some(&ws.id)).await? }),
    ))
}

#[derive(Deserialize)]
pub struct InviteBody {
    email: String,
    #[serde(default)]
    role: Option<String>,
}

/// `POST /workspaces/{id}/invites {email, role}` (admin) → 201 `{invite, delivery, link?}`.
pub async fn create_invite(
    State(st): State<AppState>,
    u: User,
    headers: HeaderMap,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<InviteBody>,
) -> ApiResult<Response> {
    let (ws, my) = tenancy::ws_guard(&st, &u, &id, Role::Admin).await?;
    let email = b.email.trim().to_lowercase();
    if !crate::v1::auth::valid_email(&email) {
        return Err(ApiError::bad("invalid email address"));
    }
    let role = Role::parse(b.role.as_deref().unwrap_or("member"))
        .filter(|r| *r != Role::Owner)
        .ok_or_else(|| ApiError::bad("role must be viewer, member or admin"))?;
    if role == Role::Admin && my < Role::Owner {
        return Err(ApiError::forbidden("only the owner can invite admins"));
    }
    if !limiter(&st).allow(&format!("invite:{}", ws.id), 50, Duration::from_secs(3600)) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many invites, try again later",
        ));
    }
    let (w0, e0) = (ws.id.clone(), email.clone());
    let already: bool = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT count(*) > 0 FROM workspace_members m JOIN users u ON u.id = m.user_id
                  WHERE m.workspace_id=?1 AND u.email=?2",
                params![w0, e0],
                |r| r.get(0),
            )
            .map_err(err)
        })
        .await?;
    if already {
        return Err(ApiError::conflict("already a member of this workspace"));
    }
    tenancy::check_quota(&st, Some(&ws.id), "members", 1).await?;
    let token = sf_core::crypto::random_hex(32);
    let hash = sf_core::crypto::sha256_hex(token.as_bytes());
    let inv_id = sf_core::new_id();
    let exp = (chrono::Utc::now() + chrono::Duration::days(INVITE_TTL_DAYS))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let (i2, w2, e2, r2, by) = (
        inv_id.clone(),
        ws.id.clone(),
        email.clone(),
        role.as_str().to_string(),
        u.user_id.clone(),
    );
    st.db
        .write(move |t| {
            // a new invite replaces older pending ones for the same address
            t.execute(
                "UPDATE workspace_invites SET revoked_at=?3 WHERE workspace_id=?1 AND email=?2
                   AND accepted_at IS NULL AND revoked_at IS NULL",
                params![w2, e2, sf_core::now_rfc3339()],
            )
            .map_err(err)?;
            t.execute(
                "INSERT INTO workspace_invites(id, workspace_id, email, role, token_hash, invited_by, expires_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![i2, w2, e2, r2, hash, by, exp],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    let p = crate::platform::get(&st).await;
    let link = mail::link(
        &p,
        mail::request_host(&headers).as_deref(),
        &format!("/invite/{token}"),
    );
    let app = p.branding.app_name.clone();
    let delivery = mail::send(
        &st,
        Mail {
            to: &email,
            subject: &format!("You are invited to {} on {app}", ws.name),
            body: format!(
                "Hello,\n\nyou have been invited to join the workspace \"{}\" on {app} as {}.\n\nAccept the invitation:\n\n{link}\n\nThe link is valid for {INVITE_TTL_DAYS} days.\n",
                ws.name,
                role.as_str()
            ),
            kind: "invite",
            link: Some(link.clone()),
        },
    )
    .await?;
    audit(
        &st,
        &u.actor(),
        u.via(),
        "workspace.invite",
        Some(&ws.id),
        json!({"email": email, "role": role.as_str()}),
    )
    .await;
    ev(
        &st,
        &u,
        "invite create",
        Some(&ws.id),
        json!({"email": email, "role": role.as_str(), "delivery": delivery.as_str()}),
    );
    let inv = invites_json(&st, Some(&ws.id))
        .await?
        .into_iter()
        .find(|v| v["id"] == inv_id.as_str())
        .unwrap_or(Value::Null);
    let mut out = json!({ "invite": inv, "delivery": delivery.as_str() });
    if delivery == mail::Delivery::Manual
        && let Some(o) = out.as_object_mut()
    {
        o.insert("link".into(), json!(link));
    }
    Ok((StatusCode::CREATED, Json(out)).into_response())
}

/// `DELETE /workspaces/{id}/invites/{invite_id}` (admin).
pub async fn revoke_invite(
    State(st): State<AppState>,
    u: User,
    Path((id, inv)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let (ws, _) = tenancy::ws_guard(&st, &u, &id, Role::Admin).await?;
    let (w, i) = (ws.id.clone(), inv.clone());
    let n = st
        .db
        .write(move |t| {
            t.execute(
                "UPDATE workspace_invites SET revoked_at=?3 WHERE id=?1 AND workspace_id=?2
                   AND accepted_at IS NULL AND revoked_at IS NULL",
                params![i, w, sf_core::now_rfc3339()],
            )
            .map_err(err)
        })
        .await?;
    if n == 0 {
        return Err(ApiError::not_found("invite not found"));
    }
    audit(
        &st,
        &u.actor(),
        u.via(),
        "workspace.invite.revoke",
        Some(&ws.id),
        json!({"invite_id": inv}),
    )
    .await;
    Ok(Json(json!({ "revoked": true })))
}

async fn invite_by_token(
    st: &AppState,
    token: &str,
) -> ApiResult<Option<(Value, String, String, String)>> {
    let token = token.trim();
    if token.is_empty() || token.len() > 200 {
        return Ok(None);
    }
    let hash = sf_core::crypto::sha256_hex(token.as_bytes());
    Ok(st
        .db
        .read(move |c| {
            c.query_row(
                &format!(
                    "SELECT {INVITE_COLS} FROM workspace_invites i JOIN workspaces w ON w.id = i.workspace_id
                      WHERE i.token_hash=?1 AND w.deleted_at IS NULL"
                ),
                [&hash],
                |r| {
                    Ok((
                        invite_row(r)?,
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(err)
        })
        .await?)
}

/// `GET /invites/{token}` (public).
pub async fn invite_info(
    State(st): State<AppState>,
    crate::v1::common::ClientIp(ip): crate::v1::common::ClientIp,
    Path(token): Path<String>,
) -> ApiResult<Json<Value>> {
    if !limiter(&st).allow(&format!("invite_info:{ip}"), 30, Duration::from_secs(60)) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts",
        ));
    }
    let Some((inv, ..)) = invite_by_token(&st, &token).await? else {
        return Ok(Json(json!({ "valid": false })));
    };
    let valid = inv["status"] == "pending";
    Ok(Json(json!({
        "valid": valid, "email": inv["email"], "role": inv["role"],
        "workspace_name": inv["workspace_name"], "expires_at": inv["expires_at"], "status": inv["status"],
    })))
}

/// `POST /invites/{token}/accept` (signed-in user with the invited email, or a platform admin).
pub async fn accept_invite(
    State(st): State<AppState>,
    u: User,
    Path(token): Path<String>,
) -> ApiResult<Json<Value>> {
    if u.token.is_some() {
        return Err(ApiError::forbidden(
            "accept invitations from a signed-in session",
        ));
    }
    let Some((inv, inv_id, ws_id, role)) = invite_by_token(&st, &token).await? else {
        return Err(ApiError::bad("invalid or expired invitation"));
    };
    if inv["status"] != "pending" {
        return Err(ApiError::bad("invalid or expired invitation"));
    }
    let uid = u.user_id.clone();
    let email: String = st
        .db
        .read(move |c| {
            c.query_row("SELECT email FROM users WHERE id=?1", [&uid], |r| r.get(0))
                .map_err(err)
        })
        .await?;
    if inv["email"].as_str() != Some(email.as_str()) && !u.is_admin() {
        return Err(ApiError::forbidden(
            "this invitation is for another email address",
        ));
    }
    let (w, u2, r2, i2) = (
        ws_id.clone(),
        u.user_id.clone(),
        role.clone(),
        inv_id.clone(),
    );
    st.db
        .write(move |t| {
            let now = sf_core::now_rfc3339();
            t.execute(
                "INSERT INTO workspace_members(workspace_id, user_id, role, invited_by)
                 SELECT ?1, ?2, ?3, invited_by FROM workspace_invites WHERE id=?4
                 ON CONFLICT(workspace_id, user_id) DO NOTHING",
                params![w, u2, r2, i2],
            )
            .map_err(err)?;
            t.execute(
                "UPDATE workspace_invites SET accepted_at=?2, accepted_by=?3 WHERE id=?1",
                params![i2, now, u2],
            )
            .map_err(err)?;
            // the invite link proves the address
            t.execute(
                "UPDATE users SET email_verified_at=coalesce(email_verified_at, ?2) WHERE id=?1",
                params![u2, now],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    st.auth_cache.clear();
    ev(
        &st,
        &u,
        "invite accept",
        Some(&ws_id),
        json!({"role": role}),
    );
    let ws = tenancy::load_ws(&st, &ws_id)
        .await?
        .ok_or_else(|| ApiError::not_found("workspace not found"))?;
    Ok(Json(
        json!({ "workspace": ws_json(&st, &ws, Some(&role)).await? }),
    ))
}

// ---------------------------------------------------------------------------
// usage, plans, billing
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct UsageQ {
    history: Option<String>,
}

/// Usage document for one workspace.
pub async fn usage_doc(st: &AppState, ws: &Ws, history: bool) -> ApiResult<Value> {
    let plan = tenancy::plan_of_ws(st, &ws.id).await?;
    let usage = tenancy::usage(st, &ws.id).await?;
    let month: String = sf_core::now_rfc3339().chars().take(7).collect();
    let mut v = json!({
        "workspace_id": ws.id, "month": month,
        "plan": plan.as_ref().map(|p| json!({"id": p.id, "name": p.name, "price_label": p.price_label})),
        "limits": plan.map_or(json!({}), |p| p.limits),
        "usage": usage,
    });
    if history {
        let id = ws.id.clone();
        let rows = st
            .db
            .read(move |c| {
                let mut q = c
                    .prepare(
                        "SELECT month, sites, posts, link_runs, ai_tokens, members FROM usage_monthly
                          WHERE workspace_id=?1 ORDER BY month DESC LIMIT 24",
                    )
                    .map_err(err)?;
                let rows = q
                    .query_map([&id], |r| {
                        Ok(json!({
                            "month": r.get::<_, String>(0)?, "sites": r.get::<_, i64>(1)?,
                            "posts": r.get::<_, i64>(2)?, "link_runs": r.get::<_, i64>(3)?,
                            "ai_tokens": r.get::<_, i64>(4)?, "members": r.get::<_, i64>(5)?,
                        }))
                    })
                    .map_err(err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(err)?;
                Ok(rows)
            })
            .await?;
        if let Some(o) = v.as_object_mut() {
            o.insert("history".into(), Value::Array(rows));
        }
    }
    Ok(v)
}

/// `GET /workspaces/{id}/usage[?history=1]`.
pub async fn usage(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
    Query(q): Query<UsageQ>,
) -> ApiResult<Json<Value>> {
    let (ws, _) = tenancy::ws_guard(&st, &u, &id, Role::Viewer).await?;
    Ok(Json(
        usage_doc(&st, &ws, truthy(q.history.as_deref())).await?,
    ))
}

pub async fn plans_json(st: &AppState, all: bool) -> ApiResult<Vec<Value>> {
    Ok(st
        .db
        .read(move |c| {
            let mut q = c
                .prepare(&format!(
                    "SELECT {}, (SELECT count(*) FROM workspaces w WHERE w.plan_id = plans.id AND w.deleted_at IS NULL)
                       FROM plans WHERE (?1 OR public = 1) ORDER BY sort, name",
                    tenancy::PLAN_COLS
                ))
                .map_err(err)?;
            let rows = q
                .query_map([all], |r| {
                    let mut v = tenancy::plan_from_row(r)?.to_json();
                    if all && let Some(o) = v.as_object_mut() {
                        o.insert("workspaces".into(), json!(r.get::<_, i64>(6)?));
                    }
                    Ok(v)
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?)
}

/// `GET /plans` (public plans; platform admins: all + workspace counts).
pub async fn plans(State(st): State<AppState>, u: User) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "plans": plans_json(&st, u.is_admin()).await? }),
    ))
}

#[derive(Deserialize)]
pub struct BillingQ {
    workspace_id: Option<String>,
}

/// `GET /billing?workspace_id=`: manual billing (platform admin assigns plans).
pub async fn billing(
    State(st): State<AppState>,
    u: User,
    Query(q): Query<BillingQ>,
) -> ApiResult<Json<Value>> {
    let ws = match q.workspace_id.as_deref().filter(|s| !s.is_empty()) {
        Some(w) => Some(tenancy::ws_guard(&st, &u, w, Role::Viewer).await?.0),
        None => tenancy::target_ws(&st, &u).await?,
    };
    let p = crate::platform::get(&st).await;
    let plan = match &ws {
        Some(w) => tenancy::plan_of_ws(&st, &w.id).await?,
        None => None,
    };
    let contact = if p.branding.support_email.is_empty() {
        Value::Null
    } else {
        json!(p.branding.support_email)
    };
    Ok(Json(json!({
        "provider": p.billing.provider,
        "workspace_id": ws.map(|w| w.id),
        "plan": plan.as_ref().map(|p| json!({"id": p.id, "name": p.name, "price_label": p.price_label})),
        "limits": plan.map_or(json!({}), |p| p.limits),
        "upgrade": {
            "contact": contact,
            "note": "Plans are assigned by the platform admin. Contact support to upgrade.",
        },
    })))
}

/// `POST /billing/webhook`: documented stub (501 until a payment provider is implemented).
pub async fn billing_webhook() -> ApiError {
    ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        "billing provider not configured",
    )
}
