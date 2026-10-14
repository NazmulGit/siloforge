//! Shared helpers for the v1 handlers: tolerant JSON body, query parsing, ownership guard, enqueue,
//! site payload shaping (secrets never leave), CSV encoding and a tiny per-IP rate limiter.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;
use rusqlite::types::ToSql;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sf_core::secrets::SiteSecrets;
use sf_core::types::JobKind;
use sf_jobs::NewJob;
use sf_store::{err, row_to_json};

use crate::auth::{Principal, User, audit, ensure_site_access};
use crate::{ApiError, ApiResult, AppState};

/// Hard cap for any `limit` query parameter (requests above it are clamped, never rejected).
pub const MAX_LIMIT: i64 = 5000;

// ---------------------------------------------------------------------------
// request parsing
// ---------------------------------------------------------------------------

/// JSON body that treats an empty/whitespace body as `{}` (v1 parser) and maps errors to 400.
/// The content type is not enforced: v1 clients always send JSON.
pub struct JsonBody<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for JsonBody<T> {
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|e| ApiError::new(e.status(), e.body_text()))?;
        let slice: &[u8] = if bytes.iter().all(u8::is_ascii_whitespace) {
            b"{}"
        } else {
            &bytes
        };
        serde_json::from_slice(slice)
            .map(JsonBody)
            .map_err(|e| ApiError::bad(format!("body {e}")))
    }
}

/// Raw query parameters (bad values fall back to defaults instead of failing the request).
pub type Params = axum::extract::Query<HashMap<String, String>>;

/// Integer query parameter clamped to `[min, max]`; missing or non-numeric -> `default`.
pub fn int_param(q: &HashMap<String, String>, key: &str, default: i64, min: i64, max: i64) -> i64 {
    match q.get(key).map(|s| s.trim().parse::<f64>()) {
        Some(Ok(v)) if v.is_finite() => (v.trunc() as i64).clamp(min, max),
        _ => default,
    }
}

pub fn str_param<'a>(q: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    q.get(key).map(String::as_str).filter(|s| !s.is_empty())
}

/// Canonical lowercase/uppercase hyphenated UUID check (v1 `isUuid`).
pub fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Absolute http(s) URL with a host (v1 `isHttpUrl`).
pub fn is_http_url(s: &str) -> bool {
    url::Url::parse(s)
        .map(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some())
        .unwrap_or(false)
}

/// Client IP from the socket. v2.4: with Settings → Platform `trust_proxy` on (SiloForge behind a
/// reverse proxy), the first valid address of `X-Forwarded-For` is used instead.
pub struct ClientIp(pub String);

/// First syntactically valid IP of an `X-Forwarded-For` value.
pub fn forwarded_ip(h: &axum::http::HeaderMap) -> Option<String> {
    let v = h.get("x-forwarded-for")?.to_str().ok()?;
    v.split(',')
        .map(str::trim)
        .find_map(|s| s.parse::<std::net::IpAddr>().ok())
        .map(|ip| ip.to_string())
}

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let socket = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip().to_string())
            .unwrap_or_else(|| "local".to_string());
        let trust = crate::platform::get(st).await.deployment.trust_proxy;
        let ip = if trust {
            forwarded_ip(&parts.headers).unwrap_or(socket)
        } else {
            socket
        };
        Ok(ClientIp(ip))
    }
}

// ---------------------------------------------------------------------------
// v1 text ordering
// ---------------------------------------------------------------------------

/// Name of the SQLite collation that reproduces v1's Postgres `en_US.UTF-8` text order.
/// Use it as `ORDER BY title COLLATE V1` after [`v1_collation`] registered it on the connection.
pub const V1_COLLATION: &str = "V1";

/// Level-1 fold of one lowercase char: strips the common Latin accents; `ß`/`æ` expand to two chars.
fn fold_lower(l: char) -> (char, Option<char>) {
    let base = match l {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
        'ç' | 'ć' | 'č' => 'c',
        'ď' | 'đ' => 'd',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' => 'i',
        'ł' => 'l',
        'ñ' | 'ń' | 'ň' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => 'o',
        'ř' => 'r',
        'ś' | 'š' | 'ş' => 's',
        'ť' => 't',
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => 'u',
        'ý' | 'ÿ' => 'y',
        'ź' | 'ż' | 'ž' => 'z',
        'ß' => return ('s', Some('s')),
        'æ' => return ('a', Some('e')),
        other => other,
    };
    (base, None)
}

fn alnum(s: &str) -> impl Iterator<Item = char> + '_ {
    s.chars().filter(|c| c.is_alphanumeric())
}

/// Approximation of glibc `en_US.UTF-8` (ISO 14651) ordering, which v1's Postgres used for every
/// `ORDER BY <text>`: punctuation and spaces are ignored first, then letters compare case- and
/// accent-insensitively (digits before letters), then accents, then lowercase before uppercase,
/// and finally the raw bytes. SQLite's default BINARY order put `Zebra` before `apple` and
/// `tag/bottle-art/` before `tag/bottle/`, which reordered lists and shifted page boundaries.
/// Allocation-free: it runs inside SQLite sorts over tens of thousands of rows.
pub fn v1_collate(a: &str, b: &str) -> std::cmp::Ordering {
    if a == b {
        return std::cmp::Ordering::Equal;
    }
    fn level1(s: &str) -> impl Iterator<Item = char> + '_ {
        alnum(s).flat_map(char::to_lowercase).flat_map(|l| {
            let (x, y) = fold_lower(l);
            std::iter::once(x).chain(y)
        })
    }
    fn level2(s: &str) -> impl Iterator<Item = char> + '_ {
        alnum(s).flat_map(char::to_lowercase)
    }
    fn level3(s: &str) -> impl Iterator<Item = bool> + '_ {
        alnum(s).map(char::is_uppercase)
    }
    level1(a)
        .cmp(level1(b))
        .then_with(|| level2(a).cmp(level2(b)))
        .then_with(|| level3(a).cmp(level3(b)))
        .then_with(|| a.cmp(b))
}

/// Registers [`V1_COLLATION`] on a (pooled) connection. Cheap; call it inside every read closure
/// whose SQL uses `COLLATE V1` (re-registering just replaces the previous definition).
pub fn v1_collation(c: &rusqlite::Connection) -> sf_core::Result<()> {
    c.create_collation(V1_COLLATION, v1_collate).map_err(err)
}

// ---------------------------------------------------------------------------
// auth helpers
// ---------------------------------------------------------------------------

pub fn principal(u: &User) -> Principal {
    u.principal()
}

/// Ownership gate for a site id taken from a path/body (non-uuid -> 404, foreign -> 404).
pub async fn guard_site(st: &AppState, who: &Principal, site_id: &str) -> ApiResult<()> {
    if !is_uuid(site_id) {
        return Err(ApiError::not_found("site not found"));
    }
    ensure_site_access(st, who, site_id).await
}

pub async fn guard_user_site(st: &AppState, u: &User, site_id: &str) -> ApiResult<()> {
    guard_site(st, &principal(u), site_id).await
}

/// v2.4: site gate for workspace-admin routes (site settings, keys, credentials, Auto-Linker).
pub async fn guard_site_admin(st: &AppState, u: &User, site_id: &str) -> ApiResult<()> {
    guard_user_site(st, u, site_id).await?;
    crate::tenancy::require_site_role(st, u, site_id, crate::tenancy::Role::Admin)
        .await
        .map(|_| ())
}

/// Same as `guard_site` but with a custom 404 message (child rows: edge, rule, schedule, ...).
pub async fn guard_child(st: &AppState, u: &User, site_id: &str, msg: &str) -> ApiResult<()> {
    guard_user_site(st, u, site_id)
        .await
        .map_err(|e| match e.status {
            StatusCode::NOT_FOUND => ApiError::not_found(msg),
            _ => e,
        })
}

/// SQL scope for "sites this user may see": `None` = admin (all sites).
pub fn owner_scope(u: &User) -> Option<String> {
    (!u.is_admin()).then(|| u.user_id.clone())
}

pub async fn audit_user(
    st: &AppState,
    u: &User,
    action: &str,
    target: Option<&str>,
    detail: Value,
) {
    audit(st, &u.actor(), u.via(), action, target, detail).await;
}

pub async fn audit_principal(
    st: &AppState,
    who: &Principal,
    action: &str,
    target: Option<&str>,
    detail: Value,
) {
    match who {
        Principal::User { user_id, token, .. } => {
            let (actor, via) = match token {
                Some(t) => (format!("token:{t}"), "api"),
                None => (format!("user:{user_id}"), "ui"),
            };
            audit(st, &actor, via, action, target, detail).await;
        }
        Principal::Site { site_id } => {
            audit(
                st,
                &format!("site:{site_id}"),
                "plugin",
                action,
                target,
                detail,
            )
            .await;
        }
    }
}

/// Maps a unique-constraint failure to a friendly 409, everything else unchanged.
pub fn conflict_as(e: sf_core::Error, msg: &str) -> ApiError {
    match e {
        sf_core::Error::Conflict(_) => ApiError::conflict(msg),
        other => other.into(),
    }
}

// ---------------------------------------------------------------------------
// jobs
// ---------------------------------------------------------------------------

/// Enqueues a job and answers v1-style `202 {job_run_id}` (+ `deduplicated`).
pub async fn enqueue(
    st: &AppState,
    u: &User,
    kind: JobKind,
    site_id: &str,
    payload: Value,
) -> ApiResult<Response> {
    let out = enqueue_raw(st, &u.actor(), kind.clone(), site_id, payload).await?;
    audit_user(
        st,
        u,
        &format!("job.enqueue.{}", kind.as_str()),
        Some(site_id),
        json!({ "job_run_id": out.job_run_id, "deduplicated": out.deduplicated }),
    )
    .await;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "job_run_id": out.job_run_id, "deduplicated": out.deduplicated })),
    )
        .into_response())
}

/// L-09: most queued (not yet running) jobs one site may have. Inject/undo jobs with different
/// `edge_ids` never dedupe, so without a cap one client could queue without bound.
pub const MAX_QUEUED_PER_SITE: i64 = 100;

pub async fn enqueue_raw(
    st: &AppState,
    actor: &str,
    kind: JobKind,
    site_id: &str,
    payload: Value,
) -> ApiResult<sf_jobs::Enqueued> {
    let sid = site_id.to_string();
    let queued: i64 = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT count(*) FROM job_runs WHERE site_id=?1 AND status='queued'",
                [&sid],
                |r| r.get(0),
            )
            .map_err(err)
        })
        .await?;
    if queued >= MAX_QUEUED_PER_SITE {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "this site already has {queued} queued jobs (max {MAX_QUEUED_PER_SITE}); wait for them to run or cancel some"
            ),
        ));
    }
    let mut job = NewJob::new(kind, Some(site_id), payload).by(actor);
    job.dedupe = Some(true);
    Ok(st.jobs.enqueue(job).await?)
}

/// Job columns for API payloads. `payload` is left out on purpose: inject/undo payloads carry
/// thousands of edge ids and the UI polls this list every few seconds. `status` reports a running
/// job whose cancel was requested as `cancelled` (v1 shape, R3).
pub const JOB_API_COLS: &str = "id, site_id, kind, CASE WHEN status='running' AND cancel_requested_at IS NOT NULL THEN 'cancelled' ELSE status END AS status, stats, error, attempts, max_attempts, priority, \
     requested_by, heartbeat_at, resources, started_at, finished_at, created_at";
pub const JOB_API_JSON: &[&str] = &["stats", "resources"];

// ---------------------------------------------------------------------------
// sites
// ---------------------------------------------------------------------------

pub const SITE_COLS: &str =
    "id, domain, wp_url, mode, api_key, settings, secrets_enc, created_at, last_sync_at, user_id";

#[derive(Debug, Clone)]
pub struct SiteRow {
    pub id: String,
    pub domain: String,
    pub wp_url: Option<String>,
    pub mode: String,
    pub api_key: String,
    pub settings: Value,
    pub secrets_enc: String,
    pub created_at: String,
    pub last_sync_at: Option<String>,
    pub user_id: Option<String>,
}

impl SiteRow {
    pub fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let settings: String = r.get("settings")?;
        Ok(Self {
            id: r.get("id")?,
            domain: r.get("domain")?,
            wp_url: r.get("wp_url")?,
            mode: r.get("mode")?,
            api_key: r.get("api_key")?,
            settings: serde_json::from_str(&settings).unwrap_or_else(|_| json!({})),
            secrets_enc: r.get("secrets_enc")?,
            created_at: r.get("created_at")?,
            last_sync_at: r.get("last_sync_at")?,
            user_id: r.get("user_id")?,
        })
    }

    pub fn secrets(&self, key: &[u8; 32]) -> SiteSecrets {
        SiteSecrets::load(key, &self.secrets_enc).unwrap_or_default()
    }

    /// Public site payload: `sites.*` minus `api_key`/`secrets_enc`, credentials stripped from
    /// `settings`, plus `has_wp_credentials` and `wp_user` (username only).
    pub fn to_json(&self, key: &[u8; 32], with_api_key: bool) -> Value {
        let mut settings = self.settings.clone();
        let mut legacy = SiteSecrets::default();
        // Legacy (unmigrated) rows may still hold plaintext credentials in settings: strip them.
        legacy.absorb_from_settings(&mut settings);
        let secrets = self.secrets(key);
        let wp_user = secrets.wp_user.clone().or(legacy.wp_user.clone());
        let has_wp = secrets.has_wp_credentials() || legacy.has_wp_credentials();
        let mut v = json!({
            "id": self.id,
            "domain": self.domain,
            "wp_url": self.wp_url,
            "mode": self.mode,
            "settings": settings,
            "created_at": self.created_at,
            "last_sync_at": self.last_sync_at,
            "user_id": self.user_id,
            "has_wp_credentials": has_wp,
            "wp_user": wp_user,
        });
        if with_api_key && let Some(o) = v.as_object_mut() {
            o.insert("api_key".into(), Value::String(self.api_key.clone()));
        }
        v
    }
}

pub fn load_site(c: &rusqlite::Connection, id: &str) -> sf_core::Result<Option<SiteRow>> {
    use rusqlite::OptionalExtension;
    c.query_row(
        &format!("SELECT {SITE_COLS} FROM sites WHERE id=?1"),
        [id],
        SiteRow::from_row,
    )
    .optional()
    .map_err(err)
}

/// `(wp_url, mode)` updates applied together with a settings mutation.
pub type SiteExtra = Option<(Option<String>, Option<String>)>;

/// Read-modify-write of `settings` + sealed secrets inside ONE write transaction (no lost updates).
/// The closure returns whether the secrets changed.
pub async fn mutate_site<F>(st: &AppState, id: &str, extra: SiteExtra, f: F) -> ApiResult<SiteRow>
where
    F: FnOnce(&mut Value, &mut SiteSecrets) -> ApiResult<bool> + Send + 'static,
{
    let key = st.secret_key.clone();
    let id = id.to_string();
    let res: sf_core::Result<Result<SiteRow, ApiError>> = st
        .db
        .write(move |t| {
            let Some(site) = load_site(t, &id)? else {
                return Ok(Err(ApiError::not_found("site not found")));
            };
            let mut settings = site.settings.clone();
            let mut secrets = site.secrets(&key);
            // Legacy plaintext credentials never survive a write.
            let mut changed = secrets.absorb_from_settings(&mut settings);
            match f(&mut settings, &mut secrets) {
                Ok(c) => changed |= c,
                Err(e) => return Ok(Err(e)),
            }
            let sealed = if changed {
                Some(secrets.seal(&key)?)
            } else {
                None
            };
            let (wp_url, mode) = extra.unwrap_or((None, None));
            t.execute(
                "UPDATE sites SET settings=?2, secrets_enc=coalesce(?3, secrets_enc),
                   wp_url=coalesce(?4, wp_url), mode=coalesce(?5, mode) WHERE id=?1",
                rusqlite::params![id, settings.to_string(), sealed, wp_url, mode],
            )
            .map_err(err)?;
            Ok(load_site(t, &id)?.ok_or_else(|| ApiError::not_found("site not found")))
        })
        .await;
    res?
}

/// L-02: hash every legacy plaintext `sites.api_key` in place (v1 imports, pre-2.0.2 rows).
/// Idempotent; the JWT fingerprint is unchanged, so plugin sessions keep working.
pub async fn hash_legacy_site_keys(db: &sf_store::Db) -> sf_core::Result<usize> {
    db.write(|t| {
        let rows: Vec<(String, String)> = {
            let mut q = t
                .prepare("SELECT id, api_key FROM sites WHERE api_key NOT LIKE 'sha256:%' AND api_key <> ''")
                .map_err(err)?;
            q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(err)?
                .collect::<rusqlite::Result<_>>()
                .map_err(err)?
        };
        let mut up = t
            .prepare("UPDATE sites SET api_key=?2 WHERE id=?1 AND api_key=?3")
            .map_err(err)?;
        let mut n = 0usize;
        for (id, key) in rows {
            n += up
                .execute(rusqlite::params![
                    id,
                    sf_core::crypto::site_key_stored(&key),
                    key
                ])
                .map_err(err)?;
        }
        Ok(n)
    })
    .await
}

pub async fn get_site(st: &AppState, id: &str) -> ApiResult<SiteRow> {
    let id = id.to_string();
    st.db
        .read(move |c| load_site(c, &id))
        .await?
        .ok_or_else(|| ApiError::not_found("site not found"))
}

// ---------------------------------------------------------------------------
// SQL -> JSON
// ---------------------------------------------------------------------------

/// Runs a query and maps every row through `row_to_json`.
pub fn query_json(
    c: &rusqlite::Connection,
    sql: &str,
    params: &[&dyn ToSql],
    json_cols: &[&str],
    bool_cols: &[&str],
) -> sf_core::Result<Vec<Value>> {
    let mut stmt = c.prepare(sql).map_err(err)?;
    let rows = stmt
        .query_map(params, |r| Ok(row_to_json(r, json_cols, bool_cols)))
        .map_err(err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(err)?;
    Ok(rows)
}

pub fn query_one_json(
    c: &rusqlite::Connection,
    sql: &str,
    params: &[&dyn ToSql],
    json_cols: &[&str],
    bool_cols: &[&str],
) -> sf_core::Result<Option<Value>> {
    Ok(query_json(c, sql, params, json_cols, bool_cols)?
        .into_iter()
        .next())
}

pub fn count(c: &rusqlite::Connection, sql: &str, params: &[&dyn ToSql]) -> sf_core::Result<i64> {
    c.query_row(sql, params, |r| r.get::<_, Option<i64>>(0))
        .map(|v| v.unwrap_or(0))
        .map_err(err)
}

/// JSON number: integral values print without a fraction (JS `Number` parity).
pub fn num(v: f64) -> Value {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.0e15 {
        Value::from(v as i64)
    } else {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

pub fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

pub const EDGE_JSON: &[&str] = &["score_breakdown"];

// ---------------------------------------------------------------------------
// text helpers
// ---------------------------------------------------------------------------

/// v1 `stripTags`: regex HTML -> text (scripts/styles/comments removed, a few entities decoded).
pub fn strip_tags(html: &str) -> String {
    use std::sync::LazyLock;
    static RES: LazyLock<Option<[regex::Regex; 4]>> = LazyLock::new(|| {
        Some([
            regex::Regex::new(r"(?is)<script.*?</script>").ok()?,
            regex::Regex::new(r"(?is)<style.*?</style>").ok()?,
            regex::Regex::new(r"(?s)<!--.*?-->").ok()?,
            regex::Regex::new(r"<[^>]+>").ok()?,
        ])
    });
    let mut s = html.to_string();
    if let Some(res) = RES.as_ref() {
        for re in res {
            s = re.replace_all(&s, " ").into_owned();
        }
    }
    let s = replace_ci(&s, "&nbsp;", " ");
    let s = replace_ci(&s, "&amp;", "&");
    let s = replace_ci(&s, "&lt;", "<");
    let s = replace_ci(&s, "&gt;", ">");
    let s = replace_ci(&s, "&quot;", "\"");
    let s = s.replace("&#039;", "'").replace("&#39;", "'");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn replace_ci(s: &str, pat: &str, with: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (i, _) in lower.match_indices(pat) {
        if let (Some(a), true) = (s.get(last..i), i >= last) {
            out.push_str(a);
            out.push_str(with);
            last = i + pat.len();
        }
    }
    out.push_str(s.get(last..).unwrap_or(""));
    out
}

pub fn word_count(text: &str) -> i64 {
    i64::try_from(text.split_whitespace().count()).unwrap_or(i64::MAX)
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

/// One CSV cell with the formula-injection guard (leading `= + - @`, tab or CR gets a `'`).
/// L-08: true when a spreadsheet could read the cell as a formula: the first non-whitespace
/// character is `= + - @ |` (or a fullwidth `＝ ＋ － ＠`), or the cell starts with a tab/CR/LF.
fn csv_formula_like(v: &str) -> bool {
    v.starts_with(['\t', '\r', '\n'])
        || v.trim_start().starts_with([
            '=', '+', '-', '@', '|', '\u{FF1D}', '\u{FF0B}', '\u{FF0D}', '\u{FF20}',
        ])
}

pub fn csv_cell(v: &str) -> String {
    let mut s = v.to_string();
    if csv_formula_like(&s) {
        s.insert(0, '\'');
    }
    if s.contains(['"', ',', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

/// Appends one [`csv_cell`] to `out` without intermediate allocations.
pub fn push_csv_cell(out: &mut String, v: &str) {
    let guard = csv_formula_like(v);
    // The `'` guard never needs quoting, so the check on `v` decides for the whole cell.
    if v.contains(['"', ',', '\n', '\r']) {
        out.push('"');
        if guard {
            out.push('\'');
        }
        for ch in v.chars() {
            if ch == '"' {
                out.push_str("\"\"");
            } else {
                out.push(ch);
            }
        }
        out.push('"');
    } else {
        if guard {
            out.push('\'');
        }
        out.push_str(v);
    }
}

/// One CSV line of [`csv_cell`]s (single allocation; large exports render ~46k lines).
pub fn csv_line(cells: &[&str]) -> String {
    let mut out = String::with_capacity(cells.iter().map(|c| c.len() + 3).sum());
    for (i, c) in cells.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_csv_cell(&mut out, c);
    }
    out
}

/// The first (primary) level of [`v1_collate`] as a byte-comparable key: when the keys of two
/// strings differ, their byte order is the `v1_collate` order (UTF-8 preserves code point order).
/// Equal keys need the full `v1_collate`. Lets big in-memory sorts compare with `memcmp`.
pub fn v1_primary_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for l in alnum(s).flat_map(char::to_lowercase) {
        let (x, y) = fold_lower(l);
        out.push(x);
        if let Some(y) = y {
            out.push(y);
        }
    }
    out
}

/// Filename-safe slug of a domain (fixes header injection, v1 S12).
pub fn safe_filename_part(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    if out.is_empty() { "site".into() } else { out }
}

pub fn csv_response(filename: &str, body: String) -> Response {
    let mut res = body.into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    res
}

// ---------------------------------------------------------------------------
// rate limiting (login / handshake)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct RateLimiter {
    hits: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl RateLimiter {
    /// Records a hit and returns false when `key` exceeded `max` hits within `window`.
    pub fn allow(&self, key: &str, max: usize, window: Duration) -> bool {
        let mut map = self.hits.lock();
        if map.len() > 10_000 {
            map.retain(|_, q| q.back().is_some_and(|t| t.elapsed() < window));
            if map.len() > 10_000 {
                map.clear();
            }
        }
        let q = map.entry(key.to_string()).or_default();
        while q.front().is_some_and(|t| t.elapsed() >= window) {
            q.pop_front();
        }
        if q.len() >= max {
            return false;
        }
        q.push_back(Instant::now());
        true
    }

    /// True while `key` has fewer than `max` hits within `window` (records nothing).
    pub fn peek(&self, key: &str, max: usize, window: Duration) -> bool {
        let mut map = self.hits.lock();
        let Some(q) = map.get_mut(key) else {
            return true;
        };
        while q.front().is_some_and(|t| t.elapsed() >= window) {
            q.pop_front();
        }
        q.len() < max
    }

    /// Records one hit for `key` (used for failures only).
    pub fn record(&self, key: &str, window: Duration) {
        let mut map = self.hits.lock();
        if map.len() > 10_000 {
            map.retain(|_, q| q.back().is_some_and(|t| t.elapsed() < window));
            if map.len() > 10_000 {
                map.clear();
            }
        }
        map.entry(key.to_string())
            .or_default()
            .push_back(Instant::now());
    }
}

/// (failures, last failure, locked until) for one account.
type LockState = (u32, Instant, Option<Instant>);

/// Per-account lockout for repeated failed logins on one email (M-03): after
/// [`LOCKOUT_FREE_FAILURES`] failures each further failure locks the account for
/// `LOCKOUT_BASE * 2^(n - free)`, capped at [`LOCKOUT_MAX`]. A success clears it; failures older
/// than [`LOCKOUT_RESET`] are forgotten. Applies to unknown emails too (no existence oracle).
#[derive(Default)]
pub struct LoginLockout {
    m: Mutex<HashMap<String, LockState>>,
}

pub const LOCKOUT_FREE_FAILURES: u32 = 5;
pub const LOCKOUT_BASE: Duration = Duration::from_secs(30);
pub const LOCKOUT_MAX: Duration = Duration::from_secs(15 * 60);
pub const LOCKOUT_RESET: Duration = Duration::from_secs(60 * 60);

impl LoginLockout {
    /// Remaining lock time for `account`, if locked.
    pub fn locked_for(&self, account: &str) -> Option<Duration> {
        let map = self.m.lock();
        let (_, _, until) = map.get(account)?;
        let until = (*until)?;
        let now = Instant::now();
        (until > now).then(|| until - now)
    }

    /// Records a failure; returns the new lock duration when the account is now locked.
    pub fn fail(&self, account: &str) -> Option<Duration> {
        let mut map = self.m.lock();
        if map.len() > 10_000 {
            map.retain(|_, (_, last, _)| last.elapsed() < LOCKOUT_RESET);
            if map.len() > 10_000 {
                map.clear();
            }
        }
        let now = Instant::now();
        let e = map.entry(account.to_string()).or_insert((0, now, None));
        if e.1.elapsed() >= LOCKOUT_RESET {
            *e = (0, now, None);
        }
        e.0 = e.0.saturating_add(1);
        e.1 = now;
        if e.0 < LOCKOUT_FREE_FAILURES {
            return None;
        }
        let exp = (e.0 - LOCKOUT_FREE_FAILURES).min(16);
        let d = LOCKOUT_BASE.saturating_mul(1u32 << exp).min(LOCKOUT_MAX);
        e.2 = Some(now + d);
        Some(d)
    }

    /// Clears the failures of `account` (successful login).
    pub fn success(&self, account: &str) {
        self.m.lock().remove(account);
    }
}

/// Per-AppState login lockout kept in the service registry.
pub fn lockout(st: &AppState) -> Arc<LoginLockout> {
    if let Some(l) = st.services.get::<LoginLockout>() {
        return l;
    }
    let l = Arc::new(LoginLockout::default());
    st.services.insert(l.clone());
    l
}

/// Per-AppState limiter kept in the service registry.
pub fn limiter(st: &AppState) -> Arc<RateLimiter> {
    if let Some(l) = st.services.get::<RateLimiter>() {
        return l;
    }
    let l = Arc::new(RateLimiter::default());
    st.services.insert(l.clone());
    l
}

pub const AUTH_RATE_MAX: usize = 10;
pub const AUTH_RATE_WINDOW: Duration = Duration::from_secs(60);

fn too_many() -> ApiError {
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "too many attempts, try again in a minute",
    )
}

/// Failure-only limiter (M-03): refuses once `route:ip` has [`AUTH_RATE_MAX`] recorded failures in
/// the window; successes are never counted. Pair with [`rate_fail`].
pub fn rate_guard(st: &AppState, route: &str, ip: &str) -> ApiResult<()> {
    if limiter(st).peek(&format!("{route}:{ip}"), AUTH_RATE_MAX, AUTH_RATE_WINDOW) {
        Ok(())
    } else {
        Err(too_many())
    }
}

/// Records one failed attempt for `route:ip`.
pub fn rate_fail(st: &AppState, route: &str, ip: &str) {
    limiter(st).record(&format!("{route}:{ip}"), AUTH_RATE_WINDOW);
}

pub fn rate_check(st: &AppState, route: &str, ip: &str) -> ApiResult<()> {
    if limiter(st).allow(&format!("{route}:{ip}"), AUTH_RATE_MAX, AUTH_RATE_WINDOW) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many attempts, try again in a minute",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pairs taken from the live v1 (Postgres en_US.UTF-8) order: each left value sorts first.
    #[test]
    fn csv_line_matches_cell_join() {
        let cells = [
            "",
            "plain",
            "=cmd",
            "+1",
            "-x",
            "@a",
            "\tt",
            "\rr",
            "a,b",
            "say \"hi\"",
            "=a,\"b\"",
            "multi\nline",
            "é,ü",
            "'",
        ];
        let old = cells
            .iter()
            .map(|c| csv_cell(c))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(csv_line(&cells), old);
        assert_eq!(csv_line(&[]), "");
        for c in cells {
            assert_eq!(csv_line(&[c]), csv_cell(c));
        }
    }

    #[test]
    fn v1_primary_key_orders_like_v1_collate() {
        let words = [
            "https://x.com/tag/bottle/",
            "https://x.com/tag/bottle-accessories/",
            "water bottled brands",
            "water bottle many",
            "beer bottle crafts",
            "Beer Bottle Crafts",
            "home decor",
            "Home Décor",
            "10 Best",
            "apple",
            "Zebra",
            "Bottle",
            "Bottle Cap Crafts",
            "eclair",
            "Éclair",
            "Straße",
            "strasse",
            "Æsir",
            "aesir",
            "",
            "--",
            "a-b",
            "ab",
            "日本",
            "x.com/A",
            "x.com/a",
        ];
        for a in words {
            for b in words {
                let (ka, kb) = (v1_primary_key(a), v1_primary_key(b));
                if ka != kb {
                    assert_eq!(ka.cmp(&kb), v1_collate(a, b), "{a:?} vs {b:?}");
                }
            }
        }
    }

    #[test]
    fn v1_collate_matches_postgres_en_us() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        let pairs = [
            (
                "https://x.com/tag/bottle/",
                "https://x.com/tag/bottle-accessories/",
            ),
            ("water bottled brands", "water bottle many"),
            ("beer bottle crafts", "Beer Bottle Crafts"),
            ("home decor", "Home Décor"),
            ("10 Best", "apple"),
            ("apple", "Zebra"),
            ("Bottle", "Bottle Cap Crafts"),
            ("eclair", "Éclair"),
        ];
        for (a, b) in pairs {
            assert_eq!(v1_collate(a, b), Less, "{a:?} < {b:?}");
            assert_eq!(v1_collate(b, a), Greater, "{b:?} > {a:?}");
        }
        assert_eq!(v1_collate("same", "same"), Equal);
        // hostile input never panics and compares equal to itself
        for s in [
            "",
            "\u{0}",
            "ß",
            "ǅ",
            "İ",
            "\u{10FFFF}",
            "-- //",
            "\u{FFFD}",
        ] {
            assert_eq!(v1_collate(s, s), Equal);
            let _ = v1_collate(s, "a");
        }
    }

    #[test]
    fn helpers() {
        assert!(is_uuid("0f8fad5b-d9cb-469f-a165-70867728950e"));
        assert!(!is_uuid("0f8fad5b-d9cb-469f-a165-70867728950"));
        assert!(!is_uuid("zz8fad5b-d9cb-469f-a165-70867728950e"));
        assert!(is_http_url("https://a.com/x"));
        assert!(!is_http_url("javascript:alert(1)"));
        assert!(!is_http_url("http://"));
        assert_eq!(csv_cell("=1+1"), "'=1+1");
        // L-08: leading whitespace, pipes, newlines and fullwidth signs are guarded too
        for v in [
            " =HYPERLINK(1)",
            "\n=1",
            "|cmd",
            "\u{FF1D}1+1",
            "  \u{FF0B}1",
        ] {
            assert!(
                csv_cell(v).trim_start_matches('"').starts_with('\''),
                "{v:?}"
            );
        }
        assert_eq!(csv_cell("plain"), "plain");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(safe_filename_part("ev\"il\r\n.com"), "ev_il__.com");
        assert_eq!(
            strip_tags("<p>Hi&nbsp;<b>there</b></p><script>x()</script>&AMP;"),
            "Hi there &"
        );
        assert_eq!(word_count("  a b  c "), 3);
        assert_eq!(num(3.0), json!(3));
        assert_eq!(num(0.5), json!(0.5));
        let mut q = HashMap::new();
        q.insert("limit".to_string(), "99999".to_string());
        assert_eq!(int_param(&q, "limit", 100, 1, 1000), 1000);
        q.insert("limit".to_string(), "abc".to_string());
        assert_eq!(int_param(&q, "limit", 100, 1, 1000), 100);
        let rl = RateLimiter::default();
        for _ in 0..3 {
            assert!(rl.allow("k", 3, Duration::from_secs(60)));
        }
        assert!(!rl.allow("k", 3, Duration::from_secs(60)));
        // failure-only API
        let w = Duration::from_secs(60);
        assert!(rl.peek("f", 2, w));
        rl.record("f", w);
        assert!(rl.peek("f", 2, w));
        rl.record("f", w);
        assert!(!rl.peek("f", 2, w));
        assert!(rl.peek("other", 2, w));
        // lockout: free failures, then exponential backoff, success clears
        let lo = LoginLockout::default();
        for _ in 1..LOCKOUT_FREE_FAILURES {
            assert!(lo.fail("a@x").is_none());
        }
        assert_eq!(lo.fail("a@x"), Some(LOCKOUT_BASE));
        assert_eq!(lo.fail("a@x"), Some(LOCKOUT_BASE * 2));
        assert!(lo.locked_for("a@x").is_some());
        assert!(lo.locked_for("b@x").is_none());
        for _ in 0..40 {
            let _ = lo.fail("a@x");
        }
        assert_eq!(lo.fail("a@x"), Some(LOCKOUT_MAX));
        lo.success("a@x");
        assert!(lo.locked_for("a@x").is_none());
    }

    #[test]
    fn strip_tags_hostile() {
        for s in [
            "<",
            "<<<>>>",
            "&",
            "<!--",
            "<script>",
            "\u{0}é<é>",
            "&nbsp&nbsp;",
        ] {
            let _ = strip_tags(s);
        }
    }
}
