//! Authentication.
//!
//! `Authorization: Bearer <credential>` where the credential is one of:
//! * a v1-compatible HS256 JWT for a human (`role` admin/user + `user_id`). Unlike v1, the user row is
//!   re-checked (exists, current role, `token_version`), so demotion, deletion and password change revoke
//!   tokens (fixes v1 S4). The result is cached for 30 s.
//! * a site JWT (`role: site`, `site_id`) from the WP plugin handshake;
//! * a v2 API token `sfk_...` (hashed in `api_tokens`) with scopes `*` | `read` | `write` | `admin`.
//!   It acts as its owner user, limited by the scopes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::FromRequestParts;
use axum::http::Method;
use axum::http::request::Parts;
use parking_lot::Mutex;
use rusqlite::OptionalExtension;
use sf_store::err;

use crate::{ApiError, AppState};

/// `kv` key prefix (`<prefix><site_id>`) written by rotate-key: from then on v1-issued site
/// tokens without `tv` are refused (L-01).
pub const SITE_KEY_ROTATED_KV: &str = "site_key_rotated:";

#[derive(Debug, Clone, PartialEq)]
pub enum Principal {
    User {
        user_id: String,
        role: String,
        /// `None` for UI/JWT sessions; `Some(token name)` for API tokens.
        token: Option<String>,
        scopes: Vec<String>,
    },
    Site {
        site_id: String,
    },
}

/// Any authenticated caller (user, API token or site). Use `User`/`Admin` for stricter routes.
#[derive(Debug, Clone)]
pub struct Auth(pub Principal);

/// A human user or API token (not a site token). 401 for site tokens.
#[derive(Debug, Clone)]
pub struct User {
    pub user_id: String,
    pub role: String,
    pub token: Option<String>,
    pub scopes: Vec<String>,
}

/// Admin user (403 otherwise).
#[derive(Debug, Clone)]
pub struct Admin(pub User);

impl User {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }
    /// Actor label for the audit log.
    pub fn actor(&self) -> String {
        match &self.token {
            Some(t) => format!("token:{t}"),
            None => format!("user:{}", self.user_id),
        }
    }
    pub fn via(&self) -> &'static str {
        if self.token.is_some() { "api" } else { "ui" }
    }
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == "*" || s == scope)
    }
}

#[derive(Clone, Default)]
pub struct AuthCache {
    inner: Arc<Mutex<HashMap<String, (Instant, Principal)>>>,
}

const CACHE_TTL: Duration = Duration::from_secs(30);
const CACHE_MAX: usize = 4096;

impl AuthCache {
    fn get(&self, key: &str) -> Option<Principal> {
        let map = self.inner.lock();
        map.get(key)
            .filter(|(t, _)| t.elapsed() < CACHE_TTL)
            .map(|(_, p)| p.clone())
    }
    fn put(&self, key: String, p: Principal) {
        let mut map = self.inner.lock();
        if map.len() >= CACHE_MAX {
            map.retain(|_, (t, _)| t.elapsed() < CACHE_TTL);
            if map.len() >= CACHE_MAX {
                map.clear();
            }
        }
        map.insert(key, (Instant::now(), p));
    }
    /// Drop everything (call after password change, role change, token revoke, user delete).
    pub fn clear(&self) {
        self.inner.lock().clear();
    }
}

fn bearer(parts: &Parts) -> Option<String> {
    let h = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, value) = h.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let v = value.trim();
    (!v.is_empty() && v.len() <= 4096).then(|| v.to_string())
}

/// Resolves a bearer credential to a principal (no scope/method checks).
pub async fn resolve(st: &AppState, cred: &str) -> Result<Principal, ApiError> {
    let cache_key = sf_core::crypto::sha256_hex(cred.as_bytes());
    if let Some(p) = st.auth_cache.get(&cache_key) {
        return Ok(p);
    }
    let principal = if cred.starts_with("sfk_") {
        let hash = cache_key.clone();
        let row = st
            .db
            .read(move |c| {
                c.query_row(
                    "SELECT t.id, t.name, t.scopes, t.expires_at, u.id, u.role FROM api_tokens t
                     JOIN users u ON u.id = t.user_id
                     WHERE t.token_hash=?1 AND t.revoked_at IS NULL",
                    [&hash],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, String>(5)?,
                        ))
                    },
                )
                .optional()
                .map_err(err)
            })
            .await?;
        let Some((token_id, name, scopes, expires, user_id, role)) = row else {
            return Err(ApiError::unauthorized("invalid API token"));
        };
        if let Some(exp) = expires
            && exp < sf_core::now_rfc3339()
        {
            return Err(ApiError::unauthorized("API token expired"));
        }
        let scopes: Vec<String> =
            serde_json::from_str(&scopes).unwrap_or_else(|_| vec!["read".into()]);
        // last_used_at is best-effort and must not slow requests down.
        let db = st.db.clone();
        tokio::spawn(async move {
            let _ = db
                .write(move |t| {
                    t.execute(
                        "UPDATE api_tokens SET last_used_at=?2 WHERE id=?1",
                        rusqlite::params![token_id, sf_core::now_rfc3339()],
                    )
                    .map_err(err)?;
                    Ok(())
                })
                .await;
        });
        Principal::User {
            user_id,
            role,
            token: Some(name),
            scopes,
        }
    } else {
        let claims = sf_core::crypto::verify_jwt(&st.cfg.auth.jwt_secret, cred)?;
        match claims.role.as_str() {
            "site" => {
                let site_id = claims
                    .site_id
                    .ok_or_else(|| ApiError::unauthorized("invalid token"))?;
                let sid = site_id.clone();
                let key = st
                    .db
                    .read(move |c| {
                        c.query_row(
                            "SELECT api_key, EXISTS(SELECT 1 FROM kv WHERE key = ?2)
                               FROM sites WHERE id=?1",
                            rusqlite::params![sid, format!("{SITE_KEY_ROTATED_KV}{sid}")],
                            |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
                        )
                        .optional()
                        .map_err(err)
                    })
                    .await?;
                let Some((key, rotated)) = key else {
                    return Err(ApiError::unauthorized("site no longer exists"));
                };
                match claims.tv {
                    Some(tv) if tv != sf_core::crypto::key_fingerprint(&key) => {
                        return Err(ApiError::unauthorized(
                            "site key was rotated, handshake again",
                        ));
                    }
                    // L-01: a v1-issued site token (no `tv`) is honoured only while the site's
                    // key has never been rotated in v2.
                    None if rotated => {
                        return Err(ApiError::unauthorized(
                            "site key was rotated, handshake again",
                        ));
                    }
                    _ => {}
                }
                Principal::Site { site_id }
            }
            "admin" | "user" => {
                let user_id = claims
                    .user_id
                    .ok_or_else(|| ApiError::unauthorized("invalid token"))?;
                let uid = user_id.clone();
                let row = st
                    .db
                    .read(move |c| {
                        c.query_row(
                            "SELECT role, token_version FROM users WHERE id=?1",
                            [&uid],
                            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                        )
                        .optional()
                        .map_err(err)
                    })
                    .await?;
                let Some((role, tv)) = row else {
                    return Err(ApiError::unauthorized("user no longer exists"));
                };
                if claims.tv.unwrap_or(0) != tv {
                    return Err(ApiError::unauthorized(
                        "session revoked, please log in again",
                    ));
                }
                Principal::User {
                    user_id,
                    role,
                    token: None,
                    scopes: vec!["*".into()],
                }
            }
            _ => return Err(ApiError::unauthorized("invalid token")),
        }
    };
    st.auth_cache.put(cache_key, principal.clone());
    Ok(principal)
}

fn check_scope_for_method(scopes: &[String], method: &Method) -> Result<(), ApiError> {
    if scopes
        .iter()
        .any(|s| s == "*" || s == "admin" || s == "write")
    {
        return Ok(());
    }
    let read_only = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    if read_only && scopes.iter().any(|s| s == "read") {
        return Ok(());
    }
    Err(ApiError::forbidden(
        "API token scope does not allow this action",
    ))
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let cred = bearer(parts).ok_or_else(|| ApiError::unauthorized("missing bearer token"))?;
        let p = resolve(st, &cred).await?;
        if let Principal::User { scopes, .. } = &p {
            check_scope_for_method(scopes, &parts.method)?;
        }
        Ok(Auth(p))
    }
}

impl FromRequestParts<AppState> for User {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        match Auth::from_request_parts(parts, st).await?.0 {
            Principal::User {
                user_id,
                role,
                token,
                scopes,
            } => Ok(User {
                user_id,
                role,
                token,
                scopes,
            }),
            Principal::Site { .. } => Err(ApiError::unauthorized("user token required")),
        }
    }
}

impl FromRequestParts<AppState> for Admin {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let u = User::from_request_parts(parts, st).await?;
        if !u.is_admin() {
            return Err(ApiError::forbidden("admin only"));
        }
        // API tokens need the admin (or *) scope for admin routes.
        if u.token.is_some() && !u.scopes.iter().any(|s| s == "*" || s == "admin") {
            return Err(ApiError::forbidden("API token lacks admin scope"));
        }
        Ok(Admin(u))
    }
}

/// Ownership gate used by every site-scoped route: admin sees all, users see their own, site tokens see
/// only their site. Foreign sites are reported as 404 (never reveal existence).
pub async fn ensure_site_access(
    st: &AppState,
    who: &Principal,
    site_id: &str,
) -> Result<(), ApiError> {
    match who {
        Principal::Site { site_id: own } => {
            if own == site_id {
                Ok(())
            } else {
                Err(ApiError::not_found("site not found"))
            }
        }
        Principal::User { user_id, role, .. } => {
            let sid = site_id.to_string();
            let owner = st
                .db
                .read(move |c| {
                    c.query_row("SELECT user_id FROM sites WHERE id=?1", [&sid], |r| {
                        r.get::<_, Option<String>>(0)
                    })
                    .optional()
                    .map_err(err)
                })
                .await?;
            match owner {
                None => Err(ApiError::not_found("site not found")),
                Some(_) if role == "admin" => Ok(()),
                Some(Some(o)) if &o == user_id => Ok(()),
                Some(_) => Err(ApiError::not_found("site not found")),
            }
        }
    }
}

/// Writes one audit_log row (best effort, never fails the request).
pub async fn audit(
    st: &AppState,
    actor: &str,
    via: &str,
    action: &str,
    target: Option<&str>,
    detail: serde_json::Value,
) {
    let (actor, via, action) = (actor.to_string(), via.to_string(), action.to_string());
    let target = target.map(str::to_string);
    let res = st
        .db
        .write(move |t| {
            t.execute(
                "INSERT INTO audit_log(actor, via, action, target, detail) VALUES(?1,?2,?3,?4,?5)",
                rusqlite::params![actor, via, action, target, detail.to_string()],
            )
            .map_err(err)?;
            Ok(())
        })
        .await;
    if let Err(e) = res {
        tracing::warn!(error = %e, "audit log write failed");
    }
}
