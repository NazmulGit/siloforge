//! The job runner: claims queued jobs (bounded concurrency; per-site exclusivity is enforced by
//! `JobQueue::claim_next`), runs each pipeline in its own task with a heartbeat, a per-kind hard
//! timeout and cooperative cancellation, records resources, logs and notifications, and survives
//! panicking pipelines. Stale `running` rows are reaped at boot and periodically.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use sf_core::types::{JobKind, JobStatus};
use sf_ops::monitor::JobMeter;
use sf_ops::notify::{Notification, events};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::{AbortHandle, JoinHandle};

use crate::ctx::{Job, JobContext, JobError};
use crate::pipelines::{self, PipelineFn};
use crate::queue::ClaimedJob;

/// Hard timeout per job kind (crawl 2 h, plan 2 h, inject 1 h, link_health 3 h, audit 6 h,
/// everything else 30 min).
pub fn kind_timeout(kind: &JobKind) -> Duration {
    let mins = match kind {
        JobKind::Crawl | JobKind::Plan => 120,
        JobKind::Inject => 60,
        JobKind::LinkHealth => 180,
        JobKind::Audit => 360,
        _ => 30,
    };
    Duration::from_secs(mins * 60)
}

/// When `resources.low_priority_jobs` is on, make every compute-pool thread run below normal
/// OS priority (Windows `BELOW_NORMAL`; best effort elsewhere) so bulk jobs never starve the
/// desktop. Call once at startup **before** `sf_engine::pool::init`. Returns true when installed.
pub fn lower_compute_priority(cfg: &sf_core::Config) -> bool {
    if !cfg.resources.low_priority_jobs {
        return false;
    }
    sf_engine::pool::set_thread_start_handler(|i| {
        let Ok(v) = thread_priority::ThreadPriorityValue::try_from(30u8) else {
            return;
        };
        if let Err(e) = thread_priority::set_current_thread_priority(
            thread_priority::ThreadPriority::Crossplatform(v),
        ) {
            tracing::debug!(thread = i, error = ?e, "could not lower compute thread priority");
        }
    })
}

/// Runner tuning. `from_config` gives production values.
#[derive(Clone)]
pub struct RunnerOptions {
    /// Jobs at once. With `follow_live` the live `resources.max_concurrent_jobs` is used instead
    /// (read before every claim, so a settings change applies without a restart).
    pub max_concurrent: usize,
    /// Read `resources.max_concurrent_jobs` from the live config before each claim (production).
    pub follow_live: bool,
    pub heartbeat: Duration,
    /// Idle wait between empty claims (an enqueue wakes the runner earlier).
    pub poll: Duration,
    /// Running rows without a heartbeat for this long are reaped (periodic sweep).
    pub stale_after: Duration,
    /// How long a cancelled / timed-out pipeline may take to reach a checkpoint before abort.
    pub cancel_grace: Duration,
    /// Replace every kind's timeout (tests).
    pub timeout_override: Option<Duration>,
    /// Pipeline overrides by kind string (tests, custom kinds).
    pub overrides: HashMap<String, PipelineFn>,
}

impl RunnerOptions {
    pub fn from_config(cfg: &sf_core::Config) -> Self {
        Self {
            max_concurrent: cfg.resources.max_concurrent_jobs.clamp(1, 64),
            follow_live: true,
            heartbeat: Duration::from_secs(15),
            poll: Duration::from_secs(5),
            stale_after: Duration::from_secs(300),
            cancel_grace: Duration::from_secs(30),
            timeout_override: None,
            overrides: HashMap::new(),
        }
    }
}

struct RunningJob {
    kind: JobKind,
    abort: AbortHandle,
}

/// Handle to a started runner.
pub struct RunnerHandle {
    stop: watch::Sender<bool>,
    shutting_down: Arc<AtomicBool>,
    running: Arc<Mutex<HashMap<String, RunningJob>>>,
    join: JoinHandle<()>,
}

impl RunnerHandle {
    /// Ids of the jobs running right now.
    pub fn running(&self) -> Vec<String> {
        let mut v: Vec<String> = self.running.lock().keys().cloned().collect();
        v.sort();
        v
    }

    /// Graceful shutdown: stop claiming, wait up to `grace` for running jobs, then abort them and
    /// leave their rows `running` for the boot reaper (no partial "failed" written).
    pub async fn shutdown(self, grace: Duration) {
        let _ = self.stop.send(true);
        let deadline = Instant::now() + grace;
        while !self.running.lock().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.shutting_down.store(true, Ordering::SeqCst);
        let left: Vec<(String, JobKind, AbortHandle)> = self
            .running
            .lock()
            .iter()
            .map(|(id, r)| (id.clone(), r.kind.clone(), r.abort.clone()))
            .collect();
        for (id, kind, abort) in left {
            tracing::warn!(job = %id, %kind, "shutdown: aborting running job (row left for the reaper)");
            abort.abort();
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), self.join).await;
    }
}

pub struct Runner;

/// Upper bound for concurrent jobs whatever the settings say.
pub const MAX_CONCURRENT_CEILING: usize = 64;

/// Jobs allowed at once right now.
fn concurrency_limit(ctx: &JobContext, opts: &RunnerOptions) -> usize {
    if opts.follow_live {
        ctx.live
            .get()
            .resources
            .max_concurrent_jobs
            .clamp(1, MAX_CONCURRENT_CEILING)
    } else {
        opts.max_concurrent.clamp(1, MAX_CONCURRENT_CEILING)
    }
}

impl Runner {
    /// Start with production options.
    pub fn start(ctx: Arc<JobContext>) -> RunnerHandle {
        let opts = RunnerOptions::from_config(&ctx.cfg);
        Self::start_with(ctx, opts)
    }

    pub fn start_with(ctx: Arc<JobContext>, opts: RunnerOptions) -> RunnerHandle {
        let (stop, stop_rx) = watch::channel(false);
        let shutting_down = Arc::new(AtomicBool::new(false));
        let running: Arc<Mutex<HashMap<String, RunningJob>>> = Arc::new(Mutex::new(HashMap::new()));
        let opts = Arc::new(opts);
        // The semaphore holds the hard ceiling; the live limit is checked before each claim.
        let ceiling = if opts.follow_live {
            MAX_CONCURRENT_CEILING
        } else {
            opts.max_concurrent.clamp(1, MAX_CONCURRENT_CEILING)
        };
        let sem = Arc::new(Semaphore::new(ceiling));
        let (sd, rn) = (shutting_down.clone(), running.clone());
        let join = tokio::spawn(async move {
            boot(&ctx).await;
            // R8: the claim loop is supervised; a panic restarts it (running jobs are untouched,
            // the semaphore and running map are shared across restarts).
            let db = ctx.db.clone();
            let h = sf_ops::supervise::spawn("job-runner", Some(db), move || {
                main_loop(
                    ctx.clone(),
                    opts.clone(),
                    sem.clone(),
                    stop_rx.clone(),
                    sd.clone(),
                    rn.clone(),
                )
            });
            let _ = h.await;
        });
        RunnerHandle {
            stop,
            shutting_down,
            running,
            join,
        }
    }
}

/// Once per process, before the first claim: reap rows left `running` by a previous process
/// (only safe because `serve` holds the instance lock, R2), and queue a reconcile-only inject for
/// every site with an unfinished WordPress write in `inject_journal` (R4).
async fn boot(ctx: &Arc<JobContext>) {
    match ctx.queue.reap_stale(0).await {
        Ok(0) => {}
        Ok(n) => tracing::warn!(n, "reaped jobs left running by a previous process"),
        Err(e) => tracing::error!(error = %e, "boot reaper failed"),
    }
    match crate::pipelines::inject::journal_sites(&ctx.db).await {
        Ok(sites) => {
            for site in sites {
                let job = crate::queue::NewJob::new(
                    JobKind::Inject,
                    Some(&site),
                    json!({"reconcile_only": true}),
                )
                .by("boot-reconcile");
                match ctx.queue.enqueue(job).await {
                    Ok(e) => {
                        tracing::warn!(site = %site, job = %e.job_run_id, "unfinished WordPress writes found; reconcile job queued")
                    }
                    Err(e) => {
                        tracing::error!(site = %site, error = %e, "could not queue reconcile job")
                    }
                }
            }
        }
        Err(e) => tracing::error!(error = %e, "inject journal scan failed"),
    }
}

async fn main_loop(
    ctx: Arc<JobContext>,
    opts: Arc<RunnerOptions>,
    sem: Arc<Semaphore>,
    mut stop: watch::Receiver<bool>,
    shutting_down: Arc<AtomicBool>,
    running: Arc<Mutex<HashMap<String, RunningJob>>>,
) -> sf_ops::supervise::Exit {
    let mut last_reap = Instant::now();
    loop {
        if *stop.borrow() {
            break;
        }
        if last_reap.elapsed() >= Duration::from_secs(60) {
            last_reap = Instant::now();
            let secs = i64::try_from(opts.stale_after.as_secs()).unwrap_or(300);
            if let Ok(n) = ctx.queue.reap_stale(secs).await
                && n > 0
            {
                tracing::warn!(n, "reaped stale running jobs");
            }
        }
        let permit = tokio::select! {
            p = sem.clone().acquire_owned() => match p { Ok(p) => p, Err(_) => break },
            _ = stop.changed() => break,
        };
        // Live limit (v2.1): wait while as many jobs run as currently allowed; a finishing job
        // or a settings change (picked up on the next poll) lets the loop continue.
        if running.lock().len() >= concurrency_limit(&ctx, &opts) {
            drop(permit);
            tokio::select! {
                _ = ctx.queue.wait_for_work(opts.poll) => {}
                _ = stop.changed() => break,
            }
            continue;
        }
        let allow_heavy = !machine_busy(&ctx);
        match ctx.queue.claim_next(allow_heavy).await {
            Ok(Some(job)) => spawn_job(&ctx, &opts, job, permit, &shutting_down, &running),
            Ok(None) => {
                drop(permit);
                tokio::select! {
                    _ = ctx.queue.wait_for_work(opts.poll) => {}
                    _ = stop.changed() => break,
                }
            }
            Err(e) => {
                drop(permit);
                tracing::error!(error = %e, "claiming a job failed");
                tokio::select! {
                    _ = tokio::time::sleep(opts.poll) => {}
                    _ = stop.changed() => break,
                }
            }
        }
    }
    tracing::info!("job runner stopped claiming");
    sf_ops::supervise::Exit::Stop
}

/// True when heavy kinds must wait: machine CPU above the live `resources.defer_when_cpu_above`,
/// or this process above its `resources.max_memory_mb` soft cap (v2.1).
fn machine_busy(ctx: &JobContext) -> bool {
    let Some(m) = ctx.monitor.as_ref() else {
        return false;
    };
    if m.memory_pressure() {
        return true;
    }
    let limit = ctx.live.get().resources.defer_when_cpu_above;
    if !(limit > 0.0 && limit.is_finite()) {
        return false;
    }
    m.latest().is_some_and(|s| s.cpu_pct > limit)
}

fn spawn_job(
    ctx: &Arc<JobContext>,
    opts: &Arc<RunnerOptions>,
    claimed: ClaimedJob,
    permit: OwnedSemaphorePermit,
    shutting_down: &Arc<AtomicBool>,
    running: &Arc<Mutex<HashMap<String, RunningJob>>>,
) {
    let job = Arc::new(Job::new(claimed, ctx.queue.clone()));
    let id = job.id.clone();
    let kind = job.kind.clone();
    let pipeline: Option<PipelineFn> = opts.overrides.get(kind.as_str()).cloned();
    let (c2, j2) = (ctx.clone(), job.clone());
    let meter = JobMeter::start();
    let task: JoinHandle<Result<Value, JobError>> = tokio::spawn(async move {
        match pipeline {
            Some(p) => p(c2, j2).await,
            None => pipelines::dispatch(c2, j2).await,
        }
    });
    running.lock().insert(
        id.clone(),
        RunningJob {
            kind: kind.clone(),
            abort: task.abort_handle(),
        },
    );
    let (ctx, opts, shutting_down, running) = (
        ctx.clone(),
        opts.clone(),
        shutting_down.clone(),
        running.clone(),
    );
    tokio::spawn(async move {
        supervise(&ctx, &opts, job, task, meter, &shutting_down).await;
        running.lock().remove(&id);
        drop(permit);
        ctx.queue.notify();
    });
}

async fn supervise(
    ctx: &Arc<JobContext>,
    opts: &RunnerOptions,
    job: Arc<Job>,
    mut task: JoinHandle<Result<Value, JobError>>,
    meter: JobMeter,
    shutting_down: &AtomicBool,
) {
    let id = job.id.clone();
    let kind = job.kind.clone();
    let started = Instant::now();
    let timeout = opts.timeout_override.unwrap_or_else(|| kind_timeout(&kind));
    let deadline = started + timeout;
    let cancel = job.cancel_flag();
    let mut cancel_at: Option<Instant> = None;
    let mut timed_out = false;
    tracing::info!(job = %id, %kind, site = ?job.site_id, attempt = job.attempts, "job started");
    let owner = job_owner(ctx, &job).await;
    let owner = owner.as_deref();
    job_event(
        ctx,
        &job,
        owner,
        "started",
        "ok",
        None,
        json!({"attempt": job.attempts}),
    );
    job.info(format!("{kind} started (attempt {})", job.attempts))
        .await;
    let result = loop {
        let now = Instant::now();
        let tick = opts
            .heartbeat
            .min(deadline.saturating_duration_since(now))
            .max(Duration::from_millis(20));
        tokio::select! {
            r = &mut task => break r,
            _ = tokio::time::sleep(tick) => {
                let _ = ctx.queue.heartbeat(&id).await;
                if !cancel.load(Ordering::Relaxed) && ctx.queue.is_cancelled(&id).await.unwrap_or(false) {
                    cancel.store(true, Ordering::Relaxed);
                }
                if !timed_out && Instant::now() >= deadline {
                    timed_out = true;
                    cancel.store(true, Ordering::Relaxed);
                    job.warn(format!("hard timeout after {}s; stopping", timeout.as_secs())).await;
                }
                if cancel.load(Ordering::Relaxed) {
                    let at = *cancel_at.get_or_insert_with(Instant::now);
                    if at.elapsed() >= opts.cancel_grace {
                        task.abort();
                    }
                }
            }
        }
    };
    let resources = meter.finish();
    if shutting_down.load(Ordering::SeqCst) && !matches!(result, Ok(Ok(_))) {
        tracing::warn!(job = %id, "job interrupted by shutdown; row left for the reaper");
        return;
    }
    let secs = started.elapsed().as_secs_f64();
    match result {
        Ok(Ok(stats)) => {
            if let Err(e) = ctx.queue.finish(&id, stats.clone(), resources).await {
                tracing::error!(job = %id, error = %e, "could not record job result");
            }
            let status = ctx.queue.get(&id).await.ok();
            let done = status
                .as_ref()
                .and_then(|v| v.get("status"))
                .and_then(Value::as_str)
                == Some("done");
            job.info(format!("{kind} finished in {secs:.1}s")).await;
            tracing::info!(job = %id, %kind, secs, "job done");
            if done {
                notify_done(ctx, &job, &stats);
            }
            job_event(
                ctx,
                &job,
                owner,
                "done",
                "ok",
                Some(started.elapsed()),
                compact_stats(&stats),
            );
        }
        Ok(Err(e)) if e.is_cancelled() && !timed_out => {
            let _ = ctx.queue.record_cancelled_resources(&id, resources).await;
            job.info("cancelled").await;
            tracing::info!(job = %id, %kind, "job cancelled");
            job_event(
                ctx,
                &job,
                owner,
                "cancelled",
                "ok",
                Some(started.elapsed()),
                Value::Null,
            );
        }
        Ok(Err(e)) => {
            let msg = if timed_out {
                format!("timed out after {}s", timeout.as_secs())
            } else {
                e.to_string()
            };
            fail(ctx, &job, owner, &msg, e.retryable && !timed_out, resources).await;
        }
        Err(join) if join.is_panic() => {
            let msg = panic_text(join);
            fail(
                ctx,
                &job,
                owner,
                &format!("pipeline panicked: {msg}"),
                false,
                resources,
            )
            .await;
        }
        Err(_) => {
            // aborted after the cancel grace period
            if timed_out {
                let msg = format!("timed out after {}s", timeout.as_secs());
                fail(ctx, &job, owner, &msg, false, resources).await;
            } else {
                let _ = ctx.queue.record_cancelled_resources(&id, resources).await;
                job.warn("aborted after cancellation (no checkpoint reached in time)")
                    .await;
                job_event(
                    ctx,
                    &job,
                    owner,
                    "cancelled",
                    "ok",
                    Some(started.elapsed()),
                    json!({"aborted": true}),
                );
            }
        }
    }
}

fn panic_text(e: tokio::task::JoinError) -> String {
    match e.try_into_panic() {
        Ok(p) => p
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".into()),
        Err(e) => e.to_string(),
    }
}

/// Owner of the job's site (event-log row owner); only looked up when the event log is on.
async fn job_owner(ctx: &JobContext, job: &Job) -> Option<String> {
    let site = job.site_id.clone()?;
    ctx.events.as_ref().filter(|e| e.is_enabled())?;
    ctx.db
        .read(move |c| {
            use rusqlite::OptionalExtension;
            c.query_row("SELECT user_id FROM sites WHERE id=?1", [&site], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()
            .map_err(sf_store::err)
        })
        .await
        .ok()
        .flatten()
        .flatten()
}

/// One `job <kind> <what>` row in the unified event log (no-op without `ctx.events`).
fn job_event(
    ctx: &JobContext,
    job: &Job,
    owner: Option<&str>,
    what: &str,
    status: &str,
    took: Option<Duration>,
    detail: Value,
) {
    let Some(log) = &ctx.events else { return };
    let mut detail = match detail {
        Value::Object(o) => o,
        Value::Null => serde_json::Map::new(),
        other => {
            let mut m = serde_json::Map::new();
            m.insert("value".into(), other);
            m
        }
    };
    detail.insert("job_id".into(), json!(job.id));
    let detail = Value::Object(detail);
    let mut ev = sf_ops::eventlog::Event::job(&format!("job {} {what}", job.kind))
        .status(status)
        .target(Some(&job.id))
        .site(job.site_id.as_deref())
        .user(owner)
        .detail(detail);
    if let Some(d) = took {
        ev = ev.duration(d);
    }
    log.record(ev);
}

/// Compact summary of a job's stats for the event log: top-level numbers, booleans and short
/// strings only (at most 20 keys).
pub(crate) fn compact_stats(stats: &Value) -> Value {
    let Some(o) = stats.as_object() else {
        return Value::Null;
    };
    let mut out = serde_json::Map::new();
    for (k, v) in o {
        if out.len() >= 20 {
            break;
        }
        let keep = match v {
            Value::Number(_) | Value::Bool(_) => true,
            Value::String(s) => s.len() <= 80,
            _ => false,
        };
        if keep {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

async fn fail(
    ctx: &Arc<JobContext>,
    job: &Job,
    owner: Option<&str>,
    msg: &str,
    retryable: bool,
    resources: Value,
) {
    job.log("error", format!("failed: {msg}")).await;
    tracing::warn!(job = %job.id, kind = %job.kind, retryable, "job failed: {msg}");
    let status = match ctx.queue.fail(&job.id, msg, retryable, resources).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(job = %job.id, error = %e, "could not record job failure");
            return;
        }
    };
    if status == JobStatus::Queued {
        job.info("will retry after backoff").await;
        job_event(
            ctx,
            job,
            owner,
            "retry",
            "error",
            None,
            json!({"error": msg}),
        );
        return;
    }
    if status != JobStatus::Failed {
        return;
    }
    job_event(
        ctx,
        job,
        owner,
        "failed",
        "error",
        None,
        json!({"error": msg}),
    );
    ctx.errors.record_bg(
        &format!("job:{}", job.kind),
        msg,
        json!({"job_run_id": job.id, "site_id": job.site_id}),
    );
    if let Some(n) = &ctx.notifier {
        n.notify(
            Notification::new(
                events::JOB_FAILED,
                format!("SiloForge {} failed", job.kind),
                msg.chars().take(500).collect::<String>(),
            )
            .site(job.site_id.as_deref())
            .job(Some(&job.id))
            .data(json!({"kind": job.kind.as_str()})),
        );
    }
}

/// Stats of the plan inside a job: the job's own stats for `plan`, the `plan` stage's stats for
/// `audit` (G2-15: audits used to report `candidates: 0` because they read the top level).
pub(crate) fn plan_stats<'a>(kind: &JobKind, stats: &'a Value) -> &'a Value {
    match kind {
        JobKind::Audit => stats
            .get("stages")
            .and_then(Value::as_array)
            .and_then(|a| a.iter().find(|s| s["stage"] == "plan"))
            .and_then(|s| s.get("stats"))
            .unwrap_or(&Value::Null),
        _ => stats,
    }
}

fn notify_done(ctx: &JobContext, job: &Job, stats: &Value) {
    let Some(n) = &ctx.notifier else { return };
    let num = |k: &str| stats.get(k).and_then(Value::as_i64).unwrap_or(0);
    let plan = plan_stats(&job.kind, stats);
    let pnum = |k: &str| plan.get(k).and_then(Value::as_i64).unwrap_or(0);
    let site = job.site_id.as_deref();
    let summary = match job.kind {
        JobKind::Crawl => format!("{} upserted, {} unchanged", num("upserted"), num("skipped")),
        JobKind::Plan | JobKind::Audit => format!(
            "{} new suggestions, {} scored",
            pnum("candidates"),
            pnum("scored")
        ),
        JobKind::Inject => format!(
            "{} applied, {} failed, {} marked",
            num("applied"),
            num("failed"),
            num("marked")
        ),
        JobKind::Undo => format!("{} reverted", num("reverted")),
        JobKind::KeywordLink => format!("{} created", num("created")),
        JobKind::LinkHealth => {
            format!("{} links checked, {} broken", num("checked"), num("broken"))
        }
        _ => String::new(),
    };
    n.notify(
        Notification::new(
            events::JOB_DONE,
            format!("SiloForge {} done", job.kind),
            summary.clone(),
        )
        .site(site)
        .job(Some(&job.id))
        .data(json!({"kind": job.kind.as_str()})),
    );
    match job.kind {
        JobKind::Plan | JobKind::Audit => {
            n.notify(
                Notification::new(events::PLAN_READY, "SiloForge plan ready", summary)
                    .site(site)
                    .job(Some(&job.id))
                    .data(json!({"candidates": pnum("candidates")})),
            );
        }
        JobKind::Inject => {
            n.notify(
                Notification::new(events::INJECT_DONE, "SiloForge injection done", summary)
                    .site(site)
                    .job(Some(&job.id))
                    .data(json!({"applied": num("applied"), "failed": num("failed")})),
            );
        }
        _ => {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// G2-15: the audit's plan.ready notification carries the plan stage's real counts.
    #[test]
    fn plan_stats_reads_the_audit_plan_stage() {
        let audit = json!({"stages": [
            {"stage": "crawl", "ok": true, "stats": {"candidates": 99}},
            {"stage": "plan", "ok": true, "stats": {"candidates": 7, "scored": 40}},
        ]});
        assert_eq!(plan_stats(&JobKind::Audit, &audit)["candidates"], 7);
        let plan = json!({"candidates": 3});
        assert_eq!(plan_stats(&JobKind::Plan, &plan)["candidates"], 3);
        assert!(plan_stats(&JobKind::Audit, &json!({})).is_null());
    }
}
