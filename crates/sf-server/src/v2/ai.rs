//! v2.3 AI tiers, usage and the Link Run token estimate (`docs/specs/v2.3-ai-tiers.md` §6).
//!
//! * `GET /ai/usage?since=7d&until=&group=day|model|tier|task|site|provider&site_id=&run_id=`
//!   (admin) -> rows + totals + today's budget position.
//! * `GET /ai/tiers?site_id=` (admin) -> the effective chat chain per tier.
//! * `POST /sites/{id}/link-runs/estimate` (user, own site) -> estimated calls / tokens / cost of
//!   a Link Run with these options, and whether it fits today's budgets.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use rusqlite::types::Value as Sql;
use serde_json::{Map, Value, json};
use sf_core::config::{AiCfg, ai_tasks};
use sf_store::err;

use super::logs::time_arg;
use crate::auth::{Admin, User};
use crate::v1::common::{JsonBody, guard_user_site, is_uuid};
use crate::{ApiError, ApiResult, AppState};

const GROUPS: &[&str] = &["day", "model", "tier", "task", "site", "provider"];

fn group_expr(g: &str) -> &'static str {
    match g {
        "model" => "coalesce(model_id, provider, '-')",
        "tier" => "coalesce(tier, '-')",
        "task" => "coalesce(task, '-')",
        "site" => "coalesce(site_id, '-')",
        "provider" => "coalesce(provider, '-')",
        _ => "substr(called_at, 1, 10)",
    }
}

const AGG: &str = "sum(CASE WHEN cache_hit=0 THEN 1 ELSE 0 END),
     sum(CASE WHEN cache_hit=0 THEN items ELSE 0 END),
     coalesce(sum(tokens_in), 0), coalesce(sum(tokens_out), 0), coalesce(sum(cost_usd), 0),
     sum(CASE WHEN cache_hit=1 THEN items ELSE 0 END),
     sum(CASE WHEN ok=0 THEN 1 ELSE 0 END), sum(estimated)";

fn agg_json(r: &rusqlite::Row<'_>, off: usize) -> rusqlite::Result<Map<String, Value>> {
    let n =
        |i: usize| -> rusqlite::Result<i64> { Ok(r.get::<_, Option<i64>>(off + i)?.unwrap_or(0)) };
    let cost: f64 = r.get::<_, Option<f64>>(off + 4)?.unwrap_or(0.0);
    let mut m = Map::new();
    m.insert("calls".into(), json!(n(0)?));
    m.insert("items".into(), json!(n(1)?));
    m.insert("tokens_in".into(), json!(n(2)?));
    m.insert("tokens_out".into(), json!(n(3)?));
    m.insert("cost_usd".into(), json!((cost * 1e6).round() / 1e6));
    m.insert("cache_hits".into(), json!(n(5)?));
    m.insert("errors".into(), json!(n(6)?));
    m.insert("estimated".into(), json!(n(7)?));
    Ok(m)
}

/// Today's budget position for every tier plus the top sites (`budget` of `/ai/usage`).
pub fn budget_doc(c: &rusqlite::Connection, cfg: &AiCfg) -> sf_core::Result<Value> {
    let mut tiers = Map::new();
    for t in ai_tasks::TIERS {
        let used = sf_ai::accounting::tokens_today(c, Some(t), None).map_err(err)?;
        let limit = cfg.budget.daily_tokens.get(t);
        tiers.insert(
            (*t).to_string(),
            json!({
                "used": used,
                "limit": limit,
                "remaining": (limit > 0).then(|| limit.saturating_sub(used)),
                "exhausted": limit > 0 && used >= limit,
            }),
        );
    }
    let site_limit = cfg.budget.per_site_daily_tokens;
    let sql = format!(
        "SELECT l.site_id, coalesce(s.domain, ''), sum(coalesce(l.tokens_in,0) + coalesce(l.tokens_out,0)) AS used
           FROM model_call_log l LEFT JOIN sites s ON s.id = l.site_id
          WHERE l.called_at >= {} AND l.cache_hit = 0 AND l.site_id IS NOT NULL
          GROUP BY l.site_id ORDER BY used DESC, l.site_id LIMIT 10",
        sf_ai::accounting::DAY_START_SQL
    );
    let mut st = c.prepare(&sql).map_err(err)?;
    let sites = st
        .query_map([], |r| {
            let used: i64 = r.get(2)?;
            let used = u64::try_from(used).unwrap_or(0);
            Ok(json!({
                "site_id": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "used": used,
                "limit": site_limit,
                "exhausted": site_limit > 0 && used >= site_limit,
            }))
        })
        .map_err(err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(err)?;
    let day: String = sf_core::now_rfc3339().chars().take(10).collect();
    Ok(json!({"day": day, "tiers": tiers, "per_site_limit": site_limit, "sites": sites}))
}

/// `GET /ai/usage`.
pub async fn usage(
    State(st): State<AppState>,
    _a: Admin,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let group = q.get("group").map(String::as_str).unwrap_or("day");
    if !GROUPS.contains(&group) {
        return Err(ApiError::bad(format!(
            "group must be one of {}",
            GROUPS.join(", ")
        )));
    }
    let since = time_arg(
        &Some(q.get("since").cloned().unwrap_or_else(|| "7d".into())),
        "since",
    )?;
    let until = time_arg(&q.get("until").cloned(), "until")?;
    let mut wh = String::from(" WHERE 1");
    let mut args: Vec<Sql> = Vec::new();
    let mut bind = |frag: &str, v: Option<String>| {
        if let Some(v) = v {
            args.push(Sql::Text(v));
            wh.push_str(&frag.replace('?', &format!("?{}", args.len())));
        }
    };
    bind(" AND called_at >= ?", since.clone());
    bind(" AND called_at < ?", until.clone());
    for (k, col) in [("site_id", "site_id"), ("run_id", "run_id")] {
        if let Some(v) = q
            .get(k)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            if v.len() > 64 {
                return Err(ApiError::bad(format!("{k} is too long")));
            }
            bind(&format!(" AND {col} = ?"), Some(v));
        }
    }
    let expr = group_expr(group);
    let cfg = st.live.get().ai.clone();
    let is_site = group == "site";
    let doc = st
        .db
        .read(move |c| {
            let sql = format!(
                "SELECT {expr} AS k, {AGG} FROM model_call_log{wh} GROUP BY k ORDER BY k LIMIT 1000"
            );
            let mut stmt = c.prepare(&sql).map_err(err)?;
            let mut rows = stmt
                .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                    let mut m = agg_json(r, 1)?;
                    m.insert("key".into(), json!(r.get::<_, String>(0)?));
                    Ok(Value::Object(m))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            if is_site {
                for r in rows.iter_mut() {
                    let k = r["key"].as_str().unwrap_or("").to_string();
                    let name: Option<String> = c
                        .query_row("SELECT domain FROM sites WHERE id=?1", [&k], |x| x.get(0))
                        .ok();
                    if let (Some(n), Some(o)) = (name, r.as_object_mut()) {
                        o.insert("label".into(), json!(n));
                    }
                }
            }
            let tsql = format!("SELECT {AGG} FROM model_call_log{wh}");
            let mut totals = c
                .query_row(&tsql, rusqlite::params_from_iter(args.iter()), |r| {
                    agg_json(r, 0)
                })
                .map_err(err)?;
            let hits = totals["cache_hits"].as_i64().unwrap_or(0);
            let items = totals["items"].as_i64().unwrap_or(0);
            let rate = if hits + items > 0 {
                (hits as f64 / (hits + items) as f64 * 1000.0).round() / 1000.0
            } else {
                0.0
            };
            totals.insert("cache_hit_rate".into(), json!(rate));
            let budget = budget_doc(c, &cfg)?;
            Ok(json!({"rows": rows, "totals": totals, "budget": budget}))
        })
        .await?;
    let mut doc = doc;
    if let Some(o) = doc.as_object_mut() {
        o.insert("group".into(), json!(group));
        o.insert("since".into(), json!(since));
        o.insert("until".into(), json!(until));
    }
    Ok(Json(doc))
}

/// `GET /ai/tiers`.
pub async fn tiers(
    State(st): State<AppState>,
    _a: Admin,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let site = q.get("site_id").cloned().filter(|s| !s.is_empty());
    if let Some(s) = &site
        && !is_uuid(s)
    {
        return Err(ApiError::bad("site_id must be a uuid"));
    }
    let ai = match st.services.get::<sf_ai::Ai>() {
        Some(a) => (*a).clone(),
        None => sf_ai::Ai::with_live(st.db.clone(), st.secret_key.clone(), st.live.clone())
            .map_err(|e| ApiError::internal(e.to_string()))?,
    };
    let t = ai.tier_details(site.as_deref()).await;
    Ok(Json(json!({ "tiers": t })))
}

/// Per-item token averages of a task over the last 30 days (`None` = under 5 calls).
fn history(c: &rusqlite::Connection, task: &str) -> sf_core::Result<Option<(f64, f64, f64)>> {
    let r: (i64, i64, i64, i64, f64) = c
        .query_row(
            "SELECT count(*), coalesce(sum(items),0), coalesce(sum(tokens_in),0), coalesce(sum(tokens_out),0), coalesce(sum(cost_usd),0)
               FROM model_call_log
              WHERE task=?1 AND cache_hit=0 AND ok=1 AND called_at >= strftime('%Y-%m-%dT%H:%M:%fZ','now','-30 days')",
            [task],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .map_err(err)?;
    if r.0 < 5 || r.1 <= 0 {
        return Ok(None);
    }
    let items = r.1 as f64;
    Ok(Some((r.2 as f64 / items, r.3 as f64 / items, r.4 / items)))
}

/// Cache hit share of the last 30 days (0..1).
fn cache_rate(c: &rusqlite::Connection) -> sf_core::Result<f64> {
    let (hits, items): (i64, i64) = c
        .query_row(
            "SELECT coalesce(sum(CASE WHEN cache_hit=1 THEN items ELSE 0 END),0),
                    coalesce(sum(CASE WHEN cache_hit=0 THEN items ELSE 0 END),0)
               FROM model_call_log WHERE task IN ('anchor_pick','relevance_check')
                AND called_at >= strftime('%Y-%m-%dT%H:%M:%fZ','now','-30 days')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(err)?;
    Ok(if hits + items > 0 {
        hits as f64 / (hits + items) as f64
    } else {
        0.0
    })
}

/// Default per-item tokens `(in, out)` when there is no history.
fn default_tokens(task: &str) -> (f64, f64) {
    match task {
        ai_tasks::ANCHOR_PICK => (260.0, 30.0),
        ai_tasks::RELEVANCE_CHECK => (140.0, 12.0),
        _ => (600.0, 120.0),
    }
}

/// Share of run items per AI task when the AI is on (the rest is skipped as confident).
const ANCHOR_SHARE: f64 = 0.6;
const RELEVANCE_SHARE: f64 = 0.25;
/// Share of items that need an AI insertion when `contextual` is ticked.
const CONTEXTUAL_SHARE: f64 = 0.3;

/// `POST /sites/{id}/link-runs/estimate`.
pub async fn estimate(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &id).await?;
    let raw = match body.get("options") {
        Some(o) => Some(o.clone()),
        None if body.as_object().is_some_and(|o| !o.is_empty()) => Some(body.clone()),
        None => None,
    };
    let opts = sf_jobs::pipelines::link_run::parse_options(raw.as_ref()).map_err(ApiError::bad)?;
    let cfg = st.live.get().ai.clone();
    let sid = id.clone();
    let (posts, site_ai, site_hp, hist, rate, budget) = st
        .db
        .read({
            let cfg = cfg.clone();
            move |c| {
                let posts: i64 = c
                    .query_row("SELECT count(*) FROM posts WHERE site_id=?1", [&sid], |r| {
                        r.get(0)
                    })
                    .map_err(err)?;
                let settings: String = c
                    .query_row(
                        "SELECT coalesce(settings,'{}') FROM sites WHERE id=?1",
                        [&sid],
                        |r| r.get(0),
                    )
                    .unwrap_or_else(|_| "{}".into());
                let s: Value = serde_json::from_str(&settings).unwrap_or(json!({}));
                let flag = |k: &str| s[k].as_bool() == Some(true) || s[k].as_str() == Some("true");
                let mut hist = HashMap::new();
                for t in [
                    ai_tasks::ANCHOR_PICK,
                    ai_tasks::RELEVANCE_CHECK,
                    ai_tasks::CONTEXT_SENTENCE,
                ] {
                    hist.insert(t, history(c, t)?);
                }
                let rate = cache_rate(c)?;
                let mut budget = Map::new();
                for t in ai_tasks::TIERS {
                    let used = sf_ai::accounting::tokens_today(c, Some(t), None).map_err(err)?;
                    let lim = cfg.budget.daily_tokens.get(t);
                    budget.insert(
                        (*t).to_string(),
                        json!((lim > 0).then(|| lim.saturating_sub(used))),
                    );
                }
                let site_used =
                    sf_ai::accounting::tokens_today(c, None, Some(&sid)).map_err(err)?;
                let sl = cfg.budget.per_site_daily_tokens;
                budget.insert(
                    "site".into(),
                    json!((sl > 0).then(|| sl.saturating_sub(site_used))),
                );
                Ok((
                    posts,
                    flag("ai_anchor_refine"),
                    flag("ai_high_priority"),
                    hist,
                    rate,
                    budget,
                ))
            }
        })
        .await?;
    let ai_enabled = cfg.always_on || opts.ai_refine || site_ai;
    let high = opts.high_priority || site_hp;
    let f = &opts.filters;
    let items = (posts.max(0) as f64 * f.max_new_per_post as f64).min(f.max_total as f64);
    let items = items.max(0.0).round();
    let anchor_items = if ai_enabled {
        (items * ANCHOR_SHARE).round()
    } else {
        0.0
    };
    let rel_items = if ai_enabled {
        (items * RELEVANCE_SHARE).round()
    } else {
        0.0
    };
    let ctx_items = if opts.strategies.contextual {
        (items * CONTEXTUAL_SHARE).round()
    } else {
        0.0
    };
    let batch = cfg.batch() as f64;
    let mut by_tier: Map<String, Value> = Map::new();
    let mut basis_history = false;
    let (mut calls, mut tin, mut tout, mut cost) = (0f64, 0f64, 0f64, 0f64);
    let tier_of = |task: &str| -> String {
        if high {
            cfg.tier_for(ai_tasks::PRIORITY)
        } else {
            cfg.tier_for(task)
        }
    };
    for (task, n, batched) in [
        (ai_tasks::ANCHOR_PICK, anchor_items, true),
        (ai_tasks::RELEVANCE_CHECK, rel_items, true),
        (ai_tasks::CONTEXT_SENTENCE, ctx_items, false),
    ] {
        if n <= 0.0 {
            continue;
        }
        // cached items cost nothing (only the batched low-tier tasks are cached)
        let billable = if batched { n * (1.0 - rate) } else { n };
        let (pi, po, pc) = match hist.get(task).copied().flatten() {
            Some(h) => {
                basis_history = true;
                h
            }
            None => {
                let (a, b) = default_tokens(task);
                (a, b, 0.0)
            }
        };
        let c_calls = if batched {
            (billable / batch).ceil()
        } else {
            billable
        };
        let (ti, to, tc) = (billable * pi, billable * po, billable * pc);
        calls += c_calls;
        tin += ti;
        tout += to;
        cost += tc;
        let tier = tier_of(task);
        let e = by_tier
            .entry(tier)
            .or_insert_with(|| json!({"calls": 0, "tokens": 0, "cost_usd": 0.0}));
        e["calls"] = json!(e["calls"].as_f64().unwrap_or(0.0) + c_calls);
        e["tokens"] = json!(e["tokens"].as_f64().unwrap_or(0.0) + ti + to);
        e["cost_usd"] = json!(e["cost_usd"].as_f64().unwrap_or(0.0) + tc);
    }
    // whole numbers for counts
    for v in by_tier.values_mut() {
        v["calls"] = json!(v["calls"].as_f64().unwrap_or(0.0).round() as i64);
        v["tokens"] = json!(v["tokens"].as_f64().unwrap_or(0.0).round() as i64);
        v["cost_usd"] = json!((v["cost_usd"].as_f64().unwrap_or(0.0) * 1e6).round() / 1e6);
    }
    let total = (tin + tout).round();
    let mut within = true;
    for (tier, v) in &by_tier {
        if let Some(rem) = budget.get(tier).and_then(Value::as_f64)
            && v["tokens"].as_f64().unwrap_or(0.0) > rem
        {
            within = false;
        }
    }
    if let Some(rem) = budget.get("site").and_then(Value::as_f64)
        && total > rem
    {
        within = false;
    }
    let note = if !ai_enabled && ctx_items == 0.0 {
        "AI is off for this run (heuristics only): no tokens.".to_string()
    } else if within {
        "Estimate from counts x average tokens per item; cached items cost nothing.".to_string()
    } else {
        "This run may exceed today's AI budget: when it runs out the run continues with heuristics."
            .to_string()
    };
    Ok(Json(json!({
        "ai_enabled": ai_enabled,
        "always_on": cfg.always_on,
        "high_priority": high,
        "items_estimated": items as i64,
        "ai_items": {
            "anchor_pick": anchor_items as i64,
            "relevance_check": rel_items as i64,
            "contextual": ctx_items as i64,
        },
        "calls": calls.round() as i64,
        "tokens_in": tin.round() as i64,
        "tokens_out": tout.round() as i64,
        "tokens_total": total as i64,
        "cost_usd": (cost * 1e6).round() / 1e6,
        "cache_hit_rate": (rate * 1000.0).round() / 1000.0,
        "by_tier": by_tier,
        "budget": budget,
        "within_budget": within,
        "basis": if basis_history { "history" } else { "defaults" },
        "note": note,
    })))
}

/// `GET /ai/settings` (admin): the `ai` section of `GET /settings`.
pub async fn settings_get(State(st): State<AppState>, _a: Admin) -> ApiResult<Json<Value>> {
    let cfg = st.live.get();
    Ok(Json(super::settings::ai_document(&cfg.ai)))
}

/// `PUT /ai/settings` (admin): `PUT /settings {"ai": body}`; answers the new `ai` section plus
/// `changed`.
pub async fn settings_put(
    State(st): State<AppState>,
    a: Admin,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult<Json<Value>> {
    if !body.is_object() {
        return Err(ApiError::bad("body must be an object of ai settings"));
    }
    let Json(doc) = super::settings::put(State(st), a, JsonBody(json!({ "ai": body }))).await?;
    let mut out = doc.get("ai").cloned().unwrap_or(json!({}));
    if let (Some(o), Some(ch)) = (out.as_object_mut(), doc.get("changed")) {
        o.insert("changed".into(), ch.clone());
    }
    Ok(Json(out))
}
