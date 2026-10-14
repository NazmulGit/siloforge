//! `gsc`: Google Search Console pull (G2-2). Mints an OAuth2 access token from the site's sealed
//! service-account JSON (self-signed RS256 JWT -> `token_uri`, jwt-bearer grant), queries
//! `searchAnalytics/query` (dimensions `[page, query]`, last 28 days ending today-3, paged by
//! `startRow`) and replaces the site's `keywords(source='gsc')` rows in one transaction:
//! `volume` = impressions, `position` = average position, `striking` = position in 4..=10 (v1
//! `packages/engine/src/keywords/gsc.ts`), `score` = min(1, ctr + 0.1) (v1).
//!
//! Property (`siteUrl`): `settings.gsc.property` when set, else `sc-domain:<host>` and, when that
//! one is refused, the URL-prefix property `https://<host>/`. Either kind works; the one used is
//! reported in `stats.property`.
//!
//! The private key, the assertion and the access token are never logged or put into errors.
//! `settings.gsc.{last_sync_at,last_error,last_property}` are stamped when `settings.gsc` exists.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sf_core::Error;
use sf_store::err;

use super::common::{SiteInfo, load_site, path_index, site_hosts, truncate_utf16};
use crate::ctx::{Job, JobContext, JobError, JobResult};

/// OAuth scope (read-only Search Console).
pub const SCOPE: &str = "https://www.googleapis.com/auth/webmasters.readonly";
/// Production API base.
pub const DEFAULT_API_BASE: &str = "https://www.googleapis.com";
/// Token endpoint used when the credentials JSON has no `token_uri`.
pub const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// Striking distance (v1): average position 4..=10.
pub const STRIKING_MIN: f64 = 4.0;
pub const STRIKING_MAX: f64 = 10.0;
/// Longest keyword stored (v1 `.slice(0, 255)`).
const MAX_KEYWORD: usize = 255;
const MAX_CREDENTIALS: usize = 64 * 1024;
const MAX_TOKEN_LEN: usize = 8 * 1024;
const MAX_ERROR_SNIPPET: usize = 300;

/// Hard caps for one pull.
#[derive(Debug, Clone)]
pub struct GscLimits {
    /// `rowLimit` per request (Google max 25000).
    pub page_size: usize,
    /// Most requests per property.
    pub max_pages: usize,
    /// Most rows kept in total (the pull stops, `truncated: true`, stale rows are kept).
    pub max_rows: usize,
    /// Largest accepted response body.
    pub max_response_bytes: usize,
    /// Per-request timeout.
    pub timeout: Duration,
}

impl Default for GscLimits {
    fn default() -> Self {
        Self {
            page_size: 25_000,
            max_pages: 8,
            max_rows: 200_000,
            max_response_bytes: 32 * 1024 * 1024,
            timeout: Duration::from_secs(90),
        }
    }
}

/// Search Console HTTP client (plain rustls reqwest; Google is a public endpoint).
#[derive(Clone)]
pub struct GscClient {
    http: reqwest::Client,
    api_base: String,
    /// Accept any `token_uri` from the credentials JSON (tests). Production only accepts https
    /// Google hosts, so a crafted credentials file cannot make the server post assertions elsewhere.
    trust_any_token_uri: bool,
    limits: GscLimits,
}

impl std::fmt::Debug for GscClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GscClient")
            .field("api_base", &self.api_base)
            .field("limits", &self.limits)
            .finish()
    }
}

impl GscClient {
    /// Production client (`https://www.googleapis.com`, Google token hosts only).
    pub fn new() -> sf_core::Result<Self> {
        Self::with_endpoints(DEFAULT_API_BASE, false, GscLimits::default())
    }

    /// Client with a custom API base (tests point this at a local stub). `trust_any_token_uri`
    /// lifts the Google-host check on the credentials' `token_uri`.
    pub fn with_endpoints(
        api_base: &str,
        trust_any_token_uri: bool,
        limits: GscLimits,
    ) -> sf_core::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(limits.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("SiloForge/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::internal(format!("gsc http client: {e}")))?;
        Ok(Self {
            http,
            api_base: api_base.trim_end_matches('/').to_string(),
            trust_any_token_uri,
            limits,
        })
    }

    /// Exchange the service account for an access token.
    pub async fn access_token(&self, sa: &ServiceAccount) -> sf_core::Result<String> {
        let token_uri = self.token_uri(sa)?;
        let assertion = build_assertion(sa, &token_uri, chrono::Utc::now().timestamp())?;
        let resp = self
            .http
            .post(&token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", assertion.as_str()),
            ])
            .send()
            .await
            .map_err(|e| {
                Error::upstream(format!("gsc token request failed: {}", e.without_url()))
            })?;
        let status = resp.status();
        let body = read_capped(resp, 256 * 1024).await?;
        if !status.is_success() {
            return Err(Error::upstream(format!(
                "gsc token exchange failed: HTTP {} {}",
                status.as_u16(),
                snippet(&body)
            )));
        }
        #[derive(Deserialize)]
        struct TokenResp {
            access_token: Option<String>,
        }
        let t: TokenResp = serde_json::from_slice(&body)
            .map_err(|_| Error::upstream("gsc token exchange returned invalid JSON"))?;
        match t.access_token {
            Some(tok) if !tok.is_empty() && tok.len() <= MAX_TOKEN_LEN && tok.is_ascii() => Ok(tok),
            _ => Err(Error::upstream(
                "gsc token exchange returned no access_token",
            )),
        }
    }

    fn token_uri(&self, sa: &ServiceAccount) -> sf_core::Result<String> {
        let raw = sa
            .token_uri
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_TOKEN_URI);
        let u = url::Url::parse(raw).map_err(|_| Error::bad("gsc token_uri is not a valid URL"))?;
        if self.trust_any_token_uri {
            return Ok(u.to_string());
        }
        let host = u.host_str().unwrap_or("").to_ascii_lowercase();
        let google = host == "oauth2.googleapis.com"
            || host == "accounts.google.com"
            || host.ends_with(".googleapis.com");
        if u.scheme() != "https" || !google {
            return Err(Error::bad(
                "gsc token_uri must be an https Google endpoint (oauth2.googleapis.com)",
            ));
        }
        Ok(u.to_string())
    }

    /// All `[page, query]` rows for `property` in `[start, end]` (paged; capped).
    /// Returns `(rows, truncated)`.
    pub async fn query_rows(
        &self,
        token: &str,
        property: &str,
        start: &str,
        end: &str,
    ) -> sf_core::Result<(Vec<GscRow>, bool)> {
        let site: String = url::form_urlencoded::byte_serialize(property.as_bytes()).collect();
        let url = format!(
            "{}/webmasters/v3/sites/{site}/searchAnalytics/query",
            self.api_base
        );
        let page_size = self.limits.page_size.clamp(1, 25_000);
        let mut out: Vec<GscRow> = Vec::new();
        let mut start_row = 0usize;
        for _ in 0..self.limits.max_pages.max(1) {
            let body = json!({
                "startDate": start, "endDate": end,
                "dimensions": ["page", "query"],
                "rowLimit": page_size, "startRow": start_row,
            });
            let resp = self
                .http
                .post(&url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    Error::upstream(format!(
                        "gsc searchAnalytics request failed: {}",
                        e.without_url()
                    ))
                })?;
            let status = resp.status();
            let bytes = read_capped(resp, self.limits.max_response_bytes).await?;
            if !status.is_success() {
                return Err(Error::upstream(format!(
                    "gsc searchAnalytics failed for {property}: HTTP {} {}",
                    status.as_u16(),
                    snippet(&bytes)
                )));
            }
            let page = parse_rows(&bytes)?;
            let n = page.len();
            for r in page {
                if out.len() >= self.limits.max_rows {
                    return Ok((out, true));
                }
                out.push(r);
            }
            if n < page_size {
                return Ok((out, false));
            }
            start_row = start_row.saturating_add(n);
        }
        // Page cap reached with full pages: more data may exist.
        Ok((out, true))
    }
}

/// The fields of a Google service-account JSON we use. `Debug` never prints the key.
#[derive(Clone, Deserialize)]
pub struct ServiceAccount {
    pub client_email: String,
    private_key: String,
    #[serde(default)]
    pub token_uri: Option<String>,
}

impl std::fmt::Debug for ServiceAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceAccount")
            .field("client_email", &self.client_email)
            .field("token_uri", &self.token_uri)
            .finish_non_exhaustive()
    }
}

impl ServiceAccount {
    /// Parse the sealed credentials JSON (bounded; errors never echo the input).
    pub fn parse(json: &str) -> sf_core::Result<Self> {
        if json.len() > MAX_CREDENTIALS {
            return Err(Error::bad("gsc credentials are too large"));
        }
        let sa: ServiceAccount = serde_json::from_str(json).map_err(|_| {
            Error::bad("gsc credentials are not a service-account JSON (client_email, private_key)")
        })?;
        if sa.client_email.trim().is_empty() || sa.private_key.trim().is_empty() {
            return Err(Error::bad(
                "gsc credentials are missing client_email or private_key",
            ));
        }
        Ok(sa)
    }
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: i64,
    exp: i64,
}

/// RS256 JWT assertion: `iss` = client_email, `scope`, `aud` = token URI, 1 h lifetime.
pub fn build_assertion(sa: &ServiceAccount, aud: &str, now: i64) -> sf_core::Result<String> {
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(sa.private_key.as_bytes())
        .map_err(|_| Error::bad("gsc private_key is not a valid RSA PEM key"))?;
    let claims = Claims {
        iss: &sa.client_email,
        scope: SCOPE,
        aud,
        iat: now,
        exp: now.saturating_add(3600),
    };
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &key,
    )
    .map_err(|_| Error::internal("gsc assertion signing failed"))
}

/// One Search Analytics row (`keys = [page, query]`).
#[derive(Debug, Clone, PartialEq)]
pub struct GscRow {
    pub page: String,
    pub query: String,
    pub clicks: f64,
    pub impressions: f64,
    pub position: f64,
}

/// Parse a searchAnalytics response body. Malformed rows are skipped; a non-JSON body errors.
pub fn parse_rows(body: &[u8]) -> sf_core::Result<Vec<GscRow>> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|_| Error::upstream("gsc searchAnalytics returned invalid JSON"))?;
    let Some(rows) = v.get("rows") else {
        return Ok(Vec::new());
    };
    let Some(rows) = rows.as_array() else {
        return Err(Error::upstream("gsc searchAnalytics: rows is not an array"));
    };
    let num = |r: &Value, k: &str| {
        r.get(k)
            .and_then(Value::as_f64)
            .filter(|x| x.is_finite() && *x >= 0.0)
    };
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let keys = r.get("keys").and_then(Value::as_array);
        let (Some(page), Some(query)) = (
            keys.and_then(|k| k.first()).and_then(Value::as_str),
            keys.and_then(|k| k.get(1)).and_then(Value::as_str),
        ) else {
            continue;
        };
        let Some(position) = num(r, "position") else {
            continue;
        };
        out.push(GscRow {
            page: page.to_string(),
            query: query.to_string(),
            clicks: num(r, "clicks").unwrap_or(0.0),
            impressions: num(r, "impressions").unwrap_or(0.0),
            position,
        });
    }
    Ok(out)
}

/// Read a response body, failing past `max` bytes.
async fn read_capped(mut resp: reqwest::Response, max: usize) -> sf_core::Result<Vec<u8>> {
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Err(Error::upstream(format!(
            "gsc response too large (> {max} bytes)"
        )));
    }
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = resp.chunk().await.map_err(|e| {
            Error::upstream(format!("gsc response read failed: {}", e.without_url()))
        })?;
        let Some(c) = chunk else { break };
        if buf.len().saturating_add(c.len()) > max {
            return Err(Error::upstream(format!(
                "gsc response too large (> {max} bytes)"
            )));
        }
        buf.extend_from_slice(&c);
    }
    Ok(buf)
}

fn snippet(body: &[u8]) -> String {
    let s = String::from_utf8_lossy(body);
    truncate_utf16(s.trim(), MAX_ERROR_SNIPPET)
}

/// Candidate properties: `settings.gsc.property`, else `sc-domain:<host>` then `https://<host>/`.
pub fn properties(site: &SiteInfo) -> Vec<String> {
    if let Some(p) = site
        .settings
        .get("gsc")
        .and_then(|g| g.get("property"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return vec![p.to_string()];
    }
    match sf_engine::graph::configured_host(&site.domain).filter(|h| !h.is_empty()) {
        Some(h) => vec![format!("sc-domain:{h}"), format!("https://{h}/")],
        None => Vec::new(),
    }
}

/// `[start, end]` dates: 28 days ending three days ago (GSC data lag).
pub fn date_range(today: chrono::NaiveDate) -> (String, String) {
    let end = today - chrono::Days::new(3);
    let start = end - chrono::Days::new(27);
    (
        start.format("%Y-%m-%d").to_string(),
        end.format("%Y-%m-%d").to_string(),
    )
}

#[derive(Default)]
struct Agg {
    impressions: f64,
    clicks: f64,
    pos_weighted: f64,
    pos_plain: f64,
    n: f64,
}

/// Pull GSC for `site` and replace its gsc keyword rows. Errors are also stamped into
/// `settings.gsc.last_error`.
pub async fn sync_site(ctx: &JobContext, site: &SiteInfo, client: &GscClient) -> JobResult<Value> {
    let res = sync_inner(ctx, site, client).await;
    let (ok_prop, err_msg) = match &res {
        Ok(v) => (
            v.get("property")
                .and_then(Value::as_str)
                .map(str::to_string),
            None,
        ),
        Err(e) => (None, Some(e.to_string())),
    };
    stamp_settings(ctx, &site.id, ok_prop, err_msg).await;
    res
}

async fn sync_inner(ctx: &JobContext, site: &SiteInfo, client: &GscClient) -> JobResult<Value> {
    let Some((creds, source)) = credentials_for(ctx, site) else {
        return Err(JobError::fatal(Error::bad(
            "Search Console is not connected for this site (no site credentials, GSC_CREDENTIALS_JSON unset)",
        )));
    };
    tracing::debug!(site = %site.id, source, "gsc credentials");
    let sa = ServiceAccount::parse(&creds)?;
    let props = properties(site);
    if props.is_empty() {
        return Err(JobError::fatal(Error::bad(
            "cannot derive a Search Console property from the site domain",
        )));
    }
    let token = client.access_token(&sa).await?;
    let (start, end) = date_range(chrono::Utc::now().date_naive());
    let mut last_err: Option<Error> = None;
    let mut got: Option<(String, Vec<GscRow>, bool)> = None;
    for p in &props {
        match client.query_rows(&token, p, &start, &end).await {
            Ok((rows, truncated)) => {
                got = Some((p.clone(), rows, truncated));
                break;
            }
            Err(e) => {
                tracing::info!(site = %site.id, property = %p, error = %e, "gsc property not usable");
                last_err = Some(e);
            }
        }
    }
    let Some((property, rows, truncated)) = got else {
        return Err(JobError::fatal(last_err.unwrap_or_else(|| {
            Error::upstream("gsc: no usable Search Console property")
        })));
    };
    let mut stats = store_rows(ctx, site, &property, rows, truncated).await?;
    if let Some(o) = stats.as_object_mut() {
        o.insert("credentials".into(), json!(source));
    }
    Ok(stats)
}

/// Service-account JSON for `site`: the per-site sealed credentials first, then the
/// install-wide `GSC_CREDENTIALS_JSON` fallback (v1 parity, G2-5). Returns the source
/// (`"site"` / `"global"`) next to it; the value itself is never logged.
pub fn credentials_for(ctx: &JobContext, site: &SiteInfo) -> Option<(String, &'static str)> {
    if let Some(c) = site
        .secrets
        .gsc_credentials
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        return Some((c.to_string(), "site"));
    }
    ctx.cfg.gsc.fallback_credentials().map(|c| (c, "global"))
}

/// Map rows to posts and replace the site's gsc keywords (exposed for tests).
pub async fn store_rows(
    ctx: &JobContext,
    site: &SiteInfo,
    property: &str,
    rows: Vec<GscRow>,
    truncated: bool,
) -> JobResult<Value> {
    let (by_path, urls) = path_index(ctx, &site.id).await?;
    let hosts: HashSet<String> = site_hosts(site, &urls).into_iter().collect();
    let total = rows.len();
    let mut matched = 0usize;
    let mut agg: BTreeMap<(String, String), Agg> = BTreeMap::new();
    for r in rows {
        let host_ok = sf_engine::graph::url_host_key(&r.page).is_some_and(|h| hosts.contains(&h));
        if !host_ok {
            continue;
        }
        let Some(post) =
            sf_engine::graph::url_path_key(&r.page, None).and_then(|k| by_path.get(&k))
        else {
            continue;
        };
        let kw = truncate_utf16(r.query.trim(), MAX_KEYWORD);
        if kw.is_empty() {
            continue;
        }
        matched += 1;
        let a = agg.entry((post.clone(), kw)).or_default();
        a.impressions += r.impressions;
        a.clicks += r.clicks;
        a.pos_weighted += r.position * r.impressions;
        a.pos_plain += r.position;
        a.n += 1.0;
    }
    let mut out: Vec<(String, String, f64, i64, f64, bool)> = Vec::with_capacity(agg.len());
    for ((post, kw), a) in agg {
        let position = if a.impressions > 0.0 {
            a.pos_weighted / a.impressions
        } else if a.n > 0.0 {
            a.pos_plain / a.n
        } else {
            0.0
        };
        let ctr = if a.impressions > 0.0 {
            a.clicks / a.impressions
        } else {
            0.0
        };
        let score = (ctr + 0.1).min(1.0);
        let volume = if a.impressions.is_finite() {
            a.impressions.round().clamp(0.0, i64::MAX as f64) as i64
        } else {
            0
        };
        let striking = (STRIKING_MIN..=STRIKING_MAX).contains(&position);
        out.push((post, kw, score, volume, position, striking));
    }
    let sid = site.id.clone();
    let keep_stale = truncated;
    let (upserted, removed) = ctx
        .db
        .write(move |t| {
            let existing: Vec<(String, String, String)> = {
                let mut st = t
                    .prepare(
                        "SELECT k.id, k.post_id, k.keyword FROM keywords k JOIN posts p ON p.id=k.post_id
                         WHERE p.site_id=?1 AND k.source='gsc'",
                    )
                    .map_err(err)?;
                st.query_map([&sid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                    .map_err(err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(err)?
            };
            let fresh: HashSet<(&str, &str)> =
                out.iter().map(|r| (r.0.as_str(), r.1.as_str())).collect();
            let mut removed = 0usize;
            if !keep_stale {
                let mut del = t
                    .prepare_cached("DELETE FROM keywords WHERE id=?1")
                    .map_err(err)?;
                for (id, pid, kw) in &existing {
                    if !fresh.contains(&(pid.as_str(), kw.as_str())) {
                        removed += del.execute([id]).map_err(err)?;
                    }
                }
            }
            let mut up = t
                .prepare_cached(
                    "INSERT INTO keywords(id, post_id, keyword, source, score, volume, position, striking)
                     VALUES(?1,?2,?3,'gsc',?4,?5,?6,?7)
                     ON CONFLICT(post_id, keyword, source) DO UPDATE SET
                       score=excluded.score, volume=excluded.volume,
                       position=excluded.position, striking=excluded.striking",
                )
                .map_err(err)?;
            let mut upserted = 0usize;
            for (pid, kw, score, volume, position, striking) in &out {
                upserted += up
                    .execute(params![
                        sf_core::new_id(),
                        pid,
                        kw,
                        score,
                        volume,
                        position,
                        *striking
                    ])
                    .map_err(err)?;
            }
            Ok((upserted, removed))
        })
        .await?;
    tracing::info!(site = %site.id, property, rows = total, matched, upserted, removed, truncated, "gsc sync done");
    Ok(json!({
        "rows": total, "matched": matched, "upserted": upserted, "removed": removed,
        "property": property, "truncated": truncated,
    }))
}

/// Stamp `settings.gsc.last_sync_at` / `last_error` / `last_property` (only when `settings.gsc`
/// is an object; one atomic statement, best effort).
async fn stamp_settings(
    ctx: &JobContext,
    site_id: &str,
    property: Option<String>,
    error: Option<String>,
) {
    let sid = site_id.to_string();
    let now = sf_core::now_rfc3339();
    let ok = error.is_none();
    let res = ctx
        .db
        .write(move |t| {
            if ok {
                t.execute(
                    "UPDATE sites SET settings=json_set(settings, '$.gsc.last_sync_at', ?2,
                       '$.gsc.last_error', NULL, '$.gsc.last_property', ?3)
                     WHERE id=?1 AND json_valid(settings) AND json_type(settings, '$.gsc')='object'",
                    params![sid, now, property],
                )
                .map_err(err)?;
            } else {
                t.execute(
                    "UPDATE sites SET settings=json_set(settings, '$.gsc.last_error', ?2,
                       '$.gsc.last_error_at', ?3)
                     WHERE id=?1 AND json_valid(settings) AND json_type(settings, '$.gsc')='object'",
                    params![sid, error, now],
                )
                .map_err(err)?;
            }
            Ok(())
        })
        .await;
    if let Err(e) = res {
        tracing::warn!(site = %site_id, error = %e, "gsc: could not stamp settings");
    }
}

/// The `gsc` job.
pub async fn run(ctx: &Arc<JobContext>, job: &Job) -> JobResult<Value> {
    let site = load_site(ctx, job.site()?).await?;
    job.progress("gsc", 5.0).await;
    let client = GscClient::new()?;
    let mut stats = sync_site(ctx, &site, &client).await?;
    if let Some(o) = stats.as_object_mut() {
        o.insert("progress".into(), json!({"phase": "done", "pct": 100}));
    }
    Ok(stats)
}

/// Audit hook: when the site (or the install-wide fallback) has GSC credentials, pull them. A failure is a job warning
/// (`{ok: false, error}`), never an audit failure. `None` when GSC is not configured.
pub async fn audit_pull(ctx: &JobContext, job: &Job, site: &SiteInfo) -> JobResult<Option<Value>> {
    if credentials_for(ctx, site).is_none() {
        return Ok(None);
    }
    let client = match GscClient::new() {
        Ok(c) => c,
        Err(e) => {
            job.warn(format!("gsc pull skipped: {e}")).await;
            return Ok(Some(json!({"ok": false, "error": e.to_string()})));
        }
    };
    match sync_site(ctx, site, &client).await {
        Ok(mut s) => {
            if let Some(o) = s.as_object_mut() {
                o.insert("ok".into(), json!(true));
            }
            Ok(Some(s))
        }
        Err(e) if e.is_cancelled() => Err(e),
        Err(e) => {
            job.warn(format!("gsc pull failed: {e}")).await;
            Ok(Some(json!({"ok": false, "error": e.to_string()})))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rows_tolerates_hostile_bodies() {
        assert!(parse_rows(b"not json").is_err());
        assert!(parse_rows(b"{\"rows\": 5}").is_err());
        assert_eq!(parse_rows(b"{}").ok(), Some(Vec::new()));
        let v = parse_rows(
            br#"{"rows":[{"keys":["https://a/x/","q"],"clicks":1,"impressions":10,"position":5.5},
                 {"keys":["only-one"]},{"keys":[1,2],"position":1},{"keys":["p","q"],"position":"x"},
                 {"keys":["p","q2"],"position":-1}]}"#,
        )
        .unwrap_or_default();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].query, "q");
    }

    #[test]
    fn dates_and_service_account() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 10, 14).unwrap_or_default();
        assert_eq!(
            date_range(d),
            ("2026-09-14".to_string(), "2026-10-11".to_string())
        );
        assert!(ServiceAccount::parse("{").is_err());
        assert!(ServiceAccount::parse(r#"{"client_email":"a@b"}"#).is_err());
        let sa = ServiceAccount::parse(
            r#"{"client_email":"a@b","private_key":"SECRET-KEY","token_uri":"http://evil/"}"#,
        );
        let sa = sa.ok();
        assert!(
            sa.as_ref()
                .is_some_and(|s| !format!("{s:?}").contains("SECRET"))
        );
        // Production client refuses non-Google token URIs; bad PEM fails without echoing it.
        if let (Some(sa), Ok(c)) = (sa, GscClient::new()) {
            assert!(c.token_uri(&sa).is_err());
            let e = build_assertion(&sa, "aud", 0)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(!e.is_empty() && !e.contains("SECRET"));
        }
    }
}
