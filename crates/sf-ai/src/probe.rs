//! Probes (v1 `POST /providers/:id/probe` and `POST /models/:id/probe`).
//!
//! Response shapes (exact v1 `ProbeResult`):
//! * failure:      `{ok:false, latency_ms, status_code: number|null, error: string}`
//! * chat success: `{ok:true,  latency_ms, status_code, error:null, sample: string(<=120 chars)}`
//! * embed success:`{ok:true,  latency_ms, status_code, error:null, dim: number}`
//! * `probe_model` adds `index_dim: 384`.
//!
//! Upstream failures never error: they ARE the result. Only a missing provider/model row is `NotFound`.

use std::time::Instant;

use serde_json::{Value, json};
use sf_core::EMBED_DIM;
use sf_store::rusqlite::{OptionalExtension, params};

use crate::adapters::{self, Rung, truncate};
use crate::http::{self, SendError};
use crate::types::{AiError, ChatMessage};
use crate::{Ai, ms, resolve};

fn fail(latency_ms: u64, status: Option<u16>, error: String) -> Value {
    json!({"ok": false, "latency_ms": latency_ms, "status_code": status, "error": error})
}

impl Ai {
    /// Probes one model row: chat with `max_tokens:1` "ping", or embed `["test"]`.
    /// Embed probes report the returned width without judging it. Updates `models.last_probe_*`.
    pub async fn probe_model(&self, model_row_id: &str) -> Result<Value, AiError> {
        let id = model_row_id.to_string();
        let raw = self
            .db()
            .read(move |c| resolve::load_model(c, &id).map_err(sf_store::err))
            .await?;
        let Some(raw) = raw else {
            return Err(AiError::NotFound("model not found".into()));
        };
        let role = raw.role.clone();
        let rung = resolve::to_rung(raw, &self.inner.key, None);
        let mut result = self.run_probe(&rung, &role).await;
        self.write_model_probe(model_row_id, &result).await;
        if let Some(o) = result.as_object_mut() {
            o.insert("index_dim".into(), json!(EMBED_DIM));
        }
        Ok(result)
    }

    /// Probes a provider through its first enabled model (chat preferred, then oldest).
    /// Updates `last_probe_*` on that model and on the provider.
    pub async fn probe_provider(&self, provider_id: &str) -> Result<Value, AiError> {
        let id = provider_id.to_string();
        let pick = self
            .db()
            .read(move |c| {
                let exists: Option<String> = c
                    .query_row(
                        "SELECT id FROM model_providers WHERE id = ?1",
                        params![id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(sf_store::err)?;
                if exists.is_none() {
                    return Ok(None);
                }
                let model: Option<String> = c
                    .query_row(
                        "SELECT id FROM models WHERE provider_id = ?1 AND enabled = 1
                          ORDER BY (role = 'chat') DESC, created_at, id LIMIT 1",
                        params![id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(sf_store::err)?;
                Ok(Some(model))
            })
            .await?;
        let Some(model) = pick else {
            return Err(AiError::NotFound("provider not found".into()));
        };
        let result = match model {
            None => fail(
                0,
                None,
                "provider has no enabled models to probe — add a model first".into(),
            ),
            Some(mid) => {
                let id = mid.clone();
                let raw = self
                    .db()
                    .read(move |c| resolve::load_model(c, &id).map_err(sf_store::err))
                    .await?;
                let r = match raw {
                    None => fail(0, None, "model disappeared mid-probe".into()),
                    Some(raw) => {
                        let role = raw.role.clone();
                        let rung = resolve::to_rung(raw, &self.inner.key, None);
                        self.run_probe(&rung, &role).await
                    }
                };
                self.write_model_probe(&mid, &r).await;
                r
            }
        };
        let (ok, err, lat) = probe_cols(&result);
        let pid = provider_id.to_string();
        let now = sf_core::now_rfc3339();
        let w = self
            .db()
            .write(move |tx| {
                tx.execute(
                    "UPDATE model_providers
                        SET last_probe_at = ?2, last_probe_ok = ?3, last_probe_error = ?4,
                            last_probe_ms = ?5, updated_at = ?2
                      WHERE id = ?1",
                    params![pid, now, ok, err, lat],
                )
                .map_err(sf_store::err)
            })
            .await;
        if let Err(e) = w {
            tracing::warn!(error = %e, "provider probe state write failed");
        }
        Ok(result)
    }

    async fn write_model_probe(&self, model_row_id: &str, result: &Value) {
        let (ok, err, lat) = probe_cols(result);
        let mid = model_row_id.to_string();
        let now = sf_core::now_rfc3339();
        let w = self
            .db()
            .write(move |tx| {
                tx.execute(
                    "UPDATE models SET last_probe_at = ?2, last_probe_ok = ?3, last_probe_error = ?4,
                            last_probe_ms = ?5
                      WHERE id = ?1",
                    params![mid, now, ok, err, lat],
                )
                .map_err(sf_store::err)
            })
            .await;
        if let Err(e) = w {
            tracing::warn!(error = %e, "model probe state write failed");
        }
    }

    /// Runs one probe. Never errors.
    async fn run_probe(&self, rung: &Rung, role: &str) -> Value {
        let started = Instant::now();
        let label = rung.label();
        let timeout = self.inner.opts.probe_timeout;
        let embed = role == "embed";
        let req = if embed {
            adapters::embed_requests(rung, &["test".to_string()]).and_then(|mut v| {
                if v.is_empty() {
                    Err("no request".to_string())
                } else {
                    Ok(v.swap_remove(0).req)
                }
            })
        } else {
            adapters::chat_request(rung, &[ChatMessage::user("ping")], 1, 0.0, false)
        };
        let req = match req {
            Ok(r) => r,
            Err(e) => return fail(ms(started.elapsed()), None, e),
        };
        let (status, body) = match http::send(&self.inner.client, &req, timeout).await {
            Ok(x) => x,
            Err(SendError::Http { status, detail, .. }) => {
                return fail(
                    ms(started.elapsed()),
                    Some(status),
                    format!("{label}: HTTP {status} {detail}")
                        .trim()
                        .to_string(),
                );
            }
            Err(SendError::BadBody { status, msg }) => {
                return fail(ms(started.elapsed()), Some(status), msg);
            }
            Err(SendError::Refused(m)) | Err(SendError::Transport(m)) => {
                return fail(ms(started.elapsed()), None, m);
            }
        };
        let latency = ms(started.elapsed());
        if embed {
            match adapters::parse_embed(&rung.kind, &body, 1) {
                Ok((vecs, _)) => {
                    let dim = vecs.first().map(Vec::len).unwrap_or(0);
                    json!({"ok": true, "latency_ms": latency, "status_code": status, "error": null, "dim": dim})
                }
                Err(e) => fail(latency, Some(status), e),
            }
        } else {
            // An empty reply is fine: with max_tokens=1 some providers return only a stop reason.
            let parsed = adapters::parse_chat(&rung.kind, &body);
            json!({
                "ok": true,
                "latency_ms": latency,
                "status_code": status,
                "error": null,
                "sample": truncate(&parsed.text, 120),
            })
        }
    }
}

fn probe_cols(v: &Value) -> (bool, Option<String>, i64) {
    let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let err = v.get("error").and_then(Value::as_str).map(str::to_string);
    let lat = v.get("latency_ms").and_then(Value::as_i64).unwrap_or(0);
    (ok, err, lat)
}
