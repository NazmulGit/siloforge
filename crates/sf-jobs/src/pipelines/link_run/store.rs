//! Link-run SQL: run rows, live stats + progress, items (flat or grouped by post), selection,
//! item edits and the pre-run opportunities overview. Shared by the pipeline and the API.

use std::collections::{BTreeMap, HashMap};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use sf_core::{Error, Result};
use sf_store::err;

use super::fragment;

/// Statuses in which a run has (or waits for) a job working on it.
pub const ACTIVE: &[&str] = &["draft", "crawling", "planning", "deploying"];

/// Item status as the API reports it (§3), from the edge columns.
pub const ITEM_STATUS_SQL: &str = "CASE
   WHEN e.status='suggested' AND e.run_selected=1 THEN 'selected'
   WHEN e.status='suggested' THEN 'suggested'
   WHEN e.status='approved' AND e.failure_kind='content_gap' THEN 'failed'
   WHEN e.status='approved' THEN 'selected'
   WHEN e.status='applied' AND e.verify_present=0 THEN 'not_present'
   WHEN e.status='applied' THEN 'applied'
   WHEN e.status='failed' AND e.failure_kind='skipped' THEN 'skipped'
   WHEN e.status='failed' THEN 'failed'
   WHEN e.status='rejected' THEN 'skipped'
   ELSE e.status END";

const STRATEGY_SQL: &str = "coalesce(json_extract(CASE WHEN json_valid(e.score_breakdown) THEN e.score_breakdown ELSE '{}' END, '$.strategy'), 'semantic')";

pub const ITEM_STATUSES: &[&str] = &[
    "suggested",
    "selected",
    "applied",
    "failed",
    "not_present",
    "skipped",
    "reverted",
];

const RUN_COLS: &str = "id, site_id, name, status, options, stats, error, crawl_job_id, plan_job_id, deploy_job_id,
   repair_job_id, verify_job_id, undo_job_id, current_job_id, created_by, created_at, updated_at, deployed_at, auto,
   coalesce((SELECT coalesce(nullif(u.name,''), u.email) FROM users u WHERE 'user:' || u.id = link_runs.created_by),
            CASE WHEN created_by LIKE 'token:%' THEN 'token ' || substr(created_by, 7) END)";

fn parse_json(s: Option<String>) -> Value {
    s.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn row_to_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, String>(0)?,
        "site_id": r.get::<_, String>(1)?,
        "name": r.get::<_, String>(2)?,
        "status": r.get::<_, String>(3)?,
        "options": parse_json(r.get(4)?),
        "stats": parse_json(r.get(5)?),
        "error": r.get::<_, Option<String>>(6)?,
        "crawl_job_id": r.get::<_, Option<String>>(7)?,
        "plan_job_id": r.get::<_, Option<String>>(8)?,
        "deploy_job_id": r.get::<_, Option<String>>(9)?,
        "repair_job_id": r.get::<_, Option<String>>(10)?,
        "verify_job_id": r.get::<_, Option<String>>(11)?,
        "undo_job_id": r.get::<_, Option<String>>(12)?,
        "current_job_id": r.get::<_, Option<String>>(13)?,
        "created_by": r.get::<_, Option<String>>(14)?,
        "created_at": r.get::<_, String>(15)?,
        "updated_at": r.get::<_, String>(16)?,
        "deployed_at": r.get::<_, Option<String>>(17)?,
        "auto": r.get::<_, i64>(18)? != 0,
        // display name for "By" (created_by is an actor id such as `user:<uuid>`)
        "created_by_name": r.get::<_, Option<String>>(19)?,
    }))
}

/// The bare run row (`None` = no such run).
pub fn run_row(c: &Connection, id: &str) -> Result<Option<Value>> {
    c.query_row(
        &format!("SELECT {RUN_COLS} FROM link_runs WHERE id=?1"),
        [id],
        row_to_run,
    )
    .optional()
    .map_err(err)
}

/// Site of a run (ownership checks).
pub fn run_site(c: &Connection, id: &str) -> Result<Option<String>> {
    c.query_row("SELECT site_id FROM link_runs WHERE id=?1", [id], |r| {
        r.get(0)
    })
    .optional()
    .map_err(err)
}

/// Live stats of a run (computed from its edges).
pub fn compute_stats(c: &Connection, run_id: &str) -> Result<Value> {
    let sql = format!(
        "SELECT {STRATEGY_SQL} AS strategy, {ITEM_STATUS_SQL} AS st, e.run_selected,
            coalesce(json_extract(CASE WHEN json_valid(e.score_breakdown) THEN e.score_breakdown ELSE '{{}}' END, '$.repaired'), 0)
         FROM link_edges e WHERE e.run_id=?1"
    );
    let mut st = c.prepare(&sql).map_err(err)?;
    let rows = st
        .query_map([run_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
            ))
        })
        .map_err(err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(err)?;
    let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
    for k in ITEM_STATUSES {
        counts.insert(k, 0);
    }
    let mut by: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    let (mut selected, mut repaired) = (0i64, 0i64);
    for (strategy, status, sel, rep) in &rows {
        if let Some(n) = counts.get_mut(status.as_str()) {
            *n += 1;
        }
        if *sel != 0 {
            selected += 1;
        }
        if *rep != 0 && status == "applied" {
            repaired += 1;
        }
        let e = by.entry(strategy.clone()).or_default();
        let bump = |m: &mut Map<String, Value>, k: &str| {
            let v = m.get(k).and_then(Value::as_i64).unwrap_or(0) + 1;
            m.insert(k.to_string(), json!(v));
        };
        bump(e, "total");
        if *sel != 0 {
            bump(e, "selected");
        }
        if status != "selected" {
            bump(e, status);
        }
    }
    let n = |k: &str| counts.get(k).copied().unwrap_or(0);
    Ok(json!({
        "opportunities": rows.len(),
        "selected": selected,
        "applied": n("applied"),
        "failed": n("failed"),
        "not_present": n("not_present"),
        "repaired": repaired,
        "skipped": n("skipped"),
        "reverted": n("reverted"),
        "pending": n("selected"),
        "suggested": n("suggested"),
        "by_strategy": by,
    }))
}

/// Run with live stats and `progress {phase, pct}`.
pub fn run_json(c: &Connection, id: &str) -> Result<Option<Value>> {
    let Some(mut run) = run_row(c, id)? else {
        return Ok(None);
    };
    let stats = compute_stats(c, id)?;
    let status = run["status"].as_str().unwrap_or("").to_string();
    let job: Option<(String, Value)> = match run["current_job_id"].as_str() {
        Some(j) => c
            .query_row("SELECT status, stats FROM job_runs WHERE id=?1", [j], |r| {
                Ok((r.get::<_, String>(0)?, parse_json(r.get(1)?)))
            })
            .optional()
            .map_err(err)?,
        None => None,
    };
    let progress = match (&job, status.as_str()) {
        (Some((js, st)), _) if js == "running" => {
            let p = &st["progress"];
            json!({
                "phase": p["phase"].as_str().unwrap_or(&status),
                "pct": p["pct"].as_i64().unwrap_or(0),
                "step": st["link_run_step"].as_str().unwrap_or(&status),
                "job_status": js,
            })
        }
        (Some((js, _)), _) if js == "queued" => {
            json!({"phase": "queued", "pct": 0, "step": status, "job_status": js})
        }
        (_, "draft") => json!({"phase": "queued", "pct": 0}),
        (_, s) if ACTIVE.contains(&s) => json!({"phase": s, "pct": 0}),
        (_, s) => json!({"phase": s, "pct": 100}),
    };
    let ai = ai_summary(c, id)?;
    if let Some(o) = run.as_object_mut() {
        o.insert("stats".into(), stats);
        o.insert("progress".into(), progress);
        o.insert("ai".into(), ai);
        o.insert(
            "job_running".into(),
            json!(
                job.as_ref()
                    .is_some_and(|(s, _)| s == "running" || s == "queued")
            ),
        );
    }
    Ok(Some(run))
}

/// v2.3 AI summary of a run: accounting rows by `run_id` + the anchor stage summary in kv
/// `ai_run:<id>` (asked, skipped_confident, fallback, ...).
pub fn ai_summary(c: &Connection, run_id: &str) -> Result<Value> {
    let mut st = c
        .prepare(
            "SELECT coalesce(task,'-'),
                    sum(CASE WHEN cache_hit=0 THEN 1 ELSE 0 END),
                    sum(CASE WHEN cache_hit=0 THEN items ELSE 0 END),
                    coalesce(sum(tokens_in),0), coalesce(sum(tokens_out),0), coalesce(sum(cost_usd),0),
                    sum(CASE WHEN cache_hit=1 THEN items ELSE 0 END),
                    sum(CASE WHEN ok=0 THEN 1 ELSE 0 END), sum(estimated)
               FROM model_call_log WHERE run_id=?1 GROUP BY task ORDER BY task",
        )
        .map_err(err)?;
    let rows = st
        .query_map([run_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                [
                    r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                ],
                r.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
            ))
        })
        .map_err(err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(err)?;
    let mut tot = [0i64; 7];
    let mut cost = 0f64;
    let mut by_task = serde_json::Map::new();
    for (task, n, c_usd) in &rows {
        for (t, x) in tot.iter_mut().zip(n.iter()) {
            *t += x;
        }
        cost += c_usd;
        by_task.insert(
            task.clone(),
            json!({"calls": n[0], "tokens_in": n[2], "tokens_out": n[3], "cache_hits": n[4]}),
        );
    }
    let kv: Value = c
        .query_row(
            "SELECT value FROM kv WHERE key=?1",
            [format!("ai_run:{run_id}")],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(err)?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    Ok(json!({
        "calls": tot[0],
        "items": tot[1],
        "tokens_in": tot[2],
        "tokens_out": tot[3],
        "cost_usd": (cost * 1e6).round() / 1e6,
        "cache_hits": tot[4],
        "errors": tot[5],
        "estimated": tot[6],
        "by_task": by_task,
        "asked": kv.get("asked").cloned().unwrap_or(Value::Null),
        "skipped_confident": kv.get("skipped_confident").cloned().unwrap_or(Value::Null),
        "fallback": kv.get("fallback").cloned().unwrap_or(Value::Null),
    }))
}

pub fn list_runs(c: &Connection, site_id: &str, limit: i64) -> Result<Vec<Value>> {
    let mut st = c
        .prepare(
            "SELECT id FROM link_runs WHERE site_id=?1 ORDER BY created_at DESC, id DESC LIMIT ?2",
        )
        .map_err(err)?;
    let ids = st
        .query_map(params![site_id, limit], |r| r.get::<_, String>(0))
        .map_err(err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(err)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(r) = run_json(c, &id)? {
            out.push(r);
        }
    }
    Ok(out)
}

/// A run with a job in flight on this site (one at a time per site).
pub fn active_run(c: &Connection, site_id: &str) -> Result<Option<String>> {
    c.query_row(
        "SELECT id FROM link_runs WHERE site_id=?1 AND status IN ('draft','crawling','planning','deploying')
         ORDER BY created_at DESC LIMIT 1",
        [site_id],
        |r| r.get(0),
    )
    .optional()
    .map_err(err)
}

/// A run stuck in an active status whose job ended (failed / cancelled / reaped / vanished) is
/// moved to `failed` / `cancelled`. Returns true when the row changed.
pub fn reconcile(t: &Connection, run_id: &str) -> Result<bool> {
    let row: Option<(String, Option<String>)> = t
        .query_row(
            "SELECT status, current_job_id FROM link_runs WHERE id=?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(err)?;
    let Some((status, job)) = row else {
        return Ok(false);
    };
    if !ACTIVE.contains(&status.as_str()) {
        return Ok(false);
    }
    let Some(job) = job else {
        return Ok(false);
    };
    let js: Option<(String, Option<String>, Option<String>)> = t
        .query_row(
            "SELECT status, error, cancel_requested_at FROM job_runs WHERE id=?1",
            [&job],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(err)?;
    let (new, msg) = match js {
        None => ("failed", Some("the run's job no longer exists".to_string())),
        Some((s, e, _)) if s == "failed" => ("failed", e),
        Some((s, _, _)) if s == "cancelled" => ("cancelled", None),
        // finished without moving the run on (should not happen): report it as failed
        Some((s, _, _)) if s == "done" => (
            "failed",
            Some("the run's job ended without finishing the run".into()),
        ),
        _ => return Ok(false),
    };
    let n = t
        .execute(
            "UPDATE link_runs SET status=?2, error=coalesce(?3, error), updated_at=?4
             WHERE id=?1 AND status IN ('draft','crawling','planning','deploying')",
            params![run_id, new, msg, sf_core::now_rfc3339()],
        )
        .map_err(err)?;
    Ok(n > 0)
}

// ---------------------------------------------------------------------------------------------
// items
// ---------------------------------------------------------------------------------------------

/// `GET /link-runs/{id}/items` filters.
#[derive(Debug, Clone, Default)]
pub struct ItemQuery {
    pub status: Option<String>,
    pub post_id: Option<String>,
    pub strategy: Option<String>,
    pub q: Option<String>,
    /// One item only (the PATCH response).
    pub edge_id: Option<String>,
    pub limit: i64,
    pub offset: i64,
    pub group_by_post: bool,
}

fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('%');
    for ch in s.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

fn item_where(q: &ItemQuery, params: &mut Vec<String>) -> String {
    let mut w = String::from("e.run_id=?1");
    if let Some(id) = &q.edge_id {
        params.push(id.clone());
        w.push_str(&format!(" AND e.id=?{}", params.len()));
    }
    if let Some(s) = &q.status {
        params.push(s.clone());
        w.push_str(&format!(" AND ({ITEM_STATUS_SQL})=?{}", params.len()));
    }
    if let Some(p) = &q.post_id {
        params.push(p.clone());
        w.push_str(&format!(" AND e.source_post_id=?{}", params.len()));
    }
    if let Some(s) = &q.strategy {
        params.push(s.clone());
        let n = params.len();
        w.push_str(&format!(
            " AND ({STRATEGY_SQL}=?{n} OR EXISTS (SELECT 1 FROM json_each(CASE WHEN json_valid(e.score_breakdown) THEN json_extract(e.score_breakdown,'$.strategies') END) j WHERE j.value=?{n}))"
        ));
    }
    if let Some(text) = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        params.push(like_escape(&text.to_lowercase()));
        let n = params.len();
        w.push_str(&format!(
            " AND (lower(e.anchor) LIKE ?{n} ESCAPE '\\' OR lower(s.title) LIKE ?{n} ESCAPE '\\' OR lower(t.title) LIKE ?{n} ESCAPE '\\' OR lower(s.url) LIKE ?{n} ESCAPE '\\' OR lower(t.url) LIKE ?{n} ESCAPE '\\')"
        ));
    }
    w
}

/// True when `run.options.strategies.contextual` is set (AI may place anchors that are not in
/// the paragraph).
pub fn run_contextual(c: &Connection, run_id: &str) -> Result<bool> {
    Ok(c.query_row(
        "SELECT coalesce(json_extract(CASE WHEN json_valid(options) THEN options ELSE '{}' END,
                '$.strategies.contextual'), 0) FROM link_runs WHERE id=?1",
        [run_id],
        |r| r.get::<_, Option<i64>>(0),
    )
    .optional()
    .map_err(err)?
    .flatten()
    .unwrap_or(0)
        != 0)
}

/// Case- and whitespace-insensitive "the paragraph contains this phrase" (in-text anchors).
pub fn paragraph_has(paragraph: &str, phrase: &str) -> bool {
    let norm = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let p = norm(phrase);
    !p.is_empty() && norm(paragraph).contains(&p)
}

/// Row mapper for [`item_select`]. Without `contextual` only alternates that occur in the
/// paragraph are offered: an in-text run cannot place any other (E2E 2.2: a picked title
/// alternate ended as a silent content gap).
fn item_mapper(
    contextual: bool,
) -> impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<(String, Value)> {
    move |r| item_from_row(r, contextual)
}

fn item_from_row(r: &rusqlite::Row<'_>, contextual: bool) -> rusqlite::Result<(String, Value)> {
    let source_id: String = r.get(1)?;
    let source_url: String = r.get(3)?;
    let anchor: String = r.get(7)?;
    let para: Option<String> = r.get(18)?;
    let breakdown = parse_json(r.get(19)?);
    let present: Option<i64> = r.get(16)?;
    let alternates: Vec<Value> = breakdown["alternates"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x["anchor"].as_str().or_else(|| x.as_str()))
                .filter(|x| !x.eq_ignore_ascii_case(&anchor))
                .filter(|x| contextual || para.as_deref().is_none_or(|p| paragraph_has(p, x)))
                .map(|x| json!(x))
                .collect()
        })
        .unwrap_or_default();
    let v = json!({
        "edge_id": r.get::<_, String>(0)?,
        "source": {"id": source_id, "title": r.get::<_, String>(2)?, "url": source_url},
        "target": {"id": r.get::<_, String>(4)?, "title": r.get::<_, String>(5)?, "url": r.get::<_, String>(6)?},
        "anchor": anchor,
        "anchor_type": r.get::<_, String>(8)?,
        "strategy": r.get::<_, String>(9)?,
        "strategies": breakdown.get("strategies").cloned().unwrap_or_else(|| json!([])),
        "insert_module": r.get::<_, Option<String>>(10)?,
        "score": r.get::<_, f64>(11)?,
        "status": r.get::<_, String>(12)?,
        "selected": r.get::<_, i64>(13)? != 0,
        "error": r.get::<_, Option<String>>(14)?,
        "failure_kind": r.get::<_, Option<String>>(17)?,
        "verify": {"checked_at": r.get::<_, Option<String>>(15)?, "present": present.map(|p| p != 0)},
        "alternates": alternates,
        "replaces": breakdown.get("replace_href").cloned().unwrap_or(Value::Null),
        "repaired": breakdown["repaired"].as_bool().unwrap_or(false),
        "applied_at": r.get::<_, Option<String>>(20)?,
        "live_url": source_url.split('#').next().unwrap_or(""),
        "open_url": fragment::open_url(&source_url, para.as_deref(), &anchor),
    });
    Ok((source_id, v))
}

fn item_select() -> String {
    format!(
        "SELECT e.id, s.id, s.title, s.url, t.id, t.title, t.url, e.anchor, e.anchor_type, {STRATEGY_SQL},
            e.insert_module, e.composite_score, {ITEM_STATUS_SQL}, e.run_selected, e.error, e.verified_at,
            e.verify_present, e.failure_kind, pa.text, e.score_breakdown, e.applied_at
         FROM link_edges e
         JOIN posts s ON s.id=e.source_post_id
         JOIN posts t ON t.id=e.target_post_id
         LEFT JOIN paragraphs pa ON pa.id=e.paragraph_id"
    )
}

/// Items of a run, flat (`{items, total}`) or grouped by source post (`{posts, total, total_items}`,
/// paging over posts).
pub fn items(c: &Connection, run_id: &str, q: &ItemQuery) -> Result<Value> {
    let contextual = run_contextual(c, run_id)?;
    let mut p: Vec<String> = vec![run_id.to_string()];
    let w = item_where(q, &mut p);
    let from = "FROM link_edges e JOIN posts s ON s.id=e.source_post_id JOIN posts t ON t.id=e.target_post_id";
    let total: i64 = c
        .query_row(
            &format!("SELECT count(*) {from} WHERE {w}"),
            rusqlite::params_from_iter(p.iter()),
            |r| r.get(0),
        )
        .map_err(err)?;
    if !q.group_by_post {
        let sql = format!(
            "{} WHERE {w} ORDER BY s.title, s.id, e.composite_score DESC, e.id LIMIT {} OFFSET {}",
            item_select(),
            q.limit,
            q.offset
        );
        let mut st = c.prepare(&sql).map_err(err)?;
        let items: Vec<Value> = st
            .query_map(
                rusqlite::params_from_iter(p.iter()),
                item_mapper(contextual),
            )
            .map_err(err)?
            .map(|r| r.map(|(_, v)| v))
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(err)?;
        return Ok(json!({"items": items, "total": total}));
    }
    let posts_total: i64 = c
        .query_row(
            &format!("SELECT count(DISTINCT e.source_post_id) {from} WHERE {w}"),
            rusqlite::params_from_iter(p.iter()),
            |r| r.get(0),
        )
        .map_err(err)?;
    let mut st = c
        .prepare(&format!(
            "SELECT s.id, s.title, s.url, count(*) {from} WHERE {w}
             GROUP BY s.id ORDER BY s.title, s.id LIMIT {} OFFSET {}",
            q.limit, q.offset
        ))
        .map_err(err)?;
    let posts: Vec<(String, String, String, i64)> = st
        .query_map(rusqlite::params_from_iter(p.iter()), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .map_err(err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(err)?;
    let mut by_post: HashMap<String, Vec<Value>> = HashMap::new();
    if !posts.is_empty() {
        let mut p2 = p.clone();
        let mut marks = Vec::new();
        for (id, ..) in &posts {
            p2.push(id.clone());
            marks.push(format!("?{}", p2.len()));
        }
        let sql = format!(
            "{} WHERE {w} AND e.source_post_id IN ({}) ORDER BY e.composite_score DESC, e.id",
            item_select(),
            marks.join(",")
        );
        let mut st = c.prepare(&sql).map_err(err)?;
        let rows = st
            .query_map(
                rusqlite::params_from_iter(p2.iter()),
                item_mapper(contextual),
            )
            .map_err(err)?;
        for row in rows {
            let (pid, v) = row.map_err(err)?;
            by_post.entry(pid).or_default().push(v);
        }
    }
    let out: Vec<Value> = posts
        .into_iter()
        .map(|(id, title, url, n)| {
            let links = by_post.remove(&id).unwrap_or_default();
            let count = |s: &str| links.iter().filter(|l| l["status"] == s).count();
            json!({
                "post": {"id": id, "title": title, "url": url},
                "counts": {
                    "total": n, "applied": count("applied"), "failed": count("failed"),
                    "not_present": count("not_present"), "selected": count("selected"),
                    "suggested": count("suggested"), "skipped": count("skipped"),
                },
                "links": links,
            })
        })
        .collect();
    Ok(json!({"posts": out, "total": posts_total, "total_items": total}))
}

/// Tick/untick items (only `suggested` ones; the run must be `ready`). `all` = every suggested
/// item of the run. Returns the number of edges changed.
pub fn select(
    t: &Connection,
    run_id: &str,
    edge_ids: &[String],
    all: bool,
    deselect: bool,
) -> Result<usize> {
    let flag = i64::from(!deselect);
    if all {
        return t
            .execute(
                "UPDATE link_edges SET run_selected=?2 WHERE run_id=?1 AND status='suggested' AND run_selected<>?2",
                params![run_id, flag],
            )
            .map_err(err);
    }
    let mut st = t
        .prepare(
            "UPDATE link_edges SET run_selected=?3 WHERE id=?2 AND run_id=?1 AND status='suggested' AND run_selected<>?3",
        )
        .map_err(err)?;
    let mut n = 0;
    for id in edge_ids {
        n += st.execute(params![run_id, id, flag]).map_err(err)?;
    }
    Ok(n)
}

/// Edit one item before deploy: a new anchor (free text or one of its alternates) and/or the
/// insert module. Returns the edge id when changed.
pub fn patch_item(
    t: &Connection,
    run_id: &str,
    edge_id: &str,
    anchor: Option<&str>,
    insert_module: Option<&str>,
) -> Result<()> {
    let row: Option<(String, String)> = t
        .query_row(
            "SELECT status, anchor FROM link_edges WHERE id=?1 AND run_id=?2",
            [edge_id, run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(err)?;
    let Some((status, old_anchor)) = row else {
        return Err(Error::not_found("item not found in this run"));
    };
    if status != "suggested" {
        return Err(Error::Conflict(format!(
            "item is {status}; only suggested items can be edited before deploy"
        )));
    }
    if let Some(a) = anchor {
        let a = a.split_whitespace().collect::<Vec<_>>().join(" ");
        if a.is_empty() || a.chars().count() > 255 {
            return Err(Error::bad("anchor must be 1..255 characters"));
        }
        if a != old_anchor {
            let module: Option<String> = t
                .query_row(
                    "SELECT coalesce(e.insert_module,'native'), pa.text FROM link_edges e
                       LEFT JOIN paragraphs pa ON pa.id=e.paragraph_id WHERE e.id=?1",
                    [edge_id],
                    |r| {
                        let m: String = r.get(0)?;
                        let p: Option<String> = r.get(1)?;
                        Ok(p.filter(|p| m == "native" && !paragraph_has(p, &a))
                            .map(|_| m))
                    },
                )
                .optional()
                .map_err(err)?
                .flatten();
            if module.is_some() && !run_contextual(t, run_id)? {
                return Err(Error::bad(format!(
                    "\"{a}\" is not in the paragraph; an in-text link needs a phrase from the paragraph (tick Contextual in a new run to let AI place other anchors)"
                )));
            }
            let r = t.execute(
                "UPDATE link_edges SET anchor=?2, score_breakdown=json_set(CASE WHEN json_valid(score_breakdown) THEN score_breakdown ELSE '{}' END,
                    '$.anchor_edited', json(?3)) WHERE id=?1",
                params![edge_id, a, json!({"from": old_anchor}).to_string()],
            );
            match r {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(f, _))
                    if f.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    return Err(Error::Conflict(
                        "another suggestion for this pair already uses that anchor".into(),
                    ));
                }
                Err(e) => return Err(err(e)),
            }
        }
    }
    if let Some(m) = insert_module {
        if !matches!(m, "native" | "rephrase" | "context") {
            return Err(Error::bad(
                "insert_module must be native, rephrase or context",
            ));
        }
        t.execute(
            "UPDATE link_edges SET insert_module=?2 WHERE id=?1",
            params![edge_id, m],
        )
        .map_err(err)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// opportunities overview (counts without the semantic pairs, which need vectors)
// ---------------------------------------------------------------------------------------------

/// Count of keyword-rule hits: posts whose text contains an active rule's keyword (first 50
/// rules), excluding the rule's own target URL.
fn keyword_hits(c: &Connection, site_id: &str) -> Result<i64> {
    let mut st = c
        .prepare(
            "SELECT keyword, target_url FROM keyword_rules WHERE site_id=?1 AND active=1 ORDER BY created_at, id LIMIT 50",
        )
        .map_err(err)?;
    let rules = st
        .query_map([site_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(err)?;
    let mut n = 0i64;
    for (kw, target) in rules {
        let kw = kw.trim().to_lowercase();
        if kw.len() < 2 {
            continue;
        }
        n += c
            .query_row(
                "SELECT count(*) FROM posts WHERE site_id=?1 AND url<>?3 AND instr(lower(content_text), ?2) > 0",
                params![site_id, kw, target],
                |r| r.get::<_, i64>(0),
            )
            .map_err(err)?;
    }
    Ok(n)
}

/// `GET /sites/{id}/opportunities` minus `semantic_pairs` (filled in by the caller).
pub fn opportunity_counts(c: &Connection, site_id: &str) -> Result<Value> {
    let one = |sql: &str| -> Result<i64> {
        c.query_row(sql, [site_id], |r| r.get::<_, Option<i64>>(0))
            .map(|v| v.unwrap_or(0))
            .map_err(err)
    };
    let posts_total = one("SELECT count(*) FROM posts WHERE site_id=?1")?;
    // Inbound counts are computed live from the committed body links (like the graph step),
    // not read from the `posts` graph columns: those are only filled by a plan, so right after
    // the first crawl they reported 0 orphans and every post as under-linked (E2E 2.2).
    let inbound = |cond: &str| -> Result<i64> {
        one(&format!(
            "WITH body AS (SELECT target_post_id t, count(DISTINCT source_post_id) n FROM link_edges
                            WHERE site_id=?1 AND (status='applied' OR origin='existing')
                              AND source_post_id<>target_post_id GROUP BY target_post_id),
                  nav AS (SELECT target_post_id t, count(DISTINCT source_post_id) n FROM link_health
                            WHERE site_id=?1 AND target_post_id IS NOT NULL
                              AND placement IN ('nav','header','footer','aside') GROUP BY target_post_id)
             SELECT count(*) FROM posts p LEFT JOIN body b ON b.t=p.id LEFT JOIN nav v ON v.t=p.id
              WHERE p.site_id=?1 AND {cond}"
        ))
    };
    let orphans = inbound("b.t IS NULL AND v.t IS NULL")?;
    let contextual_orphans = inbound("b.t IS NULL")?;
    let under_linked = inbound("coalesce(b.n,0)<=1 AND p.post_type<>'archive'")?;
    let broken_internal = one(
        "SELECT count(*) FROM link_health WHERE site_id=?1 AND scope='internal' AND is_broken=1 AND source_post_id IS NOT NULL",
    )?;
    let broken_posts = one(
        "SELECT count(DISTINCT source_post_id) FROM link_health WHERE site_id=?1 AND scope='internal' AND is_broken=1",
    )?;
    let silo_gaps = one(
        "SELECT count(*) FROM posts p JOIN clusters c ON c.id=p.cluster_id
          WHERE p.site_id=?1 AND c.pillar_post_id IS NOT NULL AND p.id<>c.pillar_post_id
            AND NOT EXISTS (SELECT 1 FROM link_edges e WHERE e.source_post_id=p.id AND e.target_post_id=c.pillar_post_id
                            AND (e.origin='existing' OR e.status IN ('applied','approved')))",
    )?;
    let money = one(
        "SELECT count(*) FROM posts WHERE site_id=?1 AND json_valid(meta) AND json_extract(meta,'$.money_page') IN (1, 'true')",
    )?;
    let kw_rules = one("SELECT count(*) FROM keyword_rules WHERE site_id=?1 AND active=1")?;
    let kw_hits = keyword_hits(c, site_id)?;
    let last: Option<String> = c
        .query_row(
            "SELECT last_sync_at FROM sites WHERE id=?1",
            [site_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(err)?
        .flatten();
    let age_h = last
        .as_deref()
        .and_then(|l| chrono::DateTime::parse_from_rfc3339(l).ok())
        .map(|d| {
            let mins = (chrono::Utc::now() - d.with_timezone(&chrono::Utc)).num_minutes();
            (mins as f64 / 60.0 * 10.0).round() / 10.0
        });
    let mut st = c
        .prepare(&format!(
            "SELECT {STRATEGY_SQL}, count(*) FROM link_edges e WHERE e.site_id=?1 AND e.status='suggested'
               AND e.origin IN ('plan','archive','keyword_linker','link_run') GROUP BY 1"
        ))
        .map_err(err)?;
    let pending: Map<String, Value> = st
        .query_map([site_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(err)?
        .filter_map(|r| r.ok())
        .map(|(k, v)| (k, json!(v)))
        .collect();
    let pending_of = |k: &str| pending.get(k).and_then(Value::as_i64).unwrap_or(0);
    Ok(json!({
        "posts_total": posts_total,
        "last_crawl_at": last,
        "last_crawl_age_hours": age_h,
        "crawl_stale": age_h.is_none_or(|h| h > 24.0),
        "counts": {
            "orphans": orphans,
            "contextual_orphans": contextual_orphans,
            "under_linked": under_linked,
            "semantic_pairs": 0,
            "keyword_rule_hits": kw_hits,
            "broken_internal": broken_internal,
            "silo_gaps": silo_gaps,
            "money_pages": money,
        },
        "by_strategy": {
            "in_text": {"available": true},
            "contextual": {"available": false},
            "semantic": {"pairs": 0, "pending": pending_of("semantic")},
            "keyword_rules": {"rules": kw_rules, "hits": kw_hits, "pending": pending_of("keyword_rules")},
            "silo": {"gaps": silo_gaps, "pending": pending_of("silo")},
            "orphan_rescue": {"orphans": orphans, "contextual_orphans": contextual_orphans, "under_linked": under_linked},
            "money_pages": {"money_pages": money},
            "fix_broken": {"broken_links": broken_internal, "posts": broken_posts, "pending": pending_of("fix_broken")},
        },
        "pending_suggestions": pending,
    }))
}

/// Post embeddings of a site for the semantic-pairs count: at most `cap` posts (the rest is
/// reported as an estimate).
pub fn post_vectors(c: &Connection, site_id: &str, cap: i64) -> Result<(Vec<Vec<f32>>, i64)> {
    let total: i64 = c
        .query_row(
            "SELECT count(*) FROM posts WHERE site_id=?1 AND embedding IS NOT NULL",
            [site_id],
            |r| r.get(0),
        )
        .map_err(err)?;
    let mut st = c
        .prepare(
            "SELECT embedding FROM posts WHERE site_id=?1 AND embedding IS NOT NULL ORDER BY id LIMIT ?2",
        )
        .map_err(err)?;
    let v = st
        .query_map(params![site_id, cap], |r| r.get::<_, Vec<u8>>(0))
        .map_err(err)?
        .filter_map(|r| r.ok())
        .filter_map(|b| sf_core::types::blob_to_vec(&b))
        .collect();
    Ok((v, total))
}

/// Pairs (i<j) with cosine >= `threshold` (vectors are normalised).
pub fn count_pairs(vecs: &[Vec<f32>], threshold: f32) -> i64 {
    let mut n = 0i64;
    for (i, a) in vecs.iter().enumerate() {
        for b in vecs.iter().skip(i + 1) {
            if a.len() == b.len() && sf_engine::vector::dot(a, b) >= threshold {
                n += 1;
            }
        }
    }
    n
}
