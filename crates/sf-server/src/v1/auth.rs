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

use super::common::{ClientIp, JsonBody, audit_user, rate_fail, rate_guard};
use crate::auth::User;
use crate::{ApiError, ApiResult, AppState};

pub(crate) const MAX_PASSWORD: usize = 1024;

pub(crate) fn valid_email(email: &str) -> bool {
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

pub(crate) async fn bcrypt_hash(pw: String) -> ApiResult<String> {
    tokio::task::spawn_blocking(move || crypto::hash_password(&pw))
        .await
        .map_err(|e| ApiError::internal(format!("hash task: {e}")))?
        .map_err(Into::into)
}

pub(crate) async fn bcrypt_verify(pw: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || crypto::verify_password(&pw, &hash))
        .await
        .unwrap_or(false)
}

/// Registration follows the v2.4 platform mode (closed by default, legacy
/// `auth.allow_registration=true` = open); the very first account becomes the admin (first-run
/// bootstrap, IP/localhost Host only). Answer keeps the v1 shape `201 {token, user}`.
pub async fn register(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    JsonBody(b): JsonBody<crate::v2::auth2::RegisterBody>,
) -> ApiResult<Response> {
    let v = crate::v2::auth2::register_core(&st, &ip, &headers, b).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "token": v["token"], "user": v["user"] })),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct LoginBody {
    email: String,
    password: String,
}

/// v1 login: `{token, user}` (30-day session). Users with 2FA must use `/api/v2/auth/login`.
pub async fn login(
    State(st): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    JsonBody(b): JsonBody<LoginBody>,
) -> ApiResult<Json<Value>> {
    let who = crate::v2::auth2::password_login(&st, &ip, &b.email, &b.password).await?;
    if who.totp {
        return Err(ApiError::unauthorized(
            "two-factor authentication required; sign in through the web app",
        ));
    }
    let s = crate::sessions::issue(
        &st,
        &who.id,
        &who.role,
        who.tv,
        true,
        &ip,
        &crate::sessions::user_agent(&headers),
    )
    .await?;
    crate::eventlog::log(&st).record_auth(
        &format!("user:{}", who.id),
        "login ok",
        "ok",
        Some(&who.id),
        Some(&who.id),
        json!({"ip": ip.clone()}),
    );
    Ok(Json(
        json!({ "token": s["token"], "user": who.user_json() }),
    ))
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
            crate::sessions::revoke_all_sync(t, &id)?;
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
