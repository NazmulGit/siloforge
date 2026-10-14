//! Cron scheduler over `site_schedules`: every tick (1 min) each active row's next fire time is
//! computed with `croner` in the row's timezone (UTC when unknown). A due row enqueues its own
//! `job_kind` with its payload (`requested_by = 'schedule'`) and gets `last_run_at`,
//! `last_job_run_id` and `next_run_at` stamped — for every kind (fixes v1 B6). Firing is guarded
//! by a compare-and-set on `next_run_at` plus queue dedupe, so a row never double-enqueues.
//! Missed runs (machine off) fire once, then the schedule continues from now.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
use sf_core::types::JobKind;
use sf_store::err;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::ctx::JobContext;
use crate::queue::NewJob;

pub struct Scheduler;

pub struct SchedulerHandle {
    stop: watch::Sender<bool>,
    join: JoinHandle<()>,
}

impl SchedulerHandle {
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.join).await;
    }
}

/// What one tick did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickReport {
    pub enqueued: Vec<String>,
    pub initialized: usize,
    pub invalid: usize,
}

impl Scheduler {
    /// Tick every minute.
    pub fn start(ctx: Arc<JobContext>) -> SchedulerHandle {
        Self::start_every(ctx, Duration::from_secs(60))
    }

    pub fn start_every(ctx: Arc<JobContext>, every: Duration) -> SchedulerHandle {
        let (stop, rx) = watch::channel(false);
        let db = ctx.db.clone();
        // R8: supervised; a panic in a tick restarts the loop instead of silently ending cron.
        let join = sf_ops::supervise::spawn("scheduler", Some(db), move || {
            let (ctx, mut rx) = (ctx.clone(), rx.clone());
            async move {
                loop {
                    if *rx.borrow() {
                        break;
                    }
                    if let Err(e) = Self::tick(&ctx, Utc::now()).await {
                        tracing::error!(error = %e, "scheduler tick failed");
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(every) => {}
                        _ = rx.changed() => break,
                    }
                }
                sf_ops::supervise::Exit::Stop
            }
        });
        SchedulerHandle { stop, join }
    }

    /// One pass over the active schedules at time `now`, plus the daily maintenance job.
    /// A failing row is logged and skipped; it never aborts the rest of the tick (R18).
    pub async fn tick(ctx: &JobContext, now: DateTime<Utc>) -> sf_core::Result<TickReport> {
        let rows: Vec<(String, String, String, String, String, String, Option<String>)> = ctx
            .db
            .read(|c| {
                let mut st = c
                    .prepare(
                        "SELECT id, site_id, job_kind, cron, tz, payload, next_run_at FROM site_schedules
                         WHERE active=1 ORDER BY id",
                    )
                    .map_err(err)?;
                let v = st
                    .query_map([], |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                        ))
                    })
                    .map_err(err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(err)?;
                Ok(v)
            })
            .await?;
        let mut rep = TickReport::default();
        for (id, site_id, kind, cron, tz, payload, next) in rows {
            let row = Row {
                id: &id,
                site_id: &site_id,
                kind: &kind,
                cron: &cron,
                tz: &tz,
                payload: &payload,
                next,
            };
            if let Err(e) = Self::tick_row(ctx, now, &mut rep, row).await {
                tracing::error!(schedule = %id, error = %e, "schedule row failed; skipped this tick");
            }
        }
        if let Err(e) = Self::tick_maintenance(ctx, now, &mut rep).await {
            tracing::error!(error = %e, "maintenance schedule failed");
        }
        Ok(rep)
    }

    async fn tick_row(
        ctx: &JobContext,
        now: DateTime<Utc>,
        rep: &mut TickReport,
        row: Row<'_>,
    ) -> sf_core::Result<()> {
        let Row {
            id,
            site_id,
            kind,
            cron,
            tz,
            payload,
            next,
        } = row;
        let Some(next_after_now) = next_run(cron, tz, now) else {
            rep.invalid += 1;
            tracing::warn!(schedule = %id, %cron, "unparsable schedule; skipped");
            return Ok(());
        };
        let next_s = fmt(next_after_now);
        let due = next
            .as_deref()
            .and_then(|n| DateTime::parse_from_rfc3339(n).ok())
            .map(|n| n.with_timezone(&Utc));
        let Some(due) = due else {
            // first sight (or unreadable stamp): just schedule it
            cas_next(ctx, id, Some(next_s), next.clone()).await?;
            rep.initialized += 1;
            return Ok(());
        };
        if due > now {
            return Ok(());
        }
        // claim this firing (compare-and-set on next_run_at)
        if cas_next(ctx, id, Some(next_s.clone()), next.clone()).await? != 1 {
            return Ok(());
        }
        let payload: Value =
            serde_json::from_str(payload).unwrap_or_else(|_| Value::Object(Default::default()));
        let mut job = NewJob::new(JobKind::parse(kind), Some(site_id), payload).by("schedule");
        job.dedupe = Some(true);
        match ctx.queue.enqueue(job).await {
            Ok(e) => {
                let (i, jid) = (id.to_string(), e.job_run_id.clone());
                let stamp = fmt(now);
                ctx.db
                    .write(move |t| {
                        t.execute(
                            "UPDATE site_schedules SET last_run_at=?2, last_job_run_id=?3, updated_at=?2 WHERE id=?1",
                            params![i, stamp, jid],
                        )
                        .map_err(err)
                    })
                    .await?;
                tracing::info!(schedule = %id, %kind, job = %e.job_run_id, dedup = e.deduplicated, "schedule fired");
                rep.enqueued.push(e.job_run_id);
            }
            Err(e) => {
                // R18: give the firing back so the next tick retries it.
                tracing::error!(schedule = %id, error = %e, "scheduled enqueue failed; will retry");
                let _ = cas_next(ctx, id, next, Some(next_s)).await;
            }
        }
        Ok(())
    }

    /// G2-3: enqueues the global `maintenance` job (retention prune + ANALYZE/optimize) on
    /// `monitor.maintenance_cron` (local time; empty = off). State lives in kv
    /// `maintenance_next_at`, claimed with a compare-and-set like the site schedules.
    async fn tick_maintenance(
        ctx: &JobContext,
        now: DateTime<Utc>,
        rep: &mut TickReport,
    ) -> sf_core::Result<()> {
        let cron = ctx.live.get().monitor.maintenance_cron.trim().to_string();
        if cron.is_empty() {
            return Ok(());
        }
        let Some(next_after_now) = next_run_local(&cron, now) else {
            rep.invalid += 1;
            tracing::warn!(%cron, "unparsable monitor.maintenance_cron; maintenance disabled");
            return Ok(());
        };
        let next_s = fmt(next_after_now);
        let cur: Option<String> = ctx
            .db
            .read(|c| {
                c.query_row(
                    "SELECT value FROM kv WHERE key='maintenance_next_at'",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .map_err(err)
            })
            .await?;
        let due = cur
            .as_deref()
            .and_then(|n| DateTime::parse_from_rfc3339(n).ok())
            .map(|n| n.with_timezone(&Utc));
        match due {
            None => {
                kv_cas(ctx, cur, next_s).await?;
                rep.initialized += 1;
            }
            Some(d) if d <= now => {
                if kv_cas(ctx, cur.clone(), next_s.clone()).await? != 1 {
                    return Ok(());
                }
                let mut job = NewJob::new(
                    JobKind::Maintenance,
                    None,
                    Value::Object(Default::default()),
                )
                .by("schedule");
                job.dedupe = Some(true);
                match ctx.queue.enqueue(job).await {
                    Ok(e) => {
                        tracing::info!(job = %e.job_run_id, "daily maintenance queued");
                        rep.enqueued.push(e.job_run_id);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "maintenance enqueue failed; will retry");
                        let _ = kv_cas(ctx, Some(next_s), cur.unwrap_or_default()).await;
                    }
                }
            }
            Some(_) => {}
        }
        Ok(())
    }
}

struct Row<'a> {
    id: &'a str,
    site_id: &'a str,
    kind: &'a str,
    cron: &'a str,
    tz: &'a str,
    payload: &'a str,
    next: Option<String>,
}

/// Compare-and-set of `site_schedules.next_run_at` (old -> new). Returns rows changed.
async fn cas_next(
    ctx: &JobContext,
    id: &str,
    new: Option<String>,
    old: Option<String>,
) -> sf_core::Result<usize> {
    let i = id.to_string();
    ctx.db
        .write(move |t| {
            t.execute(
                "UPDATE site_schedules SET next_run_at=?2 WHERE id=?1 AND next_run_at IS ?3",
                params![i, new, old],
            )
            .map_err(err)
        })
        .await
}

/// Compare-and-set of kv `maintenance_next_at` (old -> new; old None = insert).
async fn kv_cas(ctx: &JobContext, old: Option<String>, new: String) -> sf_core::Result<usize> {
    ctx.db
        .write(move |t| {
            let n = match old {
                None => t
                    .execute(
                        "INSERT OR IGNORE INTO kv(key,value) VALUES('maintenance_next_at', ?1)",
                        [&new],
                    )
                    .map_err(err)?,
                Some(o) => t
                    .execute(
                        "UPDATE kv SET value=?1, updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
                         WHERE key='maintenance_next_at' AND value=?2",
                        params![new, o],
                    )
                    .map_err(err)?,
            };
            Ok(n)
        })
        .await
}

/// Next occurrence strictly after `after` in the machine's local time zone.
pub fn next_run_local(cron: &str, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let mut c = croner::Cron::new(cron.trim());
    c.with_seconds_optional();
    let parsed = c.parse().ok()?;
    parsed
        .find_next_occurrence(&after.with_timezone(&chrono::Local), false)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn fmt(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Next occurrence strictly after `after`, evaluated in `tz` (UTC when unknown).
pub fn next_run(cron: &str, tz: &str, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let zone: chrono_tz::Tz = tz.trim().parse().unwrap_or(chrono_tz::UTC);
    let mut c = croner::Cron::new(cron.trim());
    c.with_seconds_optional();
    let parsed = c.parse().ok()?;
    let local = after.with_timezone(&zone);
    parsed
        .find_next_occurrence(&local, false)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_run_tz_and_garbage() {
        let t = DateTime::parse_from_rfc3339("2026-10-13T10:00:30Z")
            .map(|d| d.with_timezone(&Utc))
            .unwrap();
        let n = next_run("*/5 * * * *", "Europe/Berlin", t).unwrap();
        assert_eq!(fmt(n), "2026-10-13T10:05:00.000Z");
        let n = next_run("0 3 * * *", "Mars/Olympus", t).unwrap();
        assert_eq!(fmt(n), "2026-10-14T03:00:00.000Z");
        let n = next_run("0 3 * * *", "America/New_York", t).unwrap();
        assert_eq!(fmt(n), "2026-10-14T07:00:00.000Z");
        assert!(next_run("not a cron", "UTC", t).is_none());
        assert!(next_run("", "UTC", t).is_none());
    }
}
