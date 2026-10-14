//! v2.3 token accounting, daily budgets, the AI result cache and the observer hook.
//!
//! * Every chat attempt is a `model_call_log` row with task / tier / site / run, tokens (provider
//!   `usage`, else a chars/4 estimate flagged `estimated`) and `cost_usd` (`models.price_in/out`
//!   per 1M tokens).
//! * Budgets: tokens in + out of real calls since 00:00 UTC, per tier and per site; a limit of 0
//!   means unlimited. An exhausted budget fails the call before any provider is contacted.
//! * Cache: `ai_cache` keyed by sha256(task, model family, prompt-relevant inputs); hits are
//!   accounted as `cache_hit` rows with zero tokens.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use sf_store::rusqlite::{self, Connection, OptionalExtension, params};

use crate::adapters::ChatParsed;
use crate::resolve::DimPolicy;
use crate::types::{AiError, ChainSource, ChatMessage, Role, RungInfo, Tier};
use crate::{Ai, ms};

/// Accounting context of one call.
#[derive(Debug, Clone, PartialEq)]
pub struct AiCallMeta {
    pub task: String,
    pub tier: Tier,
    pub site_id: Option<String>,
    pub run_id: Option<String>,
    /// Edges carried by the call (batched) or cache hits served.
    pub items: u32,
}

impl AiCallMeta {
    pub fn new(task: &str, tier: Tier) -> Self {
        Self {
            task: task.to_string(),
            tier,
            site_id: None,
            run_id: None,
            items: 1,
        }
    }
}

/// One finished chat call (or cache-hit batch), for the event log.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AiCallEvent {
    pub task: String,
    pub tier: Tier,
    pub provider: String,
    pub model: String,
    pub ok: bool,
    pub error: Option<String>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub estimated: bool,
    pub cost_usd: f64,
    pub cache_hit: bool,
    pub items: u32,
    pub fallback_used: usize,
    pub rung: usize,
    pub latency_ms: u64,
    pub site_id: Option<String>,
    pub run_id: Option<String>,
}

impl AiCallEvent {
    pub(crate) fn failed(
        meta: &AiCallMeta,
        model: Option<&str>,
        error: &str,
        latency: u64,
    ) -> Self {
        Self {
            task: meta.task.clone(),
            tier: meta.tier,
            provider: String::new(),
            model: model.unwrap_or("none").to_string(),
            ok: false,
            error: Some(error.chars().take(300).collect()),
            tokens_in: 0,
            tokens_out: 0,
            estimated: false,
            cost_usd: 0.0,
            cache_hit: false,
            items: meta.items,
            fallback_used: 0,
            rung: 0,
            latency_ms: latency,
            site_id: meta.site_id.clone(),
            run_id: meta.run_id.clone(),
        }
    }
}

/// What the observer receives.
#[derive(Debug, Clone, PartialEq)]
pub enum AiEvent {
    Call(AiCallEvent),
    /// First exhaustion of a budget today (one per UTC day per tier / per site).
    BudgetExhausted {
        tier: Tier,
        /// `tier` | `site`
        scope: &'static str,
        site_id: Option<String>,
        used: u64,
        limit: u64,
    },
}

pub type AiObserver = Arc<dyn Fn(AiEvent) + Send + Sync>;

/// chars/4, rounded up (the usual rule of thumb for English text).
pub fn estimate_tokens(chars: usize) -> i64 {
    i64::try_from(chars.div_ceil(4)).unwrap_or(i64::MAX)
}

/// Tokens of a chat reply: provider usage when present, else chars/4 of prompt / reply.
pub(crate) fn chat_tokens(p: &ChatParsed, messages: &[ChatMessage]) -> (i64, i64, bool) {
    let mut estimated = false;
    let tin = p.tokens_in.filter(|t| *t >= 0).unwrap_or_else(|| {
        estimated = true;
        let chars: usize = messages
            .iter()
            .map(|m| m.content.chars().count())
            .fold(0usize, usize::saturating_add);
        estimate_tokens(chars)
    });
    let tout = p.tokens_out.filter(|t| *t >= 0).unwrap_or_else(|| {
        estimated = true;
        estimate_tokens(p.text.chars().count())
    });
    (tin, tout, estimated)
}

/// USD from per-1M prices (0 when unpriced).
pub fn cost(price_in: Option<f64>, price_out: Option<f64>, tin: i64, tout: i64) -> f64 {
    let c = price_in.unwrap_or(0.0) * tin.max(0) as f64 / 1e6
        + price_out.unwrap_or(0.0) * tout.max(0) as f64 / 1e6;
    if c.is_finite() && c > 0.0 {
        (c * 1e8).round() / 1e8
    } else {
        0.0
    }
}

/// `called_at` lower bound of the current UTC day.
pub const DAY_START_SQL: &str = "strftime('%Y-%m-%dT00:00:00.000Z','now')";

/// Tokens (in + out, real calls only) used today, optionally for one tier and/or one site.
pub fn tokens_today(
    conn: &Connection,
    tier: Option<&str>,
    site_id: Option<&str>,
) -> rusqlite::Result<u64> {
    let sql = format!(
        "SELECT coalesce(sum(coalesce(tokens_in,0) + coalesce(tokens_out,0)), 0) FROM model_call_log
          WHERE called_at >= {DAY_START_SQL} AND cache_hit = 0
            AND (?1 IS NULL OR tier = ?1) AND (?2 IS NULL OR site_id = ?2)"
    );
    let n: i64 = conn.query_row(&sql, params![tier, site_id], |r| r.get(0))?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// v2.4: tokens (non-cache) used this UTC month by every site of one workspace.
pub fn workspace_tokens_month(conn: &Connection, workspace_id: &str) -> rusqlite::Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT coalesce(sum(coalesce(l.tokens_in,0) + coalesce(l.tokens_out,0)), 0)
           FROM model_call_log l
          WHERE l.cache_hit = 0
            AND l.called_at >= strftime('%Y-%m-01T00:00:00.000Z','now')
            AND l.site_id IN (SELECT id FROM sites WHERE workspace_id = ?1)",
        params![workspace_id],
        |r| r.get(0),
    )?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// v2.4 monthly AI token quota of the workspace owning `site_id`: `(workspace_id, used, limit)`
/// when the plan has a limit. `None` = unlimited, unknown site, legacy site without a workspace,
/// or a lookup error (fail open).
pub fn workspace_quota(conn: &Connection, site_id: &str) -> Option<(String, u64, u64)> {
    let row: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT w.id, json_extract(p.limits, '$.ai_tokens_per_month')
               FROM sites s JOIN workspaces w ON w.id = s.workspace_id
               JOIN plans p ON p.id = w.plan_id
              WHERE s.id = ?1",
            params![site_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let (ws, limit) = row?;
    let limit = u64::try_from(limit?).ok()?;
    let used = workspace_tokens_month(conn, &ws).ok()?;
    Some((ws, used, limit))
}

/// Today's budget position for one tier (+ site).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BudgetState {
    pub tier: Tier,
    pub tier_used: u64,
    /// 0 = unlimited.
    pub tier_limit: u64,
    pub site_id: Option<String>,
    pub site_used: u64,
    pub site_limit: u64,
}

impl BudgetState {
    /// `Some("tier" | "site")` when a limit is reached.
    pub fn exhausted(&self) -> Option<&'static str> {
        if self.tier_limit > 0 && self.tier_used >= self.tier_limit {
            return Some("tier");
        }
        if self.site_id.is_some() && self.site_limit > 0 && self.site_used >= self.site_limit {
            return Some("site");
        }
        None
    }
    pub fn tier_remaining(&self) -> Option<u64> {
        (self.tier_limit > 0).then(|| self.tier_limit.saturating_sub(self.tier_used))
    }
    pub fn site_remaining(&self) -> Option<u64> {
        (self.site_limit > 0 && self.site_id.is_some())
            .then(|| self.site_limit.saturating_sub(self.site_used))
    }
}

/// The effective chat chain of one tier (`GET /api/v2/ai/tiers`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TierDetail {
    pub tier: Tier,
    pub chain_id: Option<String>,
    pub chain_name: Option<String>,
    pub source: ChainSource,
    /// `tier` (own chain) | `main` (fell back to the main chain) | `env` | `none`.
    pub resolved_from: String,
    pub reasoning_effort: String,
    pub rungs: Vec<RungInfo>,
}

/// Strips a trailing date/version stamp (`-20251001`, `@2025-10-01`) so a model refresh keeps
/// its cache family.
fn family_of(model_id: &str) -> String {
    let m = model_id.trim().to_ascii_lowercase();
    let m = m.split('@').next().unwrap_or(&m).to_string();
    let parts: Vec<&str> = m.split('-').collect();
    match parts.last() {
        Some(last)
            if parts.len() > 1 && last.len() >= 8 && last.chars().all(|c| c.is_ascii_digit()) =>
        {
            parts[..parts.len() - 1].join("-")
        }
        _ => m,
    }
}

impl Ai {
    /// Today's budget for `tier` (and `site_id`).
    pub async fn budget(&self, tier: Tier, site_id: Option<&str>) -> BudgetState {
        let cfg = self.ai_config();
        let tier_limit = cfg.budget.daily_tokens.get(tier.as_str());
        let site_limit = cfg.budget.per_site_daily_tokens;
        let site = site_id.map(str::to_string);
        let need_tier = tier_limit > 0;
        let need_site = site_limit > 0 && site.is_some();
        let mut st = BudgetState {
            tier,
            tier_used: 0,
            tier_limit,
            site_id: site.clone(),
            site_used: 0,
            site_limit,
        };
        if !need_tier && !need_site {
            return st;
        }
        let t = tier.as_str();
        let res = self
            .db()
            .read(move |c| {
                let tu = if need_tier {
                    tokens_today(c, Some(t), None).map_err(sf_store::err)?
                } else {
                    0
                };
                let su = if need_site {
                    tokens_today(c, None, site.as_deref()).map_err(sf_store::err)?
                } else {
                    0
                };
                Ok((tu, su))
            })
            .await;
        match res {
            Ok((tu, su)) => {
                st.tier_used = tu;
                st.site_used = su;
            }
            // fail open: accounting trouble never blocks linking
            Err(e) => tracing::warn!(error = %e, "ai budget lookup failed"),
        }
        st
    }

    /// Builds the error for an exhausted budget and notifies once per UTC day per scope.
    pub(crate) fn budget_exhausted(
        &self,
        meta: &AiCallMeta,
        b: &BudgetState,
        scope: &'static str,
    ) -> AiError {
        let (used, limit, what) = if scope == "site" {
            (
                b.site_used,
                b.site_limit,
                format!("site {}", b.site_id.as_deref().unwrap_or("?")),
            )
        } else {
            (
                b.tier_used,
                b.tier_limit,
                format!("tier {}", b.tier.as_str()),
            )
        };
        let day: String = sf_core::now_rfc3339().chars().take(10).collect();
        let key = match scope {
            "site" => format!("{day}:site:{}", b.site_id.as_deref().unwrap_or("")),
            _ => format!("{day}:tier:{}", b.tier.as_str()),
        };
        let first = {
            let mut seen = self.inner.budget_notified.lock();
            if seen.len() > 4096 {
                seen.clear();
            }
            seen.insert(key)
        };
        let mut ev = AiCallEvent::failed(meta, None, "budget exhausted", 0);
        ev.provider = "budget".into();
        self.emit(AiEvent::Call(ev));
        if first {
            tracing::warn!(scope, used, limit, task = %meta.task, "AI daily token budget exhausted; heuristics only");
            self.emit(AiEvent::BudgetExhausted {
                tier: b.tier,
                scope,
                site_id: b.site_id.clone(),
                used,
                limit,
            });
        }
        AiError::BudgetExhausted(format!(
            "daily token budget of {what} used up ({used} of {limit} tokens)"
        ))
    }

    /// Cache key: sha256 over the task, the model family and the prompt-relevant inputs.
    pub fn cache_key(task: &str, family: &str, parts: &[&str]) -> String {
        let mut s = String::with_capacity(256);
        s.push_str("v1\u{1f}");
        s.push_str(task);
        s.push('\u{1f}');
        s.push_str(family);
        for p in parts {
            s.push('\u{1f}');
            s.push_str(p);
        }
        sf_core::crypto::sha256_hex(s.as_bytes())
    }

    /// Model family of the chain serving `tier` (first usable rung), e.g. `gpt-5.6-luna`.
    pub async fn model_family(&self, tier: Tier, site_id: Option<&str>) -> String {
        let r = self
            .resolve_tier(Role::Chat, tier, site_id, DimPolicy::Model)
            .await;
        r.rungs
            .first()
            .map(|x| family_of(&x.model_id))
            .unwrap_or_else(|| "none".into())
    }

    /// A cached result (None when absent, expired, or the cache is off).
    pub async fn cache_get(&self, key: &str) -> Option<Value> {
        if self.ai_config().cache_ttl_days == 0 {
            return None;
        }
        let k = key.to_string();
        let fut = self.db().write(move |t| {
            let v: Option<String> = t
                .query_row(
                    "SELECT result FROM ai_cache WHERE key=?1 AND expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')",
                    [&k],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sf_store::err)?;
            if v.is_some() {
                t.execute("UPDATE ai_cache SET hits = hits + 1 WHERE key=?1", [&k])
                    .map_err(sf_store::err)?;
            }
            Ok(v)
        });
        match tokio::time::timeout(Duration::from_secs(5), fut).await {
            Ok(Ok(Some(s))) => serde_json::from_str(&s).ok(),
            Ok(Ok(None)) => None,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "ai cache read failed");
                None
            }
            Err(_) => None,
        }
    }

    /// Stores a result for `cache_ttl_days` (no-op when the cache is off).
    pub async fn cache_put(&self, key: &str, task: &str, family: &str, value: &Value) {
        let ttl = self.ai_config().cache_ttl_days;
        if ttl == 0 {
            return;
        }
        let text = value.to_string();
        if text.len() > 64 * 1024 {
            return;
        }
        let (k, t, f) = (key.to_string(), task.to_string(), family.to_string());
        let modifier = format!("+{ttl} days");
        let fut = self.db().write(move |tx| {
            tx.execute(
                "INSERT INTO ai_cache(key, task, family, result, expires_at)
                 VALUES(?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%fZ','now', ?5))
                 ON CONFLICT(key) DO UPDATE SET result=excluded.result, expires_at=excluded.expires_at,
                   task=excluded.task, family=excluded.family",
                params![k, t, f, text, modifier],
            )
            .map_err(sf_store::err)?;
            Ok(())
        });
        match tokio::time::timeout(Duration::from_secs(5), fut).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "ai cache write failed"),
            Err(_) => tracing::warn!("ai cache write timed out"),
        }
    }

    /// Accounts `n` cache hits (one `model_call_log` row with `cache_hit=1`, tokens 0) and emits
    /// one event.
    pub async fn record_cache_hits(&self, meta: &AiCallMeta, family: &str, n: u32) {
        if n == 0 {
            return;
        }
        let m = meta.clone();
        let fam = family.to_string();
        let fut = self.db().write(move |tx| {
            tx.execute(
                "INSERT INTO model_call_log
                   (provider, role, rung, ok, latency_ms, tokens_in, tokens_out, task, tier, site_id, run_id,
                    cache_hit, cost_usd, estimated, items, model_id)
                 VALUES ('cache', 'chat', 0, 1, 0, 0, 0, ?1, ?2, ?3, ?4, 1, 0, 0, ?5, ?6)",
                params![m.task, m.tier.as_str(), m.site_id, m.run_id, i64::from(n), fam],
            )
            .map_err(sf_store::err)?;
            Ok(())
        });
        match tokio::time::timeout(Duration::from_secs(5), fut).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "cache-hit accounting failed"),
            Err(_) => tracing::warn!("cache-hit accounting timed out"),
        }
        self.emit(AiEvent::Call(AiCallEvent {
            task: meta.task.clone(),
            tier: meta.tier,
            provider: "cache".into(),
            model: family.to_string(),
            ok: true,
            error: None,
            tokens_in: 0,
            tokens_out: 0,
            estimated: false,
            cost_usd: 0.0,
            cache_hit: true,
            items: n,
            fallback_used: 0,
            rung: 0,
            latency_ms: 0,
            site_id: meta.site_id.clone(),
            run_id: meta.run_id.clone(),
        }));
    }

    /// Removes expired cache rows. Returns rows removed.
    pub async fn prune_cache(&self) -> Result<usize, AiError> {
        let n = self
            .db()
            .write(|t| {
                t.execute(
                    "DELETE FROM ai_cache WHERE expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now')",
                    [],
                )
                .map_err(sf_store::err)
            })
            .await?;
        Ok(n)
    }

    /// The effective chat chain per tier (for the UI and MCP).
    pub async fn tier_details(&self, site_id: Option<&str>) -> Vec<TierDetail> {
        let cfg = self.ai_config();
        let mut out = Vec::new();
        for tier in Tier::ALL {
            let t0 = std::time::Instant::now();
            let r = self
                .resolve_tier(Role::Chat, tier, site_id, DimPolicy::Model)
                .await;
            let resolved_from = match (&r.chain_id, r.source) {
                (Some(_), _) if r.tier == tier => "tier",
                (Some(_), _) => "main",
                (None, ChainSource::Env) => "env",
                _ => "none",
            }
            .to_string();
            let mut rungs: Vec<RungInfo> =
                r.rungs.iter().map(|x| self.rung_info(x, None)).collect();
            rungs.extend(
                r.skipped
                    .iter()
                    .map(|(x, why)| self.rung_info(x, Some(why.clone()))),
            );
            tracing::trace!(tier = tier.as_str(), ms = ms(t0.elapsed()), "tier resolved");
            out.push(TierDetail {
                tier,
                chain_id: r.chain_id,
                chain_name: r.chain_name,
                source: r.source,
                resolved_from,
                reasoning_effort: cfg.reasoning_effort.get(tier.as_str()),
                rungs,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_and_costs() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 1);
        assert_eq!(estimate_tokens(8), 2);
        assert_eq!(estimate_tokens(9), 3);
        assert_eq!(cost(None, None, 1000, 1000), 0.0);
        assert!((cost(Some(1.0), Some(2.0), 1_000_000, 500_000) - 2.0).abs() < 1e-9);
        assert_eq!(cost(Some(f64::NAN), None, 10, 10), 0.0);
        assert_eq!(cost(Some(1.0), Some(1.0), -5, -5), 0.0);
    }

    #[test]
    fn chat_tokens_prefers_usage() {
        let p = ChatParsed {
            text: "abcdefgh".into(),
            tokens_in: Some(11),
            tokens_out: Some(3),
            inband_error: None,
        };
        assert_eq!(chat_tokens(&p, &[ChatMessage::user("x")]), (11, 3, false));
        let p = ChatParsed {
            text: "abcdefgh".into(),
            tokens_in: None,
            tokens_out: None,
            inband_error: None,
        };
        assert_eq!(
            chat_tokens(&p, &[ChatMessage::user("0123456789abcdef")]),
            (4, 2, true)
        );
    }

    #[test]
    fn budget_exhaustion_rules() {
        let mut b = BudgetState {
            tier: Tier::Low,
            tier_used: 10,
            tier_limit: 0,
            site_id: Some("s".into()),
            site_used: 5,
            site_limit: 5,
        };
        assert_eq!(b.exhausted(), Some("site"));
        assert_eq!(b.tier_remaining(), None);
        assert_eq!(b.site_remaining(), Some(0));
        b.site_limit = 0;
        assert_eq!(b.exhausted(), None);
        b.tier_limit = 10;
        assert_eq!(b.exhausted(), Some("tier"));
        b.tier_limit = 11;
        assert_eq!(b.exhausted(), None);
        assert_eq!(b.tier_remaining(), Some(1));
    }

    #[test]
    fn cache_keys_and_families() {
        let a = Ai::cache_key("anchor_pick", "gpt-5.6-luna", &["p", "t"]);
        assert_eq!(a, Ai::cache_key("anchor_pick", "gpt-5.6-luna", &["p", "t"]));
        assert_ne!(a, Ai::cache_key("anchor_pick", "gpt-5.6-luna", &["pt"]));
        assert_ne!(
            a,
            Ai::cache_key("relevance_check", "gpt-5.6-luna", &["p", "t"])
        );
        assert_ne!(
            a,
            Ai::cache_key("anchor_pick", "claude-haiku-4-5", &["p", "t"])
        );
        assert_eq!(family_of("claude-haiku-4-5-20251001"), "claude-haiku-4-5");
        assert_eq!(family_of("GPT-5.6-Luna"), "gpt-5.6-luna");
        assert_eq!(family_of("model@2025-10-01"), "model");
        assert_eq!(family_of("x"), "x");
    }
}
