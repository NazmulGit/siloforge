//! v2.4 login sessions (`sessions` table, JWT `sid` claim): create on login, touch on use
//! (throttled), list, revoke. Tokens without `sid` (v1 / pre-2.4) stay valid until they expire.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use parking_lot::Mutex;
use rusqlite::params;
use serde_json::{Value, json};
use sf_store::err;

use crate::{ApiResult, AppState};

/// "Remember me" lifetime (= the v1 token lifetime).
pub const REMEMBER_TTL_SECS: i64 = 30 * 24 * 3600;
/// Session without "remember me".
pub const SHORT_TTL_SECS: i64 = 24 * 3600;
const TOUCH_EVERY: Duration = Duration::from_secs(60);

fn ts_in(secs: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Bounded user agent for storage.
pub fn user_agent(h: &HeaderMap) -> String {
    h.get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(300)
        .collect()
}

/// Creates a session and signs its token: `{token, session_id, expires_at}`.
pub async fn issue(
    st: &AppState,
    user_id: &str,
    role: &str,
    tv: i64,
    remember: bool,
    ip: &str,
    ua: &str,
) -> ApiResult<Value> {
    let ttl = if remember {
        REMEMBER_TTL_SECS
    } else {
        SHORT_TTL_SECS
    };
    let sid = sf_core::new_id();
    let expires = ts_in(ttl);
    let (s2, u2, e2, ip2, ua2) = (
        sid.clone(),
        user_id.to_string(),
        expires.clone(),
        ip.chars().take(64).collect::<String>(),
        ua.to_string(),
    );
    st.db
        .write(move |t| {
            t.execute(
                "INSERT INTO sessions(id, user_id, expires_at, ip, user_agent, remember) VALUES(?1,?2,?3,?4,?5,?6)",
                params![s2, u2, e2, ip2, ua2, remember],
            )
            .map_err(err)?;
            t.execute(
                "UPDATE users SET last_login_at=?2 WHERE id=?1",
                params![u2, sf_core::now_rfc3339()],
            )
            .map_err(err)?;
            // keep the table bounded: drop sessions expired for more than 30 days
            t.execute(
                "DELETE FROM sessions WHERE user_id=?1 AND expires_at < ?2",
                params![u2, ts_in(-30 * 24 * 3600)],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    let token = sf_core::crypto::sign_jwt(
        &st.cfg.auth.jwt_secret,
        &sf_core::crypto::session_claims(role, user_id, tv, &sid, ttl),
    )?;
    Ok(json!({ "token": token, "session_id": sid, "expires_at": expires }))
}

/// Best-effort `last_seen_at` update, at most once a minute per session.
pub fn touch(st: &AppState, sid: String) {
    static SEEN: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);
    {
        let mut g = SEEN.lock();
        let m = g.get_or_insert_with(HashMap::new);
        if m.len() > 8192 {
            m.retain(|_, t| t.elapsed() < TOUCH_EVERY);
        }
        if m.get(&sid).is_some_and(|t| t.elapsed() < TOUCH_EVERY) {
            return;
        }
        m.insert(sid.clone(), Instant::now());
    }
    let db = st.db.clone();
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(async move {
            let _ = db
                .write(move |t| {
                    t.execute(
                        "UPDATE sessions SET last_seen_at=?2 WHERE id=?1",
                        params![sid, sf_core::now_rfc3339()],
                    )
                    .map_err(err)?;
                    Ok(())
                })
                .await;
        });
    }
}

/// Revokes one session of `user_id` (`None` = any user, admin). Returns whether a row changed.
pub async fn revoke(st: &AppState, user_id: Option<&str>, sid: &str) -> ApiResult<bool> {
    let (u, s) = (user_id.map(str::to_string), sid.to_string());
    let n = st
        .db
        .write(move |t| {
            t.execute(
                "UPDATE sessions SET revoked_at=?3 WHERE id=?1 AND (?2 IS NULL OR user_id=?2) AND revoked_at IS NULL",
                params![s, u, sf_core::now_rfc3339()],
            )
            .map_err(err)
        })
        .await?;
    st.auth_cache.clear();
    Ok(n > 0)
}

/// Revokes every session of a user (inside an existing transaction).
pub fn revoke_all_sync(t: &rusqlite::Transaction<'_>, user_id: &str) -> sf_core::Result<usize> {
    t.execute(
        "UPDATE sessions SET revoked_at=?2 WHERE user_id=?1 AND revoked_at IS NULL",
        params![user_id, sf_core::now_rfc3339()],
    )
    .map_err(err)
}

/// Sessions of a user, newest first (`current` marks `current_sid`).
pub async fn list(st: &AppState, user_id: &str, current_sid: Option<&str>) -> ApiResult<Value> {
    let (u, cur) = (user_id.to_string(), current_sid.map(str::to_string));
    let rows = st
        .db
        .read(move |c| {
            let mut q = c
                .prepare(
                    "SELECT id, created_at, last_seen_at, expires_at, ip, user_agent, remember, revoked_at
                       FROM sessions WHERE user_id=?1 ORDER BY created_at DESC LIMIT 100",
                )
                .map_err(err)?;
            let rows = q
                .query_map([&u], |r| {
                    let id: String = r.get(0)?;
                    Ok(json!({
                        "current": cur.as_deref() == Some(id.as_str()),
                        "id": id,
                        "created_at": r.get::<_, String>(1)?,
                        "last_seen_at": r.get::<_, String>(2)?,
                        "expires_at": r.get::<_, String>(3)?,
                        "ip": r.get::<_, String>(4)?,
                        "user_agent": r.get::<_, String>(5)?,
                        "remember": r.get::<_, bool>(6)?,
                        "revoked_at": r.get::<_, Option<String>>(7)?,
                    }))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?;
    Ok(json!({ "sessions": rows }))
}
