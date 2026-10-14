//! v2.4 accounts (docs/specs/v2.4-saas.md §3, §9.3): public auth config, login with remember-me
//! and TOTP 2FA, registration modes, email verification, forgot/reset, change email, sessions.
//!
//! One-time links (verify / reset / change email) are random 32-byte tokens stored as sha256 in
//! `auth_tokens`, single use, with an expiry. Mail goes through [`crate::mail`] (SMTP or the
//! manual outbox).

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sf_core::crypto;
use sf_store::err;

use crate::auth::User;
use crate::mail::{self, Mail};
use crate::tenancy;
use crate::v1::auth::{MAX_PASSWORD, bcrypt_hash, bcrypt_verify, valid_email};
use crate::v1::common::{ClientIp, JsonBody, limiter, lockout, rate_check, rate_fail, rate_guard};
use crate::{ApiError, ApiResult, AppState};

pub const RESET_TTL_SECS: i64 = 3600;
pub const VERIFY_TTL_SECS: i64 = 48 * 3600;
pub const CHALLENGE_TTL_SECS: i64 = 300;
pub const RECOVERY_CODES: usize = 10;
/// Workspaces a non-admin may own (self-service create).
pub const MAX_OWNED_WORKSPACES: i64 = 3;

fn ts_in(secs: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn ev(st: &AppState, actor: &str, action: &str, status: &str, uid: Option<&str>, detail: Value) {
    crate::eventlog::log(st).record_auth(actor, action, status, uid, uid, detail);
}

/// Anonymous auth event about an account (target = the email, as in 2.1-2.3).
fn ev_account(st: &AppState, action: &str, account: &str, detail: Value) {
    crate::eventlog::log(st).record_auth(
        "anonymous",
        action,
        "denied",
        None,
        Some(account),
        detail,
    );
}

fn check_password(pw: &str) -> ApiResult<()> {
    let n = pw.chars().count();
    if n < 8 {
        return Err(ApiError::bad("password must be at least 8 characters"));
    }
    if n > MAX_PASSWORD {
        return Err(ApiError::bad("password is too long"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// one-time tokens
// ---------------------------------------------------------------------------

/// Issues a one-time token (plaintext returned once). Older unused tokens of the same purpose for
/// the user are invalidated.
pub async fn issue_token(
    st: &AppState,
    purpose: &str,
    user_id: &str,
    email: &str,
    ttl_secs: i64,
) -> ApiResult<String> {
    let token = crypto::random_hex(32);
    let hash = crypto::sha256_hex(token.as_bytes());
    let (p, u, e, exp) = (
        purpose.to_string(),
        user_id.to_string(),
        email.to_string(),
        ts_in(ttl_secs),
    );
    st.db
        .write(move |t| {
            t.execute(
                "UPDATE auth_tokens SET used_at=?3 WHERE user_id=?1 AND purpose=?2 AND used_at IS NULL",
                params![u, p, sf_core::now_rfc3339()],
            )
            .map_err(err)?;
            t.execute(
                "INSERT INTO auth_tokens(token_hash, purpose, user_id, email, expires_at) VALUES(?1,?2,?3,?4,?5)",
                params![hash, p, u, e, exp],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    Ok(token)
}

/// Consumes a one-time token of one of `purposes`: `(purpose, user_id, email)`. Invalid, used or
/// expired tokens → 400.
async fn consume_token(
    st: &AppState,
    purposes: &[&str],
    token: &str,
) -> ApiResult<(String, String, String)> {
    let token = token.trim();
    if token.is_empty() || token.len() > 200 {
        return Err(ApiError::bad("invalid or expired link"));
    }
    let hash = crypto::sha256_hex(token.as_bytes());
    let purposes: Vec<String> = purposes.iter().map(|s| s.to_string()).collect();
    let row = st
        .db
        .write(move |t| {
            let row: Option<(String, String, String, String, Option<String>)> = t
                .query_row(
                    "SELECT purpose, user_id, email, expires_at, used_at FROM auth_tokens WHERE token_hash=?1",
                    [&hash],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()
                .map_err(err)?;
            let Some((purpose, uid, email, exp, used)) = row else {
                return Ok(None);
            };
            if used.is_some() || exp < sf_core::now_rfc3339() || !purposes.contains(&purpose) {
                return Ok(None);
            }
            t.execute(
                "UPDATE auth_tokens SET used_at=?2 WHERE token_hash=?1 AND used_at IS NULL",
                params![hash, sf_core::now_rfc3339()],
            )
            .map_err(err)?;
            Ok(Some((purpose, uid, email)))
        })
        .await?;
    row.ok_or_else(|| ApiError::bad("invalid or expired link"))
}

// ---------------------------------------------------------------------------
// shared login core
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LoginOk {
    pub id: String,
    pub email: String,
    pub name: String,
    pub role: String,
    pub tv: i64,
    pub totp: bool,
}

impl LoginOk {
    pub fn user_json(&self) -> Value {
        json!({ "id": self.id, "email": self.email, "name": self.name, "role": self.role })
    }
}

/// Password check with the per-IP failure limit, per-account lockout, timing-safe unknown email,
/// disabled accounts (403) and the background rehash. Shared by v1 and v2 login.
pub async fn password_login(
    st: &AppState,
    ip: &str,
    email: &str,
    password: &str,
) -> ApiResult<LoginOk> {
    rate_guard(st, "login", ip)?;
    let email = email.trim().to_lowercase();
    if let Some(d) = lockout(st).locked_for(&email) {
        ev_account(
            st,
            "login refused (account locked)",
            &email,
            json!({"ip": ip}),
        );
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "too many failed logins for this account, try again in {} s",
                d.as_secs().max(1)
            ),
        ));
    }
    let account = email.clone();
    type Row = (String, String, String, String, String, i64, bool, bool);
    let row: Option<Row> = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT id, email, name, role, password_hash, token_version,
                        disabled_at IS NOT NULL, totp_secret_enc IS NOT NULL AND totp_secret_enc <> ''
                   FROM users WHERE email=?1",
                [&email],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .optional()
            .map_err(err)
        })
        .await?;
    let hash = row
        .as_ref()
        .map(|r| r.4.clone())
        .unwrap_or_else(|| crypto::DUMMY_BCRYPT.to_string());
    let pw: String = password.chars().take(MAX_PASSWORD).collect();
    let ok = bcrypt_verify(pw.clone(), hash).await;
    let Some((id, email, name, role, old_hash, tv, disabled, totp)) = row.filter(|_| ok) else {
        rate_fail(st, "login", ip);
        ev_account(st, "login failed", &account, json!({"ip": ip}));
        if let Some(d) = lockout(st).fail(&account) {
            tracing::warn!(
                lock_secs = d.as_secs(),
                "login: account locked after repeated failures"
            );
            ev_account(
                st,
                "account locked",
                &account,
                json!({"ip": ip, "lock_secs": d.as_secs()}),
            );
        }
        return Err(ApiError::unauthorized("invalid email or password"));
    };
    lockout(st).success(&account);
    if disabled {
        ev(
            st,
            &format!("user:{id}"),
            "login refused (disabled)",
            "denied",
            Some(&id),
            json!({"ip": ip}),
        );
        return Err(ApiError::forbidden("account disabled"));
    }
    if crypto::password_needs_rehash(&old_hash) {
        let (st2, uid) = (st.clone(), id.clone());
        tokio::spawn(async move {
            let Ok(new_hash) = bcrypt_hash(pw).await else {
                return;
            };
            let _ = st2
                .db
                .write(move |t| {
                    t.execute(
                        "UPDATE users SET password_hash=?2 WHERE id=?1 AND password_hash=?3",
                        params![uid, new_hash, old_hash],
                    )
                    .map_err(err)?;
                    Ok(())
                })
                .await;
        });
    }
    Ok(LoginOk {
        id,
        email,
        name,
        role,
        tv,
        totp,
    })
}

/// Successful login answer `{token, user, session_id, expires_at}`.
async fn login_success(
    st: &AppState,
    who: &LoginOk,
    remember: bool,
    ip: &str,
    headers: &HeaderMap,
) -> ApiResult<Value> {
    let s = crate::sessions::issue(
        st,
        &who.id,
        &who.role,
        who.tv,
        remember,
        ip,
        &crate::sessions::user_agent(headers),
    )
    .await?;
    ev(
        st,
        &format!("user:{}", who.id),
        "login ok",
        "ok",
        Some(&who.id),
        json!({"ip": ip, "remember": remember, "session_id": s["session_id"]}),
    );
    Ok(json!({
        "token": s["token"], "session_id": s["session_id"], "expires_at": s["expires_at"],
        "user": who.user_json(),
    }))
}

// ---------------------------------------------------------------------------
// public config
// ---------------------------------------------------------------------------

/// `GET /auth/config` (public).
pub async fn config(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let p = crate::platform::get(&st).await;
    let users: i64 = st
        .db
        .read(|c| {
            c.query_row("SELECT count(*) FROM users", [], |r| r.get(0))
                .map_err(err)
        })
        .await?;
    Ok(Json(json!({
        "registration_mode": p.mode(st.cfg.auth.allow_registration),
        "require_email_verification": p.registration.require_email_verification,
        "email_enabled": p.smtp.configured(),
        "first_run": users == 0,
        "branding": p.branding,
        "version": sf_core::VERSION,
    })))
}

// ---------------------------------------------------------------------------
// login
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct LoginBody {
    email: String,
    password: String,
    #[serde(default)]
    remember: Option<bool>,
}

/// `POST /auth/login`.
pub async fn login(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    JsonBody(b): JsonBody<LoginBody>,
) -> ApiResult<Json<Value>> {
    let who = password_login(&st, &ip, &b.email, &b.password).await?;
    let remember = b.remember.unwrap_or(true);
    if who.totp {
        let mut c = crypto::user_claims("2fa", &who.id, who.tv);
        c.sid = Some(if remember { "remember" } else { "session" }.into());
        c.exp = c.iat.saturating_add(CHALLENGE_TTL_SECS);
        let challenge = crypto::sign_jwt(&st.cfg.auth.jwt_secret, &c)?;
        return Ok(Json(
            json!({ "two_factor_required": true, "challenge": challenge }),
        ));
    }
    Ok(Json(
        login_success(&st, &who, remember, &ip, &headers).await?,
    ))
}

#[derive(Deserialize)]
pub struct TwoFaLoginBody {
    challenge: String,
    code: String,
}

/// `POST /auth/2fa/login {challenge, code}` (TOTP or recovery code).
pub async fn login_2fa(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    JsonBody(b): JsonBody<TwoFaLoginBody>,
) -> ApiResult<Json<Value>> {
    rate_guard(&st, "2fa", &ip)?;
    let claims = crypto::verify_jwt(&st.cfg.auth.jwt_secret, b.challenge.trim())
        .map_err(|_| ApiError::unauthorized("sign-in expired, start again"))?;
    let Some(uid) = claims.user_id.clone().filter(|_| claims.role == "2fa") else {
        return Err(ApiError::unauthorized("sign-in expired, start again"));
    };
    let remember = claims.sid.as_deref() == Some("remember");
    let u2 = uid.clone();
    type Row = (String, String, String, i64, Option<String>, bool);
    let row: Option<Row> = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT email, name, role, token_version, totp_secret_enc, disabled_at IS NOT NULL FROM users WHERE id=?1",
                [&u2],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()
            .map_err(err)
        })
        .await?;
    let Some((email, name, role, tv, secret, disabled)) = row else {
        return Err(ApiError::unauthorized("sign-in expired, start again"));
    };
    if disabled {
        return Err(ApiError::forbidden("account disabled"));
    }
    if claims.tv.unwrap_or(0) != tv {
        return Err(ApiError::unauthorized("sign-in expired, start again"));
    }
    if !verify_second_factor(&st, &uid, secret.as_deref(), &b.code).await? {
        rate_fail(&st, "2fa", &ip);
        ev(
            &st,
            &format!("user:{uid}"),
            "2fa failed",
            "denied",
            Some(&uid),
            json!({"ip": ip}),
        );
        return Err(ApiError::unauthorized("invalid code"));
    }
    let who = LoginOk {
        id: uid,
        email,
        name,
        role,
        tv,
        totp: true,
    };
    Ok(Json(
        login_success(&st, &who, remember, &ip, &headers).await?,
    ))
}

/// TOTP (replay-safe: each step once) or an unused recovery code (consumed).
async fn verify_second_factor(
    st: &AppState,
    uid: &str,
    secret_enc: Option<&str>,
    code: &str,
) -> ApiResult<bool> {
    let code = code.trim();
    if code.is_empty() || code.len() > 64 {
        return Ok(false);
    }
    let secret = match secret_enc {
        Some(s) if !s.is_empty() => crypto::decrypt_secret(&st.secret_key, s).unwrap_or_default(),
        _ => return Ok(false),
    };
    if code.chars().filter(char::is_ascii_digit).count() == 6 && code.len() <= 8 {
        let Some(step) = sf_core::totp::verify(&secret, code, sf_core::now_unix()) else {
            return Ok(false);
        };
        let u = uid.to_string();
        let fresh = st
            .db
            .write(move |t| {
                let n = t
                    .execute(
                        "UPDATE users SET totp_last_step=?2 WHERE id=?1 AND totp_last_step < ?2",
                        params![u, step],
                    )
                    .map_err(err)?;
                Ok(n > 0)
            })
            .await?;
        return Ok(fresh);
    }
    let norm = sf_core::totp::normalize_recovery(code);
    let hash = crypto::sha256_hex(format!("{uid}:{norm}").as_bytes());
    let u = uid.to_string();
    let used = st
        .db
        .write(move |t| {
            let n = t
                .execute(
                    "UPDATE user_recovery_codes SET used_at=?3 WHERE user_id=?1 AND code_hash=?2 AND used_at IS NULL",
                    params![u, hash, sf_core::now_rfc3339()],
                )
                .map_err(err)?;
            Ok(n > 0)
        })
        .await?;
    Ok(used)
}

// ---------------------------------------------------------------------------
// registration
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct RegisterBody {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub invite_token: Option<String>,
    #[serde(default)]
    pub workspace_name: Option<String>,
    /// Honeypot: humans never fill it.
    #[serde(default)]
    pub website: Option<String>,
}

#[derive(Debug)]
struct InviteRow {
    id: String,
    workspace_id: String,
    email: String,
    role: String,
}

async fn find_invite(st: &AppState, token: &str) -> ApiResult<Option<InviteRow>> {
    let token = token.trim();
    if token.is_empty() || token.len() > 200 {
        return Ok(None);
    }
    let hash = crypto::sha256_hex(token.as_bytes());
    Ok(st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT i.id, i.workspace_id, i.email, i.role FROM workspace_invites i
                   JOIN workspaces w ON w.id = i.workspace_id
                  WHERE i.token_hash=?1 AND i.accepted_at IS NULL AND i.revoked_at IS NULL
                    AND i.expires_at > ?2 AND w.deleted_at IS NULL",
                params![hash, sf_core::now_rfc3339()],
                |r| {
                    Ok(InviteRow {
                        id: r.get(0)?,
                        workspace_id: r.get(1)?,
                        email: r.get(2)?,
                        role: r.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(err)
        })
        .await?)
}

/// Registration core shared by v1 and v2 (`(status, body)`).
pub async fn register_core(
    st: &AppState,
    ip: &str,
    headers: &HeaderMap,
    b: RegisterBody,
) -> ApiResult<Value> {
    rate_check(st, "register", ip)?;
    let p = crate::platform::get(st).await;
    if b.website.as_deref().is_some_and(|w| !w.trim().is_empty()) {
        ev(
            st,
            "anonymous",
            "register refused (honeypot)",
            "denied",
            None,
            json!({"ip": ip}),
        );
        return Err(ApiError::bad("registration failed"));
    }
    let users: i64 = st
        .db
        .read(|c| {
            c.query_row("SELECT count(*) FROM users", [], |r| r.get(0))
                .map_err(err)
        })
        .await?;
    let first_run = users == 0;
    let mode = p.mode(st.cfg.auth.allow_registration).to_string();
    let invite = match b.invite_token.as_deref() {
        Some(t) if !t.trim().is_empty() => Some(
            find_invite(st, t)
                .await?
                .ok_or_else(|| ApiError::bad("invalid or expired invitation"))?,
        ),
        _ => None,
    };
    if first_run {
        if !crate::v1::auth::bootstrap_host_ok(headers) {
            return Err(ApiError::forbidden(
                "first-run registration must use http://127.0.0.1:<port> or http://localhost:<port>",
            ));
        }
    } else {
        match mode.as_str() {
            "open" => {}
            "invite_only" if invite.is_some() => {}
            "invite_only" => return Err(ApiError::forbidden("an invitation is required")),
            _ if invite.is_some() => {}
            _ => return Err(ApiError::forbidden("registration is closed")),
        }
        let per_hour = usize::try_from(p.registration.rate_limit_per_hour.max(1)).unwrap_or(5);
        if !limiter(st).allow(
            &format!("register_h:{ip}"),
            per_hour,
            Duration::from_secs(3600),
        ) {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too many sign-ups from this address, try again later",
            ));
        }
    }
    let email = b.email.trim().to_lowercase();
    if !valid_email(&email) {
        return Err(ApiError::bad("invalid email address"));
    }
    if let Some(inv) = &invite
        && inv.email.to_lowercase() != email
    {
        return Err(ApiError::forbidden(
            "this invitation is for another email address",
        ));
    }
    if !first_run && invite.is_none() && !crate::platform::domain_allowed(&p, &email) {
        return Err(ApiError::forbidden(
            "sign-ups from this email domain are not allowed",
        ));
    }
    check_password(&b.password)?;
    let name: String = b
        .name
        .unwrap_or_default()
        .trim()
        .chars()
        .take(200)
        .collect();
    let ws_name: String = b
        .workspace_name
        .unwrap_or_default()
        .trim()
        .chars()
        .take(120)
        .collect();
    let hash = bcrypt_hash(b.password).await?;
    let id = sf_core::new_id();
    let verified_now = first_run || invite.is_some();
    let default_plan = p.registration.default_plan_id.clone();
    let open_ws = !first_run && invite.is_none();
    let (id2, email2, name2) = (id.clone(), email.clone(), name.clone());
    let inv2 = invite
        .as_ref()
        .map(|i| (i.id.clone(), i.workspace_id.clone(), i.role.clone()));
    let (role, ws) = st
        .db
        .write(move |t| {
            let n: i64 = t
                .query_row("SELECT count(*) FROM users", [], |r| r.get(0))
                .map_err(err)?;
            let role = if n == 0 { "admin" } else { "user" };
            let now = sf_core::now_rfc3339();
            t.execute(
                "INSERT INTO users(id, email, password_hash, name, role, email_verified_at) VALUES(?1,?2,?3,?4,?5,?6)",
                params![id2, email2, hash, name2, role, verified_now.then_some(now.clone())],
            )
            .map_err(err)?;
            let ws: Option<(String, String)> = if n == 0 {
                tenancy::ensure_default_workspace_sync(t)?;
                Some(("default".into(), "Default".into()))
            } else if let Some((inv_id, ws_id, inv_role)) = inv2 {
                t.execute(
                    "INSERT OR IGNORE INTO workspace_members(workspace_id, user_id, role, invited_by)
                     SELECT ?1, ?2, ?3, invited_by FROM workspace_invites WHERE id=?4",
                    params![ws_id, id2, inv_role, inv_id],
                )
                .map_err(err)?;
                t.execute(
                    "UPDATE workspace_invites SET accepted_at=?2, accepted_by=?3 WHERE id=?1",
                    params![inv_id, now, id2],
                )
                .map_err(err)?;
                let wn: String = t
                    .query_row("SELECT name FROM workspaces WHERE id=?1", [&ws_id], |r| r.get(0))
                    .map_err(err)?;
                Some((ws_id, wn))
            } else if open_ws {
                let label = if !ws_name.is_empty() {
                    ws_name
                } else if !name2.is_empty() {
                    format!("{name2}'s workspace")
                } else {
                    format!("{}'s workspace", email2.split('@').next().unwrap_or("My"))
                };
                let plan: String = t
                    .query_row(
                        "SELECT id FROM plans WHERE id=?1 UNION ALL SELECT 'free' LIMIT 1",
                        [&default_plan],
                        |r| r.get(0),
                    )
                    .map_err(err)?;
                let w = tenancy::create_ws_sync(t, &label, None, &plan, &id2)?;
                Some((w.id, w.name))
            } else {
                None
            };
            Ok((role.to_string(), ws))
        })
        .await
        .map_err(|e| crate::v1::common::conflict_as(e, "email already registered"))?;
    let s = crate::sessions::issue(
        st,
        &id,
        &role,
        0,
        true,
        ip,
        &crate::sessions::user_agent(headers),
    )
    .await?;
    let need_verify = !verified_now && p.registration.require_email_verification;
    let mut delivery: Option<&'static str> = None;
    if !verified_now && (need_verify || p.smtp.configured()) {
        delivery = Some(send_verification(st, headers, &id, &email).await?.as_str());
    }
    crate::auth::audit(
        st,
        &format!("user:{id}"),
        "ui",
        "auth.register",
        Some(&id),
        json!({ "role": role, "workspace_id": ws.as_ref().map(|w| &w.0), "invite": invite.is_some() }),
    )
    .await;
    ev(
        st,
        &format!("user:{id}"),
        "register",
        "ok",
        Some(&id),
        json!({"ip": ip, "mode": mode, "invite": invite.is_some()}),
    );
    Ok(json!({
        "token": s["token"], "session_id": s["session_id"], "expires_at": s["expires_at"],
        "user": { "id": id, "email": email, "name": name, "role": role },
        "workspace": ws.map(|(i, n)| json!({"id": i, "name": n})),
        "email_verification_required": need_verify,
        "verification_delivery": delivery,
    }))
}

/// `POST /auth/register`.
pub async fn register(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    JsonBody(b): JsonBody<RegisterBody>,
) -> ApiResult<Response> {
    let v = register_core(&st, &ip, &headers, b).await?;
    Ok((StatusCode::CREATED, Json(v)).into_response())
}

async fn send_verification(
    st: &AppState,
    headers: &HeaderMap,
    uid: &str,
    email: &str,
) -> ApiResult<mail::Delivery> {
    let p = crate::platform::get(st).await;
    let token = issue_token(st, "verify_email", uid, email, VERIFY_TTL_SECS).await?;
    let link = mail::link(
        &p,
        mail::request_host(headers).as_deref(),
        &format!("/verify-email?token={token}"),
    );
    let app = p.branding.app_name.clone();
    mail::send(
        st,
        Mail {
            to: email,
            subject: &format!("Confirm your email for {app}"),
            body: format!(
                "Hello,\n\nplease confirm your email address for {app}:\n\n{link}\n\nThe link is valid for 48 hours. If you did not sign up, ignore this message.\n"
            ),
            kind: "verify_email",
            link: Some(link),
        },
    )
    .await
}

/// 403 while email verification is required and the caller's address is unverified.
pub async fn require_verified(st: &AppState, u: &User) -> ApiResult<()> {
    if u.is_admin() {
        return Ok(());
    }
    let p = crate::platform::get(st).await;
    if !p.registration.require_email_verification {
        return Ok(());
    }
    let id = u.user_id.clone();
    let verified: bool = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT email_verified_at IS NOT NULL FROM users WHERE id=?1",
                [&id],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)
        })
        .await?
        .unwrap_or(false);
    if verified {
        Ok(())
    } else {
        Err(ApiError::forbidden("email not verified")
            .with_extra(json!({"code": "email_unverified"})))
    }
}

#[derive(Deserialize)]
pub struct TokenBody {
    token: String,
}

/// `POST /auth/verify-email {token}` (public): verify_email or change_email links.
pub async fn verify_email(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    JsonBody(b): JsonBody<TokenBody>,
) -> ApiResult<Json<Value>> {
    rate_guard(&st, "verify", &ip)?;
    let res = consume_token(&st, &["verify_email", "change_email"], &b.token).await;
    let (purpose, uid, email) = match res {
        Ok(x) => x,
        Err(e) => {
            rate_fail(&st, "verify", &ip);
            return Err(e);
        }
    };
    let (u2, e2, p2) = (uid.clone(), email.clone(), purpose.clone());
    st.db
        .write(move |t| {
            let now = sf_core::now_rfc3339();
            if p2 == "change_email" {
                t.execute(
                    "UPDATE users SET email=?2, email_verified_at=?3, pending_email=NULL WHERE id=?1",
                    params![u2, e2, now],
                )
                .map_err(err)?;
            } else {
                t.execute(
                    "UPDATE users SET email_verified_at=coalesce(email_verified_at, ?2) WHERE id=?1 AND email=?3",
                    params![u2, now, e2],
                )
                .map_err(err)?;
            }
            Ok(())
        })
        .await
        .map_err(|e| crate::v1::common::conflict_as(e, "email already registered"))?;
    st.auth_cache.clear();
    ev(
        &st,
        &format!("user:{uid}"),
        "email verified",
        "ok",
        Some(&uid),
        json!({"purpose": purpose}),
    );
    Ok(Json(json!({ "ok": true, "email": email })))
}

/// `POST /auth/verify-email/resend` (user).
pub async fn resend_verification(
    State(st): State<AppState>,
    u: User,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    if !limiter(&st).allow(
        &format!("resend:{}", u.user_id),
        3,
        Duration::from_secs(3600),
    ) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "try again later",
        ));
    }
    let id = u.user_id.clone();
    let row: Option<(String, bool)> = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT email, email_verified_at IS NOT NULL FROM users WHERE id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(err)
        })
        .await?;
    let Some((email, verified)) = row else {
        return Err(ApiError::unauthorized("unauthorized"));
    };
    if verified {
        return Ok(Json(json!({ "sent": false, "already_verified": true })));
    }
    let d = send_verification(&st, &headers, &u.user_id, &email).await?;
    Ok(Json(
        json!({ "sent": d == mail::Delivery::Smtp, "delivery": d.as_str() }),
    ))
}

#[derive(Deserialize)]
pub struct ForgotBody {
    email: String,
}

/// `POST /auth/forgot {email}`: always `{ok:true}` (no account oracle).
pub async fn forgot(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    JsonBody(b): JsonBody<ForgotBody>,
) -> ApiResult<Json<Value>> {
    if !limiter(&st).allow(&format!("forgot:{ip}"), 5, Duration::from_secs(900)) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts, try again later",
        ));
    }
    let email = b.email.trim().to_lowercase();
    if valid_email(&email) {
        let e2 = email.clone();
        let row: Option<(String, bool)> = st
            .db
            .read(move |c| {
                c.query_row(
                    "SELECT id, disabled_at IS NOT NULL FROM users WHERE email=?1",
                    [&e2],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(err)
            })
            .await?;
        if let Some((uid, false)) = row
            && limiter(&st).allow(&format!("forgot_u:{uid}"), 3, Duration::from_secs(3600))
        {
            send_reset(&st, &headers, &uid, &email).await?;
            ev(
                &st,
                "anonymous",
                "password reset requested",
                "ok",
                Some(&uid),
                json!({"ip": ip}),
            );
        }
    }
    Ok(Json(json!({ "ok": true })))
}

/// Emails a reset link (also used by the admin console).
pub async fn send_reset(
    st: &AppState,
    headers: &HeaderMap,
    uid: &str,
    email: &str,
) -> ApiResult<(mail::Delivery, String)> {
    let p = crate::platform::get(st).await;
    let token = issue_token(st, "reset_password", uid, email, RESET_TTL_SECS).await?;
    let link = mail::link(
        &p,
        mail::request_host(headers).as_deref(),
        &format!("/reset-password?token={token}"),
    );
    let app = p.branding.app_name.clone();
    let d = mail::send(
        st,
        Mail {
            to: email,
            subject: &format!("Reset your {app} password"),
            body: format!(
                "Hello,\n\nsomeone asked to reset the password of your {app} account. Open this link to choose a new password:\n\n{link}\n\nThe link works once and expires in 1 hour. If this was not you, ignore this message.\n"
            ),
            kind: "reset_password",
            link: Some(link.clone()),
        },
    )
    .await?;
    Ok((d, link))
}

#[derive(Deserialize)]
pub struct ResetBody {
    token: String,
    password: String,
}

/// `POST /auth/reset {token, password}`: single use, 1 h; revokes every session.
pub async fn reset(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    JsonBody(b): JsonBody<ResetBody>,
) -> ApiResult<Json<Value>> {
    rate_guard(&st, "reset", &ip)?;
    check_password(&b.password)?;
    let (_, uid, _) = match consume_token(&st, &["reset_password"], &b.token).await {
        Ok(x) => x,
        Err(e) => {
            rate_fail(&st, "reset", &ip);
            return Err(e);
        }
    };
    let hash = bcrypt_hash(b.password).await?;
    let u2 = uid.clone();
    st.db
        .write(move |t| {
            t.execute(
                "UPDATE users SET password_hash=?2, token_version=token_version+1,
                        email_verified_at=coalesce(email_verified_at, ?3) WHERE id=?1",
                params![u2, hash, sf_core::now_rfc3339()],
            )
            .map_err(err)?;
            crate::sessions::revoke_all_sync(t, &u2)?;
            Ok(())
        })
        .await?;
    st.auth_cache.clear();
    ev(
        &st,
        &format!("user:{uid}"),
        "password reset",
        "ok",
        Some(&uid),
        json!({"ip": ip}),
    );
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct ChangeEmailBody {
    new_email: String,
    password: String,
}

fn session_only(u: &User) -> ApiResult<()> {
    if u.token.is_some() {
        return Err(ApiError::forbidden(
            "API tokens cannot change account security; use an interactive session",
        ));
    }
    Ok(())
}

async fn check_own_password(st: &AppState, uid: &str, pw: &str) -> ApiResult<()> {
    let id = uid.to_string();
    let hash: String = st
        .db
        .read(move |c| {
            c.query_row("SELECT password_hash FROM users WHERE id=?1", [&id], |r| {
                r.get(0)
            })
            .optional()
            .map_err(err)
        })
        .await?
        .ok_or_else(|| ApiError::unauthorized("unauthorized"))?;
    let pw: String = pw.chars().take(MAX_PASSWORD).collect();
    if bcrypt_verify(pw, hash).await {
        Ok(())
    } else {
        Err(ApiError::bad("current password is incorrect"))
    }
}

/// `POST /auth/change-email {new_email, password}`.
pub async fn change_email(
    State(st): State<AppState>,
    u: User,
    headers: HeaderMap,
    JsonBody(b): JsonBody<ChangeEmailBody>,
) -> ApiResult<Json<Value>> {
    session_only(&u)?;
    if !limiter(&st).allow(
        &format!("chmail:{}", u.user_id),
        5,
        Duration::from_secs(3600),
    ) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "try again later",
        ));
    }
    check_own_password(&st, &u.user_id, &b.password).await?;
    let email = b.new_email.trim().to_lowercase();
    if !valid_email(&email) {
        return Err(ApiError::bad("invalid email address"));
    }
    let e2 = email.clone();
    let taken: bool = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT count(*) > 0 FROM users WHERE email=?1",
                [&e2],
                |r| r.get(0),
            )
            .map_err(err)
        })
        .await?;
    if taken {
        return Err(ApiError::conflict("email already registered"));
    }
    let (u2, e3) = (u.user_id.clone(), email.clone());
    st.db
        .write(move |t| {
            t.execute(
                "UPDATE users SET pending_email=?2 WHERE id=?1",
                params![u2, e3],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    let p = crate::platform::get(&st).await;
    let token = issue_token(&st, "change_email", &u.user_id, &email, VERIFY_TTL_SECS).await?;
    let link = mail::link(
        &p,
        mail::request_host(&headers).as_deref(),
        &format!("/verify-email?token={token}"),
    );
    let app = p.branding.app_name.clone();
    let d = mail::send(
        &st,
        Mail {
            to: &email,
            subject: &format!("Confirm your new email for {app}"),
            body: format!(
                "Hello,\n\nconfirm this address as the new sign-in email of your {app} account:\n\n{link}\n\nThe link is valid for 48 hours.\n"
            ),
            kind: "change_email",
            link: Some(link),
        },
    )
    .await?;
    ev(
        &st,
        &u.actor(),
        "email change requested",
        "ok",
        Some(&u.user_id),
        json!({}),
    );
    Ok(Json(
        json!({ "ok": true, "pending_email": email, "delivery": d.as_str() }),
    ))
}

// ---------------------------------------------------------------------------
// me
// ---------------------------------------------------------------------------

/// `GET /auth/me`.
pub async fn me(State(st): State<AppState>, u: User) -> ApiResult<Json<Value>> {
    let id = u.user_id.clone();
    let row = st
        .db
        .read(move |c| {
            let user = c
                .query_row(
                    "SELECT id, email, name, role, email_verified_at, pending_email,
                            totp_secret_enc IS NOT NULL AND totp_secret_enc <> '', created_at,
                            (SELECT count(*) FROM user_recovery_codes r WHERE r.user_id = users.id AND r.used_at IS NULL)
                       FROM users WHERE id=?1",
                    [&id],
                    |r| {
                        let verified: Option<String> = r.get(4)?;
                        Ok(json!({
                            "id": r.get::<_, String>(0)?,
                            "email": r.get::<_, String>(1)?,
                            "name": r.get::<_, String>(2)?,
                            "role": r.get::<_, String>(3)?,
                            "email_verified": verified.is_some(),
                            "email_verified_at": verified,
                            "pending_email": r.get::<_, Option<String>>(5)?,
                            "two_factor_enabled": r.get::<_, bool>(6)?,
                            "created_at": r.get::<_, String>(7)?,
                            "recovery_codes_left": r.get::<_, i64>(8)?,
                        }))
                    },
                )
                .optional()
                .map_err(err)?;
            let mut q = c
                .prepare(
                    "SELECT w.id, w.name, w.slug, m.role, w.status, p.id, p.name
                       FROM workspace_members m JOIN workspaces w ON w.id = m.workspace_id
                       JOIN plans p ON p.id = w.plan_id
                      WHERE m.user_id = ?1 AND w.deleted_at IS NULL
                      ORDER BY w.is_default DESC, m.created_at, w.id",
                )
                .map_err(err)?;
            let ws = q
                .query_map([&id], |r| {
                    Ok(json!({
                        "id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?,
                        "slug": r.get::<_, String>(2)?, "role": r.get::<_, String>(3)?,
                        "status": r.get::<_, String>(4)?,
                        "plan": {"id": r.get::<_, String>(5)?, "name": r.get::<_, String>(6)?},
                    }))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(user.map(|u| (u, ws)))
        })
        .await?;
    let Some((mut user, ws)) = row else {
        return Err(ApiError::unauthorized("unauthorized"));
    };
    let def = tenancy::default_ws(&st, &u).await?.map(|w| w.id);
    if let Some(o) = user.as_object_mut() {
        o.insert("platform_admin".into(), json!(u.is_admin()));
        o.insert("default_workspace_id".into(), json!(def));
        o.insert("workspaces".into(), Value::Array(ws));
        o.insert("session_id".into(), json!(u.session_id));
    }
    Ok(Json(user))
}

#[derive(Deserialize)]
pub struct PatchMe {
    #[serde(default)]
    name: Option<String>,
}

/// `PATCH /auth/me {name}`.
pub async fn patch_me(
    State(st): State<AppState>,
    u: User,
    JsonBody(b): JsonBody<PatchMe>,
) -> ApiResult<Json<Value>> {
    if let Some(n) = b.name {
        let n: String = n.trim().chars().take(200).collect();
        let id = u.user_id.clone();
        st.db
            .write(move |t| {
                t.execute("UPDATE users SET name=?2 WHERE id=?1", params![id, n])
                    .map_err(err)?;
                Ok(())
            })
            .await?;
    }
    me(State(st), u).await
}

// ---------------------------------------------------------------------------
// 2FA
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PasswordBody {
    password: String,
}

/// `POST /auth/2fa/setup {password}` → `{secret, otpauth_uri}` (pending until enabled).
pub async fn twofa_setup(
    State(st): State<AppState>,
    u: User,
    JsonBody(b): JsonBody<PasswordBody>,
) -> ApiResult<Json<Value>> {
    session_only(&u)?;
    check_own_password(&st, &u.user_id, &b.password).await?;
    let secret = sf_core::totp::new_secret();
    let enc = crypto::encrypt_secret(&st.secret_key, &secret)?;
    let id = u.user_id.clone();
    let email: String = st
        .db
        .write(move |t| {
            t.execute(
                "UPDATE users SET totp_pending_enc=?2 WHERE id=?1",
                params![id, enc],
            )
            .map_err(err)?;
            t.query_row("SELECT email FROM users WHERE id=?1", [&id], |r| r.get(0))
                .map_err(err)
        })
        .await?;
    let p = crate::platform::get(&st).await;
    let uri = sf_core::totp::otpauth_uri(&p.branding.app_name, &email, &secret);
    Ok(Json(json!({ "secret": secret, "otpauth_uri": uri })))
}

#[derive(Deserialize)]
pub struct CodeBody {
    code: String,
}

async fn new_recovery_codes(st: &AppState, uid: &str) -> ApiResult<Vec<String>> {
    let codes = sf_core::totp::recovery_codes(RECOVERY_CODES);
    let hashes: Vec<String> = codes
        .iter()
        .map(|c| crypto::sha256_hex(format!("{uid}:{c}").as_bytes()))
        .collect();
    let u = uid.to_string();
    st.db
        .write(move |t| {
            t.execute("DELETE FROM user_recovery_codes WHERE user_id=?1", [&u])
                .map_err(err)?;
            for h in &hashes {
                t.execute(
                    "INSERT INTO user_recovery_codes(user_id, code_hash) VALUES(?1,?2)",
                    params![u, h],
                )
                .map_err(err)?;
            }
            Ok(())
        })
        .await?;
    Ok(codes)
}

/// `POST /auth/2fa/enable {code}` → `{enabled, recovery_codes}`.
pub async fn twofa_enable(
    State(st): State<AppState>,
    u: User,
    JsonBody(b): JsonBody<CodeBody>,
) -> ApiResult<Json<Value>> {
    session_only(&u)?;
    let id = u.user_id.clone();
    let pending: Option<String> = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT totp_pending_enc FROM users WHERE id=?1",
                [&id],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)
        })
        .await?
        .flatten();
    let Some(pending) = pending.filter(|p| !p.is_empty()) else {
        return Err(ApiError::bad("start the two-factor setup first"));
    };
    let secret = crypto::decrypt_secret(&st.secret_key, &pending)?;
    let Some(step) = sf_core::totp::verify(&secret, &b.code, sf_core::now_unix()) else {
        return Err(ApiError::bad("invalid code"));
    };
    let id = u.user_id.clone();
    st.db
        .write(move |t| {
            t.execute(
                "UPDATE users SET totp_secret_enc=totp_pending_enc, totp_pending_enc=NULL, totp_last_step=?2 WHERE id=?1",
                params![id, step],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    let codes = new_recovery_codes(&st, &u.user_id).await?;
    ev(
        &st,
        &u.actor(),
        "2fa enabled",
        "ok",
        Some(&u.user_id),
        json!({}),
    );
    Ok(Json(json!({ "enabled": true, "recovery_codes": codes })))
}

#[derive(Deserialize)]
pub struct DisableBody {
    password: String,
    code: String,
}

/// `POST /auth/2fa/disable {password, code}`.
pub async fn twofa_disable(
    State(st): State<AppState>,
    u: User,
    JsonBody(b): JsonBody<DisableBody>,
) -> ApiResult<Json<Value>> {
    session_only(&u)?;
    check_own_password(&st, &u.user_id, &b.password).await?;
    let id = u.user_id.clone();
    let secret: Option<String> = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT totp_secret_enc FROM users WHERE id=?1",
                [&id],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)
        })
        .await?
        .flatten();
    if secret.as_deref().is_none_or(str::is_empty) {
        return Ok(Json(json!({ "enabled": false })));
    }
    if !verify_second_factor(&st, &u.user_id, secret.as_deref(), &b.code).await? {
        return Err(ApiError::bad("invalid code"));
    }
    clear_2fa(&st, &u.user_id).await?;
    ev(
        &st,
        &u.actor(),
        "2fa disabled",
        "ok",
        Some(&u.user_id),
        json!({}),
    );
    Ok(Json(json!({ "enabled": false })))
}

/// Removes 2FA (user disable / admin reset).
pub async fn clear_2fa(st: &AppState, uid: &str) -> ApiResult<()> {
    let id = uid.to_string();
    st.db
        .write(move |t| {
            t.execute(
                "UPDATE users SET totp_secret_enc=NULL, totp_pending_enc=NULL, totp_last_step=0 WHERE id=?1",
                [&id],
            )
            .map_err(err)?;
            t.execute("DELETE FROM user_recovery_codes WHERE user_id=?1", [&id])
                .map_err(err)?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// `POST /auth/2fa/recovery-codes {password}` → new codes (old ones stop working).
pub async fn twofa_recovery(
    State(st): State<AppState>,
    u: User,
    JsonBody(b): JsonBody<PasswordBody>,
) -> ApiResult<Json<Value>> {
    session_only(&u)?;
    check_own_password(&st, &u.user_id, &b.password).await?;
    let id = u.user_id.clone();
    let on: bool = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT totp_secret_enc IS NOT NULL AND totp_secret_enc <> '' FROM users WHERE id=?1",
                [&id],
                |r| r.get(0),
            )
            .map_err(err)
        })
        .await?;
    if !on {
        return Err(ApiError::bad("two-factor authentication is not enabled"));
    }
    let codes = new_recovery_codes(&st, &u.user_id).await?;
    Ok(Json(json!({ "recovery_codes": codes })))
}

// ---------------------------------------------------------------------------
// sessions
// ---------------------------------------------------------------------------

/// `GET /auth/sessions`.
pub async fn sessions_list(State(st): State<AppState>, u: User) -> ApiResult<Json<Value>> {
    Ok(Json(
        crate::sessions::list(&st, &u.user_id, u.session_id.as_deref()).await?,
    ))
}

/// `DELETE /auth/sessions/{id}`.
pub async fn session_revoke(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    session_only(&u)?;
    if !crate::sessions::revoke(&st, Some(&u.user_id), &id).await? {
        return Err(ApiError::not_found("session not found"));
    }
    ev(
        &st,
        &u.actor(),
        "session revoked",
        "ok",
        Some(&u.user_id),
        json!({"session_id": id}),
    );
    Ok(Json(json!({ "revoked": true })))
}

/// `POST /auth/logout`: revokes the current session (tokens without `sid` just expire).
pub async fn logout(State(st): State<AppState>, u: User) -> ApiResult<Json<Value>> {
    let revoked = match &u.session_id {
        Some(sid) => crate::sessions::revoke(&st, Some(&u.user_id), sid).await?,
        None => false,
    };
    ev(
        &st,
        &u.actor(),
        "logout",
        "ok",
        Some(&u.user_id),
        json!({"session_revoked": revoked}),
    );
    Ok(Json(json!({ "ok": true, "session_revoked": revoked })))
}

/// Can `u` create another workspace (self-service)?
pub async fn may_create_workspace(st: &AppState, u: &User) -> ApiResult<()> {
    if u.is_admin() {
        return Ok(());
    }
    let p = crate::platform::get(st).await;
    if p.mode(st.cfg.auth.allow_registration) == "closed" {
        return Err(ApiError::forbidden(
            "creating workspaces is disabled; ask the platform admin",
        ));
    }
    let id = u.user_id.clone();
    let owned: i64 = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT count(*) FROM workspaces WHERE owner_user_id=?1 AND deleted_at IS NULL",
                [&id],
                |r| r.get(0),
            )
            .map_err(err)
        })
        .await?;
    if owned >= MAX_OWNED_WORKSPACES {
        return Err(ApiError::forbidden(format!(
            "you already own {owned} workspaces (max {MAX_OWNED_WORKSPACES})"
        )));
    }
    Ok(())
}
