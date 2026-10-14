//! Read-only views for the v2 Models status page: provider/model health and resolved chains.

use sf_core::EMBED_DIM;
use sf_store::rusqlite::{self, Connection, params};

use crate::Ai;
use crate::adapters::{KeyState, Rung};
use crate::resolve::{self, DimPolicy, ENV_PROVIDER};
use crate::types::{AiError, ChainDetail, ModelStatus, ProviderStatus, Role, RungInfo};

struct Window {
    calls: u32,
    ok: u32,
    avg_latency: Option<f64>,
    last_called_at: Option<String>,
    last_error: Option<String>,
    last_error_at: Option<String>,
}

/// Stats over the last `n` attempts matching `filter` (`model_row_id = ?1` or the env provider).
fn window(conn: &Connection, filter: &str, arg: &str, n: u32) -> rusqlite::Result<Window> {
    let sql = format!(
        "SELECT ok, latency_ms, called_at FROM model_call_log WHERE {filter} ORDER BY id DESC LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![arg, n], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let calls = u32::try_from(rows.len()).unwrap_or(u32::MAX);
    let ok = u32::try_from(rows.iter().filter(|r| r.0 != 0).count()).unwrap_or(u32::MAX);
    let lats: Vec<f64> = rows.iter().filter_map(|r| r.1).map(|l| l as f64).collect();
    let avg_latency = if lats.is_empty() {
        None
    } else {
        Some(lats.iter().sum::<f64>() / lats.len() as f64)
    };
    let last_called_at = rows.first().map(|r| r.2.clone());
    let sql = format!(
        "SELECT error, called_at FROM model_call_log WHERE {filter} AND ok = 0 ORDER BY id DESC LIMIT 1"
    );
    let last = conn
        .query_row(&sql, params![arg], |r| {
            Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?))
        })
        .ok();
    Ok(Window {
        calls,
        ok,
        avg_latency,
        last_called_at,
        last_error: last.as_ref().and_then(|l| l.0.clone()),
        last_error_at: last.map(|l| l.1),
    })
}

struct ProviderRow {
    id: String,
    name: String,
    label: String,
    kind: String,
    base_url: String,
    api_key_enc: String,
    enabled: bool,
    last_probe_at: Option<String>,
    last_probe_ok: Option<bool>,
    last_probe_error: Option<String>,
}

struct ModelRow {
    id: String,
    provider_id: String,
    model_id: String,
    role: String,
    enabled: bool,
}

impl Ai {
    /// Every provider (plus the env fallback) with per-model breaker state, last error and
    /// success rate over the last `status_window` attempts in `model_call_log`.
    pub async fn status(&self) -> Result<Vec<ProviderStatus>, AiError> {
        let n = self.inner.opts.status_window.max(1);
        let env_models = resolve::env_rungs(Role::Chat, &self.inner.llm.get());
        let (providers, models, windows, env_window) = self
            .db()
            .read(move |c| {
                let mut stmt = c
                    .prepare(
                        "SELECT id, name, label, kind, base_url, api_key_enc, enabled,
                                last_probe_at, last_probe_ok, last_probe_error
                           FROM model_providers ORDER BY name, id",
                    )
                    .map_err(sf_store::err)?;
                let providers = stmt
                    .query_map([], |r| {
                        Ok(ProviderRow {
                            id: r.get(0)?,
                            name: r.get(1)?,
                            label: r.get(2)?,
                            kind: r.get(3)?,
                            base_url: r.get(4)?,
                            api_key_enc: r.get(5)?,
                            enabled: r.get::<_, i64>(6)? != 0,
                            last_probe_at: r.get(7)?,
                            last_probe_ok: r.get::<_, Option<i64>>(8)?.map(|v| v != 0),
                            last_probe_error: r.get(9)?,
                        })
                    })
                    .map_err(sf_store::err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(sf_store::err)?;
                let mut stmt = c
                    .prepare(
                        "SELECT id, provider_id, model_id, role, enabled FROM models
                          ORDER BY role, model_id, id",
                    )
                    .map_err(sf_store::err)?;
                let models = stmt
                    .query_map([], |r| {
                        Ok(ModelRow {
                            id: r.get(0)?,
                            provider_id: r.get(1)?,
                            model_id: r.get(2)?,
                            role: r.get(3)?,
                            enabled: r.get::<_, i64>(4)? != 0,
                        })
                    })
                    .map_err(sf_store::err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(sf_store::err)?;
                let mut windows = Vec::with_capacity(models.len());
                for m in &models {
                    windows.push(window(c, "model_row_id = ?1", &m.id, n).map_err(sf_store::err)?);
                }
                let env_window =
                    window(c, "model_row_id IS NULL AND provider = ?1", ENV_PROVIDER, n)
                        .map_err(sf_store::err)?;
                Ok((providers, models, windows, env_window))
            })
            .await?;

        let breakers = &self.inner.breakers;
        let mk = |row_id: Option<String>,
                  model_id: String,
                  role: String,
                  enabled: bool,
                  w: &Window,
                  bkey: &str| {
            ModelStatus {
                model_row_id: row_id,
                model_id,
                role,
                enabled,
                breaker: breakers.view(bkey),
                recent_calls: w.calls,
                recent_ok: w.ok,
                success_rate: if w.calls == 0 {
                    None
                } else {
                    Some(f64::from(w.ok) / f64::from(w.calls))
                },
                avg_latency_ms: w.avg_latency,
                last_error: w.last_error.clone(),
                last_error_at: w.last_error_at.clone(),
                last_called_at: w.last_called_at.clone(),
            }
        };

        let mut out = Vec::with_capacity(providers.len() + 1);
        for p in providers {
            let ms: Vec<ModelStatus> = models
                .iter()
                .zip(windows.iter())
                .filter(|(m, _)| m.provider_id == p.id)
                .map(|(m, w)| {
                    mk(
                        Some(m.id.clone()),
                        m.model_id.clone(),
                        m.role.clone(),
                        m.enabled,
                        w,
                        &m.id,
                    )
                })
                .collect();
            let key_ok = p.api_key_enc.is_empty()
                || sf_core::crypto::decrypt_secret(&self.inner.key, &p.api_key_enc).is_ok();
            out.push(ProviderStatus {
                provider_id: Some(p.id),
                name: p.name,
                label: p.label,
                kind: p.kind,
                enabled: p.enabled,
                base_url: p.base_url,
                has_api_key: !p.api_key_enc.is_empty(),
                key_ok,
                last_probe_at: p.last_probe_at,
                last_probe_ok: p.last_probe_ok,
                last_probe_error: p.last_probe_error,
                models: ms,
            });
        }
        if !env_models.is_empty() {
            // The call log has no model column for env rungs, so the window is shared.
            let models = env_models
                .iter()
                .map(|r| {
                    mk(
                        None,
                        r.model_id.clone(),
                        "chat".into(),
                        true,
                        &env_window,
                        &r.breaker_key(),
                    )
                })
                .collect();
            out.push(ProviderStatus {
                provider_id: None,
                name: ENV_PROVIDER.into(),
                label: "Env fallback (llm.fallback_url)".into(),
                kind: "openai-compatible".into(),
                enabled: true,
                base_url: env_models
                    .first()
                    .map(|r| r.base_url.clone())
                    .unwrap_or_default(),
                has_api_key: false,
                key_ok: true,
                last_probe_at: None,
                last_probe_ok: None,
                last_probe_error: None,
                models,
            });
        }
        Ok(out)
    }

    /// The chain that would serve `role` for `site_id`, for display (keys never included).
    /// Embed chains are judged against the 384-d index, like `embed`.
    pub async fn resolve_chain_detail(
        &self,
        role: Role,
        site_id: Option<&str>,
    ) -> Result<ChainDetail, AiError> {
        let policy = match role {
            Role::Chat => DimPolicy::Model,
            Role::Embed => DimPolicy::Index(EMBED_DIM),
        };
        let r = self.resolve(role, site_id, policy).await;
        let mut rungs: Vec<RungInfo> = r.rungs.iter().map(|x| self.info(x, None)).collect();
        rungs.extend(
            r.skipped
                .iter()
                .map(|(x, why)| self.info(x, Some(why.clone()))),
        );
        Ok(ChainDetail {
            role,
            chain_id: r.chain_id,
            chain_name: r.chain_name,
            source: r.source,
            rungs,
        })
    }

    fn info(&self, r: &Rung, skip: Option<String>) -> RungInfo {
        let skip = skip.or_else(|| match r.key {
            KeyState::Undecryptable => Some("api key cannot be decrypted".into()),
            _ => None,
        });
        RungInfo {
            position: r.position,
            model_row_id: r.model_row_id.clone(),
            model_id: r.model_id.clone(),
            dim: r.dim,
            provider_id: r.provider_id.clone(),
            provider_name: r.provider_name.clone(),
            provider_kind: r.kind.as_str().to_string(),
            base_url: r.base_url.clone(),
            timeout_ms: r.timeout_ms,
            max_retries: r.max_retries,
            key_status: r.key.status().into(),
            breaker: self.inner.breakers.view(&r.breaker_key()).state,
            usable: skip.is_none(),
            skip_reason: skip,
        }
    }
}
