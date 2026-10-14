//! Chain resolution (spec 5.3): site-scoped active chain -> global active chain -> env fallback.

use sf_core::config::LlmCfg;
use sf_store::rusqlite::{self, Connection, OptionalExtension, params};

use crate::adapters::{KeyState, Rung, headers_from_json, normalize_base_url};
use crate::types::{ChainSource, ProviderKind, Role};

pub(crate) const ENV_PROVIDER: &str = "env:dario";

/// How embed rungs are judged at resolution time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DimPolicy {
    /// Vectors go into the 384-d index: a rung whose declared `models.dim` differs is skipped
    /// (fixes B11: a dim edited after chaining no longer fails every call at runtime).
    Index(usize),
    /// Caller accepts the model's own width (`models.dim`, else 384).
    Model,
}

pub(crate) struct Resolved {
    pub chain_id: Option<String>,
    pub chain_name: Option<String>,
    pub source: ChainSource,
    pub rungs: Vec<Rung>,
    /// Rungs of the active chain excluded at resolution, with the reason (display only).
    pub skipped: Vec<(Rung, String)>,
}

/// One joined rung/model/provider row as stored.
pub(crate) struct RawRow {
    pub position: i64,
    pub timeout_ms: Option<i64>,
    pub max_retries: Option<i64>,
    pub model_row_id: String,
    pub model_id: String,
    pub role: String,
    pub dim: Option<i64>,
    pub provider_id: String,
    pub provider_name: String,
    pub kind: String,
    pub base_url: String,
    pub api_key_enc: String,
    pub headers: String,
}

const RUNG_COLS: &str = "r.position, r.timeout_ms, r.max_retries,
       m.id, m.model_id, m.role, m.dim,
       p.id, p.name, p.kind, p.base_url, p.api_key_enc, p.headers";

fn map_raw(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        position: row.get(0)?,
        timeout_ms: row.get(1)?,
        max_retries: row.get(2)?,
        model_row_id: row.get(3)?,
        model_id: row.get(4)?,
        role: row.get(5)?,
        dim: row.get(6)?,
        provider_id: row.get(7)?,
        provider_name: row.get(8)?,
        kind: row.get(9)?,
        base_url: row.get(10)?,
        api_key_enc: row.get(11)?,
        headers: row.get(12)?,
    })
}

/// Decrypts and shapes a raw row. The key is decrypted here, per call, and never logged.
pub(crate) fn to_rung(raw: RawRow, key: &[u8; 32], chain_id: Option<&str>) -> Rung {
    let kind = ProviderKind::parse(&raw.kind);
    let key_state = if raw.api_key_enc.is_empty() {
        KeyState::None
    } else {
        match sf_core::crypto::decrypt_secret(key, &raw.api_key_enc) {
            Ok(k) if k.is_empty() => KeyState::None,
            Ok(k) => KeyState::Plain(k),
            Err(_) => KeyState::Undecryptable,
        }
    };
    // L-07: header values are sealed at rest; legacy plaintext values pass through.
    let headers: Vec<(String, String)> = serde_json::from_str(&raw.headers)
        .map(|v| headers_from_json(&v))
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k, sf_core::crypto::open_header_value(key, &v)))
        .collect();
    Rung {
        position: raw.position,
        base_url: normalize_base_url(&kind, &raw.base_url),
        kind,
        model_row_id: Some(raw.model_row_id),
        model_id: raw.model_id,
        dim: raw.dim,
        provider_id: Some(raw.provider_id),
        provider_name: raw.provider_name,
        key: key_state,
        headers,
        timeout_ms: raw
            .timeout_ms
            .and_then(|t| u64::try_from(t).ok())
            .filter(|t| *t > 0),
        max_retries: raw
            .max_retries
            .and_then(|r| u32::try_from(r).ok())
            .map(|r| r.min(10)),
        chain_id: chain_id.map(str::to_string),
    }
}

/// Env fallback rungs (chat only): `llm.fallback_models` against `llm.fallback_url`, no auth.
pub(crate) fn env_rungs(role: Role, llm: &LlmCfg) -> Vec<Rung> {
    if role != Role::Chat {
        return Vec::new();
    }
    let kind = ProviderKind::OpenAiCompatible;
    let base = normalize_base_url(&kind, &llm.fallback_url);
    if base.is_empty() {
        return Vec::new();
    }
    let mut seen: Vec<&str> = Vec::new();
    for m in &llm.fallback_models {
        let m = m.trim();
        if !m.is_empty() && !seen.contains(&m) {
            seen.push(m);
        }
    }
    seen.iter()
        .enumerate()
        .map(|(i, m)| Rung {
            position: i64::try_from(i).unwrap_or(i64::MAX),
            model_row_id: None,
            model_id: (*m).to_string(),
            dim: None,
            provider_id: None,
            provider_name: ENV_PROVIDER.into(),
            kind: kind.clone(),
            base_url: base.clone(),
            key: KeyState::None,
            headers: Vec::new(),
            timeout_ms: Some(llm.timeout_ms).filter(|t| *t > 0),
            max_retries: None,
            chain_id: None,
        })
        .collect()
}

pub(crate) struct ActiveChain {
    pub id: String,
    pub name: String,
    pub source: ChainSource,
    pub rows: Vec<RawRow>,
}

/// Reads the active chain for (role, site) with its enabled rungs. Runs on a reader connection.
pub(crate) fn load_active(
    conn: &Connection,
    role: Role,
    site_id: Option<&str>,
) -> rusqlite::Result<Option<ActiveChain>> {
    let mut found: Option<(String, String, ChainSource)> = None;
    if let Some(site) = site_id {
        found = conn
            .query_row(
                "SELECT id, name FROM model_chains WHERE role = ?1 AND site_id = ?2 AND active = 1 ORDER BY id LIMIT 1",
                params![role.as_str(), site],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, ChainSource::Site)),
            )
            .optional()?;
    }
    if found.is_none() {
        found = conn
            .query_row(
                "SELECT id, name FROM model_chains WHERE role = ?1 AND site_id IS NULL AND active = 1 ORDER BY id LIMIT 1",
                params![role.as_str()],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, ChainSource::Global)),
            )
            .optional()?;
    }
    let Some((id, name, source)) = found else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT {RUNG_COLS}
           FROM model_chain_rungs r
           JOIN models m ON m.id = r.model_id
           JOIN model_providers p ON p.id = m.provider_id
          WHERE r.chain_id = ?1 AND r.enabled = 1 AND m.enabled = 1 AND p.enabled = 1
          ORDER BY r.position, r.id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![id], map_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(ActiveChain {
        id,
        name,
        source,
        rows,
    }))
}

/// One model row as a probe target (enabled flags ignored, like v1).
pub(crate) fn load_model(
    conn: &Connection,
    model_row_id: &str,
) -> rusqlite::Result<Option<RawRow>> {
    let sql = "SELECT 0, NULL, NULL, m.id, m.model_id, m.role, m.dim,
                      p.id, p.name, p.kind, p.base_url, p.api_key_enc, p.headers
                 FROM models m JOIN model_providers p ON p.id = m.provider_id
                WHERE m.id = ?1";
    conn.query_row(sql, params![model_row_id], map_raw)
        .optional()
}

/// Splits an active chain's rows into usable rungs and skipped ones.
pub(crate) fn build(
    active: ActiveChain,
    role: Role,
    policy: DimPolicy,
    key: &[u8; 32],
) -> Resolved {
    let mut rungs = Vec::new();
    let mut skipped = Vec::new();
    for raw in active.rows {
        let model_role = raw.role.clone();
        let rung = to_rung(raw, key, Some(&active.id));
        if model_role != role.as_str() {
            skipped.push((
                rung,
                format!(
                    "model role '{model_role}' does not match chain role '{}'",
                    role.as_str()
                ),
            ));
            continue;
        }
        if role == Role::Embed
            && let DimPolicy::Index(expected) = policy
            && let Some(d) = rung.dim
            && usize::try_from(d).ok() != Some(expected)
        {
            skipped.push((
                rung,
                format!("model dim {d} does not match the index dim {expected}"),
            ));
            continue;
        }
        rungs.push(rung);
    }
    Resolved {
        chain_id: Some(active.id),
        chain_name: Some(active.name),
        source: active.source,
        rungs,
        skipped,
    }
}
