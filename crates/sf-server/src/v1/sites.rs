//! `/sites*`: list, create, get, patch (settings whitelist + credential sealing), rotate-key,
//! crawl / plan / archive-plan enqueue.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sf_core::types::{InsertModule, JobKind, Topology};
use sf_store::err;

use super::common::{
    JsonBody, SITE_COLS, SiteRow, audit_user, conflict_as, enqueue, get_site, guard_user_site,
    is_http_url, load_site, mutate_site, owner_scope,
};
use crate::auth::User;
use crate::{ApiError, ApiResult, AppState};

pub async fn list(State(st): State<AppState>, u: User) -> ApiResult<Json<Value>> {
    let scope = owner_scope(&u);
    let key = st.secret_key.clone();
    let out = st
        .db
        .read(move |c| {
            let scope_sql = "(?1 IS NULL OR s.user_id = ?1)";
            let mut stmt = c
                .prepare(&format!(
                    "SELECT {} FROM sites s WHERE {scope_sql} ORDER BY s.created_at ASC, s.id ASC",
                    SITE_COLS
                        .split(", ")
                        .map(|col| format!("s.{col}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .map_err(err)?;
            let sites = stmt
                .query_map([&scope], SiteRow::from_row)
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            // One grouped pass per table instead of correlated subqueries per site (v1 P4).
            let mut posts: HashMap<String, i64> = HashMap::new();
            let mut stmt = c
                .prepare(&format!(
                    "SELECT p.site_id, count(*) FROM posts p JOIN sites s ON s.id = p.site_id
                     WHERE {scope_sql} GROUP BY p.site_id"
                ))
                .map_err(err)?;
            let rows = stmt
                .query_map([&scope], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(err)?;
            for row in rows {
                let (sid, n) = row.map_err(err)?;
                posts.insert(sid, n);
            }
            let mut edges: HashMap<String, Map<String, Value>> = HashMap::new();
            let mut stmt = c
                .prepare(&format!(
                    "SELECT e.site_id, e.status, count(*) FROM link_edges e JOIN sites s ON s.id = e.site_id
                     WHERE {scope_sql} GROUP BY e.site_id, e.status"
                ))
                .map_err(err)?;
            let rows = stmt
                .query_map([&scope], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .map_err(err)?;
            for row in rows {
                let (sid, status, n) = row.map_err(err)?;
                edges.entry(sid).or_default().insert(status, Value::from(n));
            }
            Ok(sites
                .into_iter()
                .map(|s| {
                    let mut v = s.to_json(&key, false);
                    if let Some(o) = v.as_object_mut() {
                        o.insert(
                            "posts_count".into(),
                            Value::from(posts.get(&s.id).copied().unwrap_or(0)),
                        );
                        o.insert(
                            "edge_counts".into(),
                            Value::Object(edges.remove(&s.id).unwrap_or_default()),
                        );
                    }
                    v
                })
                .collect::<Vec<_>>())
        })
        .await?;
    Ok(Json(Value::Array(out)))
}

#[derive(Deserialize)]
pub struct CreateBody {
    domain: String,
    #[serde(default)]
    wp_url: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    /// v2 addition (admin only): let this site's own hosts resolve to private addresses.
    #[serde(default)]
    allow_private_host: Option<bool>,
}

/// Trims, drops a scheme and trailing slashes; rejects characters that could break headers/paths.
fn normalize_domain(raw: &str) -> ApiResult<String> {
    let mut d = raw.trim();
    for p in ["https://", "http://"] {
        if d.len() >= p.len() && d.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p)) {
            d = d.get(p.len()..).unwrap_or("");
        }
    }
    let d = d.trim_end_matches('/');
    if d.is_empty() {
        return Err(ApiError::bad("domain must not be empty"));
    }
    if d.len() > 253
        || d.chars()
            .any(|c| c.is_whitespace() || c.is_control() || "\"'<>\\`{}|^".contains(c))
    {
        return Err(ApiError::bad("domain contains invalid characters"));
    }
    // hostname[:port] only: no userinfo, path, query or fragment (M-01).
    if d.chars().any(|c| "@/?#".contains(c)) {
        return Err(ApiError::bad(
            "domain must be a host name with an optional port (no path, user info, query or fragment)",
        ));
    }
    Ok(d.to_string())
}

/// Host names (no port) a site's jobs connect to: the domain host and the `wp_url` host.
fn site_target_hosts(domain: &str, wp_url: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(h) = url::Url::parse(&format!("http://{domain}/"))
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
    {
        out.push(h);
    }
    if let Some(h) = wp_url
        .and_then(|w| url::Url::parse(w).ok())
        .and_then(|u| u.host_str().map(str::to_string))
    {
        out.push(h);
    }
    let mut out: Vec<String> = out
        .iter()
        .map(|h| sf_crawl::ssrf::normalize_host(h))
        .filter(|h| !h.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Addresses `host` resolves to the way the crawler would (configured alias first, then DNS
/// with a short timeout). A failed lookup yields no addresses: the crawl-time guard still applies.
async fn resolve_site_host(st: &AppState, host: &str) -> Vec<std::net::IpAddr> {
    let target = st
        .cfg
        .crawl
        .host_aliases
        .iter()
        .find(|(k, _)| sf_crawl::ssrf::normalize_host(k) == host)
        .map(|(_, v)| sf_crawl::ssrf::normalize_host(v))
        .unwrap_or_else(|| host.to_string());
    if let Ok(ip) = target.parse::<std::net::IpAddr>() {
        return vec![ip];
    }
    let lookup = tokio::net::lookup_host((target.as_str(), 0));
    match tokio::time::timeout(std::time::Duration::from_secs(3), lookup).await {
        Ok(Ok(addrs)) => addrs.map(|a| a.ip()).collect(),
        _ => Vec::new(),
    }
}

/// M-01: a site whose domain or `wp_url` resolves to a private / loopback / link-local address
/// needs an admin AND either `crawl.allow_private_hosts=true` or the site's admin-only
/// `allow_private_host` setting. Cloud-metadata addresses are never allowed.
async fn check_site_target(
    st: &AppState,
    u: &User,
    domain: &str,
    wp_url: Option<&str>,
    site_allows_private: bool,
) -> ApiResult<()> {
    for host in site_target_hosts(domain, wp_url) {
        let ips = resolve_site_host(st, &host).await;
        if let Some(ip) = ips.iter().find(|ip| sf_crawl::ssrf::is_metadata_ip(**ip)) {
            return Err(ApiError::bad(format!(
                "{host} resolves to the cloud-metadata address {ip}; such sites are not allowed"
            )));
        }
        let Some(ip) = ips.iter().find(|ip| sf_crawl::is_blocked_ip(**ip)) else {
            continue;
        };
        if !u.is_admin() {
            return Err(ApiError::bad(format!(
                "{host} resolves to a private, loopback or link-local address ({ip}). Only an admin can add a site on a private host"
            )));
        }
        if !(st.cfg.crawl.allow_private_hosts || site_allows_private) {
            return Err(ApiError::bad(format!(
                "{host} resolves to a private, loopback or link-local address ({ip}). Set crawl.allow_private_hosts=true or allow_private_host=true for this site"
            )));
        }
    }
    Ok(())
}

fn check_mode(m: &Option<String>) -> ApiResult<()> {
    match m.as_deref() {
        None | Some("wp_rest") | Some("sitemap") => Ok(()),
        Some(_) => Err(ApiError::bad("mode must be one of wp_rest, sitemap")),
    }
}

fn check_wp_url(u: &Option<String>) -> ApiResult<Option<String>> {
    match u.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(s)
            if s.len() <= 2048
                && is_http_url(s)
                && url::Url::parse(s)
                    .is_ok_and(|u| u.username().is_empty() && u.password().is_none()) =>
        {
            Ok(Some(s.trim_end_matches('/').into()))
        }
        Some(_) => Err(ApiError::bad(
            "wp_url must be an http(s) URL without user info",
        )),
    }
}

/// 201 with the full site row, including `api_key` (the only place, besides rotate-key, it is shown).
pub async fn create(
    State(st): State<AppState>,
    u: User,
    JsonBody(b): JsonBody<CreateBody>,
) -> ApiResult<Response> {
    let domain = normalize_domain(&b.domain)?;
    check_mode(&b.mode)?;
    let wp_url = check_wp_url(&b.wp_url)?;
    let allow_private = b.allow_private_host.unwrap_or(false);
    if allow_private && !u.is_admin() {
        return Err(ApiError::bad(
            "allow_private_host can only be set by an admin",
        ));
    }
    check_site_target(&st, &u, &domain, wp_url.as_deref(), allow_private).await?;
    let settings = if allow_private {
        json!({ "allow_private_host": true }).to_string()
    } else {
        "{}".to_string()
    };
    let id = sf_core::new_id();
    let api_key = sf_core::crypto::new_site_api_key();
    let (id2, uid) = (id.clone(), u.user_id.clone());
    let mode = b.mode.unwrap_or_else(|| "wp_rest".into());
    let site = st
        .db
        .write(move |t| {
            t.execute(
                "INSERT INTO sites(id, domain, wp_url, mode, api_key, user_id, settings) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![id2, domain, wp_url, mode, api_key, uid, settings],
            )
            .map_err(err)?;
            load_site(t, &id2)
        })
        .await
        .map_err(|e| conflict_as(e, "domain already exists"))?
        .ok_or_else(|| ApiError::internal("site vanished"))?;
    audit_user(
        &st,
        &u,
        "site.create",
        Some(&id),
        json!({ "domain": site.domain }),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(site.to_json(&st.secret_key, true)),
    )
        .into_response())
}

pub async fn get(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &id).await?;
    let site = get_site(&st, &id).await?;
    Ok(Json(site.to_json(&st.secret_key, false)))
}

#[derive(Deserialize)]
pub struct PatchBody {
    #[serde(default)]
    settings: Option<Value>,
    #[serde(default)]
    wp_url: Option<String>,
    #[serde(default)]
    mode: Option<String>,
}

fn int_in(v: &Value, key: &str, min: i64, max: i64) -> ApiResult<()> {
    match v.as_i64() {
        Some(n) if (min..=max).contains(&n) => Ok(()),
        _ => Err(ApiError::bad(format!(
            "settings.{key} must be a whole number between {min} and {max}"
        ))),
    }
}

/// Settings whitelist with type/range checks (fixes v1 S8). `null` removes a key.
fn validate_setting(key: &str, v: &Value) -> ApiResult<()> {
    if v.is_null() {
        return Ok(());
    }
    match key {
        "preset" => match v.as_str() {
            Some("conservative" | "balanced" | "aggressive") => Ok(()),
            _ => Err(ApiError::bad(
                "settings.preset must be conservative, balanced or aggressive",
            )),
        },
        "silo_topology" => match v.as_str().and_then(Topology::parse_known) {
            Some(_) => Ok(()),
            None => Err(ApiError::bad(format!(
                "settings.silo_topology must be one of {}",
                Topology::ALL.join(", ")
            ))),
        },
        "insert_modules" => match v.as_array() {
            Some(a)
                if a.len() <= 8
                    && a.iter()
                        .all(|m| m.as_str().and_then(InsertModule::parse_known).is_some()) =>
            {
                Ok(())
            }
            _ => Err(ApiError::bad(format!(
                "settings.insert_modules must be an array of {}",
                InsertModule::ALL.join(", ")
            ))),
        },
        "max_links_per_post" | "min_links_per_post" | "max_new_links_per_pass" => {
            int_in(v, key, 0, 1000)
        }
        "link_overload_threshold" => int_in(v, key, 0, 100_000),
        "money_page_urls" => match v.as_array() {
            Some(a)
                if a.len() <= 1000
                    && a.iter().all(|u| {
                        u.as_str()
                            .is_some_and(|s| s.len() <= 2048 && is_http_url(s))
                    }) =>
            {
                Ok(())
            }
            _ => Err(ApiError::bad(
                "settings.money_page_urls must be an array of http(s) URLs (max 1000)",
            )),
        },
        "allow_private_host" => match v {
            Value::Bool(_) => Ok(()),
            _ => Err(ApiError::bad(
                "settings.allow_private_host must be a boolean",
            )),
        },
        "skip_inject_verify" | "ai_anchor_refine" | "anchor_semantic_off" => match v {
            Value::Bool(_) => Ok(()),
            _ => Err(ApiError::bad(format!("settings.{key} must be a boolean"))),
        },
        // G2-6: keys the engine reads (plan density / graph roots) that PATCH used to reject.
        "max_new_inbound_per_target" => int_in(v, key, 1, 10_000),
        "root_urls" => match v.as_array() {
            Some(a)
                if a.len() <= 100
                    && a.iter().all(|u| {
                        u.as_str()
                            .is_some_and(|s| s.len() <= 2048 && is_http_url(s))
                    }) =>
            {
                Ok(())
            }
            _ => Err(ApiError::bad(
                "settings.root_urls must be an array of http(s) URLs (max 100)",
            )),
        },
        // AI anchor refinement budget (pipelines/plan.rs anchor_options).
        "ai_anchor_refine_max" => int_in(v, key, 0, 5000),
        "ai_anchor_refine_concurrency" => int_in(v, key, 1, 16),
        "ai_anchor_refine_timeout_s" => int_in(v, key, 5, 3600),
        "anchor_semantic" => match v.as_str() {
            Some("true" | "false") => Ok(()),
            _ => Err(ApiError::bad(
                "settings.anchor_semantic must be \"true\" or \"false\"",
            )),
        },
        other => Err(ApiError::bad(format!("unknown settings key: {other}"))),
    }
}

/// Shallow merge like v1 (`settings || patch`). Credential keys (`wp_user`, `wp_app_password`,
/// `gsc.credentials`) are moved into the encrypted `secrets_enc`; `gsc` itself is managed by the GSC
/// routes and ignored here.
pub async fn patch(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<PatchBody>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &id).await?;
    check_mode(&b.mode)?;
    let wp_url = check_wp_url(&b.wp_url)?;
    let mut incoming = match b.settings {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(o)) => o,
        Some(_) => return Err(ApiError::bad("settings must be an object")),
    };
    // `gsc` (incl. its credentials) belongs to the GSC routes and is ignored here; WP credentials
    // are moved out before validation.
    incoming.remove("gsc");
    let mut wrapped = Value::Object(std::mem::take(&mut incoming));
    let mut absorbed = sf_core::secrets::SiteSecrets::default();
    let creds_given = absorbed.absorb_from_settings(&mut wrapped);
    if let Value::Object(o) = wrapped {
        incoming = o;
    }
    for (k, v) in &incoming {
        validate_setting(k, v)?;
    }
    // M-01: the private-host exemption is admin-only, and a new wp_url is re-validated.
    if incoming.contains_key("allow_private_host") && !u.is_admin() {
        return Err(ApiError::bad(
            "settings.allow_private_host can only be changed by an admin",
        ));
    }
    if wp_url.is_some() {
        let cur = get_site(&st, &id).await?;
        let flag = match incoming.get("allow_private_host") {
            Some(v) => v.as_bool().unwrap_or(false),
            None => cur
                .settings
                .get("allow_private_host")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        let target = wp_url.clone().or(cur.wp_url.clone());
        check_site_target(&st, &u, &cur.domain, target.as_deref(), flag).await?;
    }
    let changed_keys: Vec<String> = incoming.keys().cloned().collect();
    let row = mutate_site(
        &st,
        &id,
        Some((wp_url, b.mode)),
        move |settings, secrets| {
            if let Some(dst) = settings.as_object_mut() {
                for (k, v) in incoming {
                    if v.is_null() {
                        dst.remove(&k);
                    } else {
                        dst.insert(k, v);
                    }
                }
            }
            if let Some(u) = absorbed.wp_user {
                secrets.wp_user = Some(u);
            }
            if let Some(p) = absorbed.wp_app_password {
                secrets.wp_app_password = Some(p);
            }
            if let Some(g) = absorbed.gsc_credentials {
                secrets.gsc_credentials = Some(g);
            }
            Ok(creds_given)
        },
    )
    .await?;
    audit_user(
        &st,
        &u,
        "site.patch",
        Some(&id),
        json!({ "settings_keys": changed_keys, "credentials_updated": creds_given }),
    )
    .await;
    Ok(Json(row.to_json(&st.secret_key, false)))
}

/// v2 addition: `POST /api/v1/sites/:id/rotate-key` issues a new plugin API key and returns the site
/// with `api_key` (shown once). Every site JWT issued for the old key stops working at once: v2
/// tokens carry the key fingerprint (`tv`), and legacy v1 tokens without `tv` are refused once the
/// site has been rotated (`kv` marker, L-01).
pub async fn rotate_key(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &id).await?;
    let key = sf_core::crypto::new_site_api_key();
    let id2 = id.clone();
    let row = st
        .db
        .write(move |t| {
            t.execute("UPDATE sites SET api_key=?2 WHERE id=?1", params![id2, key])
                .map_err(err)?;
            t.execute(
                "INSERT INTO kv(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
                params![
                    format!("{}{id2}", crate::auth::SITE_KEY_ROTATED_KV),
                    sf_core::now_rfc3339()
                ],
            )
            .map_err(err)?;
            load_site(t, &id2)
        })
        .await?
        .ok_or_else(|| ApiError::not_found("site not found"))?;
    st.auth_cache.clear();
    audit_user(&st, &u, "site.rotate_key", Some(&id), json!({})).await;
    Ok(Json(row.to_json(&st.secret_key, true)))
}

pub async fn crawl(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &id).await?;
    enqueue(&st, &u, JobKind::Crawl, &id, json!({})).await
}

#[derive(Deserialize, Default)]
pub struct PlanBody {
    #[serde(default)]
    topology: Option<String>,
    #[serde(default)]
    archive: Option<bool>,
}

/// Accepts all five topologies, including `bidirectional` (fixes v1 B3).
fn parse_topology(t: Option<&str>) -> ApiResult<Option<String>> {
    match t {
        None => Ok(None),
        Some(s) => Topology::parse_known(s)
            .map(|t| Some(t.as_str().to_string()))
            .ok_or_else(|| {
                ApiError::bad(format!(
                    "topology must be one of {}",
                    Topology::ALL.join(", ")
                ))
            }),
    }
}

pub async fn plan(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<Option<PlanBody>>,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &id).await?;
    let b = body.unwrap_or_default();
    let topology = parse_topology(b.topology.as_deref())?;
    let payload = json!({ "topology": topology, "archive": b.archive.unwrap_or(false) });
    enqueue(&st, &u, JobKind::Plan, &id, payload).await
}

pub async fn archive_plan(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<Option<PlanBody>>,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &id).await?;
    let b = body.unwrap_or_default();
    let topology = parse_topology(b.topology.as_deref())?;
    let payload = json!({ "topology": topology, "archive": true });
    enqueue(&st, &u, JobKind::Plan, &id, payload).await
}

pub fn topology_param(t: Option<&str>) -> ApiResult<Option<String>> {
    parse_topology(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_normalisation() {
        assert_eq!(
            normalize_domain(" HTTPS://Ex.com/ ").ok().as_deref(),
            Some("Ex.com")
        );
        assert!(normalize_domain("a\"b.com").is_err());
        assert!(normalize_domain("a\r\nb").is_err());
        assert!(normalize_domain("https://").is_err());
    }

    #[test]
    fn settings_whitelist() {
        assert!(validate_setting("preset", &json!("balanced")).is_ok());
        assert!(validate_setting("preset", &json!("wild")).is_err());
        assert!(validate_setting("silo_topology", &json!("bidirectional")).is_ok());
        assert!(validate_setting("insert_modules", &json!(["native", "context"])).is_ok());
        assert!(validate_setting("insert_modules", &json!(["evil"])).is_err());
        assert!(validate_setting("max_links_per_post", &json!(0)).is_ok());
        assert!(validate_setting("max_links_per_post", &json!(-1)).is_err());
        assert!(validate_setting("max_links_per_post", &json!("5")).is_err());
        assert!(validate_setting("skip_inject_verify", &json!(true)).is_ok());
        assert!(validate_setting("money_page_urls", &json!(["javascript:x"])).is_err());
        assert!(validate_setting("whatever", &json!(1)).is_err());
        // G2-6 + AI anchor refine keys the engine reads
        assert!(validate_setting("max_new_inbound_per_target", &json!(5)).is_ok());
        assert!(validate_setting("max_new_inbound_per_target", &json!(0)).is_err());
        assert!(validate_setting("root_urls", &json!(["https://ex.com/hub/"])).is_ok());
        assert!(validate_setting("root_urls", &json!(["ftp://x"])).is_err());
        assert!(validate_setting("root_urls", &json!("https://ex.com/")).is_err());
        assert!(validate_setting("ai_anchor_refine", &json!(true)).is_ok());
        assert!(validate_setting("ai_anchor_refine", &json!("yes")).is_err());
        assert!(validate_setting("ai_anchor_refine_max", &json!(300)).is_ok());
        assert!(validate_setting("ai_anchor_refine_max", &json!(9000)).is_err());
        assert!(validate_setting("ai_anchor_refine_concurrency", &json!(0)).is_err());
        assert!(validate_setting("ai_anchor_refine_timeout_s", &json!(60)).is_ok());
        assert!(validate_setting("anchor_semantic", &json!("false")).is_ok());
        assert!(validate_setting("anchor_semantic", &json!(false)).is_err());
        assert!(validate_setting("whatever", &Value::Null).is_ok());
    }
}
