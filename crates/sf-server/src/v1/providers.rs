//! Settings > Models (admin only): `/providers*`, `/models*`, `/model-chains*`, `/model-chain-rungs/*`.
//!
//! Provider API keys are stored with `encrypt_secret` and never returned: payloads carry
//! `api_key_masked` (`'*'×min(12,len−4) + last4`, `****` if undecryptable) and `has_api_key`.
//! Probes go through `sf_ai` (registered in `AppState::services`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sf_store::{err, row_to_json};

use super::common::{
    JsonBody, Params, audit_user, is_http_url, is_uuid, query_json, query_one_json,
};
use crate::auth::Admin;
use crate::{ApiError, ApiResult, AppState};

const KINDS: &[&str] = &[
    "openai-compatible",
    "anthropic",
    "openrouter",
    "ollama",
    "gemini",
    "custom",
];
const ROLES: &[&str] = &["chat", "embed"];
const INDEX_DIM: i64 = sf_core::EMBED_DIM as i64;

const PROVIDER_COLS: &str = "id, name, label, kind, base_url, api_key_enc, headers, enabled, \
     last_probe_at, last_probe_ok, last_probe_error, last_probe_ms, created_at, updated_at";
const MODEL_COLS: &str = "id, provider_id, model_id, label, role, context_tokens, dim, enabled, price_in, \
     price_out, notes, last_probe_at, last_probe_ok, last_probe_error, last_probe_ms, created_at";
const CHAIN_COLS: &str = "id, name, role, site_id, active, notes, created_at, updated_at";
const RUNG_COLS: &str = "id, chain_id, model_id, position, timeout_ms, max_retries, enabled";
const PROBE_BOOLS: &[&str] = &["enabled", "last_probe_ok"];

// ---------------------------------------------------------------------------
// probe backend
// ---------------------------------------------------------------------------

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Probe backend (implemented for `sf_ai::Ai` below). Both methods return the exact v1 `ProbeResult`
/// JSON: `{ok, latency_ms, status_code, error, dim?, sample?}`.
pub trait ProbeService: Send + Sync {
    fn probe_provider<'a>(&'a self, provider_id: &'a str) -> BoxFut<'a, sf_core::Result<Value>>;
    fn probe_model<'a>(&'a self, model_row_id: &'a str) -> BoxFut<'a, sf_core::Result<Value>>;
}

/// Optional alternative backend (tests). Normally the binary registers `Arc<sf_ai::Ai>` in
/// `AppState::services`, which is used first.
pub struct Prober(pub Arc<dyn ProbeService>);

/// The probe backend: the registered `sf_ai::Ai`, else a registered `Prober` (tests / alternatives).
fn prober(st: &AppState) -> ApiResult<Arc<dyn ProbeService>> {
    if let Some(ai) = st.services.get::<sf_ai::Ai>() {
        return Ok(ai);
    }
    Ok(st.service::<Prober>()?.0.clone())
}

impl ProbeService for sf_ai::Ai {
    fn probe_provider<'a>(&'a self, provider_id: &'a str) -> BoxFut<'a, sf_core::Result<Value>> {
        Box::pin(async move {
            sf_ai::Ai::probe_provider(self, provider_id)
                .await
                .map_err(Into::into)
        })
    }
    fn probe_model<'a>(&'a self, model_row_id: &'a str) -> BoxFut<'a, sf_core::Result<Value>> {
        Box::pin(async move {
            sf_ai::Ai::probe_model(self, model_row_id)
                .await
                .map_err(Into::into)
        })
    }
}

// ---------------------------------------------------------------------------
// mappers
// ---------------------------------------------------------------------------

/// v1 `maskSecret`.
pub fn mask_secret(plain: &str) -> String {
    let n = plain.chars().count();
    if n == 0 {
        return String::new();
    }
    if n <= 4 {
        return "****".into();
    }
    let tail: String = plain.chars().skip(n - 4).collect();
    format!("{}{}", "*".repeat((n - 4).min(12)), tail)
}

fn provider_from_row(r: &rusqlite::Row<'_>, key: &[u8; 32]) -> Value {
    let mut v = row_to_json(r, &["headers"], PROBE_BOOLS);
    if let Some(o) = v.as_object_mut() {
        let enc = o
            .remove("api_key_enc")
            .and_then(|e| e.as_str().map(str::to_string))
            .unwrap_or_default();
        let has = !enc.is_empty();
        let masked = if has {
            sf_core::crypto::decrypt_secret(key, &enc)
                .map(|p| mask_secret(&p))
                .unwrap_or_else(|_| "****".into())
        } else {
            String::new()
        };
        o.insert("api_key_masked".into(), Value::String(masked));
        o.insert("has_api_key".into(), Value::Bool(has));
        if !o.get("headers").is_some_and(Value::is_object) {
            o.insert("headers".into(), json!({}));
        }
    }
    v
}

fn load_providers(
    c: &rusqlite::Connection,
    key: &[u8; 32],
    id: Option<&str>,
) -> sf_core::Result<Vec<Value>> {
    let (sql, p): (String, Vec<String>) = match id {
        Some(id) => (
            format!("SELECT {PROVIDER_COLS} FROM model_providers WHERE id=?1"),
            vec![id.to_string()],
        ),
        None => (
            format!("SELECT {PROVIDER_COLS} FROM model_providers ORDER BY name"),
            vec![],
        ),
    };
    let mut stmt = c.prepare(&sql).map_err(err)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(p.iter()), |r| {
            Ok(provider_from_row(r, key))
        })
        .map_err(err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(err)?;
    Ok(rows)
}

fn one_provider(
    c: &rusqlite::Connection,
    key: &[u8; 32],
    id: &str,
) -> sf_core::Result<Option<Value>> {
    Ok(load_providers(c, key, Some(id))?.into_iter().next())
}

fn not_found(what: &str) -> ApiError {
    ApiError::not_found(format!("{what} not found"))
}

fn check_headers(h: &Option<Value>) -> ApiResult<Option<String>> {
    match h {
        None => Ok(None),
        Some(Value::Object(o))
            if o.len() <= 50
                && o.iter().all(|(k, v)| {
                    k.len() <= 256 && v.as_str().is_some_and(|s| s.len() <= 4096)
                }) =>
        {
            Ok(Some(Value::Object(o.clone()).to_string()))
        }
        Some(_) => Err(ApiError::bad("headers must be an object of strings")),
    }
}

fn normalize_base_url(u: &str) -> ApiResult<String> {
    let b = u.trim().trim_end_matches('/').to_string();
    if b.is_empty() || b.len() > 2048 || !is_http_url(&b) {
        return Err(ApiError::bad("base_url must be an http(s) URL"));
    }
    Ok(b)
}

fn check_len(field: &str, v: &str, min: usize, max: usize) -> ApiResult<()> {
    let n = v.chars().count();
    if n < min || n > max {
        return Err(ApiError::bad(format!(
            "{field} must be {min}-{max} characters"
        )));
    }
    Ok(())
}

fn check_kind(k: &str) -> ApiResult<()> {
    if KINDS.contains(&k) {
        Ok(())
    } else {
        Err(ApiError::bad(format!(
            "kind must be one of {}",
            KINDS.join(", ")
        )))
    }
}

fn check_role(r: &str) -> ApiResult<()> {
    if ROLES.contains(&r) {
        Ok(())
    } else {
        Err(ApiError::bad("role must be one of chat, embed"))
    }
}

fn encrypt(st: &AppState, key: &str) -> ApiResult<String> {
    if key.len() > 8192 {
        return Err(ApiError::bad("api_key is too long"));
    }
    Ok(sf_core::crypto::encrypt_secret(&st.secret_key, key)?)
}

// ---------------------------------------------------------------------------
// providers
// ---------------------------------------------------------------------------

pub async fn list_providers(State(st): State<AppState>, _a: Admin) -> ApiResult<Json<Value>> {
    let key = st.secret_key.clone();
    let rows = st.db.read(move |c| load_providers(c, &key, None)).await?;
    Ok(Json(Value::Array(rows)))
}

pub async fn providers_health(State(st): State<AppState>, _a: Admin) -> ApiResult<Json<Value>> {
    let key = st.secret_key.clone();
    let v = st
        .db
        .read(move |c| {
            let providers = load_providers(c, &key, None)?;
            let models = query_json(
                c,
                &format!("SELECT {MODEL_COLS} FROM models ORDER BY role, model_id, id"),
                &[],
                &[],
                PROBE_BOOLS,
            )?;
            let mut by: std::collections::HashMap<String, Vec<Value>> = Default::default();
            for m in models {
                let pid = m["provider_id"].as_str().unwrap_or("").to_string();
                by.entry(pid).or_default().push(m);
            }
            let out: Vec<Value> = providers
                .into_iter()
                .map(|mut p| {
                    let id = p["id"].as_str().unwrap_or("").to_string();
                    if let Some(o) = p.as_object_mut() {
                        o.insert(
                            "models".into(),
                            Value::Array(by.remove(&id).unwrap_or_default()),
                        );
                    }
                    p
                })
                .collect();
            Ok(json!({ "providers": out, "index_dim": INDEX_DIM }))
        })
        .await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct ProviderBody {
    name: String,
    #[serde(default)]
    label: Option<String>,
    kind: String,
    base_url: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    headers: Option<Value>,
    #[serde(default)]
    enabled: Option<bool>,
}

pub async fn create_provider(
    State(st): State<AppState>,
    Admin(a): Admin,
    JsonBody(b): JsonBody<ProviderBody>,
) -> ApiResult<Response> {
    let name = b.name.trim().to_string();
    check_len("name", &name, 1, 64)?;
    if let Some(l) = &b.label {
        check_len("label", l, 0, 128)?;
    }
    check_kind(&b.kind)?;
    let base = normalize_base_url(&b.base_url)?;
    let headers = check_headers(&b.headers)?.unwrap_or_else(|| "{}".into());
    let enc = encrypt(&st, b.api_key.as_deref().unwrap_or(""))?;
    let id = sf_core::new_id();
    let label = b.label.unwrap_or_else(|| name.clone());
    let (id2, key) = (id.clone(), st.secret_key.clone());
    let enabled = b.enabled.unwrap_or(true);
    let kind = b.kind.clone();
    let v = st
        .db
        .write(move |t| {
            t.execute(
                "INSERT INTO model_providers(id, name, label, kind, base_url, api_key_enc, headers, enabled)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![id2, name, label, kind, base, enc, headers, enabled],
            )
            .map_err(err)?;
            one_provider(t, &key, &id2)
        })
        .await
        .map_err(|e| super::common::conflict_as(e, "a provider with that name already exists"))?
        .ok_or_else(|| not_found("provider"))?;
    audit_user(
        &st,
        &a,
        "provider.create",
        Some(&id),
        json!({ "kind": b.kind }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(v)).into_response())
}

pub async fn get_provider(
    State(st): State<AppState>,
    _a: Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&id) {
        return Err(not_found("provider"));
    }
    let key = st.secret_key.clone();
    st.db
        .read(move |c| one_provider(c, &key, &id))
        .await?
        .map(Json)
        .ok_or_else(|| not_found("provider"))
}

#[derive(Deserialize)]
pub struct ProviderPatch {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    /// Absent = keep the stored key; `""` = clear it.
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    headers: Option<Value>,
    #[serde(default)]
    enabled: Option<bool>,
}

pub async fn patch_provider(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<ProviderPatch>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&id) {
        return Err(not_found("provider"));
    }
    let name = b.name.as_deref().map(str::trim).map(str::to_string);
    if let Some(n) = &name {
        check_len("name", n, 1, 64)?;
    }
    if let Some(l) = &b.label {
        check_len("label", l, 0, 128)?;
    }
    if let Some(k) = &b.kind {
        check_kind(k)?;
    }
    let base = b.base_url.as_deref().map(normalize_base_url).transpose()?;
    let headers = check_headers(&b.headers)?;
    let enc = b.api_key.as_deref().map(|k| encrypt(&st, k)).transpose()?;
    let (id2, key) = (id.clone(), st.secret_key.clone());
    let key_changed = enc.is_some();
    let (label, kind, enabled) = (b.label, b.kind, b.enabled);
    let v = st
        .db
        .write(move |t| {
            let n = t
                .execute(
                    "UPDATE model_providers SET name=coalesce(?2,name), label=coalesce(?3,label),
                       kind=coalesce(?4,kind), base_url=coalesce(?5,base_url),
                       api_key_enc=coalesce(?6,api_key_enc), headers=coalesce(?7,headers),
                       enabled=coalesce(?8,enabled), updated_at=?9 WHERE id=?1",
                    params![
                        id2,
                        name,
                        label,
                        kind,
                        base,
                        enc,
                        headers,
                        enabled,
                        sf_core::now_rfc3339()
                    ],
                )
                .map_err(err)?;
            if n == 0 {
                return Ok(None);
            }
            one_provider(t, &key, &id2)
        })
        .await
        .map_err(|e| super::common::conflict_as(e, "a provider with that name already exists"))?
        .ok_or_else(|| not_found("provider"))?;
    audit_user(
        &st,
        &a,
        "provider.patch",
        Some(&id),
        json!({ "api_key_changed": key_changed }),
    )
    .await;
    Ok(Json(v))
}

async fn delete_by_id(
    st: &AppState,
    a: &crate::auth::User,
    table: &'static str,
    what: &'static str,
    id: String,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&id) {
        return Err(not_found(what));
    }
    let id2 = id.clone();
    let n = st
        .db
        .write(move |t| {
            t.execute(&format!("DELETE FROM {table} WHERE id=?1"), [&id2])
                .map_err(err)
        })
        .await?;
    if n == 0 {
        return Err(not_found(what));
    }
    audit_user(st, a, &format!("{table}.delete"), Some(&id), json!({})).await;
    Ok(Json(json!({ "deleted": true })))
}

pub async fn delete_provider(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    delete_by_id(&st, &a, "model_providers", "provider", id).await
}

fn probe_fields(v: &Value) -> (bool, Option<String>, i64) {
    (
        v.get("ok").and_then(Value::as_bool).unwrap_or(false),
        v.get("error")
            .and_then(Value::as_str)
            .map(|s| s.chars().take(500).collect()),
        v.get("latency_ms").and_then(Value::as_i64).unwrap_or(0),
    )
}

async fn stamp_probe(
    st: &AppState,
    table: &'static str,
    id: &str,
    result: &Value,
) -> ApiResult<()> {
    let (ok, error, ms) = probe_fields(result);
    let id = id.to_string();
    st.db
        .write(move |t| {
            let extra = if table == "model_providers" { ", updated_at=?2" } else { "" };
            t.execute(
                &format!(
                    "UPDATE {table} SET last_probe_at=?2, last_probe_ok=?3, last_probe_error=?4, last_probe_ms=?5{extra}
                      WHERE id=?1"
                ),
                params![id, sf_core::now_rfc3339(), ok, error, ms],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// Probes through the provider's first enabled model (chat preferred), v1 semantics.
pub async fn probe_provider(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&id) {
        return Err(not_found("provider"));
    }
    let id2 = id.clone();
    let (exists, pick): (bool, Option<String>) = st
        .db
        .read(move |c| {
            let exists: Option<String> = c
                .query_row("SELECT id FROM model_providers WHERE id=?1", [&id2], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(err)?;
            let pick: Option<String> = c
                .query_row(
                    "SELECT id FROM models WHERE provider_id=?1 AND enabled=1
                      ORDER BY (role='chat') DESC, created_at, id LIMIT 1",
                    [&id2],
                    |r| r.get(0),
                )
                .optional()
                .map_err(err)?;
            Ok((exists.is_some(), pick))
        })
        .await?;
    if !exists {
        return Err(not_found("provider"));
    }
    let result = match &pick {
        None => json!({
            "ok": false, "latency_ms": 0, "status_code": Value::Null,
            "error": "provider has no enabled models to probe — add a model first",
        }),
        Some(model) => {
            let p = prober(&st)?;
            let r = p.probe_provider(&id).await?;
            stamp_probe(&st, "models", model, &r).await?;
            r
        }
    };
    stamp_probe(&st, "model_providers", &id, &result).await?;
    audit_user(
        &st,
        &a,
        "provider.probe",
        Some(&id),
        json!({ "ok": result["ok"] }),
    )
    .await;
    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// models
// ---------------------------------------------------------------------------

pub async fn list_models(
    State(st): State<AppState>,
    _a: Admin,
    q: Params,
) -> ApiResult<Json<Value>> {
    let provider = q.get("provider_id").cloned();
    if let Some(p) = &provider
        && !is_uuid(p)
    {
        return Err(ApiError::bad("provider_id must be a uuid"));
    }
    let role = q.get("role").cloned();
    let rows = st
        .db
        .read(move |c| {
            query_json(
                c,
                &format!(
                    "SELECT {MODEL_COLS} FROM models WHERE (?1 IS NULL OR provider_id=?1)
                       AND (?2 IS NULL OR role=?2) ORDER BY role, model_id, id"
                ),
                &[&provider, &role],
                &[],
                PROBE_BOOLS,
            )
        })
        .await?;
    Ok(Json(Value::Array(rows)))
}

#[derive(Deserialize)]
pub struct ModelBody {
    provider_id: String,
    model_id: String,
    #[serde(default)]
    label: Option<String>,
    role: String,
    #[serde(default)]
    context_tokens: Option<i64>,
    #[serde(default)]
    dim: Option<i64>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    price_in: Option<f64>,
    #[serde(default)]
    price_out: Option<f64>,
    #[serde(default)]
    notes: Option<String>,
}

fn check_model_numbers(
    ctx: Option<i64>,
    dim: Option<i64>,
    pin: Option<f64>,
    pout: Option<f64>,
) -> ApiResult<()> {
    if ctx.is_some_and(|n| n < 1) {
        return Err(ApiError::bad("context_tokens must be >= 1"));
    }
    if dim.is_some_and(|n| !(1..=65_536).contains(&n)) {
        return Err(ApiError::bad("dim must be >= 1"));
    }
    for (k, p) in [("price_in", pin), ("price_out", pout)] {
        if p.is_some_and(|v| !v.is_finite() || v < 0.0) {
            return Err(ApiError::bad(format!("{k} must be >= 0")));
        }
    }
    Ok(())
}

pub async fn create_model(
    State(st): State<AppState>,
    Admin(a): Admin,
    JsonBody(b): JsonBody<ModelBody>,
) -> ApiResult<Response> {
    if !is_uuid(&b.provider_id) {
        return Err(ApiError::bad("provider_id must be a uuid"));
    }
    let model_id = b.model_id.trim().to_string();
    check_len("model_id", &model_id, 1, 200)?;
    if let Some(l) = &b.label {
        check_len("label", l, 0, 200)?;
    }
    check_role(&b.role)?;
    check_model_numbers(b.context_tokens, b.dim, b.price_in, b.price_out)?;
    let notes: String = b.notes.unwrap_or_default().chars().take(4000).collect();
    let id = sf_core::new_id();
    let id2 = id.clone();
    let label = b.label.unwrap_or_else(|| model_id.clone());
    let res: Result<Value, ApiError> = st
        .db
        .write(move |t| {
            let p: Option<String> = t
                .query_row("SELECT id FROM model_providers WHERE id=?1", [&b.provider_id], |r| r.get(0))
                .optional()
                .map_err(err)?;
            if p.is_none() {
                return Ok(Err(not_found("provider")));
            }
            t.execute(
                "INSERT INTO models(id, provider_id, model_id, label, role, context_tokens, dim, enabled,
                    price_in, price_out, notes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    id2,
                    b.provider_id,
                    model_id,
                    label,
                    b.role,
                    b.context_tokens,
                    b.dim,
                    b.enabled.unwrap_or(true),
                    b.price_in,
                    b.price_out,
                    notes
                ],
            )
            .map_err(err)?;
            Ok(query_one_json(
                t,
                &format!("SELECT {MODEL_COLS} FROM models WHERE id=?1"),
                &[&id2],
                &[],
                PROBE_BOOLS,
            )?
            .ok_or_else(|| not_found("model")))
        })
        .await
        .map_err(|e| super::common::conflict_as(e, "that model already exists for this provider"))?;
    let v = res?;
    audit_user(&st, &a, "model.create", Some(&id), json!({})).await;
    Ok((StatusCode::CREATED, Json(v)).into_response())
}

#[derive(Deserialize)]
pub struct ModelPatch {
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    context_tokens: Option<i64>,
    #[serde(default)]
    dim: Option<i64>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    price_in: Option<f64>,
    #[serde(default)]
    price_out: Option<f64>,
    #[serde(default)]
    notes: Option<String>,
}

pub async fn patch_model(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<ModelPatch>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&id) {
        return Err(not_found("model"));
    }
    let model_id = b.model_id.as_deref().map(str::trim).map(str::to_string);
    if let Some(m) = &model_id {
        check_len("model_id", m, 1, 200)?;
    }
    if let Some(l) = &b.label {
        check_len("label", l, 0, 200)?;
    }
    check_model_numbers(b.context_tokens, b.dim, b.price_in, b.price_out)?;
    let notes: Option<String> = b.notes.map(|n| n.chars().take(4000).collect());
    let id2 = id.clone();
    let v = st
        .db
        .write(move |t| {
            let n = t
                .execute(
                    "UPDATE models SET model_id=coalesce(?2,model_id), label=coalesce(?3,label),
                       context_tokens=coalesce(?4,context_tokens), dim=coalesce(?5,dim),
                       enabled=coalesce(?6,enabled), price_in=coalesce(?7,price_in),
                       price_out=coalesce(?8,price_out), notes=coalesce(?9,notes) WHERE id=?1",
                    params![
                        id2,
                        model_id,
                        b.label,
                        b.context_tokens,
                        b.dim,
                        b.enabled,
                        b.price_in,
                        b.price_out,
                        notes
                    ],
                )
                .map_err(err)?;
            if n == 0 {
                return Ok(None);
            }
            query_one_json(
                t,
                &format!("SELECT {MODEL_COLS} FROM models WHERE id=?1"),
                &[&id2],
                &[],
                PROBE_BOOLS,
            )
        })
        .await
        .map_err(|e| super::common::conflict_as(e, "that model already exists for this provider"))?
        .ok_or_else(|| not_found("model"))?;
    audit_user(&st, &a, "model.patch", Some(&id), json!({})).await;
    Ok(Json(v))
}

pub async fn delete_model(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    delete_by_id(&st, &a, "models", "model", id).await
}

pub async fn probe_model(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&id) {
        return Err(not_found("model"));
    }
    let id2 = id.clone();
    let exists: Option<String> = st
        .db
        .read(move |c| {
            c.query_row("SELECT id FROM models WHERE id=?1", [&id2], |r| r.get(0))
                .optional()
                .map_err(err)
        })
        .await?;
    if exists.is_none() {
        return Err(not_found("model"));
    }
    let p = prober(&st)?;
    let mut r = p.probe_model(&id).await?;
    stamp_probe(&st, "models", &id, &r).await?;
    if let Some(o) = r.as_object_mut() {
        o.insert("index_dim".into(), Value::from(INDEX_DIM));
    }
    audit_user(&st, &a, "model.probe", Some(&id), json!({ "ok": r["ok"] })).await;
    Ok(Json(r))
}

// ---------------------------------------------------------------------------
// chains
// ---------------------------------------------------------------------------

fn chain_json(c: &rusqlite::Connection, id: &str) -> sf_core::Result<Option<Value>> {
    query_one_json(
        c,
        &format!("SELECT {CHAIN_COLS} FROM model_chains WHERE id=?1"),
        &[&id],
        &[],
        &["active"],
    )
}

fn rungs_json(c: &rusqlite::Connection, chain_id: &str) -> sf_core::Result<Vec<Value>> {
    query_json(
        c,
        &format!("SELECT {RUNG_COLS} FROM model_chain_rungs WHERE chain_id=?1 ORDER BY position"),
        &[&chain_id],
        &[],
        &["enabled"],
    )
}

pub async fn list_chains(
    State(st): State<AppState>,
    _a: Admin,
    q: Params,
) -> ApiResult<Json<Value>> {
    let site = q.get("site_id").cloned();
    if let Some(s) = &site
        && s != "global"
        && !is_uuid(s)
    {
        return Err(ApiError::bad("site_id must be a uuid or \"global\""));
    }
    let global_only = site.as_deref() == Some("global");
    let site_f = if global_only { None } else { site };
    let role = q.get("role").cloned();
    let v = st
        .db
        .read(move |c| {
            let chains = query_json(
                c,
                &format!(
                    "SELECT {CHAIN_COLS} FROM model_chains
                      WHERE (?1 IS NULL OR role=?1) AND (?3 = 0 OR site_id IS NULL)
                        AND (?2 IS NULL OR site_id=?2)
                      ORDER BY role, site_id IS NOT NULL, site_id, name, id"
                ),
                &[&role, &site_f, &global_only],
                &[],
                &["active"],
            )?;
            let rungs = query_json(
                c,
                "SELECT r.id, r.chain_id, r.model_id, r.position, r.timeout_ms, r.max_retries, r.enabled,
                        m.model_id AS wire_model_id, m.role AS model_role, m.dim, p.name AS provider_name
                   FROM model_chain_rungs r
                   JOIN models m ON m.id = r.model_id
                   JOIN model_providers p ON p.id = m.provider_id
                  ORDER BY r.chain_id, r.position",
                &[],
                &[],
                &["enabled"],
            )?;
            let mut by: std::collections::HashMap<String, Vec<Value>> = Default::default();
            for r in rungs {
                let cid = r["chain_id"].as_str().unwrap_or("").to_string();
                by.entry(cid).or_default().push(r);
            }
            Ok(chains
                .into_iter()
                .map(|mut ch| {
                    let id = ch["id"].as_str().unwrap_or("").to_string();
                    if let Some(o) = ch.as_object_mut() {
                        o.insert("rungs".into(), Value::Array(by.remove(&id).unwrap_or_default()));
                    }
                    ch
                })
                .collect::<Vec<_>>())
        })
        .await?;
    Ok(Json(Value::Array(v)))
}

/// Deactivates every sibling in the same (role, site) scope, then activates `id`. Run inside a txn.
fn activate_within(t: &rusqlite::Transaction<'_>, id: &str) -> sf_core::Result<bool> {
    let scope: Option<(String, Option<String>)> = t
        .query_row(
            "SELECT role, site_id FROM model_chains WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(err)?;
    let Some((role, site)) = scope else {
        return Ok(false);
    };
    let now = sf_core::now_rfc3339();
    t.execute(
        "UPDATE model_chains SET active=0, updated_at=?4
          WHERE role=?1 AND site_id IS ?2 AND id<>?3 AND active=1",
        params![role, site, id, now],
    )
    .map_err(err)?;
    t.execute(
        "UPDATE model_chains SET active=1, updated_at=?2 WHERE id=?1",
        params![id, now],
    )
    .map_err(err)?;
    Ok(true)
}

#[derive(Deserialize)]
pub struct ChainBody {
    name: String,
    role: String,
    #[serde(default)]
    site_id: Option<String>,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    notes: Option<String>,
}

pub async fn create_chain(
    State(st): State<AppState>,
    Admin(a): Admin,
    JsonBody(b): JsonBody<ChainBody>,
) -> ApiResult<Response> {
    let name = b.name.trim().to_string();
    check_len("name", &name, 1, 128)?;
    check_role(&b.role)?;
    if let Some(s) = &b.site_id
        && !is_uuid(s)
    {
        return Err(ApiError::bad("site_id must be a uuid"));
    }
    let notes: String = b.notes.unwrap_or_default().chars().take(4000).collect();
    let id = sf_core::new_id();
    let id2 = id.clone();
    let res: Result<Value, ApiError> = st
        .db
        .write(move |t| {
            if let Some(s) = &b.site_id {
                let found: Option<String> = t
                    .query_row("SELECT id FROM sites WHERE id=?1", [s], |r| r.get(0))
                    .optional()
                    .map_err(err)?;
                if found.is_none() {
                    return Ok(Err(not_found("site")));
                }
            }
            t.execute(
                "INSERT INTO model_chains(id, name, role, site_id, active, notes) VALUES(?1,?2,?3,?4,0,?5)",
                params![id2, name, b.role, b.site_id, notes],
            )
            .map_err(err)?;
            if b.active == Some(true) {
                activate_within(t, &id2)?;
            }
            Ok(chain_json(t, &id2)?.ok_or_else(|| not_found("chain")))
        })
        .await?;
    let v = res?;
    audit_user(&st, &a, "chain.create", Some(&id), json!({})).await;
    Ok((StatusCode::CREATED, Json(v)).into_response())
}

#[derive(Deserialize)]
pub struct ChainPatch {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    notes: Option<String>,
}

pub async fn patch_chain(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<ChainPatch>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&id) {
        return Err(not_found("chain"));
    }
    let name = b.name.as_deref().map(str::trim).map(str::to_string);
    if let Some(n) = &name {
        check_len("name", n, 1, 128)?;
    }
    let notes: Option<String> = b.notes.map(|n| n.chars().take(4000).collect());
    let id2 = id.clone();
    let v = st
        .db
        .write(move |t| {
            let n = t
                .execute(
                    "UPDATE model_chains SET name=coalesce(?2,name), notes=coalesce(?3,notes), updated_at=?4
                      WHERE id=?1",
                    params![id2, name, notes, sf_core::now_rfc3339()],
                )
                .map_err(err)?;
            if n == 0 {
                return Ok(None);
            }
            chain_json(t, &id2)
        })
        .await?
        .ok_or_else(|| not_found("chain"))?;
    audit_user(&st, &a, "chain.patch", Some(&id), json!({})).await;
    Ok(Json(v))
}

pub async fn delete_chain(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    delete_by_id(&st, &a, "model_chains", "chain", id).await
}

pub async fn activate_chain(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&id) {
        return Err(not_found("chain"));
    }
    let id2 = id.clone();
    let v = st
        .db
        .write(move |t| {
            if !activate_within(t, &id2)? {
                return Ok(None);
            }
            chain_json(t, &id2)
        })
        .await?
        .ok_or_else(|| not_found("chain"))?;
    audit_user(&st, &a, "chain.activate", Some(&id), json!({})).await;
    Ok(Json(v))
}

pub async fn deactivate_chain(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&id) {
        return Err(not_found("chain"));
    }
    let id2 = id.clone();
    let v = st
        .db
        .write(move |t| {
            let n = t
                .execute(
                    "UPDATE model_chains SET active=0, updated_at=?2 WHERE id=?1",
                    params![id2, sf_core::now_rfc3339()],
                )
                .map_err(err)?;
            if n == 0 {
                return Ok(None);
            }
            chain_json(t, &id2)
        })
        .await?
        .ok_or_else(|| not_found("chain"))?;
    audit_user(&st, &a, "chain.deactivate", Some(&id), json!({})).await;
    Ok(Json(v))
}

// ---------------------------------------------------------------------------
// rungs
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone)]
pub struct RungBody {
    model_row_id: String,
    #[serde(default)]
    timeout_ms: Option<i64>,
    #[serde(default)]
    max_retries: Option<i64>,
    #[serde(default)]
    enabled: Option<bool>,
}

fn check_rung_numbers(timeout: Option<i64>, retries: Option<i64>) -> ApiResult<()> {
    if timeout.is_some_and(|n| !(1..=3_600_000).contains(&n)) {
        return Err(ApiError::bad("timeout_ms must be >= 1"));
    }
    if retries.is_some_and(|n| !(0..=10).contains(&n)) {
        return Err(ApiError::bad("max_retries must be between 0 and 10"));
    }
    Ok(())
}

fn check_rung_body(r: &RungBody) -> ApiResult<()> {
    if !is_uuid(&r.model_row_id) {
        return Err(ApiError::bad("model_row_id must be a uuid"));
    }
    check_rung_numbers(r.timeout_ms, r.max_retries)
}

/// Role match + embed width guard (an embed rung must produce `INDEX_DIM`-d vectors).
fn rung_problem(
    t: &rusqlite::Transaction<'_>,
    chain_role: &str,
    model_row_id: &str,
) -> sf_core::Result<Option<String>> {
    let m: Option<(String, String, Option<i64>)> = t
        .query_row(
            "SELECT model_id, role, dim FROM models WHERE id=?1",
            [model_row_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(err)?;
    let Some((wire, role, dim)) = m else {
        return Ok(Some(format!("model {model_row_id} does not exist")));
    };
    if role != chain_role {
        return Ok(Some(format!(
            "model \"{wire}\" has role {role} and cannot be a rung of a {chain_role} chain"
        )));
    }
    if chain_role == "embed" {
        match dim {
            None => {
                return Ok(Some(format!(
                    "embed model \"{wire}\" has no declared dim — probe it first, then set dim to {INDEX_DIM}. \
                     An unknown embedding width cannot be chained: mixing widths silently corrupts the vector index."
                )));
            }
            Some(d) if d != INDEX_DIM => {
                return Ok(Some(format!(
                    "embed model \"{wire}\" returns {d}-d vectors but the live index is {INDEX_DIM}-d. \
                     Mixing embedding widths silently corrupts the vector index — reindex everything before \
                     switching width, or pick a model with the same width."
                )));
            }
            _ => {}
        }
    }
    Ok(None)
}

fn chain_role(t: &rusqlite::Transaction<'_>, chain_id: &str) -> sf_core::Result<Option<String>> {
    t.query_row(
        "SELECT role FROM model_chains WHERE id=?1",
        [chain_id],
        |r| r.get(0),
    )
    .optional()
    .map_err(err)
}

pub async fn list_rungs(
    State(st): State<AppState>,
    _a: Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&id) {
        return Err(not_found("chain"));
    }
    let rows = st.db.read(move |c| rungs_json(c, &id)).await?;
    Ok(Json(Value::Array(rows)))
}

#[derive(Deserialize)]
pub struct RungsBody {
    rungs: Vec<RungBody>,
}

pub async fn put_rungs(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<RungsBody>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&id) {
        return Err(not_found("chain"));
    }
    if b.rungs.len() > 100 {
        return Err(ApiError::bad("too many rungs (max 100)"));
    }
    for r in &b.rungs {
        check_rung_body(r)?;
    }
    let id2 = id.clone();
    let res: Result<Vec<Value>, ApiError> = st
        .db
        .write(move |t| {
            let Some(role) = chain_role(t, &id2)? else {
                return Ok(Err(not_found("chain")));
            };
            for r in &b.rungs {
                if let Some(p) = rung_problem(t, &role, &r.model_row_id)? {
                    return Ok(Err(ApiError::bad(p)));
                }
            }
            t.execute("DELETE FROM model_chain_rungs WHERE chain_id=?1", [&id2])
                .map_err(err)?;
            for (i, r) in b.rungs.iter().enumerate() {
                t.execute(
                    "INSERT INTO model_chain_rungs(id, chain_id, model_id, position, timeout_ms, max_retries, enabled)
                     VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        sf_core::new_id(),
                        id2,
                        r.model_row_id,
                        i64::try_from(i).unwrap_or(i64::MAX),
                        r.timeout_ms,
                        r.max_retries,
                        r.enabled.unwrap_or(true)
                    ],
                )
                .map_err(err)?;
            }
            Ok(Ok(rungs_json(t, &id2)?))
        })
        .await?;
    let rows = res?;
    audit_user(
        &st,
        &a,
        "chain.rungs.put",
        Some(&id),
        json!({ "count": rows.len() }),
    )
    .await;
    Ok(Json(Value::Array(rows)))
}

pub async fn add_rung(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<RungBody>,
) -> ApiResult<Response> {
    if !is_uuid(&id) {
        return Err(not_found("chain"));
    }
    check_rung_body(&b)?;
    let id2 = id.clone();
    let res: Result<Value, ApiError> = st
        .db
        .write(move |t| {
            let Some(role) = chain_role(t, &id2)? else {
                return Ok(Err(not_found("chain")));
            };
            if let Some(p) = rung_problem(t, &role, &b.model_row_id)? {
                return Ok(Err(ApiError::bad(p)));
            }
            let next: i64 = t
                .query_row(
                    "SELECT coalesce(max(position) + 1, 0) FROM model_chain_rungs WHERE chain_id=?1",
                    [&id2],
                    |r| r.get(0),
                )
                .map_err(err)?;
            let rid = sf_core::new_id();
            t.execute(
                "INSERT INTO model_chain_rungs(id, chain_id, model_id, position, timeout_ms, max_retries, enabled)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    rid,
                    id2,
                    b.model_row_id,
                    next,
                    b.timeout_ms,
                    b.max_retries,
                    b.enabled.unwrap_or(true)
                ],
            )
            .map_err(err)?;
            Ok(query_one_json(
                t,
                &format!("SELECT {RUNG_COLS} FROM model_chain_rungs WHERE id=?1"),
                &[&rid],
                &[],
                &["enabled"],
            )?
            .ok_or_else(|| not_found("rung")))
        })
        .await?;
    let v = res?;
    audit_user(&st, &a, "chain.rungs.add", Some(&id), json!({})).await;
    Ok((StatusCode::CREATED, Json(v)).into_response())
}

#[derive(Deserialize)]
pub struct ReorderBody {
    rung_ids: Vec<String>,
}

/// `rung_ids` must be an exact permutation of the chain's rungs; gaps from deletes are accepted.
pub async fn reorder_rungs(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<ReorderBody>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&id) {
        return Err(not_found("chain"));
    }
    if b.rung_ids.len() > 1000 {
        return Err(ApiError::bad("too many rung_ids"));
    }
    let id2 = id.clone();
    let res: Result<Vec<Value>, ApiError> = st
        .db
        .write(move |t| {
            let mut stmt = t
                .prepare("SELECT id, position FROM model_chain_rungs WHERE chain_id=?1")
                .map_err(err)?;
            let existing = stmt
                .query_map([&id2], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            drop(stmt);
            let ids: std::collections::HashSet<&str> =
                existing.iter().map(|(i, _)| i.as_str()).collect();
            let given: std::collections::HashSet<&str> =
                b.rung_ids.iter().map(String::as_str).collect();
            if ids.len() != b.rung_ids.len() || given.len() != b.rung_ids.len() || ids != given {
                return Ok(Err(ApiError::bad(
                    "rung_ids must list every rung of the chain exactly once",
                )));
            }
            // Park above every current position first (UNIQUE(chain_id, position) + CHECK >= 0).
            let park = existing.iter().map(|(_, p)| *p).max().unwrap_or(0)
                + i64::try_from(existing.len()).unwrap_or(0)
                + 1;
            t.execute(
                "UPDATE model_chain_rungs SET position = position + ?2 WHERE chain_id=?1",
                params![id2, park],
            )
            .map_err(err)?;
            for (i, rid) in b.rung_ids.iter().enumerate() {
                t.execute(
                    "UPDATE model_chain_rungs SET position=?2 WHERE id=?1",
                    params![rid, i64::try_from(i).unwrap_or(i64::MAX)],
                )
                .map_err(err)?;
            }
            Ok(Ok(rungs_json(t, &id2)?))
        })
        .await?;
    let rows = res?;
    audit_user(&st, &a, "chain.rungs.reorder", Some(&id), json!({})).await;
    Ok(Json(Value::Array(rows)))
}

#[derive(Deserialize)]
pub struct RungPatch {
    #[serde(default)]
    timeout_ms: Option<i64>,
    #[serde(default)]
    max_retries: Option<i64>,
    #[serde(default)]
    enabled: Option<bool>,
}

pub async fn patch_rung(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<RungPatch>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&id) {
        return Err(not_found("rung"));
    }
    check_rung_numbers(b.timeout_ms, b.max_retries)?;
    let id2 = id.clone();
    let v = st
        .db
        .write(move |t| {
            let n = t
                .execute(
                    "UPDATE model_chain_rungs SET timeout_ms=coalesce(?2,timeout_ms),
                       max_retries=coalesce(?3,max_retries), enabled=coalesce(?4,enabled) WHERE id=?1",
                    params![id2, b.timeout_ms, b.max_retries, b.enabled],
                )
                .map_err(err)?;
            if n == 0 {
                return Ok(None);
            }
            query_one_json(
                t,
                &format!("SELECT {RUNG_COLS} FROM model_chain_rungs WHERE id=?1"),
                &[&id2],
                &[],
                &["enabled"],
            )
        })
        .await?
        .ok_or_else(|| not_found("rung"))?;
    audit_user(&st, &a, "chain.rungs.patch", Some(&id), json!({})).await;
    Ok(Json(v))
}

pub async fn delete_rung(
    State(st): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    delete_by_id(&st, &a, "model_chain_rungs", "rung", id).await
}

#[cfg(test)]
mod tests {
    use super::mask_secret;

    #[test]
    fn masks_like_v1() {
        assert_eq!(mask_secret(""), "");
        assert_eq!(mask_secret("abcd"), "****");
        assert_eq!(mask_secret("sk-123456"), "*****3456");
        assert_eq!(
            mask_secret("sk-abcdefghijklmnopqrstuvwxyz"),
            "************wxyz"
        );
    }
}
