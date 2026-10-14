//! MCP management (contract v2.1 §3): tool policy, per-credential effective policy, usage stats
//! and the `/mcp` HTTP on/off gate.
//!
//! * The policy lives in `kv` key `mcp_policy` (JSON) and is cached in `AppState::mcp_policy`.
//!   Missing entries fall back to the catalogue defaults the binary registers in
//!   [`crate::state::McpTools`] (`min_scope`, `default_enabled`).
//! * Usage: sf-mcp posts `POST /api/v2/mcp/usage {tool, ok, duration_ms, error}` after every
//!   tool call (stdio and HTTP alike, so both transports are counted the same way). Calls are
//!   aggregated per tool and hour in `kv` (`mcp_usage:<YYYY-MM-DDTHH>:<tool>`), pruned after 48 h,
//!   so stats never grow without bound. The unified event log (§9) records each call too.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sf_store::err;

use crate::auth::{Admin, Principal, User, audit};
use crate::v1::common::JsonBody;
use crate::{ApiError, ApiResult, AppState};

const KV_POLICY: &str = "mcp_policy";
const KV_USAGE_PREFIX: &str = "mcp_usage:";
const SCOPES: &[&str] = &["read", "write", "admin"];
const MAX_TOOL_NAME: usize = 100;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_scope: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TokenRule {
    /// `None` = every tool the token's scope allows.
    #[serde(default)]
    pub allow: Option<Vec<String>>,
}

/// Stored policy (only overrides; defaults come from the catalogue).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpPolicy {
    #[serde(default = "yes")]
    pub http_enabled: bool,
    #[serde(default)]
    pub tools: BTreeMap<String, ToolRule>,
    #[serde(default)]
    pub per_token: BTreeMap<String, TokenRule>,
}

fn yes() -> bool {
    true
}

impl Default for McpPolicy {
    fn default() -> Self {
        Self {
            http_enabled: true,
            tools: BTreeMap::new(),
            per_token: BTreeMap::new(),
        }
    }
}

/// One catalogue tool with its defaults.
#[derive(Debug, Clone)]
pub struct CatalogueTool {
    pub name: String,
    pub min_scope: String,
    pub default_enabled: bool,
    pub info: Value,
}

/// Catalogue registered by the binary (empty in unit tests without one).
pub fn catalogue(st: &AppState) -> Vec<CatalogueTool> {
    st.services
        .get::<crate::state::McpTools>()
        .map(|t| t.0.clone())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| {
            let name = v.get("name")?.as_str()?.to_string();
            let read_only = v.get("read_only").and_then(Value::as_bool).unwrap_or(false);
            let min_scope = v
                .get("min_scope")
                .and_then(Value::as_str)
                .filter(|s| SCOPES.contains(s))
                .map(str::to_string)
                .unwrap_or_else(|| if read_only { "read" } else { "write" }.to_string());
            let default_enabled = v
                .get("default_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            Some(CatalogueTool {
                name,
                min_scope,
                default_enabled,
                info: v,
            })
        })
        .collect()
}

fn scope_rank(s: &str) -> u8 {
    match s {
        "admin" => 3,
        "write" => 2,
        "read" => 1,
        _ => 0,
    }
}

/// Highest MCP scope a caller has: sessions act as their role; API tokens are capped by role.
pub fn caller_scope(u: &User) -> &'static str {
    let role_max = if u.is_admin() { "admin" } else { "write" };
    if u.token.is_none() {
        return role_max;
    }
    let best = u
        .scopes
        .iter()
        .map(|s| match s.as_str() {
            "*" | "admin" => role_max,
            "write" => "write",
            "read" => "read",
            _ => "",
        })
        .max_by_key(|s| scope_rank(s))
        .unwrap_or("");
    if scope_rank(best) > scope_rank(role_max) {
        role_max
    } else {
        best
    }
}

/// Cached policy (kv `mcp_policy`, defaults when unset or unreadable).
pub async fn load_policy(st: &AppState) -> ApiResult<McpPolicy> {
    if let Some(p) = st.mcp_policy.read().clone() {
        return Ok(p);
    }
    let raw: Option<String> = st
        .db
        .read(|c| {
            c.query_row("SELECT value FROM kv WHERE key=?1", [KV_POLICY], |r| {
                r.get(0)
            })
            .optional()
            .map_err(err)
        })
        .await?;
    let p = raw
        .and_then(|r| serde_json::from_str::<McpPolicy>(&r).ok())
        .unwrap_or_default();
    *st.mcp_policy.write() = Some(p.clone());
    Ok(p)
}

async fn store_policy(st: &AppState, p: &McpPolicy) -> ApiResult<()> {
    let text = serde_json::to_string(p).map_err(|e| ApiError::internal(e.to_string()))?;
    st.db
        .write(move |t| {
            t.execute(
                "INSERT INTO kv(key, value, updated_at) VALUES(?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
                params![KV_POLICY, text],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    *st.mcp_policy.write() = Some(p.clone());
    Ok(())
}

/// Turns MCP-over-HTTP on/off (also `server.mcp_http_enabled` in settings).
pub async fn set_http_enabled(st: &AppState, on: bool) -> ApiResult<()> {
    let mut p = load_policy(st).await?;
    if p.http_enabled != on {
        p.http_enabled = on;
        store_policy(st, &p).await?;
    }
    Ok(())
}

/// Per-token allow list (`None` = no restriction), for the token rows (`tool_policy`).
pub async fn token_rule(st: &AppState, token_id: &str) -> Option<TokenRule> {
    load_policy(st).await.ok()?.per_token.get(token_id).cloned()
}

/// Sets or clears one token's allow list (PATCH /tokens/{id} `tool_policy`).
pub async fn set_token_rule(
    st: &AppState,
    token_id: &str,
    rule: Option<TokenRule>,
) -> ApiResult<()> {
    let mut p = load_policy(st).await?;
    match rule {
        Some(r) if r.allow.is_some() => {
            if let Some(a) = &r.allow {
                check_tool_names(st, a)?;
            }
            p.per_token.insert(token_id.to_string(), r);
        }
        _ => {
            p.per_token.remove(token_id);
        }
    }
    store_policy(st, &p).await
}

fn valid_tool_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= MAX_TOOL_NAME
        && n.starts_with("siloforge_")
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

pub(crate) fn check_tool_names(st: &AppState, names: &[String]) -> ApiResult<()> {
    let cat = catalogue(st);
    if names.len() > 500 {
        return Err(ApiError::bad("too many tool names"));
    }
    for n in names {
        let known = if cat.is_empty() {
            valid_tool_name(n)
        } else {
            cat.iter().any(|t| &t.name == n)
        };
        if !known {
            return Err(ApiError::bad(format!(
                "unknown MCP tool `{}`",
                n.chars().take(MAX_TOOL_NAME).collect::<String>()
            )));
        }
    }
    Ok(())
}

/// Full view: every catalogue tool with its effective `enabled`/`min_scope`.
fn full_view(cat: &[CatalogueTool], p: &McpPolicy) -> Value {
    let mut tools = serde_json::Map::new();
    for t in cat {
        let rule = p.tools.get(&t.name);
        tools.insert(
            t.name.clone(),
            json!({
                "enabled": rule.and_then(|r| r.enabled).unwrap_or(t.default_enabled),
                "min_scope": rule.and_then(|r| r.min_scope.clone()).unwrap_or_else(|| t.min_scope.clone()),
                "default_enabled": t.default_enabled,
                "default_min_scope": t.min_scope,
            }),
        );
    }
    // overrides for tools the catalogue does not know (kept, reported)
    for (n, r) in &p.tools {
        if !tools.contains_key(n) {
            tools.insert(
                n.clone(),
                json!({"enabled": r.enabled.unwrap_or(true), "min_scope": r.min_scope.clone().unwrap_or_else(|| "write".into()), "unknown": true}),
            );
        }
    }
    json!({
        "http_enabled": p.http_enabled,
        "tools": tools,
        "per_token": p.per_token,
    })
}

/// `GET /mcp/policy` (admin).
pub async fn get_policy(State(st): State<AppState>, _a: Admin) -> ApiResult<Json<Value>> {
    let p = load_policy(&st).await?;
    Ok(Json(full_view(&catalogue(&st), &p)))
}

#[derive(Deserialize)]
pub struct PolicyBody {
    #[serde(default)]
    http_enabled: Option<bool>,
    #[serde(default)]
    tools: Option<BTreeMap<String, ToolRule>>,
    #[serde(default)]
    per_token: Option<BTreeMap<String, TokenRule>>,
}

/// `PUT /mcp/policy` (admin): replaces the given parts (`tools` / `per_token` are full maps).
pub async fn put_policy(
    State(st): State<AppState>,
    Admin(u): Admin,
    JsonBody(b): JsonBody<PolicyBody>,
) -> ApiResult<Json<Value>> {
    let mut p = load_policy(&st).await?;
    let cat = catalogue(&st);
    if let Some(h) = b.http_enabled {
        p.http_enabled = h;
    }
    if let Some(tools) = b.tools {
        let names: Vec<String> = tools.keys().cloned().collect();
        check_tool_names(&st, &names)?;
        let mut clean = BTreeMap::new();
        for (n, r) in tools {
            if let Some(s) = &r.min_scope
                && !SCOPES.contains(&s.as_str())
            {
                return Err(ApiError::bad(format!(
                    "{n}: min_scope must be read, write or admin"
                )));
            }
            // store only what differs from the catalogue default
            let def = cat.iter().find(|t| t.name == n);
            let rule = ToolRule {
                enabled: r
                    .enabled
                    .filter(|e| def.is_none_or(|d| *e != d.default_enabled)),
                min_scope: r
                    .min_scope
                    .filter(|s| def.is_none_or(|d| *s != d.min_scope)),
            };
            if rule != ToolRule::default() {
                clean.insert(n, rule);
            }
        }
        p.tools = clean;
    }
    if let Some(per) = b.per_token {
        if per.len() > 1000 {
            return Err(ApiError::bad("too many per_token entries"));
        }
        let ids: Vec<String> = per.keys().cloned().collect();
        let known: Vec<String> = st
            .db
            .read(move |c| {
                let mut out = Vec::new();
                let mut stmt = c
                    .prepare("SELECT id FROM api_tokens WHERE id=?1 AND revoked_at IS NULL")
                    .map_err(err)?;
                for id in &ids {
                    if stmt.exists([id]).map_err(err)? {
                        out.push(id.clone());
                    }
                }
                Ok(out)
            })
            .await?;
        let mut clean = BTreeMap::new();
        for (id, r) in per {
            if !known.contains(&id) {
                return Err(ApiError::bad(format!(
                    "per_token: unknown or revoked token id `{}`",
                    id.chars().take(64).collect::<String>()
                )));
            }
            if let Some(a) = &r.allow {
                check_tool_names(&st, a)?;
                clean.insert(id, r);
            }
        }
        p.per_token = clean;
    }
    store_policy(&st, &p).await?;
    audit(
        &st,
        &u.actor(),
        u.via(),
        "mcp.policy.update",
        None,
        json!({"http_enabled": p.http_enabled, "tool_overrides": p.tools.len(), "per_token": p.per_token.len()}),
    )
    .await;
    Ok(Json(full_view(&cat, &p)))
}

/// Allowed tool names for a caller.
pub fn allowed_for(
    cat: &[CatalogueTool],
    p: &McpPolicy,
    scope: &str,
    token_id: Option<&str>,
) -> Vec<String> {
    let allow = token_id
        .and_then(|id| p.per_token.get(id))
        .and_then(|r| r.allow.clone());
    cat.iter()
        .filter(|t| {
            let rule = p.tools.get(&t.name);
            let enabled = rule.and_then(|r| r.enabled).unwrap_or(t.default_enabled);
            let min = rule
                .and_then(|r| r.min_scope.clone())
                .unwrap_or_else(|| t.min_scope.clone());
            enabled
                && scope_rank(scope) >= scope_rank(&min)
                && allow.as_ref().is_none_or(|a| a.contains(&t.name))
        })
        .map(|t| t.name.clone())
        .collect()
}

/// `GET /mcp/policy/effective` (any user or API token): the policy for the calling credential.
pub async fn effective(
    State(st): State<AppState>,
    u: User,
    headers: axum::http::HeaderMap,
) -> ApiResult<Json<Value>> {
    let p = load_policy(&st).await?;
    let cat = catalogue(&st);
    let token_id = if u.token.is_some() {
        super::tokens::caller_token(&st, &headers)
            .await
            .ok()
            .map(|(id, _, _)| id)
    } else {
        None
    };
    let scope = caller_scope(&u);
    let allowed = allowed_for(&cat, &p, scope, token_id.as_deref());
    let view = full_view(&cat, &p);
    Ok(Json(json!({
        "http_enabled": p.http_enabled,
        "scope": scope,
        "allowed": allowed,
        "tools": view.get("tools").cloned().unwrap_or(Value::Null),
        "token_id": token_id,
        "restricted": token_id.as_ref().is_some_and(|id| p.per_token.contains_key(id)),
    })))
}

#[derive(Deserialize)]
pub struct UsageBody {
    tool: String,
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    duration_ms: Option<f64>,
    #[serde(default)]
    error: Option<String>,
}

/// Usage reports come from MCP clients that may hold read-only tokens, so this route accepts
/// any authenticated user/token for POST (no method-scope check).
pub struct AnyUser;

impl axum::extract::FromRequestParts<AppState> for AnyUser {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        st: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let cred = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.split_once(' '))
            .filter(|(s, _)| s.eq_ignore_ascii_case("bearer"))
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty() && v.len() <= 4096)
            .ok_or_else(|| ApiError::unauthorized("missing bearer token"))?;
        match crate::auth::resolve(st, &cred).await? {
            Principal::User { .. } => Ok(AnyUser),
            Principal::Site { .. } => Err(ApiError::unauthorized("user token required")),
        }
    }
}

fn hour_bucket(ts: &str) -> String {
    ts.get(..13).unwrap_or("").to_string()
}

/// `POST /mcp/usage` -> 204.
pub async fn usage(
    State(st): State<AppState>,
    _u: AnyUser,
    JsonBody(b): JsonBody<UsageBody>,
) -> ApiResult<StatusCode> {
    let tool = b.tool.trim().to_string();
    if !valid_tool_name(&tool) {
        return Err(ApiError::bad("tool must be a siloforge_* tool name"));
    }
    let ok = b.ok.unwrap_or(true);
    let ms = b
        .duration_ms
        .filter(|d| d.is_finite() && *d >= 0.0)
        .map_or(0, |d| d.min(1e9) as i64);
    let err_msg: Option<String> = b.error.map(|e| e.chars().take(300).collect());
    let now = sf_core::now_rfc3339();
    let key = format!("{KV_USAGE_PREFIX}{}:{tool}", hour_bucket(&now));
    let cutoff = format!(
        "{KV_USAGE_PREFIX}{}",
        hour_bucket(
            &(chrono::Utc::now() - chrono::Duration::hours(48))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        )
    );
    st.db
        .write(move |t| {
            let cur: Option<String> = t
                .query_row("SELECT value FROM kv WHERE key=?1", [&key], |r| r.get(0))
                .optional()
                .map_err(err)?;
            let mut v: Value = cur
                .and_then(|c| serde_json::from_str(&c).ok())
                .unwrap_or_else(|| json!({"calls": 0, "errors": 0, "ms_total": 0}));
            let get = |v: &Value, k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
            let next = json!({
                "calls": get(&v, "calls").saturating_add(1),
                "errors": get(&v, "errors").saturating_add(i64::from(!ok)),
                "ms_total": get(&v, "ms_total").saturating_add(ms),
                "last_called_at": now,
                "last_error": if ok { v.get("last_error").cloned().unwrap_or(Value::Null) } else { json!(err_msg) },
            });
            v = next;
            t.execute(
                "INSERT INTO kv(key, value, updated_at) VALUES(?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
                params![key, v.to_string(), now],
            )
            .map_err(err)?;
            // keep only the last 48 hourly buckets
            t.execute(
                "DELETE FROM kv WHERE key >= ?1 AND key < ?2",
                params![KV_USAGE_PREFIX, cutoff],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /mcp/stats` (admin): per tool `{name, calls_24h, errors_24h, last_called_at, avg_ms}`.
pub async fn stats(State(st): State<AppState>, _a: Admin) -> ApiResult<Json<Value>> {
    let from = format!(
        "{KV_USAGE_PREFIX}{}",
        hour_bucket(
            &(chrono::Utc::now() - chrono::Duration::hours(23))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        )
    );
    let rows: Vec<(String, String)> = st
        .db
        .read(move |c| {
            let mut stmt = c
                .prepare("SELECT key, value FROM kv WHERE key >= ?1 AND key < ?2 ORDER BY key")
                .map_err(err)?;
            let end = format!("{KV_USAGE_PREFIX}~");
            let rows = stmt
                .query_map(params![from, end], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?;
    let mut agg: BTreeMap<String, (i64, i64, i64, Option<String>)> = BTreeMap::new();
    for (k, v) in rows {
        // key = mcp_usage:<YYYY-MM-DDTHH>:<tool>
        let Some(tool) = k
            .strip_prefix(KV_USAGE_PREFIX)
            .and_then(|r| r.split_once(':'))
            .map(|(_, t)| t.to_string())
        else {
            continue;
        };
        let v: Value = serde_json::from_str(&v).unwrap_or(Value::Null);
        let e = agg.entry(tool).or_insert((0, 0, 0, None));
        e.0 =
            e.0.saturating_add(v.get("calls").and_then(Value::as_i64).unwrap_or(0));
        e.1 =
            e.1.saturating_add(v.get("errors").and_then(Value::as_i64).unwrap_or(0));
        e.2 =
            e.2.saturating_add(v.get("ms_total").and_then(Value::as_i64).unwrap_or(0));
        if let Some(l) = v.get("last_called_at").and_then(Value::as_str)
            && e.3.as_deref().is_none_or(|cur| l > cur)
        {
            e.3 = Some(l.to_string());
        }
    }
    let cat = catalogue(&st);
    let category = |n: &str| {
        cat.iter()
            .find(|t| t.name == n)
            .and_then(|t| t.info.get("category").cloned())
            .unwrap_or(Value::Null)
    };
    let mut names: Vec<String> = cat.iter().map(|t| t.name.clone()).collect();
    for k in agg.keys() {
        if !names.contains(k) {
            names.push(k.clone());
        }
    }
    let tools: Vec<Value> = names
        .into_iter()
        .map(|n| {
            let (calls, errors, ms, last) = agg.get(&n).cloned().unwrap_or((0, 0, 0, None));
            json!({
                "category": category(&n),
                "name": n,
                "calls_24h": calls,
                "errors_24h": errors,
                "avg_ms": if calls > 0 { ms / calls } else { 0 },
                "last_called_at": last,
            })
        })
        .collect();
    Ok(Json(json!({ "tools": tools, "window_hours": 24 })))
}

/// Middleware for the `/mcp` HTTP transport: 404 while `http_enabled` is false.
pub async fn http_gate(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if path == "/mcp" || path.starts_with("/mcp/") {
        let on = load_policy(&st).await.map_or(true, |p| p.http_enabled);
        if !on {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "MCP over HTTP is disabled (Settings -> API & MCP)"})),
            )
                .into_response();
        }
    }
    next.run(req).await
}
