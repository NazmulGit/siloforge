//! Durable job queue on the `job_runs` table.
//!
//! Guarantees (fixing v1 J1–J5): a job row exists before its id is returned; identical queued/running
//! jobs collapse via `dedupe_key`; claiming is atomic on the writer thread; site-exclusive kinds never
//! run concurrently on the same site; running rows carry a heartbeat and stale ones are reaped at boot
//! and periodically.

use std::sync::Arc;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sf_core::types::{JobKind, JobStatus};
use sf_core::{Error, Result};
use sf_store::{Db, err, row_to_json};
use tokio::sync::Notify;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewJob {
    pub kind: JobKind,
    pub site_id: Option<String>,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub requested_by: Option<String>,
    /// Retries after the first attempt fails with a retryable error.
    #[serde(default)]
    pub max_attempts: Option<i64>,
    /// Collapse with an identical queued/running job. Default: `kind:site_id` for site-exclusive kinds.
    #[serde(default)]
    pub dedupe: Option<bool>,
}

impl NewJob {
    pub fn new(kind: JobKind, site_id: Option<&str>, payload: Value) -> Self {
        Self {
            kind,
            site_id: site_id.map(str::to_string),
            payload,
            priority: 0,
            requested_by: None,
            max_attempts: None,
            dedupe: None,
        }
    }
    pub fn by(mut self, who: impl Into<String>) -> Self {
        self.requested_by = Some(who.into());
        self
    }
}

/// Outcome of `enqueue`: the job id, and whether an existing identical job was reused.
#[derive(Debug, Clone, Serialize)]
pub struct Enqueued {
    pub job_run_id: String,
    pub deduplicated: bool,
}

#[derive(Debug, Clone)]
pub struct ClaimedJob {
    pub id: String,
    pub kind: JobKind,
    pub site_id: Option<String>,
    pub payload: Value,
    pub attempts: i64,
    pub max_attempts: i64,
}

/// Cheap clone handle.
#[derive(Clone)]
pub struct JobQueue {
    db: Db,
    wake: Arc<Notify>,
}

/// Columns returned by list/get (v1 shape + v2 extras). `status` reports a running row whose
/// cancel was requested as `cancelled` (v1 shape, R3); `cancelling` = 1 while it is still stopping.
pub const JOB_COLS: &str = "id, site_id, kind, CASE WHEN status='running' AND cancel_requested_at IS NOT NULL THEN 'cancelled' ELSE status END AS status, stats, error, payload, attempts, max_attempts, priority, requested_by, heartbeat_at, resources, started_at, finished_at, created_at, (status='running' AND cancel_requested_at IS NOT NULL) AS cancelling";

/// [`JOB_COLS`] with every column qualified by `alias` (for joins).
pub fn job_cols(alias: &str) -> String {
    format!(
        "{a}.id, {a}.site_id, {a}.kind, CASE WHEN {a}.status='running' AND {a}.cancel_requested_at IS NOT NULL THEN 'cancelled' ELSE {a}.status END AS status, \
         {a}.stats, {a}.error, {a}.payload, {a}.attempts, {a}.max_attempts, {a}.priority, {a}.requested_by, {a}.heartbeat_at, \
         {a}.resources, {a}.started_at, {a}.finished_at, {a}.created_at, \
         ({a}.status='running' AND {a}.cancel_requested_at IS NOT NULL) AS cancelling",
        a = alias
    )
}

/// SQL expression for the API-visible status of `job_runs` (see [`JOB_COLS`]).
pub const API_STATUS_SQL: &str = "CASE WHEN status='running' AND cancel_requested_at IS NOT NULL THEN 'cancelled' ELSE status END";
pub const JOB_JSON_COLS: &[&str] = &["stats", "payload", "resources"];

impl JobQueue {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            wake: Arc::new(Notify::new()),
        }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Wakes the runner (used after enqueue / when a slot frees up).
    pub fn notify(&self) {
        self.wake.notify_one();
    }

    pub async fn wait_for_work(&self, max: std::time::Duration) {
        let _ = tokio::time::timeout(max, self.wake.notified()).await;
    }

    pub async fn enqueue(&self, job: NewJob) -> Result<Enqueued> {
        let dedupe_key = match job.dedupe {
            Some(false) => None,
            _ if job.kind.is_site_exclusive() || job.dedupe == Some(true) => Some(format!(
                "{}:{}:{}",
                job.kind,
                job.site_id.as_deref().unwrap_or("-"),
                sf_core::crypto::sha256_hex(job.payload.to_string().as_bytes())
                    .get(..16)
                    .unwrap_or("")
            )),
            _ => None,
        };
        let max_attempts = job.max_attempts.unwrap_or(match job.kind {
            JobKind::Crawl | JobKind::LinkHealth => 2,
            _ => 1,
        });
        let out = self
            .db
            .write(move |t| {
                if let Some(key) = &dedupe_key {
                    let existing: Option<String> = t
                        .query_row(
                            "SELECT id FROM job_runs WHERE dedupe_key=?1 AND status IN ('queued','running') AND cancel_requested_at IS NULL",
                            [key],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(err)?;
                    if let Some(id) = existing {
                        return Ok(Enqueued {
                            job_run_id: id,
                            deduplicated: true,
                        });
                    }
                }
                let id = sf_core::new_id();
                t.execute(
                    "INSERT INTO job_runs(id, site_id, kind, status, payload, priority, dedupe_key, requested_by, max_attempts)
                     VALUES(?1,?2,?3,'queued',?4,?5,?6,?7,?8)",
                    params![
                        id,
                        job.site_id,
                        job.kind.as_str(),
                        job.payload.to_string(),
                        job.priority,
                        dedupe_key,
                        job.requested_by,
                        max_attempts
                    ],
                )
                .map_err(err)?;
                Ok(Enqueued {
                    job_run_id: id,
                    deduplicated: false,
                })
            })
            .await?;
        self.notify();
        Ok(out)
    }

    /// Atomically claims the next runnable job. Site-exclusive kinds wait while their site has a
    /// running row (a job whose cancel was requested stays `running` until it really stopped, R3).
    /// `allow_heavy` = false skips CPU-heavy kinds (machine busy).
    ///
    /// The filtering happens in SQL over the whole queue (R6: a fixed 50-row window let one busy
    /// site's backlog starve every other site). Among runnable rows: priority first, then sites
    /// with fewer running jobs, then the oldest.
    pub async fn claim_next(&self, allow_heavy: bool) -> Result<Option<ClaimedJob>> {
        let quote = |k: &&str| format!("'{}'", k.replace('\'', "''"));
        let exclusive = JobKind::ALL
            .iter()
            .filter(|k| JobKind::parse(k).is_site_exclusive())
            .map(quote)
            .collect::<Vec<_>>()
            .join(",");
        let heavy = JobKind::ALL
            .iter()
            .filter(|k| JobKind::parse(k).is_heavy())
            .map(quote)
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT q.id, q.kind, q.site_id, q.payload, q.attempts, q.max_attempts FROM job_runs q
             WHERE q.status='queued' AND (q.run_after IS NULL OR q.run_after <= ?1)
               AND (?2 OR q.kind NOT IN ({heavy}))
               AND (q.site_id IS NULL OR q.kind NOT IN ({exclusive})
                    OR NOT EXISTS (SELECT 1 FROM job_runs r WHERE r.site_id = q.site_id AND r.status='running'))
             ORDER BY q.priority DESC,
               (SELECT count(*) FROM job_runs r2 WHERE r2.site_id = q.site_id AND r2.status='running') ASC,
               q.created_at ASC, q.id ASC
             LIMIT 1"
        );
        self.db
            .write(move |t| {
                let now = sf_core::now_rfc3339();
                let row = t
                    .query_row(&sql, params![now, allow_heavy], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, i64>(5)?,
                        ))
                    })
                    .optional()
                    .map_err(err)?;
                if let Some((id, kind, site_id, payload, attempts, max_attempts)) = row {
                    let kind = JobKind::parse(&kind);
                    t.execute(
                        "UPDATE job_runs SET status='running', started_at=?2, heartbeat_at=?2, attempts=attempts+1, error=NULL
                         WHERE id=?1 AND status='queued'",
                        params![id, now],
                    )
                    .map_err(err)?;
                    return Ok(Some(ClaimedJob {
                        id,
                        kind,
                        site_id,
                        payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
                        attempts: attempts + 1,
                        max_attempts,
                    }));
                }
                Ok(None)
            })
            .await
    }

    pub async fn heartbeat(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        self.db
            .write(move |t| {
                t.execute(
                    "UPDATE job_runs SET heartbeat_at=?2 WHERE id=?1 AND status='running'",
                    params![id, sf_core::now_rfc3339()],
                )
                .map_err(err)?;
                Ok(())
            })
            .await
    }

    /// Shallow-merges `partial` into `stats` (v1 `mergeStats`), e.g. `{"progress":{"phase":"embed","pct":40}}`.
    pub async fn merge_stats(&self, id: &str, partial: Value) -> Result<()> {
        let id = id.to_string();
        self.db
            .write(move |t| {
                let cur: Option<String> = t
                    .query_row("SELECT stats FROM job_runs WHERE id=?1", [&id], |r| {
                        r.get(0)
                    })
                    .optional()
                    .map_err(err)?;
                let mut stats: Value = cur
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_else(|| Value::Object(Default::default()));
                if let (Some(dst), Value::Object(src)) = (stats.as_object_mut(), partial) {
                    for (k, v) in src {
                        dst.insert(k, v);
                    }
                }
                t.execute(
                    "UPDATE job_runs SET stats=?2, heartbeat_at=?3 WHERE id=?1",
                    params![id, stats.to_string(), sf_core::now_rfc3339()],
                )
                .map_err(err)?;
                Ok(())
            })
            .await
    }

    pub async fn finish(&self, id: &str, stats: Value, resources: Value) -> Result<()> {
        let id = id.to_string();
        self.db
            .write(move |t| {
                // Only a running row can finish: a job cancelled (or reaped) meanwhile keeps its
                // status, and a running row whose cancel was requested ends as 'cancelled'.
                t.execute(
                    "UPDATE job_runs SET status=CASE WHEN cancel_requested_at IS NULL THEN 'done' ELSE 'cancelled' END,
                       stats=?2, resources=?3, finished_at=?4,
                       error=CASE WHEN cancel_requested_at IS NULL THEN NULL ELSE error END
                     WHERE id=?1 AND status='running'",
                    params![id, stats.to_string(), resources.to_string(), sf_core::now_rfc3339()],
                )
                .map_err(err)?;
                Ok(())
            })
            .await?;
        self.notify();
        Ok(())
    }

    /// Called by the runner once a cancelled pipeline has really stopped: the row becomes
    /// `cancelled` (releasing the site, R3) and keeps the job's resource usage (pilot 2026-10-13:
    /// cancelled runs showed `resources: {}`).
    pub async fn record_cancelled_resources(&self, id: &str, resources: Value) -> Result<()> {
        let id = id.to_string();
        self.db
            .write(move |t| {
                t.execute(
                    "UPDATE job_runs SET status='cancelled', resources=?2,
                       finished_at=CASE WHEN status='running' THEN ?3 ELSE coalesce(finished_at, ?3) END,
                       error=coalesce(error, 'cancelled by user'), dedupe_key=NULL
                     WHERE id=?1 AND status IN ('running','cancelled')",
                    params![id, resources.to_string(), sf_core::now_rfc3339()],
                )
                .map_err(err)?;
                Ok(())
            })
            .await?;
        self.notify();
        Ok(())
    }

    /// Marks failed, or re-queues with backoff when attempts remain and `retryable`.
    pub async fn fail(
        &self,
        id: &str,
        error: &str,
        retryable: bool,
        resources: Value,
    ) -> Result<JobStatus> {
        let id = id.to_string();
        let error: String = error.chars().take(2000).collect();
        let status = self
            .db
            .write(move |t| {
                let (attempts, max, cur, cancel_req): (i64, i64, String, Option<String>) = t
                    .query_row(
                        "SELECT attempts, max_attempts, status, cancel_requested_at FROM job_runs WHERE id=?1",
                        [&id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .map_err(err)?;
                // A cancelled/finished row is never overwritten (cancel wins over a late failure).
                if cur != "running" {
                    return Ok(JobStatus::parse(&cur));
                }
                if cancel_req.is_some() {
                    t.execute(
                        "UPDATE job_runs SET status='cancelled', resources=?2, finished_at=?3, dedupe_key=NULL WHERE id=?1",
                        params![id, resources.to_string(), sf_core::now_rfc3339()],
                    )
                    .map_err(err)?;
                    return Ok(JobStatus::Cancelled);
                }
                if retryable && attempts < max {
                    let backoff = 30_i64.saturating_mul(attempts.max(1));
                    let after = (chrono::Utc::now() + chrono::Duration::seconds(backoff))
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                    t.execute(
                        "UPDATE job_runs SET status='queued', error=?2, run_after=?3, heartbeat_at=NULL WHERE id=?1",
                        params![id, error, after],
                    )
                    .map_err(err)?;
                    Ok(JobStatus::Queued)
                } else {
                    t.execute(
                        "UPDATE job_runs SET status='failed', error=?2, resources=?3, finished_at=?4 WHERE id=?1",
                        params![id, error, resources.to_string(), sf_core::now_rfc3339()],
                    )
                    .map_err(err)?;
                    Ok(JobStatus::Failed)
                }
            })
            .await?;
        self.notify();
        Ok(status)
    }

    /// Cancels a queued job immediately. A running job is only flagged (`cancel_requested_at`): it
    /// stays `running`, so its site stays busy, until the runner sees the pipeline stop and writes
    /// `cancelled` (R3). The API reports a flagged row as `cancelled` (v1 shape), so the returned
    /// status is `Cancelled` in both cases.
    pub async fn cancel(&self, id: &str) -> Result<JobStatus> {
        let id = id.to_string();
        let out = self
            .db
            .write(move |t| {
                let st: String = t
                    .query_row("SELECT status FROM job_runs WHERE id=?1", [&id], |r| r.get(0))
                    .map_err(err)?;
                match JobStatus::parse(&st) {
                    JobStatus::Queued => {
                        t.execute(
                            "UPDATE job_runs SET status='cancelled', finished_at=?2, error='cancelled by user', dedupe_key=NULL WHERE id=?1",
                            params![id, sf_core::now_rfc3339()],
                        )
                        .map_err(err)?;
                        Ok(JobStatus::Cancelled)
                    }
                    JobStatus::Running => {
                        // dedupe_key is cleared so a new identical job can be queued meanwhile
                        // (it waits for the site; the unique index covers 'running' rows).
                        t.execute(
                            "UPDATE job_runs SET cancel_requested_at=coalesce(cancel_requested_at, ?2),
                               error='cancelled by user', dedupe_key=NULL WHERE id=?1",
                            params![id, sf_core::now_rfc3339()],
                        )
                        .map_err(err)?;
                        Ok(JobStatus::Cancelled)
                    }
                    other => Ok(other),
                }
            })
            .await?;
        self.notify();
        Ok(out)
    }

    /// True when the job was cancelled or its cancel was requested (pipelines poll this at
    /// checkpoints).
    pub async fn is_cancelled(&self, id: &str) -> Result<bool> {
        let id = id.to_string();
        self.db
            .read(move |c| {
                let st: Option<(String, Option<String>)> = c
                    .query_row(
                        "SELECT status, cancel_requested_at FROM job_runs WHERE id=?1",
                        [&id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(err)?;
                Ok(match st {
                    None => true,
                    Some((s, req)) => s == "cancelled" || req.is_some(),
                })
            })
            .await
    }

    /// Recovers `running` rows whose heartbeat is older than `stale_secs` (crash/kill recovery,
    /// v1 J3): a row whose cancel was requested becomes `cancelled`; an idempotent kind (crawl,
    /// link_health, embed, gsc, maintenance) with attempts left is re-queued with a short backoff
    /// (R15: a crawl interrupted by a reboot is retried); everything else is `failed`.
    /// `stale_secs = 0` (boot) reaps every running row: a heartbeat written in the same millisecond
    /// still counts as stale (`<=`), which also keeps the reaper test deterministic.
    pub async fn reap_stale(&self, stale_secs: i64) -> Result<usize> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(stale_secs))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let retry_after = (chrono::Utc::now() + chrono::Duration::seconds(30))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        self.db
            .write(move |t| {
                let now = sf_core::now_rfc3339();
                let stale = "status='running' AND (heartbeat_at IS NULL OR heartbeat_at <= ?1)";
                let mut n = t
                    .execute(
                        &format!(
                            "UPDATE job_runs SET status='cancelled', finished_at=?2, dedupe_key=NULL,
                               error=coalesce(error,'cancelled by user')
                             WHERE {stale} AND cancel_requested_at IS NOT NULL"
                        ),
                        params![cutoff, now],
                    )
                    .map_err(err)?;
                n += t
                    .execute(
                        &format!(
                            "UPDATE job_runs SET status='queued', run_after=?2, heartbeat_at=NULL,
                               error='worker stopped while running (re-queued)'
                             WHERE {stale} AND cancel_requested_at IS NULL AND attempts < max_attempts
                               AND kind IN ('crawl','link_health','embed','gsc','maintenance')"
                        ),
                        params![cutoff, retry_after],
                    )
                    .map_err(err)?;
                n += t
                    .execute(
                        &format!(
                            "UPDATE job_runs SET status='failed', finished_at=?2,
                               error=coalesce(error,'') || 'worker stopped while running (reaped)'
                             WHERE {stale}"
                        ),
                        params![cutoff, now],
                    )
                    .map_err(err)?;
                Ok(n)
            })
            .await
    }

    pub async fn log(
        &self,
        id: &str,
        level: &str,
        message: impl Into<String>,
        data: Option<Value>,
    ) -> Result<()> {
        let id = id.to_string();
        let level = match level {
            "trace" | "debug" | "info" | "warn" | "error" => level.to_string(),
            _ => "info".to_string(),
        };
        let message: String = message.into().chars().take(4000).collect();
        self.db
            .write(move |t| {
                t.execute(
                    "INSERT INTO job_logs(job_run_id, level, message, data) VALUES(?1,?2,?3,?4)",
                    params![id, level, message, data.map(|d| d.to_string())],
                )
                .map_err(err)?;
                Ok(())
            })
            .await
    }

    pub async fn get(&self, id: &str) -> Result<Value> {
        let id = id.to_string();
        self.db
            .read(move |c| {
                c.query_row(
                    &format!("SELECT {JOB_COLS} FROM job_runs WHERE id=?1"),
                    [&id],
                    |r| Ok(row_to_json(r, JOB_JSON_COLS, &[])),
                )
                .optional()
                .map_err(err)?
                .ok_or_else(|| Error::not_found("job not found"))
            })
            .await
    }

    pub async fn counts(&self) -> Result<(i64, i64)> {
        self.db
            .read(|c| {
                c.query_row(
                    "SELECT sum(status='running'), sum(status='queued') FROM job_runs WHERE status IN ('running','queued')",
                    [],
                    |r| Ok((r.get::<_, Option<i64>>(0)?.unwrap_or(0), r.get::<_, Option<i64>>(1)?.unwrap_or(0))),
                )
                .map_err(err)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn setup() -> (JobQueue, std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("sf-jobs-test-{}", sf_core::new_id()));
        let db = Db::open(&dir.join("t.db")).unwrap();
        let site = sf_core::new_id();
        let s2 = site.clone();
        db.write(move |t| {
            t.execute(
                "INSERT INTO sites(id,domain,api_key) VALUES(?1,'ex.com','sf_x')",
                [&s2],
            )
            .map_err(err)?;
            Ok(())
        })
        .await
        .unwrap();
        (JobQueue::new(db), dir, site)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dedupe_exclusivity_and_lifecycle() {
        let (q, dir, site) = setup().await;
        let a = q
            .enqueue(NewJob::new(JobKind::Crawl, Some(&site), json!({})))
            .await
            .unwrap();
        let b = q
            .enqueue(NewJob::new(JobKind::Crawl, Some(&site), json!({})))
            .await
            .unwrap();
        assert!(!a.deduplicated && b.deduplicated && a.job_run_id == b.job_run_id);
        let p = q
            .enqueue(NewJob::new(
                JobKind::Plan,
                Some(&site),
                json!({"topology":"reverse"}),
            ))
            .await
            .unwrap();

        let first = q.claim_next(true).await.unwrap().unwrap();
        assert_eq!(first.id, a.job_run_id);
        // plan on the same site must wait while crawl runs
        assert!(q.claim_next(true).await.unwrap().is_none());
        q.merge_stats(&first.id, json!({"progress":{"phase":"crawl","pct":50}}))
            .await
            .unwrap();
        q.log(&first.id, "info", "hello", None).await.unwrap();
        q.finish(&first.id, json!({"upserted":3}), json!({}))
            .await
            .unwrap();

        let second = q.claim_next(true).await.unwrap().unwrap();
        assert_eq!(second.id, p.job_run_id);
        assert_eq!(
            q.fail(&second.id, "boom", true, json!({})).await.unwrap(),
            JobStatus::Failed
        );
        let got = q.get(&second.id).await.unwrap();
        assert_eq!(got["status"], "failed");
        assert_eq!(q.reap_stale(0).await.unwrap(), 0);
        q.db().close();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn heavy_deferred_and_cancel() {
        let (q, dir, site) = setup().await;
        let p = q
            .enqueue(NewJob::new(JobKind::Plan, Some(&site), json!({})))
            .await
            .unwrap();
        assert!(q.claim_next(false).await.unwrap().is_none());
        assert_eq!(q.cancel(&p.job_run_id).await.unwrap(), JobStatus::Cancelled);
        assert!(q.is_cancelled(&p.job_run_id).await.unwrap());
        assert!(q.claim_next(true).await.unwrap().is_none());
        q.db().close();
        let _ = std::fs::remove_dir_all(dir);
    }

    async fn add_site(q: &JobQueue) -> String {
        let site = sf_core::new_id();
        let s2 = site.clone();
        q.db()
            .write(move |t| {
                t.execute(
                    "INSERT INTO sites(id,domain,api_key) VALUES(?1,?1,?1)",
                    [&s2],
                )
                .map_err(err)
            })
            .await
            .unwrap();
        site
    }

    /// R3: a cancelled running job keeps its site busy until the runner reports it stopped,
    /// while the API already reports `cancelled`.
    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_keeps_site_lock_until_pipeline_stops() {
        let (q, dir, site) = setup().await;
        let inj = q
            .enqueue(NewJob::new(
                JobKind::Inject,
                Some(&site),
                json!({"edge_ids":["a"]}),
            ))
            .await
            .unwrap();
        let running = q.claim_next(true).await.unwrap().unwrap();
        assert_eq!(running.id, inj.job_run_id);
        assert_eq!(q.cancel(&running.id).await.unwrap(), JobStatus::Cancelled);
        let v = q.get(&running.id).await.unwrap();
        assert_eq!(v["status"], "cancelled");
        assert_eq!(v["cancelling"], 1);
        assert!(q.is_cancelled(&running.id).await.unwrap());
        // the same inject again is a NEW job (not deduped onto the stopping one) and must wait
        let again = q
            .enqueue(NewJob::new(
                JobKind::Inject,
                Some(&site),
                json!({"edge_ids":["a"]}),
            ))
            .await
            .unwrap();
        assert!(!again.deduplicated && again.job_run_id != inj.job_run_id);
        assert!(
            q.claim_next(true).await.unwrap().is_none(),
            "site must stay busy"
        );
        // a late finish of the cancelled pipeline does not turn it into 'done'
        q.finish(&running.id, json!({"applied":1}), json!({}))
            .await
            .unwrap();
        let v = q.get(&running.id).await.unwrap();
        assert_eq!(
            (v["status"].as_str(), v["cancelling"].as_i64()),
            (Some("cancelled"), Some(0))
        );
        let next = q.claim_next(true).await.unwrap().unwrap();
        assert_eq!(next.id, again.job_run_id);
        // runner path: pipeline stopped at a checkpoint
        q.cancel(&next.id).await.unwrap();
        assert!(q.claim_next(true).await.unwrap().is_none());
        q.record_cancelled_resources(&next.id, json!({"peak_rss_mb":1}))
            .await
            .unwrap();
        let v = q.get(&next.id).await.unwrap();
        assert_eq!(v["status"], "cancelled");
        assert_eq!(v["resources"]["peak_rss_mb"], 1);
        q.db().close();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// R6: a big backlog for one busy site never starves another site.
    #[tokio::test(flavor = "multi_thread")]
    async fn busy_site_backlog_does_not_starve_other_sites() {
        let (q, dir, a) = setup().await;
        let b = add_site(&q).await;
        for i in 0..60 {
            q.enqueue(NewJob::new(
                JobKind::Inject,
                Some(&a),
                json!({"edge_ids":[i]}),
            ))
            .await
            .unwrap();
        }
        let first = q.claim_next(true).await.unwrap().unwrap();
        assert_eq!(first.site_id.as_deref(), Some(a.as_str()));
        let jb = q
            .enqueue(NewJob::new(JobKind::Crawl, Some(&b), json!({})))
            .await
            .unwrap();
        let next = q.claim_next(true).await.unwrap().unwrap();
        assert_eq!(next.id, jb.job_run_id);
        // heavy kinds are skipped in SQL too, without hiding light ones behind them
        let c = add_site(&q).await;
        for _ in 0..3 {
            let s = add_site(&q).await;
            q.enqueue(NewJob::new(JobKind::Plan, Some(&s), json!({})))
                .await
                .unwrap();
        }
        let lh = q
            .enqueue(NewJob::new(JobKind::LinkHealth, Some(&c), json!({})))
            .await
            .unwrap();
        let light = q.claim_next(false).await.unwrap().unwrap();
        assert_eq!(light.id, lh.job_run_id);
        q.db().close();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// R15: reaping re-queues idempotent kinds with attempts left, fails the rest, and finishes
    /// cancel-requested rows as cancelled.
    #[tokio::test(flavor = "multi_thread")]
    async fn reaper_requeues_idempotent_jobs() {
        let (q, dir, site) = setup().await;
        let crawl = q
            .enqueue(NewJob::new(JobKind::Crawl, Some(&site), json!({})))
            .await
            .unwrap();
        let s2 = add_site(&q).await;
        let plan = q
            .enqueue(NewJob::new(JobKind::Plan, Some(&s2), json!({})))
            .await
            .unwrap();
        let s3 = add_site(&q).await;
        let inj = q
            .enqueue(NewJob::new(JobKind::Inject, Some(&s3), json!({})))
            .await
            .unwrap();
        for _ in 0..3 {
            q.claim_next(true).await.unwrap().unwrap();
        }
        q.cancel(&inj.job_run_id).await.unwrap();
        assert_eq!(q.reap_stale(0).await.unwrap(), 3);
        let st = |v: Value| v["status"].as_str().unwrap_or("").to_string();
        assert_eq!(st(q.get(&crawl.job_run_id).await.unwrap()), "queued");
        assert_eq!(st(q.get(&plan.job_run_id).await.unwrap()), "failed");
        let i = q.get(&inj.job_run_id).await.unwrap();
        assert_eq!(
            (st(i.clone()), i["cancelling"].as_i64()),
            ("cancelled".into(), Some(0))
        );
        q.db().close();
        let _ = std::fs::remove_dir_all(dir);
    }
}
