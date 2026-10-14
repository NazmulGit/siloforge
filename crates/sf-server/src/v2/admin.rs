//! v2.4 admin console API (docs/specs/v2.4-saas.md §4, §9.5): platform admins only (API tokens
//! need `*`/`admin`, enforced by the [`Admin`] extractor).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sf_store::err;

use crate::auth::{Admin, User, audit};
use crate::mail::{self, Mail};
use crate::tenancy::{self, Role};
use crate::v1::common::JsonBody;
use crate::{ApiError, ApiResult, AppState};

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

fn clamp(v: Option<i64>, d: i64, max: i64) -> i64 {
    v.unwrap_or(d).clamp(1, max)
}

/// `LIKE` pattern with escaped wildcards.
fn like(q: &str) -> String {
    let esc: String = q
        .trim()
        .chars()
        .take(200)
        .flat_map(|c| match c {
            '%' | '_' | '\\' => vec!['\\', c],
            c => vec![c],
        })
        .collect();
    format!("%{esc}%")
}

// ---------------------------------------------------------------------------
// overview
// ---------------------------------------------------------------------------

/// `GET /admin/overview`.
pub async fn overview(State(st): State<AppState>, _a: Admin) -> ApiResult<Json<Value>> {
    let day = format!(
        "{}T00:00:00.000Z",
        sf_core::now_rfc3339().chars().take(10).collect::<String>()
    );
    let month = tenancy::month_start();
    let mut v = st
        .db
        .read(move |c| {
            let n = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> sf_core::Result<i64> {
                c.query_row(sql, p, |r| r.get(0)).map_err(err)
            };
            Ok(json!({
                "users": {
                    "total": n("SELECT count(*) FROM users", &[])?,
                    "admins": n("SELECT count(*) FROM users WHERE role='admin'", &[])?,
                    "disabled": n("SELECT count(*) FROM users WHERE disabled_at IS NOT NULL", &[])?,
                    "unverified": n("SELECT count(*) FROM users WHERE email_verified_at IS NULL", &[])?,
                    "with_2fa": n("SELECT count(*) FROM users WHERE totp_secret_enc IS NOT NULL AND totp_secret_enc <> ''", &[])?,
                },
                "workspaces": {
                    "total": n("SELECT count(*) FROM workspaces WHERE deleted_at IS NULL", &[])?,
                    "suspended": n("SELECT count(*) FROM workspaces WHERE status='suspended' AND deleted_at IS NULL", &[])?,
                    "deleted": n("SELECT count(*) FROM workspaces WHERE deleted_at IS NOT NULL", &[])?,
                },
                "sites": n("SELECT count(*) FROM sites", &[])?,
                "posts": n("SELECT count(*) FROM posts", &[])?,
                "runs_today": n("SELECT count(*) FROM link_runs WHERE created_at >= ?1", &[&day])?,
                "jobs_today": n("SELECT count(*) FROM job_runs WHERE created_at >= ?1", &[&day])?,
                "ai_tokens_month": n(
                    "SELECT coalesce(sum(coalesce(tokens_in,0)+coalesce(tokens_out,0)),0) FROM model_call_log
                      WHERE cache_hit=0 AND called_at >= ?1", &[&month])?,
                "errors_open": n("SELECT count(*) FROM errors WHERE resolved_at IS NULL", &[])?,
                "mail": {
                    "outbox_pending": n("SELECT count(*) FROM mail_outbox WHERE sent_at IS NULL", &[])?,
                },
            }))
        })
        .await?;
    let p = crate::platform::get(&st).await;
    let (running, queued) = st.jobs.counts().await.unwrap_or((0, 0));
    if let Some(o) = v.as_object_mut() {
        if let Some(m) = o.get_mut("mail").and_then(Value::as_object_mut) {
            m.insert("configured".into(), json!(p.smtp.configured()));
        }
        o.insert(
            "registration_mode".into(),
            json!(p.mode(st.cfg.auth.allow_registration)),
        );
        o.insert(
            "health".into(),
            json!({
                "version": sf_core::VERSION,
                "uptime_secs": st.started.elapsed().as_secs(),
                "jobs": {"running": running, "queued": queued},
            }),
        );
    }
    Ok(Json(v))
}

// ---------------------------------------------------------------------------
// users
// ---------------------------------------------------------------------------

const USER_SQL: &str = "SELECT u.id, u.email, u.name, u.role, u.created_at, u.email_verified_at, u.disabled_at,
        u.totp_secret_enc IS NOT NULL AND u.totp_secret_enc <> '', u.last_login_at, u.pending_email,
        (SELECT count(*) FROM sessions s WHERE s.user_id = u.id AND s.revoked_at IS NULL AND s.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now'))
   FROM users u";

fn user_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, String>(0)?, "email": r.get::<_, String>(1)?, "name": r.get::<_, String>(2)?,
        "role": r.get::<_, String>(3)?, "created_at": r.get::<_, String>(4)?,
        "email_verified_at": r.get::<_, Option<String>>(5)?, "disabled_at": r.get::<_, Option<String>>(6)?,
        "two_factor_enabled": r.get::<_, bool>(7)?, "last_login_at": r.get::<_, Option<String>>(8)?,
        "pending_email": r.get::<_, Option<String>>(9)?, "sessions_active": r.get::<_, i64>(10)?,
    }))
}

fn user_workspaces(c: &rusqlite::Connection, uid: &str) -> sf_core::Result<Vec<Value>> {
    let mut q = c
        .prepare(
            "SELECT w.id, w.name, m.role FROM workspace_members m JOIN workspaces w ON w.id = m.workspace_id
              WHERE m.user_id=?1 AND w.deleted_at IS NULL ORDER BY w.is_default DESC, w.name",
        )
        .map_err(err)?;
    let rows = q
        .query_map([uid], |r| {
            Ok(json!({"id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?, "role": r.get::<_, String>(2)?}))
        })
        .map_err(err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(err)?;
    Ok(rows)
}

#[derive(Deserialize)]
pub struct ListQ {
    q: Option<String>,
    status: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// `GET /admin/users?q=&limit=&offset=`.
pub async fn users(
    State(st): State<AppState>,
    _a: Admin,
    Query(q): Query<ListQ>,
) -> ApiResult<Json<Value>> {
    let limit = clamp(q.limit, 50, 500);
    let offset = q.offset.unwrap_or(0).clamp(0, 1_000_000);
    let pat = q.q.as_deref().filter(|s| !s.trim().is_empty()).map(like);
    let v = st
        .db
        .read(move |c| {
            let wh = "WHERE (?1 IS NULL OR u.email LIKE ?1 ESCAPE '\\' OR u.name LIKE ?1 ESCAPE '\\' OR u.id = ?2)";
            let total: i64 = c
                .query_row(
                    &format!("SELECT count(*) FROM users u {wh}"),
                    params![pat, q.q.as_deref().map(str::trim)],
                    |r| r.get(0),
                )
                .map_err(err)?;
            let mut stmt = c
                .prepare(&format!(
                    "{USER_SQL} {wh} ORDER BY u.created_at, u.id LIMIT ?3 OFFSET ?4"
                ))
                .map_err(err)?;
            let rows = stmt
                .query_map(
                    params![pat, q.q.as_deref().map(str::trim), limit, offset],
                    user_row,
                )
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            let mut out = Vec::with_capacity(rows.len());
            for mut r in rows {
                let id = r["id"].as_str().unwrap_or("").to_string();
                if let Some(o) = r.as_object_mut() {
                    o.insert("workspaces".into(), Value::Array(user_workspaces(c, &id)?));
                }
                out.push(r);
            }
            Ok(json!({ "users": out, "total": total }))
        })
        .await?;
    Ok(Json(v))
}

async fn user_doc(st: &AppState, id: &str) -> ApiResult<Value> {
    let id = id.to_string();
    st.db
        .read(move |c| {
            let row = c
                .query_row(
                    &format!("{USER_SQL} WHERE u.id = ?1"),
                    params![id],
                    user_row,
                )
                .optional()
                .map_err(err)?;
            let Some(mut row) = row else {
                return Ok(None);
            };
            if let Some(o) = row.as_object_mut() {
                o.insert("workspaces".into(), Value::Array(user_workspaces(c, &id)?));
            }
            Ok(Some(row))
        })
        .await?
        .ok_or_else(|| ApiError::not_found("user not found"))
}

/// `GET /admin/users/{id}`.
pub async fn user_get(
    State(st): State<AppState>,
    _a: Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(user_doc(&st, &id).await?))
}

#[derive(Deserialize)]
pub struct CreateUser {
    email: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    workspace_role: Option<String>,
}

/// `POST /admin/users` → 201 `{user, delivery?, link?}`.
pub async fn user_create(
    State(st): State<AppState>,
    Admin(a): Admin,
    headers: HeaderMap,
    JsonBody(b): JsonBody<CreateUser>,
) -> ApiResult<Response> {
    let email = b.email.trim().to_lowercase();
    if !crate::v1::auth::valid_email(&email) {
        return Err(ApiError::bad("invalid email address"));
    }
    let role = b.role.as_deref().unwrap_or("user");
    if !["user", "admin"].contains(&role) {
        return Err(ApiError::bad("role must be user or admin"));
    }
    let ws_role = Role::parse(b.workspace_role.as_deref().unwrap_or("member"))
        .ok_or_else(|| ApiError::bad("workspace_role must be viewer, member, admin or owner"))?;
    if let Some(w) = &b.workspace_id {
        tenancy::load_ws(&st, w)
            .await?
            .filter(|w| w.deleted_at.is_none())
            .ok_or_else(|| ApiError::bad("unknown workspace"))?;
    }
    let pw = match b.password.as_deref() {
        Some(p) if !p.is_empty() => {
            let n = p.chars().count();
            if !(8..=crate::v1::auth::MAX_PASSWORD).contains(&n) {
                return Err(ApiError::bad("password must be at least 8 characters"));
            }
            p.to_string()
        }
        // unusable random password until the set-password link is used
        _ => sf_core::crypto::random_hex(24),
    };
    let set_link = b.password.as_deref().is_none_or(str::is_empty);
    let hash = crate::v1::auth::bcrypt_hash(pw).await?;
    let id = sf_core::new_id();
    let name: String = b
        .name
        .unwrap_or_default()
        .trim()
        .chars()
        .take(200)
        .collect();
    let plan = crate::platform::get(&st).await.registration.default_plan_id;
    let (i2, e2, n2, r2, w2, wr) = (
        id.clone(),
        email.clone(),
        name.clone(),
        role.to_string(),
        b.workspace_id.clone(),
        ws_role.as_str().to_string(),
    );
    let by = a.user_id.clone();
    st.db
        .write(move |t| {
            t.execute(
                "INSERT INTO users(id, email, password_hash, name, role, email_verified_at) VALUES(?1,?2,?3,?4,?5,?6)",
                params![i2, e2, hash, n2, r2, sf_core::now_rfc3339()],
            )
            .map_err(err)?;
            match w2 {
                Some(w) => {
                    if wr == "owner" {
                        crate::v2::workspaces::transfer_sync(t, &w, &i2)?;
                    } else {
                        t.execute(
                            "INSERT INTO workspace_members(workspace_id, user_id, role, invited_by) VALUES(?1,?2,?3,?4)",
                            params![w, i2, wr, by],
                        )
                        .map_err(err)?;
                    }
                }
                None if r2 != "admin" => {
                    let label = if n2.is_empty() {
                        format!("{}'s workspace", e2.split('@').next().unwrap_or("My"))
                    } else {
                        format!("{n2}'s workspace")
                    };
                    let plan: String = t
                        .query_row(
                            "SELECT id FROM plans WHERE id=?1 UNION ALL SELECT 'free' LIMIT 1",
                            [&plan],
                            |r| r.get(0),
                        )
                        .map_err(err)?;
                    tenancy::create_ws_sync(t, &label, None, &plan, &i2)?;
                }
                None => {}
            }
            Ok(())
        })
        .await
        .map_err(|e| crate::v1::common::conflict_as(e, "email already registered"))?;
    let mut out = json!({ "user": user_doc(&st, &id).await? });
    if set_link {
        let (d, link) = super::auth2::send_reset(&st, &headers, &id, &email).await?;
        if let Some(o) = out.as_object_mut() {
            o.insert("delivery".into(), json!(d.as_str()));
            if d == mail::Delivery::Manual {
                o.insert("link".into(), json!(link));
            }
        }
    }
    audit(
        &st,
        &a.actor(),
        a.via(),
        "admin.user.create",
        Some(&id),
        json!({"email": email, "role": role}),
    )
    .await;
    ev(
        &st,
        &a,
        "admin user create",
        Some(&id),
        json!({"email": email, "role": role}),
    );
    Ok((StatusCode::CREATED, Json(out)).into_response())
}

#[derive(Deserialize)]
pub struct PatchUser {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    role: Option<String>,
}

async fn admins_left(st: &AppState, except: &str) -> ApiResult<i64> {
    let e = except.to_string();
    Ok(st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT count(*) FROM users WHERE role='admin' AND disabled_at IS NULL AND id<>?1",
                [&e],
                |r| r.get(0),
            )
            .map_err(err)
        })
        .await?)
}

/// `PATCH /admin/users/{id} {name?, email?, role?}`.
pub async fn user_patch(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<PatchUser>,
) -> ApiResult<Json<Value>> {
    let cur = user_doc(&st, &id).await?;
    if let Some(r) = &b.role {
        if !["user", "admin"].contains(&r.as_str()) {
            return Err(ApiError::bad("role must be user or admin"));
        }
        if r == "user" && cur["role"] == "admin" && admins_left(&st, &id).await? == 0 {
            return Err(ApiError::bad("cannot demote the last platform admin"));
        }
    }
    let email = match &b.email {
        Some(e) => {
            let e = e.trim().to_lowercase();
            if !crate::v1::auth::valid_email(&e) {
                return Err(ApiError::bad("invalid email address"));
            }
            Some(e)
        }
        None => None,
    };
    let name = b
        .name
        .map(|n| n.trim().chars().take(200).collect::<String>());
    let (i2, n2, e2, r2) = (id.clone(), name.clone(), email.clone(), b.role.clone());
    st.db
        .write(move |t| {
            if let Some(n) = n2 {
                t.execute("UPDATE users SET name=?2 WHERE id=?1", params![i2, n]).map_err(err)?;
            }
            if let Some(e) = e2 {
                t.execute(
                    "UPDATE users SET email=?2, email_verified_at=coalesce(email_verified_at, ?3), pending_email=NULL WHERE id=?1",
                    params![i2, e, sf_core::now_rfc3339()],
                )
                .map_err(err)?;
            }
            if let Some(r) = r2 {
                t.execute(
                    "UPDATE users SET role=?2, token_version = token_version + (role <> ?2) WHERE id=?1",
                    params![i2, r],
                )
                .map_err(err)?;
            }
            Ok(())
        })
        .await
        .map_err(|e| crate::v1::common::conflict_as(e, "email already registered"))?;
    st.auth_cache.clear();
    audit(
        &st,
        &a.actor(),
        a.via(),
        "admin.user.update",
        Some(&id),
        json!({"name": name, "email": email, "role": b.role}),
    )
    .await;
    ev(
        &st,
        &a,
        "admin user update",
        Some(&id),
        json!({"role": b.role, "email_changed": email.is_some()}),
    );
    Ok(Json(user_doc(&st, &id).await?))
}

/// `POST /admin/users/{id}/{action}`: reset-password | logout | disable | enable | verify-email | reset-2fa.
pub async fn user_action(
    State(st): State<AppState>,
    Admin(a): Admin,
    headers: HeaderMap,
    Path((id, action)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let cur = user_doc(&st, &id).await?;
    let email = cur["email"].as_str().unwrap_or("").to_string();
    let mut out = json!({ "ok": true });
    match action.as_str() {
        "reset-password" => {
            let (d, link) = super::auth2::send_reset(&st, &headers, &id, &email).await?;
            out = json!({ "ok": true, "delivery": d.as_str() });
            if d == mail::Delivery::Manual
                && let Some(o) = out.as_object_mut()
            {
                o.insert("link".into(), json!(link));
            }
        }
        "logout" | "disable" => {
            if action == "disable" {
                if id == a.user_id {
                    return Err(ApiError::bad("you cannot disable yourself"));
                }
                if cur["role"] == "admin" && admins_left(&st, &id).await? == 0 {
                    return Err(ApiError::bad("cannot disable the last platform admin"));
                }
            }
            let (i2, dis) = (id.clone(), action == "disable");
            st.db
                .write(move |t| {
                    t.execute(
                        "UPDATE users SET token_version=token_version+1 WHERE id=?1",
                        [&i2],
                    )
                    .map_err(err)?;
                    crate::sessions::revoke_all_sync(t, &i2)?;
                    if dis {
                        t.execute(
                            "UPDATE users SET disabled_at=?2 WHERE id=?1 AND disabled_at IS NULL",
                            params![i2, sf_core::now_rfc3339()],
                        )
                        .map_err(err)?;
                    }
                    Ok(())
                })
                .await?;
        }
        "enable" => {
            let i2 = id.clone();
            st.db
                .write(move |t| {
                    t.execute("UPDATE users SET disabled_at=NULL WHERE id=?1", [&i2])
                        .map_err(err)?;
                    Ok(())
                })
                .await?;
        }
        "verify-email" => {
            let i2 = id.clone();
            st.db
                .write(move |t| {
                    t.execute(
                        "UPDATE users SET email_verified_at=coalesce(email_verified_at, ?2) WHERE id=?1",
                        params![i2, sf_core::now_rfc3339()],
                    )
                    .map_err(err)?;
                    Ok(())
                })
                .await?;
        }
        "reset-2fa" => super::auth2::clear_2fa(&st, &id).await?,
        _ => return Err(ApiError::not_found("route not found")),
    }
    st.auth_cache.clear();
    audit(
        &st,
        &a.actor(),
        a.via(),
        &format!("admin.user.{action}"),
        Some(&id),
        json!({}),
    )
    .await;
    ev(
        &st,
        &a,
        &format!("admin user {action}"),
        Some(&id),
        json!({}),
    );
    if let Some(o) = out.as_object_mut() {
        o.insert("user".into(), user_doc(&st, &id).await?);
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct DeleteUserQ {
    transfer_to: Option<String>,
}

/// `DELETE /admin/users/{id}?transfer_to=`: owned workspaces and created sites move to
/// `transfer_to` (default: the acting admin), then the user row is deleted.
pub async fn user_delete(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
    Query(q): Query<DeleteUserQ>,
) -> ApiResult<Json<Value>> {
    let cur = user_doc(&st, &id).await?;
    if id == a.user_id {
        return Err(ApiError::bad("you cannot delete yourself"));
    }
    if cur["role"] == "admin" && admins_left(&st, &id).await? == 0 {
        return Err(ApiError::bad("cannot delete the last platform admin"));
    }
    let to = q
        .transfer_to
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| a.user_id.clone());
    if to == id {
        return Err(ApiError::bad("transfer_to must be another user"));
    }
    user_doc(&st, &to)
        .await
        .map_err(|_| ApiError::bad("transfer_to: unknown user"))?;
    let (i2, t2) = (id.clone(), to.clone());
    let moved = st
        .db
        .write(move |t| {
            let owned: Vec<String> = {
                let mut q = t
                    .prepare("SELECT id FROM workspaces WHERE owner_user_id=?1")
                    .map_err(err)?;
                q.query_map([&i2], |r| r.get(0))
                    .map_err(err)?
                    .collect::<rusqlite::Result<Vec<String>>>()
                    .map_err(err)?
            };
            for w in &owned {
                crate::v2::workspaces::transfer_sync(t, w, &t2)?;
            }
            // sites.user_id cascades on delete: keep the sites with the new owner
            t.execute(
                "UPDATE sites SET user_id=?2 WHERE user_id=?1",
                params![i2, t2],
            )
            .map_err(err)?;
            t.execute("DELETE FROM users WHERE id=?1", [&i2])
                .map_err(err)?;
            Ok(owned.len())
        })
        .await?;
    st.auth_cache.clear();
    audit(
        &st,
        &a.actor(),
        a.via(),
        "admin.user.delete",
        Some(&id),
        json!({"transfer_to": to, "workspaces_moved": moved}),
    )
    .await;
    ev(
        &st,
        &a,
        "admin user delete",
        Some(&id),
        json!({"transfer_to": to}),
    );
    Ok(Json(
        json!({ "deleted": true, "transfer_to": to, "workspaces_moved": moved }),
    ))
}

// ---------------------------------------------------------------------------
// workspaces
// ---------------------------------------------------------------------------

async fn ws_row_doc(st: &AppState, ws: &tenancy::Ws) -> ApiResult<Value> {
    let mut v = super::workspaces::detail(st, ws, Role::Owner).await?;
    let (oid, wid) = (ws.owner_user_id.clone(), ws.id.clone());
    let (owner, members): (Value, i64) = st
        .db
        .read(move |c| {
            let owner = match oid {
                Some(o) => c
                    .query_row("SELECT id, email, name FROM users WHERE id=?1", [&o], |r| {
                        Ok(json!({"id": r.get::<_, String>(0)?, "email": r.get::<_, String>(1)?, "name": r.get::<_, String>(2)?}))
                    })
                    .optional()
                    .map_err(err)?
                    .unwrap_or(Value::Null),
                None => Value::Null,
            };
            let m: i64 = c
                .query_row("SELECT count(*) FROM workspace_members WHERE workspace_id=?1", [&wid], |r| r.get(0))
                .map_err(err)?;
            Ok((owner, m))
        })
        .await?;
    if let Some(o) = v.as_object_mut() {
        o.remove("my_role");
        o.remove("role");
        o.insert("owner".into(), owner);
        o.insert("members".into(), json!(members));
    }
    Ok(v)
}

/// `GET /admin/workspaces?q=&status=&limit=&offset=`.
pub async fn workspaces(
    State(st): State<AppState>,
    _a: Admin,
    Query(q): Query<ListQ>,
) -> ApiResult<Json<Value>> {
    let limit = clamp(q.limit, 50, 500);
    let offset = q.offset.unwrap_or(0).clamp(0, 1_000_000);
    let pat = q.q.as_deref().filter(|s| !s.trim().is_empty()).map(like);
    let status = q.status.clone().filter(|s| !s.is_empty());
    let (rows, total) = st
        .db
        .read(move |c| {
            let wh = "WHERE (?1 IS NULL OR w.name LIKE ?1 ESCAPE '\\' OR w.slug LIKE ?1 ESCAPE '\\' OR w.id = ?2
                  OR w.owner_user_id IN (SELECT id FROM users WHERE email LIKE ?1 ESCAPE '\\'))
                AND (CASE ?3 WHEN 'deleted' THEN w.deleted_at IS NOT NULL
                             WHEN 'active' THEN w.deleted_at IS NULL AND w.status='active'
                             WHEN 'suspended' THEN w.deleted_at IS NULL AND w.status='suspended'
                             ELSE w.deleted_at IS NULL END)";
            let qq = q.q.as_deref().map(str::trim);
            let total: i64 = c
                .query_row(
                    &format!("SELECT count(*) FROM workspaces w {wh}"),
                    params![pat, qq, status],
                    |r| r.get(0),
                )
                .map_err(err)?;
            let cols = tenancy::WS_COLS
                .split(", ")
                .map(|x| format!("w.{x}"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut stmt = c
                .prepare(&format!(
                    "SELECT {cols} FROM workspaces w {wh} ORDER BY w.is_default DESC, w.created_at, w.id LIMIT ?4 OFFSET ?5"
                ))
                .map_err(err)?;
            let rows = stmt
                .query_map(params![pat, qq, status, limit, offset], tenancy::Ws::from_row)
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok((rows, total))
        })
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for ws in &rows {
        out.push(ws_row_doc(&st, ws).await?);
    }
    Ok(Json(json!({ "workspaces": out, "total": total })))
}

async fn load_any(st: &AppState, id: &str) -> ApiResult<tenancy::Ws> {
    tenancy::load_ws(st, id)
        .await?
        .ok_or_else(|| ApiError::not_found("workspace not found"))
}

/// `GET /admin/workspaces/{id}` (+ members, sites).
pub async fn workspace_get(
    State(st): State<AppState>,
    _a: Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ws = load_any(&st, &id).await?;
    let mut v = ws_row_doc(&st, &ws).await?;
    let members = super::workspaces::members_json(&st, &ws.id).await?;
    let wid = ws.id.clone();
    let sites = st
        .db
        .read(move |c| {
            let mut q = c
                .prepare(
                    "SELECT s.id, s.domain, s.created_at, s.last_sync_at, (SELECT count(*) FROM posts p WHERE p.site_id = s.id)
                       FROM sites s WHERE s.workspace_id=?1 ORDER BY s.created_at, s.id",
                )
                .map_err(err)?;
            let rows = q
                .query_map([&wid], |r| {
                    Ok(json!({"id": r.get::<_, String>(0)?, "domain": r.get::<_, String>(1)?,
                              "created_at": r.get::<_, String>(2)?, "last_sync_at": r.get::<_, Option<String>>(3)?,
                              "posts": r.get::<_, i64>(4)?}))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?;
    if let Some(o) = v.as_object_mut() {
        o.insert("members".into(), Value::Array(members));
        o.insert("site_list".into(), Value::Array(sites));
    }
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct PatchWs {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    plan_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

/// `PATCH /admin/workspaces/{id} {name?, plan_id?, status?}`.
pub async fn workspace_patch(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<PatchWs>,
) -> ApiResult<Json<Value>> {
    let ws = load_any(&st, &id).await?;
    if let Some(s) = &b.status
        && !["active", "suspended"].contains(&s.as_str())
    {
        return Err(ApiError::bad("status must be active or suspended"));
    }
    if let Some(n) = &b.name
        && (n.trim().is_empty() || n.chars().count() > 120)
    {
        return Err(ApiError::bad("name is required (max 120 chars)"));
    }
    let (w, n, p, s) = (
        ws.id.clone(),
        b.name.clone(),
        b.plan_id.clone(),
        b.status.clone(),
    );
    st.db
        .write(move |t| {
            if let Some(p) = p {
                let ok: bool = t
                    .query_row("SELECT count(*) > 0 FROM plans WHERE id=?1", [&p], |r| {
                        r.get(0)
                    })
                    .map_err(err)?;
                if !ok {
                    return Err(sf_core::Error::BadRequest("unknown plan".into()));
                }
                t.execute(
                    "UPDATE workspaces SET plan_id=?2 WHERE id=?1",
                    params![w, p],
                )
                .map_err(err)?;
            }
            if let Some(n) = n {
                t.execute(
                    "UPDATE workspaces SET name=?2 WHERE id=?1",
                    params![w, n.trim()],
                )
                .map_err(err)?;
            }
            if let Some(s) = s {
                t.execute("UPDATE workspaces SET status=?2 WHERE id=?1", params![w, s])
                    .map_err(err)?;
            }
            Ok(())
        })
        .await?;
    audit(
        &st,
        &a.actor(),
        a.via(),
        "admin.workspace.update",
        Some(&ws.id),
        json!({"plan_id": b.plan_id, "status": b.status, "name": b.name}),
    )
    .await;
    ev(
        &st,
        &a,
        "admin workspace update",
        Some(&ws.id),
        json!({"plan_id": b.plan_id, "status": b.status}),
    );
    let ws = load_any(&st, &id).await?;
    Ok(Json(ws_row_doc(&st, &ws).await?))
}

/// `POST /admin/workspaces/{id}/{suspend|resume|restore}`.
pub async fn workspace_action(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path((id, action)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let ws = load_any(&st, &id).await?;
    let sql = match action.as_str() {
        "suspend" => "UPDATE workspaces SET status='suspended' WHERE id=?1",
        "resume" => "UPDATE workspaces SET status='active' WHERE id=?1",
        "restore" => "UPDATE workspaces SET deleted_at=NULL WHERE id=?1",
        _ => return Err(ApiError::not_found("route not found")),
    };
    let w = ws.id.clone();
    st.db
        .write(move |t| {
            t.execute(sql, [&w]).map_err(err)?;
            Ok(())
        })
        .await?;
    st.auth_cache.clear();
    audit(
        &st,
        &a.actor(),
        a.via(),
        &format!("admin.workspace.{action}"),
        Some(&ws.id),
        json!({}),
    )
    .await;
    ev(
        &st,
        &a,
        &format!("admin workspace {action}"),
        Some(&ws.id),
        json!({}),
    );
    let ws = load_any(&st, &id).await?;
    Ok(Json(ws_row_doc(&st, &ws).await?))
}

/// `DELETE /admin/workspaces/{id}`: soft delete (purged after 30 days).
pub async fn workspace_delete(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ws = load_any(&st, &id).await?;
    let at = super::workspaces::soft_delete(&st, &ws).await?;
    audit(
        &st,
        &a.actor(),
        a.via(),
        "admin.workspace.delete",
        Some(&ws.id),
        json!({"soft": true}),
    )
    .await;
    ev(&st, &a, "admin workspace delete", Some(&ws.id), json!({}));
    Ok(Json(
        json!({ "deleted": true, "deleted_at": at, "purge_after_days": tenancy::PURGE_AFTER_DAYS }),
    ))
}

// ---------------------------------------------------------------------------
// plans
// ---------------------------------------------------------------------------

fn validate_limits(v: &Value) -> ApiResult<Value> {
    let o = v
        .as_object()
        .ok_or_else(|| ApiError::bad("limits must be an object"))?;
    let mut out = serde_json::Map::new();
    for (k, val) in o {
        if tenancy::LIMIT_KEYS.contains(&k.as_str()) {
            match val {
                Value::Null => {
                    out.insert(k.clone(), Value::Null);
                }
                Value::Number(n)
                    if n.as_i64()
                        .is_some_and(|n| (0..=1_000_000_000_000).contains(&n)) =>
                {
                    out.insert(k.clone(), val.clone());
                }
                _ => {
                    return Err(ApiError::bad(format!(
                        "limits.{k} must be a number >= 0 or null"
                    )));
                }
            }
        } else if tenancy::FEATURE_KEYS.contains(&k.as_str()) {
            let b = val
                .as_bool()
                .ok_or_else(|| ApiError::bad(format!("limits.{k} must be true or false")))?;
            out.insert(k.clone(), json!(b));
        } else {
            return Err(ApiError::bad(format!("unknown limit {k}")));
        }
    }
    Ok(Value::Object(out))
}

#[derive(Deserialize)]
pub struct PlanBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    limits: Option<Value>,
    #[serde(default)]
    price_label: Option<String>,
    #[serde(default)]
    public: Option<bool>,
    #[serde(default)]
    sort: Option<i64>,
}

async fn plan_doc(st: &AppState, id: &str) -> ApiResult<Value> {
    super::workspaces::plans_json(st, true)
        .await?
        .into_iter()
        .find(|p| p["id"] == id)
        .ok_or_else(|| ApiError::not_found("plan not found"))
}

/// `POST /admin/plans {id?, name, limits, price_label?, public?, sort?}` → 201.
pub async fn plan_create(
    State(st): State<AppState>,
    Admin(a): Admin,
    JsonBody(b): JsonBody<PlanBody>,
) -> ApiResult<Response> {
    let name = b
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty() && n.chars().count() <= 60)
        .ok_or_else(|| ApiError::bad("name is required (max 60 chars)"))?
        .to_string();
    let id = match b.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(i) if tenancy::valid_slug(i) => i.to_string(),
        Some(_) => {
            return Err(ApiError::bad(
                "id must be 3-48 lowercase letters, digits or dashes",
            ));
        }
        None => tenancy::slugify(&name),
    };
    let limits = validate_limits(b.limits.as_ref().unwrap_or(&json!({})))?.to_string();
    let (i2, label, public, sort) = (
        id.clone(),
        b.price_label
            .unwrap_or_default()
            .chars()
            .take(60)
            .collect::<String>(),
        b.public.unwrap_or(true),
        b.sort.unwrap_or(50).clamp(-1000, 1000),
    );
    st.db
        .write(move |t| {
            t.execute(
                "INSERT INTO plans(id, name, limits, price_label, public, sort) VALUES(?1,?2,?3,?4,?5,?6)",
                params![i2, name, limits, label, public, sort],
            )
            .map_err(err)?;
            Ok(())
        })
        .await
        .map_err(|e| crate::v1::common::conflict_as(e, "a plan with this id or name exists"))?;
    audit(
        &st,
        &a.actor(),
        a.via(),
        "admin.plan.create",
        Some(&id),
        json!({}),
    )
    .await;
    Ok((StatusCode::CREATED, Json(plan_doc(&st, &id).await?)).into_response())
}

/// `PATCH /admin/plans/{id}` (limits given replace the stored object).
pub async fn plan_patch(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<PlanBody>,
) -> ApiResult<Json<Value>> {
    plan_doc(&st, &id).await?;
    let limits = b
        .limits
        .as_ref()
        .map(validate_limits)
        .transpose()?
        .map(|v| v.to_string());
    if let Some(n) = &b.name
        && (n.trim().is_empty() || n.chars().count() > 60)
    {
        return Err(ApiError::bad("name is required (max 60 chars)"));
    }
    let (i2, name, label, public, sort) = (
        id.clone(),
        b.name.clone(),
        b.price_label.clone(),
        b.public,
        b.sort,
    );
    st.db
        .write(move |t| {
            if let Some(n) = name {
                t.execute(
                    "UPDATE plans SET name=?2 WHERE id=?1",
                    params![i2, n.trim()],
                )
                .map_err(err)?;
            }
            if let Some(l) = limits {
                t.execute("UPDATE plans SET limits=?2 WHERE id=?1", params![i2, l])
                    .map_err(err)?;
            }
            if let Some(l) = label {
                t.execute(
                    "UPDATE plans SET price_label=?2 WHERE id=?1",
                    params![i2, l.chars().take(60).collect::<String>()],
                )
                .map_err(err)?;
            }
            if let Some(p) = public {
                t.execute("UPDATE plans SET public=?2 WHERE id=?1", params![i2, p])
                    .map_err(err)?;
            }
            if let Some(s) = sort {
                t.execute(
                    "UPDATE plans SET sort=?2 WHERE id=?1",
                    params![i2, s.clamp(-1000, 1000)],
                )
                .map_err(err)?;
            }
            t.execute(
                "UPDATE plans SET updated_at=?2 WHERE id=?1",
                params![i2, sf_core::now_rfc3339()],
            )
            .map_err(err)?;
            Ok(())
        })
        .await
        .map_err(|e| crate::v1::common::conflict_as(e, "a plan with this name exists"))?;
    audit(
        &st,
        &a.actor(),
        a.via(),
        "admin.plan.update",
        Some(&id),
        json!({}),
    )
    .await;
    Ok(Json(plan_doc(&st, &id).await?))
}

/// `DELETE /admin/plans/{id}` (409 while a workspace uses it).
pub async fn plan_delete(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    plan_doc(&st, &id).await?;
    if id == "unlimited" || id == "free" {
        return Err(ApiError::bad(
            "built-in plans free and unlimited cannot be deleted",
        ));
    }
    let i2 = id.clone();
    let used: i64 = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT count(*) FROM workspaces WHERE plan_id=?1",
                [&i2],
                |r| r.get(0),
            )
            .map_err(err)
        })
        .await?;
    if used > 0 {
        return Err(ApiError::conflict(format!(
            "plan is used by {used} workspaces"
        )));
    }
    let i3 = id.clone();
    st.db
        .write(move |t| {
            t.execute("DELETE FROM plans WHERE id=?1", [&i3])
                .map_err(err)?;
            Ok(())
        })
        .await?;
    audit(
        &st,
        &a.actor(),
        a.via(),
        "admin.plan.delete",
        Some(&id),
        json!({}),
    )
    .await;
    Ok(Json(json!({ "deleted": true })))
}

// ---------------------------------------------------------------------------
// invites, email, platform, audit
// ---------------------------------------------------------------------------

/// `GET /admin/invites`.
pub async fn invites(State(st): State<AppState>, _a: Admin) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "invites": super::workspaces::invites_json(&st, None).await? }),
    ))
}

/// `GET /admin/email`.
pub async fn email(State(st): State<AppState>, _a: Admin) -> ApiResult<Json<Value>> {
    let p = crate::platform::get(&st).await;
    let outbox = st
        .db
        .read(|c| {
            let mut q = c
                .prepare(
                    "SELECT id, to_email, subject, kind, link, created_at, sent_at, error FROM mail_outbox
                      ORDER BY id DESC LIMIT 200",
                )
                .map_err(err)?;
            let rows = q
                .query_map([], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?, "to": r.get::<_, String>(1)?, "subject": r.get::<_, String>(2)?,
                        "kind": r.get::<_, String>(3)?, "link": r.get::<_, Option<String>>(4)?,
                        "created_at": r.get::<_, String>(5)?, "sent_at": r.get::<_, Option<String>>(6)?,
                        "error": r.get::<_, Option<String>>(7)?,
                    }))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?;
    let view = p.to_json(st.cfg.auth.allow_registration);
    Ok(Json(
        json!({ "configured": p.smtp.configured(), "smtp": view["smtp"], "outbox": outbox }),
    ))
}

#[derive(Deserialize)]
pub struct TestBody {
    to: String,
}

/// `POST /admin/email/test {to}` → `{ok, delivery, error?}`.
pub async fn email_test(
    State(st): State<AppState>,
    Admin(a): Admin,
    JsonBody(b): JsonBody<TestBody>,
) -> ApiResult<Json<Value>> {
    let to = b.to.trim().to_lowercase();
    if !crate::v1::auth::valid_email(&to) {
        return Err(ApiError::bad("invalid email address"));
    }
    let p = crate::platform::get(&st).await;
    if !p.smtp.configured() {
        return Ok(Json(
            json!({ "ok": false, "delivery": "manual", "error": "SMTP is not configured" }),
        ));
    }
    let app = p.branding.app_name.clone();
    let m = Mail {
        to: &to,
        subject: &format!("{app} test email"),
        body: format!("This is a test email from {app}. SMTP works.\n"),
        kind: "test",
        link: None,
    };
    let res = mail::send_smtp(&st, &p, &m).await;
    audit(
        &st,
        &a.actor(),
        a.via(),
        "admin.email.test",
        None,
        json!({"ok": res.is_ok()}),
    )
    .await;
    Ok(Json(match res {
        Ok(()) => json!({ "ok": true, "delivery": "smtp" }),
        Err(e) => json!({ "ok": false, "delivery": "smtp", "error": e }),
    }))
}

/// `DELETE /admin/email/outbox/{id}`.
pub async fn outbox_delete(
    State(st): State<AppState>,
    _a: Admin,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let n = st
        .db
        .write(move |t| {
            t.execute("DELETE FROM mail_outbox WHERE id=?1", [id])
                .map_err(err)
        })
        .await?;
    if n == 0 {
        return Err(ApiError::not_found("message not found"));
    }
    Ok(Json(json!({ "deleted": true })))
}

/// `GET /admin/platform`.
pub async fn platform_get(State(st): State<AppState>, _a: Admin) -> ApiResult<Json<Value>> {
    let p = crate::platform::get(&st).await;
    Ok(Json(p.to_json(st.cfg.auth.allow_registration)))
}

/// `PUT /admin/platform` (sections merged).
pub async fn platform_put(
    State(st): State<AppState>,
    Admin(a): Admin,
    JsonBody(b): JsonBody<Value>,
) -> ApiResult<Json<Value>> {
    let (p, changed) = crate::platform::put(&st, &b).await?;
    audit(
        &st,
        &a.actor(),
        a.via(),
        "admin.platform.update",
        None,
        json!({"changed": changed}),
    )
    .await;
    crate::eventlog::record_system(
        &st,
        "platform settings changed",
        true,
        json!({"changed": changed}),
    );
    let mut v = p.to_json(st.cfg.auth.allow_registration);
    if let Some(o) = v.as_object_mut() {
        o.insert("changed".into(), json!(changed));
    }
    Ok(Json(v))
}

/// `GET /admin/audit?...` = `/api/v2/logs` platform-wide.
pub async fn audit_log(
    st: State<AppState>,
    Admin(a): Admin,
    q: Query<super::logs::LogsQ>,
) -> ApiResult<Json<Value>> {
    super::logs::list(st, a, q).await
}
