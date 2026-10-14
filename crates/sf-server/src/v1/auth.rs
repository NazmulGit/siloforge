//! `/auth/*`: register, login, me, change-password, handshake (WP plugin).

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sf_core::crypto;
use sf_store::err;

use super::common::{
    ClientIp, JsonBody, audit_user, conflict_as, lockout, rate_check, rate_fail, rate_guard,
};
use crate::auth::User;
use crate::{ApiError, ApiResult, AppState};

const MAX_PASSWORD: usize = 1024;

fn valid_email(email: &str) -> bool {
    if email.len() > 320 || email.chars().any(char::is_whitespace) {
        return false;
    }
    let mut parts = email.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && domain
            .char_indices()
            .any(|(i, c)| c == '.' && i > 0 && i + 1 < domain.len())
}

/// Host header is an IP literal (v4 / `[v6]`) or `localhost`, with an optional port.
pub(crate) fn bootstrap_host_ok(headers: &HeaderMap) -> bool {
    let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
    else {
        // HTTP/1.0 without Host or an in-process call: no rebinding vector.
        return true;
    };
    let host = host.trim().to_ascii_lowercase();
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((v6, _)) => return v6.parse::<std::net::Ipv6Addr>().is_ok(),
            None => return false,
        }
    } else {
        host.rsplit_once(':')
            .filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit()))
            .map_or(host.as_str(), |(h, _)| h)
    };
    name == "localhost" || name.parse::<std::net::Ipv4Addr>().is_ok()
}

fn sign_user(st: &AppState, role: &str, id: &str, tv: i64) -> ApiResult<String> {
    Ok(crypto::sign_jwt(
        &st.cfg.auth.jwt_secret,
        &crypto::user_claims(role, id, tv),
    )?)
}

async fn bcrypt_hash(pw: String) -> ApiResult<String> {
    tokio::task::spawn_blocking(move || crypto::hash_password(&pw))
        .await
        .map_err(|e| ApiError::internal(format!("hash task: {e}")))?
        .map_err(Into::into)
}

async fn bcrypt_verify(pw: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || crypto::verify_password(&pw, &hash))
        .await
        .unwrap_or(false)
}

#[derive(Deserialize)]
pub struct RegisterBody {
    email: String,
    password: String,
    #[serde(default)]
    name: Option<String>,
}

/// Registration is closed unless `auth.allow_registration` (fixes v1 S6) or no user exists yet; the
/// very first account becomes the admin.
pub async fn register(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    JsonBody(b): JsonBody<RegisterBody>,
) -> ApiResult<Response> {
    rate_check(&st, "register", &ip)?;
    let users: i64 = st
        .db
        .read(|c| {
            c.query_row("SELECT count(*) FROM users", [], |r| r.get(0))
                .map_err(err)
        })
        .await?;
    if users > 0 && !st.cfg.auth.allow_registration {
        return Err(ApiError::forbidden("registration is disabled"));
    }
    // L-17: the first-run bootstrap (becomes admin) only answers a Host that is an IP literal or
    // `localhost`, so a web page using DNS rebinding cannot claim a fresh install.
    if users == 0 && !bootstrap_host_ok(&headers) {
        return Err(ApiError::forbidden(
            "first-run registration must use http://127.0.0.1:<port> or http://localhost:<port>",
        ));
    }
    let email = b.email.trim().to_lowercase();
    if !valid_email(&email) {
        return Err(ApiError::bad("invalid email address"));
    }
    let pw_len = b.password.chars().count();
    if pw_len < 8 {
        return Err(ApiError::bad("password must be at least 8 characters"));
    }
    if pw_len > MAX_PASSWORD {
        return Err(ApiError::bad("password is too long"));
    }
    let name: String = b
        .name
        .unwrap_or_default()
        .trim()
        .chars()
        .take(200)
        .collect();
    let hash = bcrypt_hash(b.password).await?;
    let id = sf_core::new_id();
    let (id2, email2, name2) = (id.clone(), email.clone(), name.clone());
    let role = st
        .db
        .write(move |t| {
            let n: i64 = t
                .query_row("SELECT count(*) FROM users", [], |r| r.get(0))
                .map_err(err)?;
            let role = if n == 0 { "admin" } else { "user" };
            t.execute(
                "INSERT INTO users(id, email, password_hash, name, role) VALUES(?1,?2,?3,?4,?5)",
                params![id2, email2, hash, name2, role],
            )
            .map_err(err)?;
            Ok(role.to_string())
        })
        .await
        .map_err(|e| conflict_as(e, "email already registered"))?;
    let token = sign_user(&st, &role, &id, 0)?;
    crate::auth::audit(
        &st,
        &format!("user:{id}"),
        "ui",
        "auth.register",
        Some(&id),
        json!({ "role": role }),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "token": token, "user": { "id": id, "email": email, "name": name, "role": role } })),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct LoginBody {
    email: String,
    password: String,
}

pub async fn login(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    JsonBody(b): JsonBody<LoginBody>,
) -> ApiResult<Json<Value>> {
    // M-03: per-IP limit on failures (successes are free) plus per-account lockout.
    rate_guard(&st, "login", &ip)?;
    let email = b.email.trim().to_lowercase();
    if let Some(d) = lockout(&st).locked_for(&email) {
        crate::eventlog::log(&st).record_auth(
            "anonymous",
            "login refused (account locked)",
            "denied",
            None,
            Some(&email),
            json!({"ip": ip.clone()}),
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
    let row = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT id, email, name, role, password_hash, token_version FROM users WHERE email=?1",
                [&email],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(err)
        })
        .await?;
    // Always run one bcrypt compare so timing does not reveal whether the email exists.
    let hash = row
        .as_ref()
        .map(|r| r.4.clone())
        .unwrap_or_else(|| crypto::DUMMY_BCRYPT.to_string());
    let pw: String = b.password.chars().take(MAX_PASSWORD).collect();
    let ok = bcrypt_verify(pw.clone(), hash).await;
    let Some((id, email, name, role, old_hash, tv)) = row.filter(|_| ok) else {
        rate_fail(&st, "login", &ip);
        let ev = crate::eventlog::log(&st);
        ev.record_auth(
            "anonymous",
            "login failed",
            "denied",
            None,
            Some(&account),
            json!({"ip": ip.clone()}),
        );
        if let Some(d) = lockout(&st).fail(&account) {
            tracing::warn!(
                lock_secs = d.as_secs(),
                "login: account locked after repeated failures"
            );
            ev.record_auth(
                "anonymous",
                "account locked",
                "denied",
                None,
                Some(&account),
                json!({"ip": ip.clone(), "lock_secs": d.as_secs()}),
            );
        }
        return Err(ApiError::unauthorized("invalid email or password"));
    };
    lockout(&st).success(&account);
    crate::eventlog::log(&st).record_auth(
        &format!("user:{id}"),
        "login ok",
        "ok",
        Some(&id),
        Some(&id),
        json!({"ip": ip.clone()}),
    );
    // L-15: upgrade a weaker (v1 cost 10) hash in the background; the session is unaffected.
    if crypto::password_needs_rehash(&old_hash) {
        let (st2, uid) = (st.clone(), id.clone());
        tokio::spawn(async move {
            let Ok(new_hash) = bcrypt_hash(pw).await else {
                return;
            };
            let res = st2
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
            if let Err(e) = res {
                tracing::warn!(error = %e, "login: password rehash failed");
            }
        });
    }
    let token = sign_user(&st, &role, &id, tv)?;
    Ok(Json(json!({
        "token": token,
        "user": { "id": id, "email": email, "name": name, "role": role },
    })))
}

pub async fn me(State(st): State<AppState>, u: User) -> ApiResult<Json<Value>> {
    let id = u.user_id.clone();
    let row = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT id, email, name, role FROM users WHERE id=?1",
                [&id],
                |r| {
                    Ok(json!({
                        "id": r.get::<_, String>(0)?,
                        "email": r.get::<_, String>(1)?,
                        "name": r.get::<_, String>(2)?,
                        "role": r.get::<_, String>(3)?,
                    }))
                },
            )
            .optional()
            .map_err(err)
        })
        .await?;
    row.map(Json)
        .ok_or_else(|| ApiError::unauthorized("unauthorized"))
}

#[derive(Deserialize)]
pub struct ChangePasswordBody {
    current_password: String,
    new_password: String,
}

/// Bumps `users.token_version`, so every older session token is revoked (fixes v1 S4). The response
/// carries a fresh `token` for the caller (v1 returned only `{ok:true}`).
pub async fn change_password(
    State(st): State<AppState>,
    u: User,
    JsonBody(b): JsonBody<ChangePasswordBody>,
) -> ApiResult<Json<Value>> {
    if u.token.is_some() {
        return Err(ApiError::forbidden(
            "API tokens cannot change passwords; use an interactive session",
        ));
    }
    let id = u.user_id.clone();
    let hash = st
        .db
        .read(move |c| {
            c.query_row("SELECT password_hash FROM users WHERE id=?1", [&id], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(err)
        })
        .await?
        .ok_or_else(|| ApiError::unauthorized("unauthorized"))?;
    let cur: String = b.current_password.chars().take(MAX_PASSWORD).collect();
    if !bcrypt_verify(cur, hash).await {
        return Err(ApiError::bad("current password is incorrect"));
    }
    let n = b.new_password.chars().count();
    if n < 8 {
        return Err(ApiError::bad("new password must be at least 8 characters"));
    }
    if n > MAX_PASSWORD {
        return Err(ApiError::bad("new password is too long"));
    }
    let new_hash = bcrypt_hash(b.new_password).await?;
    let id = u.user_id.clone();
    let (role, tv) = st
        .db
        .write(move |t| {
            t.execute(
                "UPDATE users SET password_hash=?2, token_version=token_version+1 WHERE id=?1",
                params![id, new_hash],
            )
            .map_err(err)?;
            t.query_row(
                "SELECT role, token_version FROM users WHERE id=?1",
                [&id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .map_err(err)
        })
        .await?;
    st.auth_cache.clear();
    audit_user(&st, &u, "auth.change_password", Some(&u.user_id), json!({})).await;
    let token = sign_user(&st, &role, &u.user_id, tv)?;
    Ok(Json(json!({ "ok": true, "token": token })))
}

#[derive(Deserialize)]
pub struct HandshakeBody {
    api_key: String,
}

/// WP plugin handshake. Must answer exactly 200 with `{token, site_id}` (the plugin checks `200 ===`).
pub async fn handshake(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    JsonBody(b): JsonBody<HandshakeBody>,
) -> ApiResult<Json<Value>> {
    // M-03: keyed separately from human login ("handshake:<ip>"), failures only.
    rate_guard(&st, "handshake", &ip)?;
    let key = b.api_key.trim().to_string();
    if key.is_empty() || key.len() > 256 {
        // Same work as a miss below.
        let _ = crypto::constant_time_eq(&crypto::sha256_hex(key.as_bytes()), "");
        rate_fail(&st, "handshake", &ip);
        return Err(ApiError::unauthorized("invalid api key"));
    }
    let hashed = crypto::site_key_stored(&key);
    let (k2, h2) = (key.clone(), hashed.clone());
    let row = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT id, api_key FROM sites WHERE api_key=?1 OR api_key=?2 LIMIT 1",
                params![h2, k2],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(err)
        })
        .await?;
    let stored = row
        .as_ref()
        .map(|r| r.1.clone())
        .unwrap_or_else(|| crypto::site_key_stored("sf_00000000000000000000000000000000"));
    let matches = crypto::site_key_matches(&stored, &key);
    let Some((site_id, _)) = row.filter(|_| matches) else {
        rate_fail(&st, "handshake", &ip);
        return Err(ApiError::unauthorized("invalid api key"));
    };
    // L-02: a legacy plaintext key is hashed in place on first use (same JWT fingerprint).
    if !crypto::site_key_is_hashed(&stored) {
        let sid = site_id.clone();
        let (h3, old) = (hashed.clone(), stored.clone());
        if let Err(e) = st
            .db
            .write(move |t| {
                t.execute(
                    "UPDATE sites SET api_key=?2 WHERE id=?1 AND api_key=?3",
                    params![sid, h3, old],
                )
                .map_err(err)?;
                Ok(())
            })
            .await
        {
            tracing::warn!(error = %e, "handshake: could not hash legacy site key");
        }
    }
    let token = crypto::sign_jwt(
        &st.cfg.auth.jwt_secret,
        &crypto::site_claims(&site_id, &stored),
    )?;
    crate::auth::audit(
        &st,
        &format!("site:{site_id}"),
        "plugin",
        "auth.handshake",
        Some(&site_id),
        json!({}),
    )
    .await;
    Ok(Json(json!({ "token": token, "site_id": site_id })))
}

#[cfg(test)]
mod tests {
    use super::{bootstrap_host_ok, valid_email};
    use axum::http::{HeaderMap, HeaderValue, header};

    #[test]
    fn bootstrap_host_rule() {
        let h = |v: &str| {
            let mut m = HeaderMap::new();
            m.insert(header::HOST, HeaderValue::from_str(v).unwrap());
            m
        };
        for ok in [
            "127.0.0.1:8600",
            "localhost:8601",
            "LOCALHOST",
            "192.168.1.5",
            "[::1]:8600",
        ] {
            assert!(bootstrap_host_ok(&h(ok)), "{ok}");
        }
        for bad in [
            "evil.example:8600",
            "rebind.attacker.test",
            "[nope]",
            "localhost.evil.com",
        ] {
            assert!(!bootstrap_host_ok(&h(bad)), "{bad}");
        }
        assert!(bootstrap_host_ok(&HeaderMap::new()));
    }

    #[test]
    fn email_rule_matches_v1_regex() {
        assert!(valid_email("a@b.co"));
        assert!(!valid_email("a@b."));
        assert!(!valid_email("a@.b"));
        assert!(!valid_email("ab.co"));
        assert!(!valid_email("a b@c.de"));
        assert!(!valid_email("a@b@c.de"));
        assert!(valid_email("x@sub.domain.com"));
    }
}
