//! sf-ai: unified AI model layer — provider adapters, chain resolution (site -> global -> env),
//! retries, per-rung circuit breaker, per-call hard deadline, call log, probes and status.
//!
//! ```text
//! Ai::chat / Ai::embed
//!   -> resolve chain (site-scoped active chain, else global active chain, else env fallback)
//!   -> for each rung: breaker permit? -> attempts = max_retries+1 (retry only transport/408/409/425/429/5xx)
//!        -> adapter builds request -> guarded HTTP send (SSRF policy, timeout) -> parse -> validate
//!        -> every attempt logged to model_call_log
//!   -> first valid result wins; hard deadline caps the whole call
//! ```

mod adapters;
mod breaker;
mod http;
mod probe;
mod resolve;
mod status;
mod types;

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use sf_core::EMBED_DIM;
use sf_core::config::LlmCfg;
use sf_store::Db;
use sf_store::rusqlite::params;

use crate::adapters::{ChatParsed, Rung, truncate};
use crate::breaker::{Breakers, Permit};
use crate::http::SendError;
use crate::resolve::{DimPolicy, Resolved};

pub use crate::types::{
    AiError, BreakerView, ChainDetail, ChainSource, ChatMessage, ChatRequest, ChatResponse,
    EmbedResponse, ModelStatus, ProviderKind, ProviderStatus, Role, RungInfo,
};

/// Probe timeout (v1: 20 s).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// Most texts accepted by one `embed` call.
pub const MAX_EMBED_TEXTS: usize = 2048;
/// Most bytes of message content accepted by one `chat` call.
pub const MAX_CHAT_BYTES: usize = 4 * 1024 * 1024;

const RETRYABLE: [u16; 4] = [408, 409, 425, 429];
const MIN_NORM: f64 = 1e-8;

/// Tuning knobs. `Default` is what production uses; tests shrink the durations.
#[derive(Debug, Clone)]
pub struct AiOptions {
    /// Default per-attempt timeout for chat rungs without `timeout_ms` (v1: 120 s).
    pub chat_timeout: Duration,
    /// Default per-attempt timeout for embed rungs without `timeout_ms` (v1: 20 s).
    pub embed_timeout: Duration,
    pub probe_timeout: Duration,
    /// Hard deadline for a whole chat call across all rungs and retries.
    pub chat_deadline: Duration,
    /// Hard deadline for a whole embed call.
    pub embed_deadline: Duration,
    /// First retry backoff on the same rung (doubles per retry, capped by `max_backoff`).
    pub retry_backoff: Duration,
    pub max_backoff: Duration,
    /// Consecutive health failures that open a rung's breaker.
    pub breaker_threshold: u32,
    /// How long an opened breaker skips the rung before a half-open probe.
    pub breaker_cooldown: Duration,
    /// Attempts per model considered by `status()` success rates.
    pub status_window: u32,
}

impl Default for AiOptions {
    fn default() -> Self {
        Self {
            chat_timeout: Duration::from_secs(120),
            embed_timeout: Duration::from_secs(20),
            probe_timeout: PROBE_TIMEOUT,
            chat_deadline: Duration::from_secs(300),
            embed_deadline: Duration::from_secs(90),
            retry_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(8),
            breaker_threshold: 3,
            breaker_cooldown: Duration::from_secs(30),
            status_window: 50,
        }
    }
}

struct Inner {
    db: Db,
    key: Arc<[u8; 32]>,
    llm: LlmCfg,
    opts: AiOptions,
    client: reqwest::Client,
    breakers: Breakers,
}

/// Cheap-to-clone handle to the AI layer.
#[derive(Clone)]
pub struct Ai {
    inner: Arc<Inner>,
}

enum Op<'a> {
    Chat {
        messages: &'a [ChatMessage],
        max_tokens: u32,
        temperature: f32,
        json_mode: bool,
    },
    Embed {
        texts: &'a [String],
        policy: DimPolicy,
    },
}

enum Outcome {
    Chat(ChatParsed),
    Embed(Vec<Vec<f32>>, Option<i64>),
}

impl Outcome {
    fn tokens(&self) -> (Option<i64>, Option<i64>) {
        match self {
            Outcome::Chat(p) => (p.tokens_in, p.tokens_out),
            Outcome::Embed(_, t) => (*t, None),
        }
    }
}

#[derive(Debug)]
struct Failure {
    msg: String,
    status: Option<u16>,
    retryable: bool,
    /// Counts toward the breaker (provider health signal).
    health: bool,
    retry_after: Option<Duration>,
}

impl Failure {
    fn config(msg: String) -> Self {
        Self {
            msg,
            status: None,
            retryable: false,
            health: false,
            retry_after: None,
        }
    }
    fn bad_output(msg: String, status: u16, health: bool) -> Self {
        Self {
            msg,
            status: Some(status),
            retryable: false,
            health,
            retry_after: None,
        }
    }
}

fn map_send_error(label: &str, e: SendError) -> Failure {
    match e {
        SendError::Refused(m) => Failure::config(format!("{label}: {m}")),
        SendError::Transport(m) => Failure {
            msg: format!("{label}: {m}"),
            status: None,
            retryable: true,
            health: true,
            retry_after: None,
        },
        SendError::Http {
            status,
            detail,
            retry_after,
        } => Failure {
            msg: format!("{label}: HTTP {status} {detail}")
                .trim()
                .to_string(),
            status: Some(status),
            retryable: RETRYABLE.contains(&status) || (500..600).contains(&status),
            health: !matches!(status, 400 | 413 | 422),
            retry_after,
        },
        SendError::BadBody { status, msg } => {
            Failure::bad_output(format!("{label}: {msg}"), status, true)
        }
    }
}

/// Validates and L2-normalises one vector.
fn unit_vector(raw: &[f64], dim: usize, label: &str) -> Result<Vec<f32>, String> {
    if raw.len() != dim {
        return Err(format!("{label} has dim {}, expected {dim}", raw.len()));
    }
    let mut norm = 0.0f64;
    for (i, x) in raw.iter().enumerate() {
        if !x.is_finite() || !(*x as f32).is_finite() {
            return Err(format!("{label}[{i}] is not a finite number"));
        }
        norm += x * x;
    }
    let norm = norm.sqrt();
    if !norm.is_finite() || norm <= MIN_NORM {
        return Err(format!("{label} is a zero vector"));
    }
    Ok(raw.iter().map(|x| (x / norm) as f32).collect())
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Extracts a JSON value from a model reply (strips code fences / surrounding prose).
fn parse_json_reply<T: DeserializeOwned>(text: &str) -> Result<T, String> {
    let t = text.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```JSON"))
        .or_else(|| t.strip_prefix("```"))
        .map(|s| s.trim_end().trim_end_matches("```").trim())
        .unwrap_or(t);
    let first_err = match serde_json::from_str::<T>(t) {
        Ok(v) => return Ok(v),
        Err(e) => e.to_string(),
    };
    for (open, close) in [('{', '}'), ('[', ']')] {
        if let (Some(s), Some(e)) = (t.find(open), t.rfind(close))
            && s < e
            && let Some(slice) = t.get(s..=e)
            && let Ok(v) = serde_json::from_str::<T>(slice)
        {
            return Ok(v);
        }
    }
    Err(truncate(&first_err, 300))
}

impl Ai {
    /// Builds the handle with production defaults.
    pub fn new(db: Db, secret_key: Arc<[u8; 32]>, llm: LlmCfg) -> Result<Self, AiError> {
        Self::with_options(db, secret_key, llm, AiOptions::default())
    }

    pub fn with_options(
        db: Db,
        secret_key: Arc<[u8; 32]>,
        llm: LlmCfg,
        opts: AiOptions,
    ) -> Result<Self, AiError> {
        let client = http::build_client().map_err(AiError::BadRequest)?;
        let breakers = Breakers::new(opts.breaker_threshold, opts.breaker_cooldown);
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                key: secret_key,
                llm,
                opts,
                client,
                breakers,
            }),
        })
    }

    pub(crate) fn db(&self) -> &Db {
        &self.inner.db
    }

    /// Resolves the executable chain for a role. Never fails on DB errors (falls back to env).
    async fn resolve(&self, role: Role, site_id: Option<&str>, policy: DimPolicy) -> Resolved {
        let site = site_id.map(str::to_string);
        let res = self
            .inner
            .db
            .read(move |c| resolve::load_active(c, role, site.as_deref()).map_err(sf_store::err))
            .await;
        let mut skipped = Vec::new();
        match res {
            Ok(Some(active)) => {
                let chain_id = active.id.clone();
                let r = resolve::build(active, role, policy, &self.inner.key);
                if !r.rungs.is_empty() {
                    return r;
                }
                tracing::warn!(
                    chain = %chain_id,
                    role = role.as_str(),
                    "active model chain has no usable rungs; using env fallback"
                );
                skipped = r.skipped;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, role = role.as_str(), "model chain lookup failed; using env fallback");
            }
        }
        let env = resolve::env_rungs(role, &self.inner.llm);
        Resolved {
            chain_id: None,
            chain_name: None,
            source: if env.is_empty() {
                ChainSource::None
            } else {
                ChainSource::Env
            },
            rungs: env,
            skipped,
        }
    }

    /// Chat across the resolved chain (site chain first when `site_id` is set).
    pub async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, AiError> {
        if req.messages.is_empty() {
            return Err(AiError::BadRequest(
                "chat needs at least one message".into(),
            ));
        }
        let bytes: usize = req
            .messages
            .iter()
            .map(|m| m.content.len().saturating_add(m.role.len()))
            .fold(0usize, usize::saturating_add);
        if bytes > MAX_CHAT_BYTES {
            return Err(AiError::BadRequest(format!(
                "chat messages too large ({bytes} bytes, max {MAX_CHAT_BYTES})"
            )));
        }
        let temperature = if req.temperature.is_finite() {
            req.temperature.clamp(0.0, 2.0)
        } else {
            0.3
        };
        let max_tokens = req.max_tokens.clamp(1, 200_000);
        let resolved = self
            .resolve(Role::Chat, req.site_id.as_deref(), DimPolicy::Model)
            .await;
        if resolved.rungs.is_empty() {
            return Err(AiError::NotConfigured(
                "no chat rungs available - activate a chat chain in Settings > Models or set llm.fallback_url/fallback_models".into(),
            ));
        }
        let op = Op::Chat {
            messages: &req.messages,
            max_tokens,
            temperature,
            json_mode: req.json_mode,
        };
        let (out, rung, idx, latency, fallbacks) = self
            .execute(Role::Chat, &resolved, &op, self.inner.opts.chat_deadline)
            .await?;
        let Outcome::Chat(parsed) = out else {
            return Err(AiError::InvalidOutput("unexpected outcome".into()));
        };
        Ok(ChatResponse {
            text: parsed.text,
            provider: rung.provider_name.clone(),
            model: rung.model_id.clone(),
            rung: idx,
            latency_ms: latency,
            tokens_in: parsed.tokens_in,
            tokens_out: parsed.tokens_out,
            fallbacks_used: fallbacks,
        })
    }

    /// Chat in JSON mode and deserialize the reply; one repair round-trip if it does not parse.
    pub async fn chat_json<T: DeserializeOwned>(
        &self,
        req: ChatRequest,
    ) -> Result<(T, ChatResponse), AiError> {
        let mut req = req;
        req.json_mode = true;
        let first = self.chat(req.clone()).await?;
        let err = match parse_json_reply::<T>(&first.text) {
            Ok(v) => return Ok((v, first)),
            Err(e) => e,
        };
        tracing::debug!(error = %err, "chat_json: reply did not parse, asking for a repair");
        let mut repair = req;
        repair
            .messages
            .push(ChatMessage::assistant(truncate(&first.text, 20_000)));
        repair.messages.push(ChatMessage::user(format!(
            "Your previous reply was not valid JSON for the expected shape ({err}). Reply again with ONLY the corrected JSON value."
        )));
        let second = self.chat(repair).await?;
        match parse_json_reply::<T>(&second.text) {
            Ok(v) => Ok((v, second)),
            Err(e) => Err(AiError::InvalidOutput(format!(
                "reply is not valid JSON after one repair: {e}"
            ))),
        }
    }

    /// Embeds texts through the active embed chain (site chain first), validated to the 384-d
    /// index. `Err(NotConfigured)` when no embed chain is active: sf-embed then uses its own chain.
    pub async fn embed(
        &self,
        texts: &[String],
        site_id: Option<&str>,
    ) -> Result<EmbedResponse, AiError> {
        self.embed_inner(texts, site_id, DimPolicy::Index(EMBED_DIM))
            .await
    }

    /// Like `embed`, but accepts the model's declared width (`models.dim`, else 384). For
    /// non-index uses only; never store these vectors in the 384-d index.
    pub async fn embed_model_dim(
        &self,
        texts: &[String],
        site_id: Option<&str>,
    ) -> Result<EmbedResponse, AiError> {
        self.embed_inner(texts, site_id, DimPolicy::Model).await
    }

    async fn embed_inner(
        &self,
        texts: &[String],
        site_id: Option<&str>,
        policy: DimPolicy,
    ) -> Result<EmbedResponse, AiError> {
        if texts.len() > MAX_EMBED_TEXTS {
            return Err(AiError::BadRequest(format!(
                "too many texts ({}, max {MAX_EMBED_TEXTS})",
                texts.len()
            )));
        }
        let resolved = self.resolve(Role::Embed, site_id, policy).await;
        if resolved.rungs.is_empty() {
            return Err(AiError::NotConfigured(
                "no active embed chain with usable rungs".into(),
            ));
        }
        if texts.is_empty() {
            let first = resolved.rungs.first();
            return Ok(EmbedResponse {
                vectors: Vec::new(),
                provider: first.map(|r| r.provider_name.clone()).unwrap_or_default(),
                model: first.map(|r| r.model_id.clone()).unwrap_or_default(),
                rung: 0,
                latency_ms: 0,
                tokens_in: None,
                fallbacks_used: 0,
            });
        }
        let op = Op::Embed { texts, policy };
        let (out, rung, idx, latency, fallbacks) = self
            .execute(Role::Embed, &resolved, &op, self.inner.opts.embed_deadline)
            .await?;
        let Outcome::Embed(vectors, tokens_in) = out else {
            return Err(AiError::InvalidOutput("unexpected outcome".into()));
        };
        Ok(EmbedResponse {
            vectors,
            provider: rung.provider_name.clone(),
            model: rung.model_id.clone(),
            rung: idx,
            latency_ms: latency,
            tokens_in,
            fallbacks_used: fallbacks,
        })
    }

    /// The fallback engine shared by chat and embed.
    async fn execute<'r>(
        &self,
        role: Role,
        resolved: &'r Resolved,
        op: &Op<'_>,
        deadline: Duration,
    ) -> Result<(Outcome, &'r Rung, usize, u64, usize), AiError> {
        let started = Instant::now();
        let deadline_at = started + deadline;
        let opts = &self.inner.opts;
        let breakers = &self.inner.breakers;
        let mut failures: Vec<String> = Vec::new();
        let total = resolved.rungs.len();
        for (idx, rung) in resolved.rungs.iter().enumerate() {
            let label = format!("#{idx} {}", rung.label());
            let bkey = rung.breaker_key();
            let permit = breakers.permit(&bkey);
            if let Permit::Skip(wait) = permit {
                failures.push(format!(
                    "{label}: circuit open (next probe in {}s)",
                    wait.as_secs()
                ));
                continue;
            }
            let attempts = rung.max_retries.unwrap_or(0).min(10).saturating_add(1);
            let rung_timeout = rung
                .timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(match role {
                    Role::Chat => opts.chat_timeout,
                    Role::Embed => opts.embed_timeout,
                });
            for a in 1..=attempts {
                let now = Instant::now();
                if now >= deadline_at {
                    return Err(self.deadline_error(role, deadline, &failures));
                }
                let timeout = rung_timeout.min(deadline_at - now);
                let t0 = Instant::now();
                let res = self.attempt(rung, op, timeout).await;
                let latency = ms(t0.elapsed());
                match res {
                    Ok((out, status)) => {
                        breakers.success(&bkey);
                        let (tin, tout) = out.tokens();
                        self.log_call(
                            rung,
                            role,
                            idx,
                            true,
                            Some(status),
                            latency,
                            tin,
                            tout,
                            None,
                        )
                        .await;
                        if idx > 0 {
                            tracing::info!(role = role.as_str(), rung = %label, fallbacks = idx, "model chain served by fallback rung");
                        }
                        return Ok((out, rung, idx, ms(started.elapsed()), idx));
                    }
                    Err(f) => {
                        self.log_call(
                            rung,
                            role,
                            idx,
                            false,
                            f.status,
                            latency,
                            None,
                            None,
                            Some(&f.msg),
                        )
                        .await;
                        if f.health {
                            breakers.failure(&bkey, &f.msg);
                        } else {
                            breakers.neutral(&bkey);
                        }
                        let can_retry = a < attempts && f.retryable && !breakers.is_open(&bkey);
                        if can_retry {
                            let exp = 1u32 << (a - 1).min(6);
                            let backoff = f
                                .retry_after
                                .unwrap_or_else(|| opts.retry_backoff.saturating_mul(exp))
                                .min(opts.max_backoff);
                            let left = deadline_at.saturating_duration_since(Instant::now());
                            tokio::time::sleep(backoff.min(left)).await;
                            continue;
                        }
                        tracing::warn!(
                            role = role.as_str(),
                            rung = %label,
                            error = %f.msg,
                            next = if idx + 1 < total { "falling back" } else { "no more rungs" },
                            "model rung failed"
                        );
                        failures.push(f.msg.clone());
                        break;
                    }
                }
            }
        }
        if Instant::now() >= deadline_at {
            return Err(self.deadline_error(role, deadline, &failures));
        }
        Err(AiError::AllFailed(format!(
            "all {total} {} rung(s) failed: {}",
            role.as_str(),
            failures.join(" | ")
        )))
    }

    fn deadline_error(&self, role: Role, deadline: Duration, failures: &[String]) -> AiError {
        AiError::DeadlineExceeded(format!(
            "{} call exceeded its {} ms deadline; failures so far: {}",
            role.as_str(),
            ms(deadline),
            if failures.is_empty() {
                "none".to_string()
            } else {
                failures.join(" | ")
            }
        ))
    }

    /// One attempt against one rung, including output validation.
    async fn attempt(
        &self,
        rung: &Rung,
        op: &Op<'_>,
        timeout: Duration,
    ) -> Result<(Outcome, u16), Failure> {
        let label = rung.label();
        match op {
            Op::Chat {
                messages,
                max_tokens,
                temperature,
                json_mode,
            } => {
                let req =
                    adapters::chat_request(rung, messages, *max_tokens, *temperature, *json_mode)
                        .map_err(|m| Failure::config(format!("{label}: {m}")))?;
                let (status, body) = http::send(&self.inner.client, &req, timeout)
                    .await
                    .map_err(|e| map_send_error(&label, e))?;
                let parsed = adapters::parse_chat(&rung.kind, &body);
                if parsed.text.trim().is_empty() {
                    return Err(match parsed.inband_error {
                        Some(e) => Failure::bad_output(
                            format!("{label}: provider error: {e}"),
                            status,
                            true,
                        ),
                        None => {
                            Failure::bad_output(format!("{label}: empty chat reply"), status, false)
                        }
                    });
                }
                Ok((Outcome::Chat(parsed), status))
            }
            Op::Embed { texts, policy } => {
                let calls = adapters::embed_requests(rung, texts)
                    .map_err(|m| Failure::config(format!("{label}: {m}")))?;
                let dim = match policy {
                    DimPolicy::Index(d) => *d,
                    DimPolicy::Model => rung
                        .dim
                        .and_then(|d| usize::try_from(d).ok())
                        .filter(|d| *d > 0)
                        .unwrap_or(EMBED_DIM),
                };
                let attempt_deadline = Instant::now() + timeout;
                let mut raw: Vec<Vec<f64>> = Vec::with_capacity(texts.len());
                let mut tokens: Option<i64> = None;
                let mut status = 200u16;
                for call in calls {
                    let left = attempt_deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(map_send_error(
                            &label,
                            SendError::Transport("timeout".into()),
                        ));
                    }
                    let (st, body) = http::send(&self.inner.client, &call.req, left)
                        .await
                        .map_err(|e| map_send_error(&label, e))?;
                    status = st;
                    let (vecs, t) = adapters::parse_embed(&rung.kind, &body, call.expected)
                        .map_err(|m| Failure::bad_output(format!("{label}: {m}"), st, true))?;
                    if let Some(t) = t {
                        tokens = Some(tokens.unwrap_or(0).saturating_add(t));
                    }
                    raw.extend(vecs);
                }
                if raw.len() != texts.len() {
                    return Err(Failure::bad_output(
                        format!(
                            "{label}: expected {} embeddings, got {}",
                            texts.len(),
                            raw.len()
                        ),
                        status,
                        true,
                    ));
                }
                let mut out = Vec::with_capacity(raw.len());
                for (i, v) in raw.iter().enumerate() {
                    let u = unit_vector(v, dim, &format!("{label}: embedding[{i}]"))
                        .map_err(|m| Failure::bad_output(m, status, true))?;
                    out.push(u);
                }
                Ok((Outcome::Embed(out, tokens), status))
            }
        }
    }

    /// Best-effort write of one attempt to `model_call_log`. Never fails the call.
    #[allow(clippy::too_many_arguments)]
    async fn log_call(
        &self,
        rung: &Rung,
        role: Role,
        idx: usize,
        ok: bool,
        status: Option<u16>,
        latency_ms: u64,
        tokens_in: Option<i64>,
        tokens_out: Option<i64>,
        error: Option<&str>,
    ) {
        let model_row_id = rung.model_row_id.clone();
        let chain_id = rung.chain_id.clone();
        let provider = rung.provider_name.clone();
        let error = error.map(|e| truncate(e, 500));
        let rung_idx = i64::try_from(idx).unwrap_or(i64::MAX);
        let latency = i64::try_from(latency_ms).unwrap_or(i64::MAX);
        let fut = self.inner.db.write(move |tx| {
            tx.execute(
                "INSERT INTO model_call_log
                   (model_row_id, chain_id, provider, role, rung, ok, status_code, latency_ms, tokens_in, tokens_out, error)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    model_row_id,
                    chain_id,
                    provider,
                    role.as_str(),
                    rung_idx,
                    ok,
                    status,
                    latency,
                    tokens_in,
                    tokens_out,
                    error
                ],
            )
            .map_err(sf_store::err)?;
            Ok(())
        });
        match tokio::time::timeout(Duration::from_secs(5), fut).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "model_call_log write failed"),
            Err(_) => tracing::warn!("model_call_log write timed out"),
        }
    }

    /// Deletes call-log rows older than `older_than_days`. Returns rows removed.
    pub async fn prune_call_log(&self, older_than_days: u32) -> Result<usize, AiError> {
        let modifier = format!("-{older_than_days} days");
        let n = self
            .inner
            .db
            .write(move |tx| {
                tx.execute(
                    "DELETE FROM model_call_log WHERE called_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?1)",
                    params![modifier],
                )
                .map_err(sf_store::err)
            })
            .await?;
        Ok(n)
    }

    /// Closes a rung's breaker (UI "retry now"). `key` = model row id, or `env:<model>`.
    pub fn reset_breaker(&self, key: &str) {
        self.inner.breakers.reset(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_vector_rules() {
        assert!(unit_vector(&[3.0, 4.0], 2, "v").is_ok());
        let u = unit_vector(&[3.0, 4.0], 2, "v").unwrap();
        assert!((u[0] - 0.6).abs() < 1e-6 && (u[1] - 0.8).abs() < 1e-6);
        assert!(unit_vector(&[1.0], 2, "v").is_err());
        assert!(unit_vector(&[0.0, 0.0], 2, "v").is_err());
        assert!(unit_vector(&[f64::NAN, 1.0], 2, "v").is_err());
        assert!(unit_vector(&[1e300, 1.0], 2, "v").is_err());
    }

    #[test]
    fn json_reply_parsing() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct A {
            a: i32,
        }
        assert_eq!(parse_json_reply::<A>("{\"a\":1}"), Ok(A { a: 1 }));
        assert_eq!(
            parse_json_reply::<A>("```json\n{\"a\":2}\n```"),
            Ok(A { a: 2 })
        );
        assert_eq!(
            parse_json_reply::<A>("Sure! Here: {\"a\":3} hope it helps"),
            Ok(A { a: 3 })
        );
        assert!(parse_json_reply::<A>("nope").is_err());
        assert!(parse_json_reply::<A>("}{").is_err());
    }
}
