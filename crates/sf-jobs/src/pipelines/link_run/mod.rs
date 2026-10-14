//! `link_run`: guided bulk linking (contract docs/specs/v2.2-link-runs.md).
//!
//! One job kind drives every step of a run (payload `{link_run_id, step}`):
//! * `plan`   — crawl (per `options.crawl`) → plan with the run's scope (`opportunity` = semantic
//!   only, no silo shape; other modes = that silo topology) → keyword rules → fix-broken edges →
//!   adopt the site's pending suggestions that match the strategies + filters (tagged with
//!   `run_id` and `score_breakdown.strategy`), boosts, caps → `ready`.
//! * `deploy` — approve the ticked items, inject them (existing inject pipeline, run edges only),
//!   verify on the live pages → `deployed` / `partially_deployed` / `failed`.
//! * `verify` — re-check every applied link on the live pages.
//! * `repair` — "Apply properly" (§4): present-check, re-locate, alternates, AI module, skip.
//! * `undo`   — remove every link this run applied (exact removal) → `undone`.
//! * payload `{auto: true}` (scheduler, Auto-Linker §5): create an `auto` run from the site
//!   setting `auto_linker`, plan it, then stop at `ready` (dry run) or deploy + verify + repair.
//!
//! Every status change is written to `link_runs` and the unified event log (`link_run <what>`).

pub mod fragment;
pub mod options;
pub mod store;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sf_core::Error;
use sf_crawl::{WpWriter, verify_live};
use sf_engine::insert::count_links;
use sf_ops::notify::Notification;
use sf_store::{Db, err};

pub use options::{AutoLinker, RunOptions, parse_auto_linker, parse_options};

use super::common::{SiteInfo, load_site};
use super::inject::{InjectOpts, inject_edges, reconcile_for_job};
use super::plan::{PlanScope, plan_site_scoped};
use super::{chunk, crawl, keyword_link};
use crate::ctx::{Job, JobContext, JobError, JobResult};
use crate::queue::{JobQueue, NewJob};

/// Notification events (also accepted by notification channels).
pub mod events {
    pub use sf_ops::notify::events::{
        AUTO_LINKER_PAUSED as AUTO_PAUSED, LINK_RUN_DONE as DONE, LINK_RUN_READY as READY,
    };
}

/// Steps a `link_run` job can run.
pub const STEPS: &[&str] = &["plan", "deploy", "verify", "repair", "undo"];
const STALE_CRAWL_HOURS: i64 = 24;
const MAX_ALTERNATES: usize = 3;
const BOOST: f64 = 1.15;

// ---------------------------------------------------------------------------------------------
// event log + status helpers
// ---------------------------------------------------------------------------------------------

async fn site_owner(db: &Db, site_id: &str) -> Option<String> {
    let sid = site_id.to_string();
    db.read(move |c| {
        c.query_row("SELECT user_id FROM sites WHERE id=?1", [&sid], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()
        .map_err(err)
    })
    .await
    .ok()
    .flatten()
    .flatten()
}

/// One `link_run <what>` row in the unified event log (no-op without a log).
pub async fn record_event(
    db: &Db,
    events: Option<&sf_ops::eventlog::EventLog>,
    run_id: &str,
    site_id: &str,
    what: &str,
    status: &str,
    detail: Value,
) {
    let Some(log) = events.filter(|l| l.is_enabled()) else {
        return;
    };
    let owner = site_owner(db, site_id).await;
    let mut d = match detail {
        Value::Object(o) => o,
        _ => serde_json::Map::new(),
    };
    d.insert("link_run_id".into(), json!(run_id));
    log.record(
        sf_ops::eventlog::Event::job(&format!("link_run {what}"))
            .status(status)
            .target(Some(run_id))
            .site(Some(site_id))
            .user(owner.as_deref())
            .detail(Value::Object(d)),
    );
}

async fn set_status(
    ctx: &JobContext,
    run_id: &str,
    site_id: &str,
    status: &str,
    error: Option<String>,
) -> JobResult<()> {
    let (id, st, e) = (run_id.to_string(), status.to_string(), error.clone());
    let stats = {
        let id = id.clone();
        ctx.db
            .read(move |c| store::compute_stats(c, &id))
            .await
            .unwrap_or_else(|_| json!({}))
    };
    let s2 = stats.to_string();
    ctx.db
        .write(move |t| {
            t.execute(
                "UPDATE link_runs SET status=?2, error=?3, stats=?4, updated_at=?5,
                   deployed_at=CASE WHEN ?2 IN ('deployed','partially_deployed') AND deployed_at IS NULL THEN ?5 ELSE deployed_at END
                 WHERE id=?1",
                params![id, st, e, s2, sf_core::now_rfc3339()],
            )
            .map_err(err)
        })
        .await?;
    let ok = if status == "failed" { "error" } else { "ok" };
    let mut detail = json!({"status": status, "stats": compact(&stats)});
    if let Some(e) = error {
        detail["error"] = json!(e.chars().take(300).collect::<String>());
    }
    record_event(
        &ctx.db,
        ctx.events.as_ref(),
        run_id,
        site_id,
        status,
        ok,
        detail,
    )
    .await;
    Ok(())
}

fn compact(stats: &Value) -> Value {
    let mut o = serde_json::Map::new();
    for k in [
        "opportunities",
        "selected",
        "applied",
        "failed",
        "not_present",
        "repaired",
        "skipped",
    ] {
        if let Some(v) = stats.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    Value::Object(o)
}

async fn set_job_col(ctx: &JobContext, run_id: &str, col: &'static str, job_id: &str) {
    let (id, j) = (run_id.to_string(), job_id.to_string());
    let _ = ctx
        .db
        .write(move |t| {
            t.execute(
                &format!(
                    "UPDATE link_runs SET {col}=?2, current_job_id=?2, updated_at=?3 WHERE id=?1"
                ),
                params![id, j, sf_core::now_rfc3339()],
            )
            .map_err(err)
        })
        .await;
}

#[derive(Debug, Clone)]
struct RunInfo {
    id: String,
    site_id: String,
    status: String,
    options: RunOptions,
    auto: bool,
}

async fn load_run(db: &Db, run_id: &str) -> JobResult<RunInfo> {
    let id = run_id.to_string();
    let row = db.read(move |c| store::run_row(c, &id)).await?;
    let Some(r) = row else {
        return Err(JobError::fatal(Error::not_found(format!(
            "link run {run_id} not found"
        ))));
    };
    let options = parse_options(r.get("options")).unwrap_or_default();
    Ok(RunInfo {
        id: run_id.to_string(),
        site_id: r["site_id"].as_str().unwrap_or("").to_string(),
        status: r["status"].as_str().unwrap_or("").to_string(),
        options,
        auto: r["auto"].as_bool().unwrap_or(false),
    })
}

// ---------------------------------------------------------------------------------------------
// API-side entry points (create / enqueue a step / cancel)
// ---------------------------------------------------------------------------------------------

/// Insert a run row (status `draft`). Fails with Conflict while another run of the site is active.
pub async fn create_run(
    db: &Db,
    site_id: &str,
    name: &str,
    options: &RunOptions,
    created_by: &str,
    auto: bool,
) -> sf_core::Result<String> {
    let id = sf_core::new_id();
    let name = name.trim().chars().take(200).collect::<String>();
    let name = if name.is_empty() {
        format!(
            "{} run {}",
            if auto { "Auto-Linker" } else { "Link" },
            chrono::Utc::now().format("%Y-%m-%d %H:%M")
        )
    } else {
        name
    };
    let (i, s, o, by) = (
        id.clone(),
        site_id.to_string(),
        options.to_value().to_string(),
        created_by.to_string(),
    );
    db.write(move |t| {
        if let Some(other) = store::active_run(t, &s)? {
            // a stuck row whose job ended does not block a new run
            if !store::reconcile(t, &other)? {
                return Err(Error::Conflict(format!(
                    "link run {other} is still running on this site; wait for it or cancel it"
                )));
            }
        }
        t.execute(
            "INSERT INTO link_runs(id, site_id, name, status, options, created_by, auto) VALUES(?1,?2,?3,'draft',?4,?5,?6)",
            params![i, s, name, o, by, i64::from(auto)],
        )
        .map_err(err)?;
        Ok(())
    })
    .await?;
    Ok(id)
}

/// Which statuses each step may start from, and the status set while it runs.
fn step_rule(step: &str) -> Option<(&'static [&'static str], Option<&'static str>)> {
    const AFTER: &[&str] = &["deployed", "partially_deployed", "failed", "cancelled"];
    match step {
        "plan" => Some((&["draft"], None)),
        "deploy" => Some((&["ready"], Some("deploying"))),
        "verify" => Some((AFTER, None)),
        "repair" => Some((AFTER, Some("deploying"))),
        "undo" => Some((
            &[
                "deployed",
                "partially_deployed",
                "failed",
                "cancelled",
                "ready",
            ],
            Some("deploying"),
        )),
        _ => None,
    }
}

/// Queue a step of a run: checks the status (Conflict otherwise), sets the step's status and job
/// column, and returns the job id.
pub async fn enqueue_step(
    db: &Db,
    queue: &JobQueue,
    events: Option<&sf_ops::eventlog::EventLog>,
    run_id: &str,
    step: &str,
    requested_by: &str,
) -> sf_core::Result<String> {
    let Some((from, during)) = step_rule(step) else {
        return Err(Error::bad(format!("unknown link run step: {step}")));
    };
    let id = run_id.to_string();
    let (site_id, status, applied) = db
        .write(move |t| {
            store::reconcile(t, &id)?;
            let row: Option<(String, String, i64)> = t
                .query_row(
                    "SELECT site_id, status, (SELECT count(*) FROM link_edges WHERE run_id=?1 AND status='applied')
                     FROM link_runs WHERE id=?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(err)?;
            row.ok_or_else(|| Error::not_found("link run not found"))
        })
        .await?;
    if !from.contains(&status.as_str()) {
        return Err(Error::Conflict(format!(
            "link run is {status}; {step} needs one of: {}",
            from.join(", ")
        )));
    }
    if matches!(step, "verify" | "undo") && applied == 0 && status != "ready" {
        return Err(Error::Conflict(format!(
            "link run has no applied links to {step}"
        )));
    }
    if step == "undo" && status == "ready" && applied == 0 {
        return Err(Error::Conflict("link run has not been deployed".into()));
    }
    let mut job = NewJob::new(
        sf_core::types::JobKind::LinkRun,
        Some(&site_id),
        json!({"link_run_id": run_id, "step": step}),
    )
    .by(requested_by);
    job.dedupe = Some(step == "verify");
    let enq = queue.enqueue(job).await?;
    let col = match step {
        "plan" => "plan_job_id",
        "deploy" => "deploy_job_id",
        "verify" => "verify_job_id",
        "repair" => "repair_job_id",
        _ => "undo_job_id",
    };
    let (i, j, d) = (run_id.to_string(), enq.job_run_id.clone(), during);
    db.write(move |t| {
        t.execute(
            &format!(
                "UPDATE link_runs SET {col}=?2, current_job_id=?2, status=coalesce(?3, status), error=NULL, updated_at=?4 WHERE id=?1"
            ),
            params![i, j, d, sf_core::now_rfc3339()],
        )
        .map_err(err)
    })
    .await?;
    record_event(
        db,
        events,
        run_id,
        &site_id,
        &format!("{step} queued"),
        "ok",
        json!({"job_id": enq.job_run_id, "requested_by": requested_by}),
    )
    .await;
    Ok(enq.job_run_id)
}

/// Cancel a run while it is crawling / planning / deploying (its job is cancelled too).
pub async fn cancel_run(
    db: &Db,
    queue: &JobQueue,
    events: Option<&sf_ops::eventlog::EventLog>,
    run_id: &str,
) -> sf_core::Result<()> {
    let id = run_id.to_string();
    let row: Option<(String, String, Option<String>)> = db
        .read(move |c| {
            c.query_row(
                "SELECT site_id, status, current_job_id FROM link_runs WHERE id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(err)
        })
        .await?;
    let Some((site_id, status, job)) = row else {
        return Err(Error::not_found("link run not found"));
    };
    // a `ready` run holds its links until it is deployed or cancelled (a later run does not
    // adopt them), so it must be cancellable to release them (E2E 2.2: a new run silently took
    // the 20 links of a ready Auto-Linker dry run)
    if !store::ACTIVE.contains(&status.as_str()) && status != "ready" {
        return Err(Error::Conflict(format!(
            "link run is {status}; only a crawling, planning, ready or deploying run can be cancelled"
        )));
    }
    if let Some(j) = job {
        let _ = queue.cancel(&j).await;
    }
    let id = run_id.to_string();
    db.write(move |t| {
        t.execute(
            "UPDATE link_runs SET status='cancelled', updated_at=?2 WHERE id=?1",
            params![id, sf_core::now_rfc3339()],
        )
        .map_err(err)
    })
    .await?;
    record_event(db, events, run_id, &site_id, "cancelled", "ok", json!({})).await;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// job entry point
// ---------------------------------------------------------------------------------------------

pub async fn run(ctx: &Arc<JobContext>, job: &Job) -> JobResult<Value> {
    // no run id = a scheduled Auto-Linker run (`{auto: true}`, or a bare /schedules row)
    let Some(run_id) = job.payload_str("link_run_id", "linkRunId") else {
        return auto_run(ctx, job).await;
    };
    let step = job
        .payload_str("step", "step")
        .unwrap_or_else(|| "plan".into());
    let run = load_run(&ctx.db, &run_id).await?;
    if run.site_id != job.site()? {
        return Err(JobError::fatal(Error::bad(
            "link run belongs to another site",
        )));
    }
    if run.status == "cancelled" && step != "undo" {
        return Err(JobError::cancelled());
    }
    job.merge_stats(json!({"link_run_id": run_id, "link_run_step": step}))
        .await;
    let res = match step.as_str() {
        "plan" => step_plan(ctx, job, &run).await,
        "deploy" => step_deploy(ctx, job, &run).await,
        "verify" => step_verify(ctx, job, &run).await,
        "repair" => step_repair(ctx, job, &run).await,
        "undo" => step_undo(ctx, job, &run).await,
        other => Err(JobError::fatal(Error::bad(format!(
            "unknown link run step: {other}"
        )))),
    };
    after_step(ctx, &run, &step, res).await
}

/// Map a failed/cancelled step onto the run status.
async fn after_step(
    ctx: &JobContext,
    run: &RunInfo,
    step: &str,
    res: JobResult<Value>,
) -> JobResult<Value> {
    match res {
        Ok(v) => Ok(v),
        Err(e) if e.is_cancelled() => {
            let _ = set_status(ctx, &run.id, &run.site_id, "cancelled", None).await;
            Err(e)
        }
        Err(e) => {
            let msg = e.to_string();
            let status = match step {
                "plan" => "failed".to_string(),
                "verify" => run.status.clone(),
                _ => deployed_status(ctx, &run.id)
                    .await
                    .unwrap_or_else(|_| "failed".into()),
            };
            let _ = set_status(ctx, &run.id, &run.site_id, &status, Some(msg)).await;
            Err(e)
        }
    }
}

/// `deployed` (every ticked item applied), `partially_deployed` (some), `failed` (none).
async fn deployed_status(ctx: &JobContext, run_id: &str) -> JobResult<String> {
    let id = run_id.to_string();
    let (applied, other): (i64, i64) = ctx
        .db
        .read(move |c| {
            c.query_row(
                "SELECT sum(status='applied' AND coalesce(verify_present,1)=1),
                        sum(run_selected=1 AND NOT (status='applied' AND coalesce(verify_present,1)=1) AND status<>'reverted')
                 FROM link_edges WHERE run_id=?1",
                [&id],
                |r| {
                    Ok((
                        r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                        r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    ))
                },
            )
            .map_err(err)
        })
        .await?;
    Ok(match (applied, other) {
        (0, _) => "failed",
        (_, 0) => "deployed",
        _ => "partially_deployed",
    }
    .into())
}

// ---------------------------------------------------------------------------------------------
// plan step
// ---------------------------------------------------------------------------------------------

async fn crawl_needed(ctx: &JobContext, site_id: &str, mode: &str) -> JobResult<bool> {
    match mode {
        "always" => return Ok(true),
        "skip" => return Ok(false),
        _ => {}
    }
    let sid = site_id.to_string();
    let last: Option<String> = ctx
        .db
        .read(move |c| {
            c.query_row("SELECT last_sync_at FROM sites WHERE id=?1", [&sid], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()
            .map_err(err)
        })
        .await?
        .flatten();
    Ok(last
        .as_deref()
        .and_then(|l| chrono::DateTime::parse_from_rfc3339(l).ok())
        .is_none_or(|d| {
            chrono::Utc::now() - d.with_timezone(&chrono::Utc)
                > chrono::Duration::hours(STALE_CRAWL_HOURS)
        }))
}

/// Inject options of a run step (v2.3: AI accounting by run, priority tier, repair task).
/// When the AI is allowed it is also the fallback for any edge that cannot be placed natively
/// (E2E 2.2: the Contextual strategy / the repair AI pass must not leave "no AI insertion
/// model" content gaps when an AI chain is active).
fn run_inject_opts(run: &RunInfo, site: &SiteInfo, allow_llm: bool, repair: bool) -> InjectOpts {
    InjectOpts {
        allow_llm,
        llm_fallback: allow_llm,
        repair,
        high_priority: run.options.high_priority || site.setting_bool("ai_high_priority"),
        run_id: Some(run.id.clone()),
    }
}

async fn step_plan(ctx: &Arc<JobContext>, job: &Job, run: &RunInfo) -> JobResult<Value> {
    let o = &run.options;
    let mut site = load_site(ctx, &run.site_id).await?;
    let started = sf_core::now_rfc3339();
    let mut out = serde_json::Map::new();
    if crawl_needed(ctx, &site.id, &o.crawl).await? {
        set_job_col(ctx, &run.id, "crawl_job_id", &job.id).await;
        set_status(ctx, &run.id, &site.id, "crawling", None).await?;
        let t = crawl::crawl_site(ctx, job, &site, (0.0, 20.0)).await?;
        let ch = chunk::chunk_changed(ctx, job, &site, "chunk", (20.0, 25.0)).await?;
        crawl::finish_sync(ctx, &site.id).await?;
        out.insert(
            "crawl".into(),
            json!({"upserted": t.upserted, "skipped": t.skipped, "chunked_posts": ch.chunked_posts}),
        );
        site = load_site(ctx, &run.site_id).await?;
    }
    job.checkpoint().await?;
    set_job_col(ctx, &run.id, "plan_job_id", &job.id).await;
    set_status(ctx, &run.id, &site.id, "planning", None).await?;
    let s = &o.strategies;
    let scope = PlanScope {
        topology: s.silo && o.topology().is_some(),
        semantic: s.semantic,
        max_links_per_post: Some(o.filters.max_links_per_post as f64),
        max_new_links_per_pass: Some(o.filters.max_new_per_post as f64),
        ai_refine: o.ai_refine.then_some(true),
        high_priority: o.high_priority,
        run_id: Some(run.id.clone()),
    };
    if scope.topology || scope.semantic {
        let topology = o
            .topology()
            .map(str::to_string)
            .or_else(|| site.setting_str("silo_topology"))
            .unwrap_or_else(|| "reverse".into());
        let mut st = plan_site_scoped(ctx, job, &site, &topology, false, None, &scope).await?;
        if let Some(m) = st.as_object_mut() {
            m.remove("progress");
            m.remove("timings_ms");
        }
        out.insert("plan".into(), st);
    } else {
        // keyword rules / fix-broken only: paragraphs must still be current
        chunk::chunk_changed(ctx, job, &site, "chunk", (25.0, 40.0)).await?;
    }
    job.checkpoint().await?;
    if s.keyword_rules {
        job.progress("keyword_rules", 90.0).await;
        let mut st = keyword_link::run(ctx, job).await?;
        if let Some(m) = st.as_object_mut() {
            m.remove("progress");
        }
        out.insert("keyword_rules".into(), st);
    }
    if s.fix_broken {
        job.progress("fix_broken", 93.0).await;
        let n = fix_broken_edges(ctx, job, &site, &run.id).await?;
        out.insert("fix_broken".into(), json!({"created": n}));
    }
    job.checkpoint().await?;
    job.progress("select", 96.0).await;
    let tagged = adopt_and_select(ctx, &site, run, &started).await?;
    out.insert("adopted".into(), json!(tagged.0));
    out.insert("selected".into(), json!(tagged.1));
    set_status(ctx, &run.id, &site.id, "ready", None).await?;
    job.info(format!(
        "link run ready: {} opportunities, {} selected",
        tagged.0, tagged.1
    ))
    .await;
    out.insert("progress".into(), json!({"phase": "ready", "pct": 100}));
    Ok(Value::Object(out))
}

/// Candidate-source strategy of a suggestion.
fn derive_strategy(origin: &str, topology: Option<&str>, breakdown: &Value) -> &'static str {
    if origin == "keyword_linker" || breakdown.get("keyword_rule").is_some() {
        return "keyword_rules";
    }
    if breakdown["strategy"].as_str() == Some("fix_broken") {
        return "fix_broken";
    }
    if topology.is_some() {
        return "silo";
    }
    "semantic"
}

struct Cand {
    id: String,
    source: String,
    target: String,
    anchor_type: String,
    score: f64,
    breakdown: Value,
    module: Option<String>,
    strategy: &'static str,
    boosts: Vec<&'static str>,
}

/// Adopt the site's pending suggestions that this run's strategies and filters accept, apply
/// boosts, pick the module, tick the items within the caps. Returns (adopted, selected).
async fn adopt_and_select(
    ctx: &JobContext,
    site: &SiteInfo,
    run: &RunInfo,
    _started: &str,
) -> JobResult<(usize, usize)> {
    let o = run.options.clone();
    let money = site.money_urls();
    let (sid, rid) = (site.id.clone(), run.id.clone());
    let n = ctx
        .db
        .write(move |t| {
            type Row = (
                String, String, String, String, f64, String, Option<String>, Option<String>,
                String, String, String, Option<String>, String, String, Option<String>, i64, i64,
                String, bool,
            );
            let mut st = t
                .prepare(
                    "SELECT e.id, e.source_post_id, e.target_post_id, e.anchor_type, e.composite_score,
                        e.score_breakdown, e.silo_topology, e.insert_module, e.origin,
                        s.url, s.post_type, s.cluster_id, t.url, t.post_type, t.cluster_id,
                        t.is_contextual_orphan, t.body_inbound_count, t.meta,
                        coalesce(instr(lower(pa.text), lower(e.anchor)) > 0, 0)
                     FROM link_edges e
                     JOIN posts s ON s.id=e.source_post_id JOIN posts t ON t.id=e.target_post_id
                     LEFT JOIN paragraphs pa ON pa.id=e.paragraph_id
                     WHERE e.site_id=?1 AND e.status='suggested' AND trim(e.anchor)<>''
                       AND e.origin IN ('plan','archive','keyword_linker','link_run')
                       AND (e.run_id IS NULL OR e.run_id=?2 OR e.run_id IN (
                            SELECT id FROM link_runs WHERE site_id=?1 AND id<>?2
                              AND status NOT IN ('draft','crawling','planning','ready','deploying')))
                     ORDER BY e.id",
                )
                .map_err(err)?;
            let rows: Vec<Row> = st
                .query_map(params![sid, rid], |r| {
                    Ok((
                        r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?,
                        r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?,
                        r.get(13)?, r.get(14)?, r.get(15)?, r.get(16)?, r.get(17)?, r.get(18)?,
                    ))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            drop(st);
            let f = &o.filters;
            let s = &o.strategies;
            let mut cands: Vec<Cand> = Vec::new();
            for (id, src, tgt, atype, score, bd, topo, module, origin, surl, stype, sclu, turl, ttype, tclu, torph, tin, tmeta, in_para) in rows {
                let breakdown: Value = serde_json::from_str(&bd).unwrap_or_else(|_| json!({}));
                let strategy = derive_strategy(&origin, topo.as_deref(), &breakdown);
                let allowed = match strategy {
                    "silo" => s.silo && o.topology().is_some(),
                    other => s.enabled(other),
                };
                if !allowed {
                    continue;
                }
                if !f.post_types.iter().any(|p| p == &stype) || !f.post_types.iter().any(|p| p == &ttype) {
                    continue;
                }
                if !f.anchor_types.iter().any(|a| a == &atype) {
                    continue;
                }
                if !f.clusters.is_empty()
                    && !f.clusters.iter().any(|c| Some(c) == sclu.as_ref() || Some(c) == tclu.as_ref())
                {
                    continue;
                }
                if !f.include_post_ids.is_empty()
                    && !f.include_post_ids.iter().any(|p| p == &src || p == &tgt)
                {
                    continue;
                }
                if f.exclude_post_ids.iter().any(|p| p == &src || p == &tgt) {
                    continue;
                }
                if f.exclude_url_patterns
                    .iter()
                    .any(|p| options::url_matches(p, &surl) || options::url_matches(p, &turl))
                {
                    continue;
                }
                // insertion strategies
                let replace = breakdown.get("replace_href").is_some();
                // in_text = the anchor already is in the paragraph (native, no text change);
                // contextual = an AI sentence carries the link (context/rephrase module)
                let in_text = replace || in_para || module.as_deref() == Some("native");
                let module = if replace || (s.in_text && in_text) {
                    Some("native".to_string())
                } else if s.contextual {
                    match module.as_deref() {
                        Some(m @ ("rephrase" | "context")) => Some(m.to_string()),
                        _ => Some("context".into()),
                    }
                } else {
                    continue;
                };
                let mut boosts = Vec::new();
                let mut boosted = score;
                if s.orphan_rescue && (torph != 0 || tin <= 1) {
                    boosts.push("orphan_rescue");
                    boosted *= BOOST;
                }
                let meta: Value = serde_json::from_str(&tmeta).unwrap_or(Value::Null);
                let is_money = meta["money_page"].as_bool() == Some(true)
                    || meta["money_page"].as_i64() == Some(1)
                    || money.contains(&sf_engine::html::normalize_url(&turl));
                if s.money_pages && is_money {
                    boosts.push("money_pages");
                    boosted *= BOOST;
                }
                let boosted = boosted.min(1.0);
                if boosted < f.min_score {
                    continue;
                }
                cands.push(Cand {
                    id,
                    source: src,
                    target: tgt,
                    anchor_type: atype,
                    score: if boosts.is_empty() { score } else { boosted },
                    breakdown,
                    module,
                    strategy,
                    boosts,
                });
            }
            // selection within the caps, best first
            cands.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
            let mut existing_out: HashMap<String, i64> = HashMap::new();
            {
                let mut q = t
                    .prepare(
                        "SELECT source_post_id, count(DISTINCT target_post_id) FROM link_edges
                         WHERE site_id=?1 AND (origin='existing' OR status IN ('applied','approved'))
                         GROUP BY source_post_id",
                    )
                    .map_err(err)?;
                let rows = q
                    .query_map([&sid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                    .map_err(err)?;
                for r in rows {
                    let (k, v) = r.map_err(err)?;
                    existing_out.insert(k, v);
                }
            }
            let mut per_source: HashMap<&str, i64> = HashMap::new();
            let mut pairs: HashSet<(&str, &str)> = HashSet::new();
            let (mut total, mut exact) = (0i64, 0i64);
            let mut picks: Vec<bool> = Vec::with_capacity(cands.len());
            for c in &cands {
                let pick = match o.approve.as_str() {
                    "none" => false,
                    "all" => pairs.insert((c.source.as_str(), c.target.as_str())),
                    _ => {
                        let mine = per_source.get(c.source.as_str()).copied().unwrap_or(0);
                        let have = existing_out.get(&c.source).copied().unwrap_or(0);
                        let exact_ok = c.anchor_type != "exact"
                            || ((exact + 1) as f64) <= (f.max_exact_share * (total + 1) as f64).max(1.0);
                        let ok = total < f.max_total
                            && mine < f.max_new_per_post
                            && have + mine < f.max_links_per_post
                            && exact_ok
                            && !pairs.contains(&(c.source.as_str(), c.target.as_str()));
                        if ok {
                            pairs.insert((c.source.as_str(), c.target.as_str()));
                        }
                        ok
                    }
                };
                if pick {
                    total += 1;
                    *per_source.entry(c.source.as_str()).or_insert(0) += 1;
                    if c.anchor_type == "exact" {
                        exact += 1;
                    }
                }
                picks.push(pick);
            }
            let mut up = t
                .prepare(
                    "UPDATE link_edges SET run_id=?2, run_selected=?3, insert_module=?4, composite_score=?5,
                       score_breakdown=?6, verified_at=NULL, verify_present=NULL
                     WHERE id=?1 AND status='suggested'",
                )
                .map_err(err)?;
            let mut selected = 0usize;
            for (c, pick) in cands.iter().zip(picks) {
                let mut b = c.breakdown.clone();
                if let Some(m) = b.as_object_mut() {
                    m.insert("strategy".into(), json!(c.strategy));
                    let mut all = vec![c.strategy];
                    all.extend(c.boosts.iter().copied());
                    all.push(if c.module.as_deref() == Some("native") { "in_text" } else { "contextual" });
                    m.insert("strategies".into(), json!(all));
                    m.insert("run_id".into(), json!(rid));
                    if !c.boosts.is_empty() {
                        m.insert("boost".into(), json!({"factor": BOOST, "by": c.boosts}));
                    }
                }
                up.execute(params![c.id, rid, i64::from(pick), c.module, c.score, b.to_string()])
                    .map_err(err)?;
                if pick {
                    selected += 1;
                }
            }
            Ok((cands.len(), selected))
        })
        .await?;
    Ok(n)
}

// ---------------------------------------------------------------------------------------------
// fix broken internal links
// ---------------------------------------------------------------------------------------------

fn slug_tokens(url: &str) -> HashSet<String> {
    let path = url::Url::parse(url)
        .map(|u| u.path().to_string())
        .unwrap_or_else(|_| url.split(['?', '#']).next().unwrap_or("").to_string());
    path.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1)
        .map(|w| {
            let w = w.to_lowercase();
            w.strip_suffix('s')
                .filter(|s| s.len() > 2)
                .map(str::to_string)
                .unwrap_or(w)
        })
        .filter(|w| !matches!(w.as_str(), "www" | "html" | "php" | "htm"))
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    inter / union
}

/// One suggested `replace` edge per broken internal link (link_health rows): the best live
/// target by URL-slug similarity (cosine to the source post breaks ties). Returns how many
/// edges were created.
async fn fix_broken_edges(
    ctx: &JobContext,
    job: &Job,
    site: &SiteInfo,
    run_id: &str,
) -> JobResult<usize> {
    let sid = site.id.clone();
    type Broken = (String, String, String, Option<i64>);
    let (broken, posts): (Vec<Broken>, Vec<(String, String, String)>) = ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare(
                    "SELECT source_post_id, href, anchor, status_code FROM link_health
                     WHERE site_id=?1 AND scope='internal' AND is_broken=1 AND source_post_id IS NOT NULL
                       AND trim(anchor)<>'' AND placement IN ('body','unknown')
                     ORDER BY source_post_id, href LIMIT 5000",
                )
                .map_err(err)?;
            let b = st
                .query_map([&sid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            let mut st = c
                .prepare(
                    "SELECT id, url, title FROM posts WHERE site_id=?1 AND post_status='publish' ORDER BY id",
                )
                .map_err(err)?;
            let p = st
                .query_map([&sid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok((b, p))
        })
        .await?;
    if broken.is_empty() {
        return Ok(0);
    }
    let vectors = ctx.vectors.get(&site.id).await.ok();
    // (id, url, slug tokens, slug + title tokens)
    let toks: Vec<(String, String, HashSet<String>, HashSet<String>)> = posts
        .iter()
        .map(|(id, url, title)| {
            let slug = slug_tokens(url);
            let mut t = slug.clone();
            t.extend(slug_tokens(&format!("/{}", title.replace(' ', "-"))));
            (id.clone(), url.clone(), slug, t)
        })
        .collect();
    let mut rows: Vec<(String, String, String, String, Option<i64>, f64)> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for (source, href, anchor, code) in broken {
        if !seen.insert((source.clone(), href.clone())) {
            continue;
        }
        let want = slug_tokens(&href);
        let mut best: Option<(f64, f64, f64, &str)> = None;
        for (pid, url, slug, t) in &toks {
            if pid == &source || sf_engine::insert::urls_match(url, &href) || want.is_empty() {
                continue;
            }
            // share of the broken slug's words found in the candidate's slug/title
            let cover = want.intersection(t).count() as f64 / want.len() as f64;
            if cover < 0.5 {
                continue;
            }
            let j = jaccard(&want, slug);
            let cos = vectors
                .as_ref()
                .and_then(|v| {
                    let a = v.posts.position(&source).and_then(|r| v.posts.vector(r))?;
                    let b = v.posts.position(pid).and_then(|r| v.posts.vector(r))?;
                    Some(f64::from(sf_engine::vector::dot(a, b)))
                })
                .unwrap_or(0.0);
            let better = best.is_none_or(|(bv, bj, bc, _)| {
                cover > bv || (cover == bv && (j > bj || (j == bj && cos > bc)))
            });
            if better {
                best = Some((cover, j, cos, pid.as_str()));
            }
        }
        if let Some((cover, j, _, target)) = best {
            rows.push((
                source,
                target.to_string(),
                href,
                anchor,
                code,
                (cover + j) / 2.0,
            ));
        }
    }
    job.info(format!(
        "fix_broken: {} broken internal links with a replacement target",
        rows.len()
    ))
    .await;
    let rid = run_id.to_string();
    let sid = site.id.clone();
    let n = ctx
        .db
        .write(move |t| {
            let mut n = 0usize;
            for (source, target, href, anchor, code, j) in &rows {
                // never two pending/live links for the same pair
                let busy: i64 = t
                    .query_row(
                        "SELECT count(*) FROM link_edges WHERE site_id=?1 AND source_post_id=?2 AND target_post_id=?3
                           AND (origin='existing' OR status IN ('applied','approved')
                                OR (status='suggested' AND origin='link_run'))",
                        params![sid, source, target],
                        |r| r.get(0),
                    )
                    .map_err(err)?;
                if busy > 0 {
                    continue;
                }
                // re-pointing the broken link beats adding a second link for the same pair
                t.execute(
                    "DELETE FROM link_edges WHERE site_id=?1 AND source_post_id=?2 AND target_post_id=?3
                       AND status='suggested' AND origin IN ('plan','archive','keyword_linker')",
                    params![sid, source, target],
                )
                .map_err(err)?;
                let para: Option<String> = t
                    .query_row(
                        "SELECT id FROM paragraphs WHERE post_id=?1 AND instr(lower(text), lower(?2)) > 0 ORDER BY idx LIMIT 1",
                        params![source, anchor],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(err)?;
                let bd = json!({
                    "strategy": "fix_broken", "replace_href": href, "broken_status": code,
                    "slug_similarity": (j * 1000.0).round() / 1000.0,
                });
                n += t
                    .execute(
                        "INSERT INTO link_edges(id, site_id, source_post_id, target_post_id, paragraph_id, anchor, anchor_type,
                           composite_score, score_breakdown, insert_module, status, origin, run_id)
                         VALUES(?1,?2,?3,?4,?5,?6,'partial',?7,?8,'native','suggested','link_run',?9)
                         ON CONFLICT(site_id, source_post_id, target_post_id, anchor) DO NOTHING",
                        params![
                            super::ids::row_id(),
                            sid,
                            source,
                            target,
                            para,
                            super::common::truncate_utf16(anchor, 255),
                            (0.6 + 0.4 * j).min(1.0),
                            bd.to_string(),
                            rid
                        ],
                    )
                    .map_err(err)?;
            }
            Ok(n)
        })
        .await?;
    Ok(n)
}

// ---------------------------------------------------------------------------------------------
// deploy / verify
// ---------------------------------------------------------------------------------------------

async fn run_edge_ids(
    ctx: &JobContext,
    run_id: &str,
    filter: &'static str,
) -> JobResult<Vec<String>> {
    let id = run_id.to_string();
    Ok(ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare(&format!(
                    "SELECT id FROM link_edges WHERE run_id=?1 AND ({filter}) ORDER BY source_post_id, id"
                ))
                .map_err(err)?;
            let v = st
                .query_map([&id], |r| r.get::<_, String>(0))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(v)
        })
        .await?)
}

async fn step_deploy(ctx: &Arc<JobContext>, job: &Job, run: &RunInfo) -> JobResult<Value> {
    let site = load_site(ctx, &run.site_id).await?;
    set_job_col(ctx, &run.id, "deploy_job_id", &job.id).await;
    set_status(ctx, &run.id, &site.id, "deploying", None).await?;
    reconcile_for_job(ctx, job, &site).await?;
    let rid = run.id.clone();
    let ids: Vec<String> = ctx
        .db
        .write(move |t| {
            let mut st = t
                .prepare(
                    "SELECT id FROM link_edges WHERE run_id=?1 AND status='suggested' AND run_selected=1 ORDER BY id",
                )
                .map_err(err)?;
            let ids = st
                .query_map([&rid], |r| r.get::<_, String>(0))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            drop(st);
            let now = sf_core::now_rfc3339();
            for id in &ids {
                t.execute(
                    "UPDATE link_edges SET status='approved', decided_at=?2, error=NULL, failure_kind=NULL WHERE id=?1 AND status='suggested'",
                    params![id, now],
                )
                .map_err(err)?;
            }
            Ok(ids)
        })
        .await?;
    job.info(format!("deploy: {} items approved", ids.len()))
        .await;
    let opts = run_inject_opts(run, &site, run.options.strategies.contextual, false);
    let stats = inject_edges(ctx, job, &site, &ids, opts).await?;
    let verified = if run.options.verify {
        job.progress("verify", 95.0).await;
        verify_edges(ctx, job, &site, &run.id, None).await?
    } else {
        json!({"skipped": true})
    };
    let status = deployed_status(ctx, &run.id).await?;
    set_status(ctx, &run.id, &site.id, &status, None).await?;
    if !run.auto {
        notify_done(ctx, run, &status, "deployed").await;
    }
    Ok(json!({
        "approved": ids.len(),
        "inject": stats,
        "verify": verified,
        "status": status,
        "progress": {"phase": "done", "pct": 100},
    }))
}

/// Check applied links of the run on the live pages (`only` = these edge ids). Stores
/// `verified_at` + `verify_present` (NULL when the check was inconclusive).
async fn verify_edges(
    ctx: &JobContext,
    job: &Job,
    site: &SiteInfo,
    run_id: &str,
    only: Option<&[String]>,
) -> JobResult<Value> {
    let id = run_id.to_string();
    let rows: Vec<(String, String, String, String)> = ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare(
                    "SELECT e.id, s.url, t.url, e.anchor FROM link_edges e
                     JOIN posts s ON s.id=e.source_post_id JOIN posts t ON t.id=e.target_post_id
                     WHERE e.run_id=?1 AND e.status='applied' ORDER BY s.url, e.id",
                )
                .map_err(err)?;
            let v = st
                .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(v)
        })
        .await?;
    let rows: Vec<_> = match only {
        Some(ids) => rows.into_iter().filter(|r| ids.contains(&r.0)).collect(),
        None => rows,
    };
    let client = site.http_client(ctx)?;
    let mut results: Vec<(String, Option<bool>)> = Vec::with_capacity(rows.len());
    let total = rows.len().max(1);
    for (i, (eid, page, target, anchor)) in rows.into_iter().enumerate() {
        job.checkpoint().await?;
        let present = match verify_live(&client, &page, &target, &anchor, &ctx.verify).await {
            Ok(v) if v.found() => Some(true),
            Ok(v) if v.status == Some(200) && !v.stale_cache => Some(false),
            _ => None,
        };
        results.push((eid, present));
        job.progress("verify", 95.0 + 5.0 * (i + 1) as f64 / total as f64)
            .await;
    }
    let (mut ok, mut missing, mut unknown) = (0usize, 0usize, 0usize);
    for (_, p) in &results {
        match p {
            Some(true) => ok += 1,
            Some(false) => missing += 1,
            None => unknown += 1,
        }
    }
    ctx.db
        .write(move |t| {
            let now = sf_core::now_rfc3339();
            for (id, p) in &results {
                // the reason shown in Results follows the latest check (E2E 2.2: a stale
                // "verify_skipped: blocked by ssrf guard" from the deploy stayed next to links
                // that a later verify found missing or present)
                t.execute(
                    "UPDATE link_edges SET verified_at=?2, verify_present=?3,
                        error=CASE WHEN ?3=0 THEN 'link not found on the live page'
                                   WHEN ?3=1 AND (error LIKE 'verify%' OR error='link not found on the live page') THEN NULL
                                   ELSE error END
                     WHERE id=?1",
                    params![id, now, p.map(i64::from)],
                )
                .map_err(err)?;
            }
            Ok(())
        })
        .await?;
    Ok(json!({"present": ok, "not_present": missing, "inconclusive": unknown}))
}

async fn step_verify(ctx: &Arc<JobContext>, job: &Job, run: &RunInfo) -> JobResult<Value> {
    let site = load_site(ctx, &run.site_id).await?;
    set_job_col(ctx, &run.id, "verify_job_id", &job.id).await;
    let v = verify_edges(ctx, job, &site, &run.id, None).await?;
    let status = deployed_status(ctx, &run.id).await?;
    let status = if run.status == "cancelled" {
        run.status.clone()
    } else {
        status
    };
    set_status(ctx, &run.id, &site.id, &status, None).await?;
    record_event(
        &ctx.db,
        ctx.events.as_ref(),
        &run.id,
        &site.id,
        "verified",
        "ok",
        v.clone(),
    )
    .await;
    Ok(json!({"verify": v, "status": status, "progress": {"phase": "done", "pct": 100}}))
}

// ---------------------------------------------------------------------------------------------
// repair ("Apply properly", §4)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RepairItem {
    id: String,
    status: String,
    anchor: String,
    target_url: String,
    wp_id: Option<i64>,
    post_type: String,
    source: String,
    alternates: Vec<String>,
    error: Option<String>,
    failure_kind: Option<String>,
}

async fn repair_items(ctx: &JobContext, run_id: &str) -> JobResult<Vec<RepairItem>> {
    let id = run_id.to_string();
    Ok(ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare(
                    "SELECT e.id, e.status, e.verify_present, e.anchor, t.url, s.wp_post_id, s.post_type, s.id,
                        e.score_breakdown, e.error, e.failure_kind
                     FROM link_edges e JOIN posts s ON s.id=e.source_post_id JOIN posts t ON t.id=e.target_post_id
                     WHERE e.run_id=?1 AND (
                        (e.status='failed' AND coalesce(e.failure_kind,'')<>'skipped')
                        OR (e.status='applied' AND coalesce(e.verify_present,-1)<>1)
                        OR (e.status='approved'))
                     ORDER BY s.id, e.id",
                )
                .map_err(err)?;
            let v = st
                .query_map([&id], |r| {
                    let bd: String = r.get(8)?;
                    let bd: Value = serde_json::from_str(&bd).unwrap_or(Value::Null);
                    let alternates = bd["alternates"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x["anchor"].as_str().or_else(|| x.as_str()))
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    Ok(RepairItem {
                        id: r.get(0)?,
                        status: r.get(1)?,
                        anchor: r.get(3)?,
                        target_url: r.get(4)?,
                        wp_id: r.get(5)?,
                        post_type: r.get(6)?,
                        source: r.get(7)?,
                        alternates,
                        error: r.get(9)?,
                        failure_kind: r.get(10)?,
                    })
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(v)
        })
        .await?)
}

/// Append one attempt to `score_breakdown.repair` and optionally flag the edge `repaired`.
fn note_attempt(
    t: &rusqlite::Transaction<'_>,
    id: &str,
    attempt: &Value,
    repaired: bool,
) -> sf_core::Result<()> {
    t.execute(
        "UPDATE link_edges SET score_breakdown=json_set(
            CASE WHEN json_valid(score_breakdown) THEN score_breakdown ELSE '{}' END,
            '$.repair', json_insert(coalesce(json_extract(CASE WHEN json_valid(score_breakdown) THEN score_breakdown END, '$.repair'), '[]'), '$[#]', json(?2)),
            '$.repaired', CASE WHEN ?3 THEN json('true') ELSE coalesce(json_extract(CASE WHEN json_valid(score_breakdown) THEN score_breakdown END, '$.repaired'), json('false')) END)
         WHERE id=?1",
        params![id, attempt.to_string(), repaired],
    )
    .map_err(err)?;
    Ok(())
}

/// Status per edge id after an inject round.
async fn statuses(
    ctx: &JobContext,
    ids: &[String],
) -> JobResult<HashMap<String, (String, Option<String>, Option<String>)>> {
    let ids = ids.to_vec();
    Ok(ctx
        .db
        .read(move |c| {
            let mut out = HashMap::new();
            let mut st = c
                .prepare("SELECT status, error, failure_kind FROM link_edges WHERE id=?1")
                .map_err(err)?;
            for id in &ids {
                if let Some(r) = st
                    .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                    .optional()
                    .map_err(err)?
                {
                    out.insert(id.clone(), r);
                }
            }
            Ok(out)
        })
        .await?)
}

fn skip_reason(error: Option<&str>, kind: Option<&str>) -> String {
    let e = error.unwrap_or("").to_lowercase();
    match kind.unwrap_or("") {
        "content_gap" | "insert" | "replace" => "anchor text no longer in post".into(),
        "conflict" | "content_changed" | "local_verify" => "post locked/changed".into(),
        "no_wp_id" => "post has no WordPress id".into(),
        "fetch" | "wp_update" | "wp_unknown" => format!(
            "WordPress write failed ({})",
            error.unwrap_or("").chars().take(200).collect::<String>()
        ),
        "already_linked" => "the paragraph already links to the target".into(),
        "verify_failed" => "link did not appear on the live page".into(),
        _ if e.contains("anchor") => "anchor text no longer in post".into(),
        _ if error.is_some() => error.unwrap_or("").chars().take(300).collect(),
        _ => "could not be applied".into(),
    }
}

async fn step_repair(ctx: &Arc<JobContext>, job: &Job, run: &RunInfo) -> JobResult<Value> {
    let site = load_site(ctx, &run.site_id).await?;
    set_job_col(ctx, &run.id, "repair_job_id", &job.id).await;
    set_status(ctx, &run.id, &site.id, "deploying", None).await?;
    reconcile_for_job(ctx, job, &site).await?;
    let items = repair_items(ctx, &run.id).await?;
    let mut report = json!({"items": items.len(), "present": 0, "relocated": 0, "alternate": 0, "contextual": 0, "skipped": 0});
    let bump = |r: &mut Value, k: &str, n: usize| {
        let v = r[k].as_u64().unwrap_or(0) + n as u64;
        r[k] = json!(v);
    };
    if items.is_empty() {
        let status = deployed_status(ctx, &run.id).await?;
        set_status(ctx, &run.id, &site.id, &status, None).await?;
        report["progress"] = json!({"phase": "done", "pct": 100});
        return Ok(report);
    }
    job.info(format!("repair: {} items", items.len())).await;
    if !site.has_wp_credentials() {
        let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
        let n = ids.len();
        ctx.db
            .write(move |t| {
                for id in &ids {
                    t.execute(
                        "UPDATE link_edges SET status='failed', failure_kind='skipped', error='no WP credentials' WHERE id=?1 AND status IN ('failed','approved')",
                        [id],
                    )
                    .map_err(err)?;
                    note_attempt(t, id, &json!({"step": "skip", "reason": "no WP credentials"}), false)?;
                }
                Ok(())
            })
            .await?;
        bump(&mut report, "skipped", n);
        let status = deployed_status(ctx, &run.id).await?;
        set_status(ctx, &run.id, &site.id, &status, None).await?;
        return Ok(report);
    }
    let writer =
        WpWriter::new(site.wp_http_client(ctx)?, site.wp_site(true)?).map_err(JobError::from)?;
    // 1. applied but not seen: is the link in the raw content after all?
    let mut by_post: BTreeMap<String, Vec<&RepairItem>> = BTreeMap::new();
    for it in items.iter().filter(|i| i.status == "applied") {
        by_post.entry(it.source.clone()).or_default().push(it);
    }
    let mut present_ids: Vec<String> = Vec::new();
    let mut absent_ids: Vec<String> = Vec::new();
    for (_, group) in by_post {
        job.checkpoint().await?;
        let Some(first) = group.first() else { continue };
        let raw = match first.wp_id.and_then(|i| u64::try_from(i).ok()) {
            Some(wp) => writer.get_post_raw(wp, &first.post_type).await.ok(),
            None => None,
        };
        for it in group {
            match &raw {
                Some(r) if count_links(&r.raw, &it.target_url, &it.anchor) > 0 => {
                    present_ids.push(it.id.clone())
                }
                Some(_) => absent_ids.push(it.id.clone()),
                None => {}
            }
        }
    }
    let retry: Vec<String> = items
        .iter()
        .filter(|i| i.status != "applied")
        .map(|i| i.id.clone())
        .chain(absent_ids.iter().cloned())
        .collect();
    let (p2, a2, r2) = (present_ids.clone(), absent_ids.clone(), retry.clone());
    ctx.db
        .write(move |t| {
            let now = sf_core::now_rfc3339();
            for id in &p2 {
                t.execute(
                    "UPDATE link_edges SET verify_present=1, verified_at=?2 WHERE id=?1",
                    params![id, now],
                )
                .map_err(err)?;
                note_attempt(t, id, &json!({"step": "present", "ok": true}), false)?;
            }
            for id in &a2 {
                t.execute(
                    "UPDATE link_edges SET status='approved', applied_at=NULL, link_start=NULL, verified_at=NULL,
                       verify_present=NULL, error=NULL, failure_kind=NULL WHERE id=?1 AND status='applied'",
                    [id],
                )
                .map_err(err)?;
            }
            for id in &r2 {
                t.execute(
                    "UPDATE link_edges SET status='approved', error=NULL, failure_kind=NULL WHERE id=?1 AND status='failed'",
                    [id],
                )
                .map_err(err)?;
            }
            Ok(())
        })
        .await?;
    bump(&mut report, "present", present_ids.len());
    // 2. re-locate (paragraph hash, then anchor search) + native insert
    job.progress("relocate", 20.0).await;
    let no_llm = run_inject_opts(run, &site, false, true);
    if !retry.is_empty() {
        inject_edges(ctx, job, &site, &retry, no_llm.clone()).await?;
    }
    let mut st = statuses(ctx, &retry).await?;
    let mut repaired: Vec<String> = Vec::new();
    let mut left: Vec<String> = Vec::new();
    for id in &retry {
        if st.get(id).is_some_and(|s| s.0 == "applied") {
            repaired.push(id.clone());
        } else {
            left.push(id.clone());
        }
    }
    bump(&mut report, "relocated", repaired.len());
    {
        let rep = repaired.clone();
        ctx.db
            .write(move |t| {
                for id in &rep {
                    note_attempt(t, id, &json!({"step": "relocate", "ok": true}), true)?;
                }
                Ok(())
            })
            .await?;
    }
    // 3. alternates from the anchor picker
    let by_id: HashMap<&str, &RepairItem> = items.iter().map(|i| (i.id.as_str(), i)).collect();
    for round in 0..MAX_ALTERNATES {
        job.checkpoint().await?;
        let mut tries: Vec<(String, String, String)> = Vec::new(); // (id, alt, original)
        for id in &left {
            if let Some(it) = by_id.get(id.as_str())
                && let Some(alt) = it
                    .alternates
                    .iter()
                    .filter(|a| !a.eq_ignore_ascii_case(&it.anchor))
                    .nth(round)
            {
                tries.push((id.clone(), alt.clone(), it.anchor.clone()));
            }
        }
        if tries.is_empty() {
            break;
        }
        let t2 = tries.clone();
        let started: Vec<String> = ctx
            .db
            .write(move |t| {
                let mut ok = Vec::new();
                for (id, alt, _) in &t2 {
                    match t.execute(
                        "UPDATE link_edges SET anchor=?2, status='approved', error=NULL, failure_kind=NULL WHERE id=?1 AND status IN ('approved','failed')",
                        params![id, alt],
                    ) {
                        Ok(n) if n > 0 => ok.push(id.clone()),
                        Ok(_) => {}
                        Err(rusqlite::Error::SqliteFailure(f, _))
                            if f.code == rusqlite::ErrorCode::ConstraintViolation => {}
                        Err(e) => return Err(err(e)),
                    }
                }
                Ok(ok)
            })
            .await?;
        if started.is_empty() {
            continue;
        }
        job.info(format!(
            "repair: alternate anchors round {} for {} items",
            round + 1,
            started.len()
        ))
        .await;
        inject_edges(ctx, job, &site, &started, no_llm.clone()).await?;
        st = statuses(ctx, &started).await?;
        let mut done_now = Vec::new();
        for id in &started {
            if st.get(id).is_some_and(|s| s.0 == "applied") {
                done_now.push(id.clone());
            }
        }
        bump(&mut report, "alternate", done_now.len());
        let (dn, t3) = (done_now.clone(), tries.clone());
        ctx.db
            .write(move |t| {
                for (id, alt, orig) in &t3 {
                    let ok = dn.contains(id);
                    note_attempt(
                        t,
                        id,
                        &json!({"step": "alternate", "anchor": alt, "ok": ok}),
                        ok,
                    )?;
                    if !ok {
                        // put the original anchor back (keeps the item recognisable)
                        let _ = t.execute(
                            "UPDATE link_edges SET anchor=?2 WHERE id=?1 AND status<>'applied'",
                            params![id, orig],
                        );
                    }
                }
                Ok(())
            })
            .await?;
        repaired.extend(done_now.iter().cloned());
        left.retain(|id| !done_now.contains(id));
        // failed ones go back to approved for the next round
        let l2 = left.clone();
        ctx.db
            .write(move |t| {
                for id in &l2 {
                    t.execute(
                        "UPDATE link_edges SET status='approved' WHERE id=?1 AND status='failed'",
                        [id],
                    )
                    .map_err(err)?;
                }
                Ok(())
            })
            .await?;
    }
    // 4. contextual (AI) when the run allows it
    if run.options.strategies.contextual && ctx.ai.is_some() && !left.is_empty() {
        job.progress("contextual", 70.0).await;
        let l2 = left.clone();
        ctx.db
            .write(move |t| {
                for id in &l2 {
                    t.execute(
                        "UPDATE link_edges SET insert_module='context', status='approved', error=NULL WHERE id=?1 AND status IN ('approved','failed')",
                        [id],
                    )
                    .map_err(err)?;
                }
                Ok(())
            })
            .await?;
        inject_edges(
            ctx,
            job,
            &site,
            &left,
            run_inject_opts(run, &site, true, true),
        )
        .await?;
        st = statuses(ctx, &left).await?;
        let done_now: Vec<String> = left
            .iter()
            .filter(|id| st.get(*id).is_some_and(|s| s.0 == "applied"))
            .cloned()
            .collect();
        bump(&mut report, "contextual", done_now.len());
        let dn = done_now.clone();
        ctx.db
            .write(move |t| {
                for id in &dn {
                    note_attempt(t, id, &json!({"step": "contextual", "ok": true}), true)?;
                }
                Ok(())
            })
            .await?;
        repaired.extend(done_now.iter().cloned());
        left.retain(|id| !done_now.contains(id));
    }
    // 5. the rest is skipped with a reason
    let st = statuses(ctx, &left).await?;
    let skips: Vec<(String, String)> = left
        .iter()
        .map(|id| {
            let (e, k) = st
                .get(id)
                .map(|s| (s.1.clone(), s.2.clone()))
                .unwrap_or((None, None));
            let orig = by_id.get(id.as_str());
            let e = e.or_else(|| orig.and_then(|o| o.error.clone()));
            let k = k.or_else(|| orig.and_then(|o| o.failure_kind.clone()));
            (id.clone(), skip_reason(e.as_deref(), k.as_deref()))
        })
        .collect();
    bump(&mut report, "skipped", skips.len());
    ctx.db
        .write(move |t| {
            for (id, reason) in &skips {
                t.execute(
                    "UPDATE link_edges SET status='failed', failure_kind='skipped', error=?2 WHERE id=?1 AND status IN ('approved','failed')",
                    params![id, reason],
                )
                .map_err(err)?;
                note_attempt(t, id, &json!({"step": "skip", "reason": reason}), false)?;
            }
            Ok(())
        })
        .await?;
    // verify what was repaired
    if run.options.verify && !repaired.is_empty() {
        job.progress("verify", 95.0).await;
        let v = verify_edges(ctx, job, &site, &run.id, Some(&repaired)).await?;
        report["verify"] = v;
    }
    report["repaired"] = json!(repaired.len());
    let status = deployed_status(ctx, &run.id).await?;
    set_status(ctx, &run.id, &site.id, &status, None).await?;
    record_event(
        &ctx.db,
        ctx.events.as_ref(),
        &run.id,
        &site.id,
        "repaired",
        "ok",
        compact_report(&report),
    )
    .await;
    report["status"] = json!(status);
    report["progress"] = json!({"phase": "done", "pct": 100});
    Ok(report)
}

fn compact_report(r: &Value) -> Value {
    let mut o = serde_json::Map::new();
    for k in [
        "items",
        "present",
        "relocated",
        "alternate",
        "contextual",
        "skipped",
        "repaired",
    ] {
        if let Some(v) = r.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    Value::Object(o)
}

// ---------------------------------------------------------------------------------------------
// undo
// ---------------------------------------------------------------------------------------------

async fn step_undo(ctx: &Arc<JobContext>, job: &Job, run: &RunInfo) -> JobResult<Value> {
    let site = load_site(ctx, &run.site_id).await?;
    set_job_col(ctx, &run.id, "undo_job_id", &job.id).await;
    reconcile_for_job(ctx, job, &site).await?;
    let ids = run_edge_ids(ctx, &run.id, "status='applied'").await?;
    let stats = super::undo::undo_edges(ctx, job, &site, &ids, false, false).await?;
    // approved-but-not-written items of this run are released too
    let rid = run.id.clone();
    ctx.db
        .write(move |t| {
            t.execute(
                "UPDATE link_edges SET status='suggested', run_selected=0 WHERE run_id=?1 AND status='approved'",
                [&rid],
            )
            .map_err(err)
        })
        .await?;
    let still: i64 = {
        let rid = run.id.clone();
        ctx.db
            .read(move |c| {
                c.query_row(
                    "SELECT count(*) FROM link_edges WHERE run_id=?1 AND status='applied'",
                    [&rid],
                    |r| r.get(0),
                )
                .map_err(err)
            })
            .await?
    };
    let status = if still == 0 {
        "undone".to_string()
    } else {
        deployed_status(ctx, &run.id).await?
    };
    set_status(ctx, &run.id, &site.id, &status, None).await?;
    Ok(json!({"undo": stats, "status": status, "progress": {"phase": "done", "pct": 100}}))
}

// ---------------------------------------------------------------------------------------------
// notifications + Auto-Linker
// ---------------------------------------------------------------------------------------------

async fn run_stats(ctx: &JobContext, run_id: &str) -> Value {
    let id = run_id.to_string();
    ctx.db
        .read(move |c| store::compute_stats(c, &id))
        .await
        .unwrap_or_else(|_| json!({}))
}

async fn notify_done(ctx: &JobContext, run: &RunInfo, status: &str, what: &str) {
    let Some(n) = &ctx.notifier else { return };
    let s = run_stats(ctx, &run.id).await;
    let num = |k: &str| s[k].as_i64().unwrap_or(0);
    n.notify(
        Notification::new(
            events::DONE,
            format!("SiloForge link run {what}"),
            format!(
                "{status}: {} applied, {} failed, {} not present, {} skipped, {} repaired",
                num("applied"),
                num("failed"),
                num("not_present"),
                num("skipped"),
                num("repaired")
            ),
        )
        .site(Some(&run.site_id))
        .data(json!({"link_run_id": run.id, "status": status, "stats": compact(&s), "auto": run.auto})),
    );
}

/// Applied links of Auto-Linker runs of this site today (UTC).
async fn applied_today(ctx: &JobContext, site_id: &str) -> JobResult<i64> {
    let sid = site_id.to_string();
    let day = chrono::Utc::now().format("%Y-%m-%dT00:00:00").to_string();
    Ok(ctx
        .db
        .read(move |c| {
            c.query_row(
                "SELECT count(*) FROM link_edges e JOIN link_runs r ON r.id=e.run_id
                 WHERE r.site_id=?1 AND r.auto=1 AND e.status='applied' AND e.applied_at>=?2",
                params![sid, day],
                |r| r.get(0),
            )
            .map_err(err)
        })
        .await?)
}

/// Scheduler entry (`{auto: true}`): one Auto-Linker run for the site.
async fn auto_run(ctx: &Arc<JobContext>, job: &Job) -> JobResult<Value> {
    let site = load_site(ctx, job.site()?).await?;
    let Some(cfg) = site.settings.get("auto_linker") else {
        return Ok(
            json!({"skipped": "auto_linker is not configured", "progress": {"phase": "done", "pct": 100}}),
        );
    };
    let cfg = match parse_auto_linker(cfg) {
        Ok(c) => c,
        Err(e) => {
            return Err(JobError::fatal(Error::bad(e)));
        }
    };
    // `manual` = "run now" from the API/MCP: allowed while disabled (e.g. a first dry run)
    if !cfg.enabled && !job.payload_bool("manual", "manual") {
        return Ok(
            json!({"skipped": "auto_linker is disabled", "progress": {"phase": "done", "pct": 100}}),
        );
    }
    let today = applied_today(ctx, &site.id).await?;
    let left = cfg.max_per_day - today;
    if left <= 0 {
        job.info(format!(
            "auto-linker: daily cap reached ({today}/{})",
            cfg.max_per_day
        ))
        .await;
        return Ok(
            json!({"skipped": "daily cap reached", "applied_today": today, "progress": {"phase": "done", "pct": 100}}),
        );
    }
    let mut opts = cfg.options.clone();
    opts.filters.max_total = opts.filters.max_total.min(left).max(1);
    let run_id = match create_run(&ctx.db, &site.id, "", &opts, "auto-linker", true).await {
        Ok(id) => id,
        Err(Error::Conflict(m)) => {
            return Ok(json!({"skipped": m, "progress": {"phase": "done", "pct": 100}}));
        }
        Err(e) => return Err(e.into()),
    };
    record_event(
        &ctx.db,
        ctx.events.as_ref(),
        &run_id,
        &site.id,
        "created",
        "ok",
        json!({"auto": true, "live": cfg.live}),
    )
    .await;
    job.merge_stats(json!({"link_run_id": run_id, "link_run_step": "auto"}))
        .await;
    let run = load_run(&ctx.db, &run_id).await?;
    let planned = step_plan(ctx, job, &run).await;
    let planned = after_step(ctx, &run, "plan", planned).await?;
    let stats = run_stats(ctx, &run_id).await;
    let selected = stats["selected"].as_i64().unwrap_or(0);
    if !cfg.live || selected == 0 {
        if let Some(n) = &ctx.notifier {
            n.notify(
                Notification::new(
                    events::READY,
                    "SiloForge Auto-Linker: links ready",
                    format!("{selected} links ready to deploy"),
                )
                .site(Some(&site.id))
                .job(Some(&job.id))
                .data(json!({"link_run_id": run_id, "selected": selected, "live": cfg.live})),
            );
        }
        return Ok(
            json!({"link_run_id": run_id, "plan": planned, "live": cfg.live, "progress": {"phase": "ready", "pct": 100}}),
        );
    }
    let run = load_run(&ctx.db, &run_id).await?;
    let dep = step_deploy(ctx, job, &run).await;
    let dep = after_step(ctx, &run, "deploy", dep).await?;
    let run = load_run(&ctx.db, &run_id).await?;
    let rep = step_repair(ctx, job, &run).await;
    let rep = after_step(ctx, &run, "repair", rep).await?;
    let run = load_run(&ctx.db, &run_id).await?;
    notify_done(ctx, &run, &run.status, "deployed (Auto-Linker)").await;
    // auto-pause on failure rate
    let s = run_stats(ctx, &run_id).await;
    let bad = s["failed"].as_i64().unwrap_or(0)
        + s["not_present"].as_i64().unwrap_or(0)
        + s["skipped"].as_i64().unwrap_or(0);
    let sel = s["selected"].as_i64().unwrap_or(0).max(1);
    let rate = bad as f64 / sel as f64;
    let mut paused = false;
    if rate > cfg.pause_on_failure_rate {
        paused = true;
        let reason = format!(
            "failure rate {:.0}% > {:.0}% in run {run_id}",
            rate * 100.0,
            cfg.pause_on_failure_rate * 100.0
        );
        pause_auto_linker(ctx, &site.id, &reason).await?;
        record_event(
            &ctx.db,
            ctx.events.as_ref(),
            &run_id,
            &site.id,
            "auto_linker paused",
            "error",
            json!({"reason": reason}),
        )
        .await;
        if let Some(n) = &ctx.notifier {
            n.notify(
                Notification::new(
                    events::AUTO_PAUSED,
                    "SiloForge Auto-Linker paused",
                    reason.clone(),
                )
                .level("warn")
                .site(Some(&site.id))
                .data(json!({"link_run_id": run_id, "failure_rate": rate})),
            );
        }
    }
    Ok(json!({
        "link_run_id": run_id, "plan": planned, "deploy": dep, "repair": rep,
        "failure_rate": rate, "paused": paused, "progress": {"phase": "done", "pct": 100},
    }))
}

/// `auto_linker.enabled = false` (+ reason) and the schedule row off.
async fn pause_auto_linker(ctx: &JobContext, site_id: &str, reason: &str) -> JobResult<()> {
    let (sid, r) = (site_id.to_string(), reason.to_string());
    ctx.db
        .write(move |t| {
            t.execute(
                "UPDATE sites SET settings=json_set(CASE WHEN json_valid(settings) THEN settings ELSE '{}' END,
                    '$.auto_linker.enabled', json('false'), '$.auto_linker.paused_reason', ?2) WHERE id=?1",
                params![sid, r],
            )
            .map_err(err)?;
            t.execute(
                "UPDATE site_schedules SET active=0, updated_at=?2 WHERE site_id=?1 AND job_kind='link_run'",
                params![sid, sf_core::now_rfc3339()],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// Keep the `link_run` schedule row in step with the site's `auto_linker` setting (call after
/// the setting changed). Disabled / absent = row kept but inactive.
pub fn sync_auto_linker_schedule(
    t: &rusqlite::Connection,
    site_id: &str,
    cfg: Option<&AutoLinker>,
) -> sf_core::Result<()> {
    let now = sf_core::now_rfc3339();
    match cfg {
        Some(a) if a.enabled => {
            t.execute(
                "INSERT INTO site_schedules(id, site_id, job_kind, cron, tz, active, payload, next_run_at, updated_at)
                 VALUES(?1,?2,'link_run',?3,?4,1,'{\"auto\":true}',NULL,?5)
                 ON CONFLICT(site_id, job_kind) DO UPDATE SET
                   next_run_at=CASE WHEN site_schedules.cron<>excluded.cron OR site_schedules.tz<>excluded.tz OR site_schedules.active=0
                                    THEN NULL ELSE site_schedules.next_run_at END,
                   cron=excluded.cron, tz=excluded.tz, active=1, payload=excluded.payload, updated_at=excluded.updated_at",
                params![sf_core::new_id(), site_id, a.cron, a.tz, now],
            )
            .map_err(err)?;
        }
        _ => {
            t.execute(
                "UPDATE site_schedules SET active=0, updated_at=?2 WHERE site_id=?1 AND job_kind='link_run'",
                params![site_id, now],
            )
            .map_err(err)?;
        }
    }
    Ok(())
}

/// Enqueue an Auto-Linker run now (ignores the schedule, honours enabled + daily cap).
pub async fn enqueue_auto_now(
    queue: &JobQueue,
    site_id: &str,
    requested_by: &str,
) -> sf_core::Result<String> {
    let mut job = NewJob::new(
        sf_core::types::JobKind::LinkRun,
        Some(site_id),
        json!({"auto": true, "manual": true}),
    )
    .by(requested_by);
    job.dedupe = Some(true);
    Ok(queue.enqueue(job).await?.job_run_id)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn strategy_and_slug_helpers() {
        assert_eq!(
            derive_strategy("keyword_linker", None, &json!({})),
            "keyword_rules"
        );
        assert_eq!(
            derive_strategy("link_run", None, &json!({"strategy": "fix_broken"})),
            "fix_broken"
        );
        assert_eq!(derive_strategy("plan", Some("reverse"), &json!({})), "silo");
        assert_eq!(derive_strategy("plan", None, &json!({})), "semantic");
        let a = slug_tokens("https://x.com/bike-lock/");
        let b = slug_tokens("https://x.com/bike-locks/");
        assert_eq!(a, b);
        assert!(jaccard(&a, &slug_tokens("/road-bike-saddles/")) < 0.34);
        assert_eq!(jaccard(&a, &HashSet::new()), 0.0);
        assert_eq!(
            skip_reason(None, Some("content_gap")),
            "anchor text no longer in post"
        );
        assert_eq!(skip_reason(None, Some("conflict")), "post locked/changed");
        assert!(step_rule("deploy").is_some() && step_rule("x").is_none());
    }
}
