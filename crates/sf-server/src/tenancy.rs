//! v2.4 tenancy (docs/specs/v2.4-saas.md §1, §2, §9.1/§9.2): workspace roles, the request context
//! (method + `X-SiloForge-Workspace` / `X-SiloForge-View-As`), site access by membership, list
//! scopes, create targets, plan limits, usage and quota checks, and the boot-time back-fill.
//!
//! Compatibility: a site whose `workspace_id` is still NULL (rows written by older code paths or
//! tools, before the boot back-fill ran) keeps v1 semantics: its creator (`sites.user_id`) and
//! platform admins see it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::http::Method;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::Response;
use parking_lot::Mutex;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sf_store::err;

use crate::auth::{Principal, User};
use crate::{ApiError, ApiResult, AppState};

pub const DEFAULT_WS: &str = "default";
pub const WORKSPACE_HEADER: &str = "x-siloforge-workspace";
pub const VIEW_AS_HEADER: &str = "x-siloforge-view-as";
/// Days a soft-deleted workspace is kept before the purge.
pub const PURGE_AFTER_DAYS: i64 = 30;

// ---------------------------------------------------------------------------
// roles
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Viewer = 1,
    Member = 2,
    Admin = 3,
    Owner = 4,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "viewer" => Some(Role::Viewer),
            "member" => Some(Role::Member),
            "admin" => Some(Role::Admin),
            "owner" => Some(Role::Owner),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Member => "member",
            Role::Admin => "admin",
            Role::Owner => "owner",
        }
    }
}

// ---------------------------------------------------------------------------
// request context
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ReqCtx {
    /// `true` once the middleware ran (handlers called outside a request: strict defaults).
    pub known: bool,
    pub write: bool,
    pub workspace: Option<String>,
    pub view_as: Option<String>,
}

impl ReqCtx {
    /// Role a site-scoped route needs by default: GET/HEAD viewer, everything else member.
    pub fn default_role(&self) -> Role {
        if self.known && !self.write {
            Role::Viewer
        } else {
            Role::Member
        }
    }
}

tokio::task_local! {
    static REQ: ReqCtx;
}

/// The current request's context (strict defaults outside a request).
pub fn ctx() -> ReqCtx {
    REQ.try_with(Clone::clone).unwrap_or_default()
}

fn header_id(req_headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let v = req_headers.get(name)?.to_str().ok()?.trim();
    (!v.is_empty() && v.len() <= 128).then(|| v.to_string())
}

/// Puts the request context in scope for the handler.
pub async fn middleware(req: Request, next: Next) -> Response {
    let c = ReqCtx {
        known: true,
        write: !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS),
        workspace: header_id(req.headers(), WORKSPACE_HEADER),
        view_as: header_id(req.headers(), VIEW_AS_HEADER),
    };
    REQ.scope(c, next.run(req)).await
}

/// `X-SiloForge-View-As`: platform admins only, read-only, logged (throttled per admin + workspace).
pub fn check_view_as(st: &AppState, p: &Principal, parts: &Parts) -> ApiResult<()> {
    let Some(ws) = header_id(&parts.headers, VIEW_AS_HEADER) else {
        return Ok(());
    };
    let Principal::User { user_id, role, .. } = p else {
        return Err(ApiError::forbidden("view-as is for platform admins"));
    };
    if role != "admin" {
        return Err(ApiError::forbidden("view-as is for platform admins"));
    }
    if !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return Err(ApiError::forbidden("view-as is read-only"));
    }
    static SEEN: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);
    let key = format!("{user_id}:{ws}");
    let first = {
        let mut g = SEEN.lock();
        let m = g.get_or_insert_with(HashMap::new);
        if m.len() > 1024 {
            m.clear();
        }
        match m.get(&key) {
            Some(t) if t.elapsed() < Duration::from_secs(600) => false,
            _ => {
                m.insert(key, Instant::now());
                true
            }
        }
    };
    if first {
        crate::eventlog::log(st).record_auth(
            &format!("user:{user_id}"),
            "admin view-as",
            "ok",
            Some(user_id),
            Some(&ws),
            json!({ "workspace_id": ws, "path": parts.uri.path() }),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// workspaces
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Ws {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub plan_id: String,
    pub status: String,
    pub owner_user_id: Option<String>,
    pub is_default: bool,
    pub created_at: String,
    pub deleted_at: Option<String>,
}

pub const WS_COLS: &str =
    "id, name, slug, plan_id, status, owner_user_id, is_default, created_at, deleted_at";

impl Ws {
    pub fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            name: r.get(1)?,
            slug: r.get(2)?,
            plan_id: r.get(3)?,
            status: r.get(4)?,
            owner_user_id: r.get(5)?,
            is_default: r.get::<_, i64>(6)? != 0,
            created_at: r.get(7)?,
            deleted_at: r.get(8)?,
        })
    }
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id, "name": self.name, "slug": self.slug, "plan_id": self.plan_id,
            "status": self.status, "owner_user_id": self.owner_user_id, "is_default": self.is_default,
            "created_at": self.created_at, "deleted_at": self.deleted_at,
        })
    }
}

pub async fn load_ws(st: &AppState, id: &str) -> ApiResult<Option<Ws>> {
    let id = id.to_string();
    Ok(st
        .db
        .read(move |c| {
            c.query_row(
                &format!("SELECT {WS_COLS} FROM workspaces WHERE id=?1"),
                [&id],
                Ws::from_row,
            )
            .optional()
            .map_err(err)
        })
        .await?)
}

pub async fn member_role(st: &AppState, user_id: &str, ws_id: &str) -> ApiResult<Option<Role>> {
    let (u, w) = (user_id.to_string(), ws_id.to_string());
    let r: Option<String> = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT role FROM workspace_members WHERE workspace_id=?1 AND user_id=?2",
                params![w, u],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)
        })
        .await?;
    Ok(r.as_deref().and_then(Role::parse))
}

/// The caller's effective role in a workspace: platform admins act as owner. `None` = no access
/// (not a member, deleted workspace, or an API token bound to another workspace).
pub async fn ws_role(st: &AppState, u: &User, ws: &Ws) -> ApiResult<Option<Role>> {
    if u.is_admin() {
        return Ok(Some(Role::Owner));
    }
    if ws.deleted_at.is_some() {
        return Ok(None);
    }
    if u.token.is_some()
        && let Some(tw) = &u.workspace_id
        && tw != &ws.id
    {
        return Ok(None);
    }
    member_role(st, &u.user_id, &ws.id).await
}

/// Loads a workspace the caller may act on with at least `need` (404 when it is not visible to
/// them, 403 when visible but the role is too low). Writes to a suspended workspace are refused.
pub async fn ws_guard(st: &AppState, u: &User, ws_id: &str, need: Role) -> ApiResult<(Ws, Role)> {
    let ws = load_ws(st, ws_id)
        .await?
        .ok_or_else(|| ApiError::not_found("workspace not found"))?;
    let role = ws_role(st, u, &ws)
        .await?
        .ok_or_else(|| ApiError::not_found("workspace not found"))?;
    if role < need {
        return Err(ApiError::forbidden(format!(
            "your role in this workspace ({}) does not allow this",
            role.as_str()
        )));
    }
    if ws.status == "suspended" && ctx().write && !u.is_admin() {
        return Err(ApiError::forbidden("workspace suspended"));
    }
    Ok((ws, role))
}

// ---------------------------------------------------------------------------
// site access
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SiteAccess {
    pub workspace_id: Option<String>,
    pub role: Role,
}

/// Site gate: 404 for unknown / foreign sites, 403 when the member role is too low or the
/// workspace is suspended (writes).
pub async fn site_access(
    st: &AppState,
    who: &Principal,
    site_id: &str,
    need: Role,
) -> ApiResult<SiteAccess> {
    let (user_id, role, token, token_ws) = match who {
        Principal::Site { site_id: own } => {
            return if own == site_id {
                Ok(SiteAccess {
                    workspace_id: None,
                    role: Role::Member,
                })
            } else {
                Err(ApiError::not_found("site not found"))
            };
        }
        Principal::User {
            user_id,
            role,
            token,
            workspace_id,
            ..
        } => (user_id, role, token, workspace_id),
    };
    let (sid, uid) = (site_id.to_string(), user_id.clone());
    type Row = (
        Option<String>,
        Option<String>,
        Option<String>,
        bool,
        Option<String>,
    );
    let row: Option<Row> = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT s.user_id, s.workspace_id, w.status, coalesce(w.deleted_at IS NOT NULL, 0),
                        (SELECT m.role FROM workspace_members m
                          WHERE m.workspace_id = s.workspace_id AND m.user_id = ?2)
                   FROM sites s LEFT JOIN workspaces w ON w.id = s.workspace_id
                  WHERE s.id = ?1",
                params![sid, uid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(err)
        })
        .await?;
    let Some((owner, ws, status, deleted, member)) = row else {
        return Err(ApiError::not_found("site not found"));
    };
    let c = ctx();
    if role == "admin" {
        if let Some(v) = &c.view_as
            && ws.as_deref().unwrap_or(DEFAULT_WS) != v
        {
            return Err(ApiError::not_found("site not found"));
        }
        return Ok(SiteAccess {
            workspace_id: ws,
            role: Role::Owner,
        });
    }
    let Some(ws) = ws else {
        // legacy row without a workspace: v1 ownership
        return if owner.as_deref() == Some(user_id.as_str()) {
            Ok(SiteAccess {
                workspace_id: None,
                role: Role::Owner,
            })
        } else {
            Err(ApiError::not_found("site not found"))
        };
    };
    if deleted {
        return Err(ApiError::not_found("site not found"));
    }
    if token.is_some()
        && let Some(tw) = token_ws
        && tw != &ws
    {
        return Err(ApiError::not_found("site not found"));
    }
    let Some(r) = member.as_deref().and_then(Role::parse) else {
        return Err(ApiError::not_found("site not found"));
    };
    if r < need {
        return Err(ApiError::forbidden(format!(
            "your role in this workspace ({}) does not allow this",
            r.as_str()
        )));
    }
    if status.as_deref() == Some("suspended") && c.write {
        return Err(ApiError::forbidden("workspace suspended"));
    }
    Ok(SiteAccess {
        workspace_id: Some(ws),
        role: r,
    })
}

/// Stronger site gate for admin-level site routes (settings, keys, credentials).
pub async fn require_site_role(
    st: &AppState,
    u: &User,
    site_id: &str,
    need: Role,
) -> ApiResult<SiteAccess> {
    site_access(st, &u.principal(), site_id, need).await
}

// ---------------------------------------------------------------------------
// list scope + create target
// ---------------------------------------------------------------------------

/// SQL string literal (ids come from the database or were validated against it).
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The workspace a request selects (header or view-as), validated for the caller.
pub async fn selected_ws(st: &AppState, u: &User) -> ApiResult<Option<Ws>> {
    let c = ctx();
    let Some(id) = c.view_as.or(c.workspace) else {
        return Ok(None);
    };
    let ws = load_ws(st, &id)
        .await?
        .ok_or_else(|| ApiError::not_found("workspace not found"))?;
    if ws_role(st, u, &ws).await?.is_none() {
        return Err(ApiError::not_found("workspace not found"));
    }
    Ok(Some(ws))
}

/// SQL predicate over the sites alias `a` (e.g. `s`) for "sites this caller may list".
pub async fn site_scope(st: &AppState, u: &User, a: &str) -> ApiResult<String> {
    let mut parts: Vec<String> = Vec::new();
    let uid = lit(&u.user_id);
    if !u.is_admin() {
        parts.push(format!(
            "({a}.workspace_id IN (SELECT m.workspace_id FROM workspace_members m
               JOIN workspaces w ON w.id = m.workspace_id
              WHERE m.user_id = {uid} AND w.deleted_at IS NULL)
              OR ({a}.workspace_id IS NULL AND {a}.user_id = {uid}))"
        ));
        if u.token.is_some()
            && let Some(tw) = &u.workspace_id
        {
            let tw = lit(tw);
            parts.push(format!(
                "({a}.workspace_id = {tw} OR {a}.workspace_id IS NULL)"
            ));
        }
    }
    if let Some(ws) = selected_ws(st, u).await? {
        let w = lit(&ws.id);
        parts.push(format!(
            "({a}.workspace_id = {w} OR ({a}.workspace_id IS NULL AND (
                ({w} = 'default' AND ({a}.user_id IS NULL OR {a}.user_id IN (SELECT id FROM users WHERE role = 'admin')))
                OR {w} = 'ws-' || {a}.user_id)))"
        ));
    }
    if parts.is_empty() {
        Ok("1=1".into())
    } else {
        Ok(parts.join(" AND "))
    }
}

/// SQL predicate over sites alias `a`: the caller may write there (member role or better, workspace
/// not suspended). Platform admins: always.
pub fn writable_pred(u: &User, a: &str) -> String {
    if u.is_admin() {
        return "1=1".into();
    }
    let uid = lit(&u.user_id);
    format!(
        "({a}.workspace_id IS NULL OR (EXISTS (SELECT 1 FROM workspace_members m
            WHERE m.workspace_id = {a}.workspace_id AND m.user_id = {uid}
              AND m.role IN ('owner','admin','member'))
          AND NOT EXISTS (SELECT 1 FROM workspaces w WHERE w.id = {a}.workspace_id AND w.status = 'suspended')))"
    )
}

/// Where a new site / token / schedule goes: the selected workspace (role checked by the
/// caller), the token's workspace, or the caller's default. `None` = legacy (no workspace yet).
pub async fn target_ws(st: &AppState, u: &User) -> ApiResult<Option<Ws>> {
    if let Some(ws) = selected_ws(st, u).await? {
        return Ok(Some(ws));
    }
    if let Some(tw) = &u.workspace_id
        && let Some(ws) = load_ws(st, tw).await?
        && ws.deleted_at.is_none()
    {
        return Ok(Some(ws));
    }
    default_ws(st, u).await
}

/// Default workspace: platform admins → Default; others → first owned, else first joined.
pub async fn default_ws(st: &AppState, u: &User) -> ApiResult<Option<Ws>> {
    let (uid, admin) = (u.user_id.clone(), u.is_admin());
    Ok(st
        .db
        .read(move |c| {
            let cols = WS_COLS
                .split(", ")
                .map(|x| format!("w.{x}"))
                .collect::<Vec<_>>()
                .join(", ");
            if admin
                && let Some(w) = c
                    .query_row(
                        &format!("SELECT {WS_COLS} FROM workspaces WHERE id='default' AND deleted_at IS NULL"),
                        [],
                        Ws::from_row,
                    )
                    .optional()
                    .map_err(err)?
            {
                return Ok(Some(w));
            }
            c.query_row(
                &format!(
                    "SELECT {cols} FROM workspace_members m JOIN workspaces w ON w.id = m.workspace_id
                      WHERE m.user_id = ?1 AND w.deleted_at IS NULL
                      ORDER BY (m.role = 'owner') DESC, w.is_default DESC, m.created_at, w.id LIMIT 1"
                ),
                [&uid],
                Ws::from_row,
            )
            .optional()
            .map_err(err)
        })
        .await?)
}

// ---------------------------------------------------------------------------
// plans, usage, quotas
// ---------------------------------------------------------------------------

pub const LIMIT_KEYS: &[&str] = &[
    "sites",
    "posts_per_site",
    "link_runs_per_month",
    "ai_tokens_per_month",
    "members",
    "api_tokens",
    "schedules",
];
pub const FEATURE_KEYS: &[&str] = &["auto_linker", "mcp"];

#[derive(Debug, Clone)]
pub struct Plan {
    pub id: String,
    pub name: String,
    pub price_label: String,
    pub limits: Value,
    pub public: bool,
    pub sort: i64,
}

impl Plan {
    pub fn to_json(&self) -> Value {
        json!({ "id": self.id, "name": self.name, "price_label": self.price_label,
                "limits": self.limits, "public": self.public, "sort": self.sort })
    }
    /// Numeric limit (`None` = unlimited).
    pub fn limit(&self, key: &str) -> Option<i64> {
        self.limits.get(key).and_then(Value::as_i64)
    }
    pub fn feature(&self, key: &str) -> bool {
        self.limits
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }
}

pub const PLAN_COLS: &str = "id, name, price_label, limits, public, sort";

pub fn plan_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Plan> {
    let limits: String = r.get(3)?;
    Ok(Plan {
        id: r.get(0)?,
        name: r.get(1)?,
        price_label: r.get(2)?,
        limits: serde_json::from_str(&limits).unwrap_or_else(|_| json!({})),
        public: r.get::<_, i64>(4)? != 0,
        sort: r.get(5)?,
    })
}

pub async fn plan_of_ws(st: &AppState, ws_id: &str) -> ApiResult<Option<Plan>> {
    let w = ws_id.to_string();
    Ok(st
        .db
        .read(move |c| {
            c.query_row(
                &format!(
                    "SELECT p.{} FROM workspaces w JOIN plans p ON p.id = w.plan_id WHERE w.id = ?1",
                    PLAN_COLS.replace(", ", ", p.")
                ),
                [&w],
                plan_from_row,
            )
            .optional()
            .map_err(err)
        })
        .await?)
}

/// Start of the current UTC month as stored timestamps compare (`YYYY-MM-01T00:00:00.000Z`).
pub fn month_start() -> String {
    format!(
        "{}-01T00:00:00.000Z",
        sf_core::now_rfc3339().chars().take(7).collect::<String>()
    )
}

/// Current usage of one limit key in a workspace.
pub fn used_sync(c: &rusqlite::Connection, ws: &str, key: &str) -> sf_core::Result<i64> {
    let ms = month_start();
    let now = sf_core::now_rfc3339();
    let one = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> sf_core::Result<i64> {
        c.query_row(sql, p, |r| r.get(0)).map_err(err)
    };
    match key {
        "sites" => one("SELECT count(*) FROM sites WHERE workspace_id=?1", &[&ws]),
        "posts_per_site" => one(
            "SELECT coalesce(max(n),0) FROM (SELECT count(p.id) AS n FROM sites s
               LEFT JOIN posts p ON p.site_id = s.id WHERE s.workspace_id=?1 GROUP BY s.id)",
            &[&ws],
        ),
        "posts_total" => one(
            "SELECT count(*) FROM posts WHERE site_id IN (SELECT id FROM sites WHERE workspace_id=?1)",
            &[&ws],
        ),
        "link_runs_per_month" => one(
            "SELECT count(*) FROM link_runs WHERE created_at >= ?2
               AND site_id IN (SELECT id FROM sites WHERE workspace_id=?1)",
            &[&ws, &ms],
        ),
        "ai_tokens_per_month" => sf_ai::accounting::workspace_tokens_month(c, ws)
            .map(|n| i64::try_from(n).unwrap_or(i64::MAX))
            .map_err(err),
        "members" => one(
            "SELECT (SELECT count(*) FROM workspace_members WHERE workspace_id=?1)
                  + (SELECT count(*) FROM workspace_invites WHERE workspace_id=?1
                       AND accepted_at IS NULL AND revoked_at IS NULL AND expires_at > ?2)",
            &[&ws, &now],
        ),
        "members_only" => one(
            "SELECT count(*) FROM workspace_members WHERE workspace_id=?1",
            &[&ws],
        ),
        "pending_invites" => one(
            "SELECT count(*) FROM workspace_invites WHERE workspace_id=?1
               AND accepted_at IS NULL AND revoked_at IS NULL AND expires_at > ?2",
            &[&ws, &now],
        ),
        "api_tokens" => one(
            "SELECT count(*) FROM api_tokens WHERE workspace_id=?1 AND revoked_at IS NULL
               AND (expires_at IS NULL OR expires_at > ?2)",
            &[&ws, &now],
        ),
        "schedules" => one(
            "SELECT count(*) FROM site_schedules WHERE site_id IN (SELECT id FROM sites WHERE workspace_id=?1)",
            &[&ws],
        ),
        _ => Ok(0),
    }
}

/// `{key: used}` for every usage figure of a workspace.
pub async fn usage(st: &AppState, ws: &str) -> ApiResult<Value> {
    let w = ws.to_string();
    Ok(st
        .db
        .read(move |c| {
            let g = |k: &str| used_sync(c, &w, k);
            Ok(json!({
                "sites": g("sites")?,
                "posts_total": g("posts_total")?,
                "posts_max_site": g("posts_per_site")?,
                "link_runs_month": g("link_runs_per_month")?,
                "ai_tokens_month": g("ai_tokens_per_month")?,
                "members": g("members_only")?,
                "pending_invites": g("pending_invites")?,
                "api_tokens": g("api_tokens")?,
                "schedules": g("schedules")?,
            }))
        })
        .await?)
}

/// The 402 error.
pub fn plan_limit(ws: &str, plan: &Plan, key: &str, limit: Value, used: i64) -> ApiError {
    let what = key.replace('_', " ");
    let msg = match &limit {
        Value::Bool(_) => format!("plan limit: the {} plan does not include {what}", plan.name),
        l => format!(
            "plan limit: the {} plan allows {l} {what} and {used} are used",
            plan.name
        ),
    };
    ApiError::new(axum::http::StatusCode::PAYMENT_REQUIRED, msg).with_extra(json!({
        "error": "plan limit", "key": key, "limit": limit, "used": used,
        "plan": plan.name, "plan_id": plan.id, "workspace_id": ws,
    }))
}

/// Refuses (402) when `used + adding > limit` for `key` in `ws` (no-op for unlimited/legacy).
pub async fn check_quota(st: &AppState, ws: Option<&str>, key: &str, adding: i64) -> ApiResult<()> {
    let Some(ws) = ws else { return Ok(()) };
    let Some(plan) = plan_of_ws(st, ws).await? else {
        return Ok(());
    };
    let Some(limit) = plan.limit(key) else {
        return Ok(());
    };
    let (w, k) = (ws.to_string(), key.to_string());
    let used = st.db.read(move |c| used_sync(c, &w, &k)).await?;
    if used.saturating_add(adding) > limit {
        return Err(plan_limit(ws, &plan, key, json!(limit), used));
    }
    Ok(())
}

/// Refuses (402) a boolean plan feature (`auto_linker`, `mcp`).
pub async fn check_feature(st: &AppState, ws: Option<&str>, key: &str) -> ApiResult<()> {
    let Some(ws) = ws else { return Ok(()) };
    let Some(plan) = plan_of_ws(st, ws).await? else {
        return Ok(());
    };
    if plan.feature(key) {
        Ok(())
    } else {
        Err(plan_limit(ws, &plan, key, json!(false), 0))
    }
}

/// Post cap for one site (crawl / sync / CSV import): 402 when the site already holds the cap.
pub async fn check_posts(st: &AppState, site_id: &str) -> ApiResult<()> {
    let sid = site_id.to_string();
    let row: Option<(Option<String>, i64)> = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT workspace_id, (SELECT count(*) FROM posts WHERE site_id = s.id) FROM sites s WHERE id=?1",
                [&sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(err)
        })
        .await?;
    let Some((Some(ws), n)) = row else {
        return Ok(());
    };
    let Some(plan) = plan_of_ws(st, &ws).await? else {
        return Ok(());
    };
    match plan.limit("posts_per_site") {
        Some(l) if n >= l => Err(plan_limit(&ws, &plan, "posts_per_site", json!(l), n)),
        _ => Ok(()),
    }
}

/// Workspace id of a site (`None` = legacy / unknown).
pub async fn site_ws(st: &AppState, site_id: &str) -> ApiResult<Option<String>> {
    let sid = site_id.to_string();
    Ok(st
        .db
        .read(move |c| {
            c.query_row("SELECT workspace_id FROM sites WHERE id=?1", [&sid], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()
            .map_err(err)
        })
        .await?
        .flatten())
}

// ---------------------------------------------------------------------------
// back-fill / maintenance
// ---------------------------------------------------------------------------

/// Boot pass (idempotent): Default exists and has an owner (the first admin); sites and tokens
/// without a workspace move to Default (admin / unowned) or to their owner's personal workspace;
/// soft-deleted workspaces older than 30 days are purged; this month's usage snapshot is written.
pub fn ensure_default_workspace_sync(t: &rusqlite::Transaction<'_>) -> sf_core::Result<()> {
    let x = |sql: &str| t.execute_batch(sql).map_err(err);
    x("INSERT OR IGNORE INTO workspaces(id, name, slug, plan_id, status, is_default)
         VALUES ('default', 'Default', 'default', 'unlimited', 'active', 1);
       UPDATE workspaces SET owner_user_id =
              (SELECT id FROM users WHERE role = 'admin' AND disabled_at IS NULL ORDER BY created_at, id LIMIT 1)
        WHERE id = 'default' AND owner_user_id IS NULL;
       INSERT OR IGNORE INTO workspace_members(workspace_id, user_id, role)
       SELECT 'default', owner_user_id, 'owner' FROM workspaces WHERE id = 'default' AND owner_user_id IS NOT NULL;
       INSERT OR IGNORE INTO workspaces(id, name, slug, plan_id, status, owner_user_id)
       SELECT DISTINCT 'ws-' || u.id,
              CASE WHEN trim(u.name) <> '' THEN trim(u.name) ELSE u.email END || ' workspace',
              'ws-' || substr(u.id, 1, 8) || '-' || substr(lower(hex(randomblob(2))), 1, 4),
              'unlimited', 'active', u.id
         FROM users u
        WHERE u.role <> 'admin'
          AND (EXISTS (SELECT 1 FROM sites s WHERE s.user_id = u.id AND s.workspace_id IS NULL)
            OR EXISTS (SELECT 1 FROM api_tokens k WHERE k.user_id = u.id AND k.workspace_id IS NULL));
       INSERT OR IGNORE INTO workspace_members(workspace_id, user_id, role)
       SELECT w.id, w.owner_user_id, 'owner' FROM workspaces w
        WHERE w.id LIKE 'ws-%' AND w.owner_user_id IS NOT NULL
          AND NOT EXISTS (SELECT 1 FROM workspace_members m WHERE m.workspace_id = w.id);
       UPDATE sites SET workspace_id = CASE
           WHEN user_id IS NOT NULL AND EXISTS (SELECT 1 FROM users u WHERE u.id = sites.user_id AND u.role <> 'admin')
             THEN 'ws-' || user_id ELSE 'default' END
        WHERE workspace_id IS NULL;
       UPDATE api_tokens SET workspace_id = CASE
           WHEN EXISTS (SELECT 1 FROM users u WHERE u.id = api_tokens.user_id AND u.role <> 'admin')
             THEN 'ws-' || user_id ELSE 'default' END
        WHERE workspace_id IS NULL;",
    )?;
    // purge soft-deleted workspaces after PURGE_AFTER_DAYS
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(PURGE_AFTER_DAYS))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let ids: Vec<String> = {
        let mut q = t
            .prepare("SELECT id FROM workspaces WHERE deleted_at IS NOT NULL AND deleted_at < ?1 AND is_default = 0")
            .map_err(err)?;
        q.query_map([&cutoff], |r| r.get(0))
            .map_err(err)?
            .collect::<rusqlite::Result<Vec<String>>>()
            .map_err(err)?
    };
    for id in ids {
        t.execute("DELETE FROM sites WHERE workspace_id=?1", [&id])
            .map_err(err)?;
        t.execute("DELETE FROM workspaces WHERE id=?1", [&id])
            .map_err(err)?;
    }
    snapshot_usage_sync(t)?;
    Ok(())
}

/// Writes this month's `usage_monthly` row for every live workspace.
pub fn snapshot_usage_sync(c: &rusqlite::Connection) -> sf_core::Result<()> {
    let month: String = sf_core::now_rfc3339().chars().take(7).collect();
    let ids: Vec<String> = {
        let mut q = c
            .prepare("SELECT id FROM workspaces WHERE deleted_at IS NULL")
            .map_err(err)?;
        q.query_map([], |r| r.get(0))
            .map_err(err)?
            .collect::<rusqlite::Result<Vec<String>>>()
            .map_err(err)?
    };
    for id in ids {
        let sites = used_sync(c, &id, "sites")?;
        let posts = used_sync(c, &id, "posts_total")?;
        let runs = used_sync(c, &id, "link_runs_per_month")?;
        let ai = used_sync(c, &id, "ai_tokens_per_month")?;
        let members = used_sync(c, &id, "members_only")?;
        c.execute(
            "INSERT INTO usage_monthly(workspace_id, month, sites, posts, link_runs, ai_tokens, members, updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(workspace_id, month) DO UPDATE SET sites=excluded.sites, posts=excluded.posts,
               link_runs=excluded.link_runs, ai_tokens=excluded.ai_tokens, members=excluded.members,
               updated_at=excluded.updated_at",
            params![id, month, sites, posts, runs, ai, members, sf_core::now_rfc3339()],
        )
        .map_err(err)?;
    }
    Ok(())
}

/// Async wrapper of [`ensure_default_workspace_sync`] (call at boot and after the first user).
pub async fn ensure_default_workspace(db: &sf_store::Db) -> sf_core::Result<()> {
    db.write(ensure_default_workspace_sync).await
}

/// URL-safe slug from a name (`a-z0-9-`, 3..48 chars; random suffix when too short).
pub fn slugify(name: &str) -> String {
    let mut s = String::new();
    let mut dash = false;
    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            s.push(ch);
            dash = false;
        } else if !dash && !s.is_empty() {
            s.push('-');
            dash = true;
        }
        if s.len() >= 40 {
            break;
        }
    }
    let s = s.trim_matches('-').to_string();
    if s.len() < 3 {
        format!("ws-{}", sf_core::crypto::random_hex(3))
    } else {
        s
    }
}

pub fn valid_slug(s: &str) -> bool {
    (3..=48).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// Creates a workspace (+ owner membership) in one transaction; the slug gets a numeric suffix
/// when taken.
pub fn create_ws_sync(
    t: &rusqlite::Transaction<'_>,
    name: &str,
    slug: Option<&str>,
    plan_id: &str,
    owner: &str,
) -> sf_core::Result<Ws> {
    let id = sf_core::new_id();
    let base = slug.map(str::to_string).unwrap_or_else(|| slugify(name));
    let mut chosen = base.clone();
    for i in 2..1000 {
        let taken: bool = t
            .query_row(
                "SELECT count(*) > 0 FROM workspaces WHERE slug=?1",
                [&chosen],
                |r| r.get(0),
            )
            .map_err(err)?;
        if !taken {
            break;
        }
        if slug.is_some() {
            return Err(sf_core::Error::Conflict("slug already taken".into()));
        }
        chosen = format!("{base}-{i}");
    }
    t.execute(
        "INSERT INTO workspaces(id, name, slug, plan_id, status, owner_user_id) VALUES(?1,?2,?3,?4,'active',?5)",
        params![id, name, chosen, plan_id, owner],
    )
    .map_err(err)?;
    t.execute(
        "INSERT INTO workspace_members(workspace_id, user_id, role) VALUES(?1,?2,'owner')",
        params![id, owner],
    )
    .map_err(err)?;
    t.query_row(
        &format!("SELECT {WS_COLS} FROM workspaces WHERE id=?1"),
        [&id],
        Ws::from_row,
    )
    .map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_order_and_slugs() {
        assert!(
            Role::Viewer < Role::Member && Role::Member < Role::Admin && Role::Admin < Role::Owner
        );
        assert_eq!(Role::parse("admin"), Some(Role::Admin));
        assert_eq!(Role::parse("root"), None);
        assert_eq!(slugify("My Agency, Inc."), "my-agency-inc");
        assert!(slugify("!").starts_with("ws-"));
        assert!(valid_slug("acme-1"));
        assert!(!valid_slug("Acme"));
        assert!(!valid_slug("-a-"));
        assert_eq!(lit("a'b"), "'a''b'");
        assert_eq!(ReqCtx::default().default_role(), Role::Member);
    }
}
