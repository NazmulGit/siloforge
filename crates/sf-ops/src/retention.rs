//! Retention pruning and SQLite maintenance.
//!
//! Policy (days, from `[monitor]`; zero disables a rule): `job_logs` and `model_call_log` =
//! `job_log_retain_days`, `system_metrics` = `retain_days`, `notifications` =
//! `notifications_retain_days`, `audit_log` = `audit_log_retain_days`, finished `job_runs` =
//! `job_runs_retain_days` (the newest `job_runs_keep_per_site` per site and kind, and runs a
//! schedule points at, are always kept; their logs cascade), `snapshots` = `snapshot_retain_days`
//! (only snapshots that back no applied edge and no in-flight write), resolved `errors` =
//! `errors_retain_days`, `event_log` = `event_log_retain_days`. `site_metrics_history` is kept (it is the trend history). Large deletes
//! run in batches so the single writer is never held for long.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use sf_core::{Config, Result};
use sf_store::{Db, err};

#[derive(Debug, Clone, Serialize)]
pub struct RetentionPolicy {
    pub job_logs_days: u32,
    pub model_call_log_days: u32,
    pub notifications_days: u32,
    pub system_metrics_days: u32,
    pub audit_log_days: u32,
    pub job_runs_days: u32,
    pub job_runs_keep_per_site: u32,
    pub snapshots_days: u32,
    pub errors_days: u32,
    pub event_log_days: u32,
}

impl RetentionPolicy {
    pub fn from_config(cfg: &Config) -> Self {
        let m = &cfg.monitor;
        Self {
            job_logs_days: m.job_log_retain_days,
            model_call_log_days: m.job_log_retain_days,
            notifications_days: m.notifications_retain_days,
            system_metrics_days: m.retain_days,
            audit_log_days: m.audit_log_retain_days,
            job_runs_days: m.job_runs_retain_days,
            job_runs_keep_per_site: m.job_runs_keep_per_site,
            snapshots_days: m.snapshot_retain_days,
            errors_days: m.errors_retain_days,
            event_log_days: m.event_log_retain_days,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct RetentionReport {
    pub job_logs: usize,
    pub model_call_log: usize,
    pub notifications: usize,
    pub system_metrics: usize,
    pub audit_log: usize,
    pub job_runs: usize,
    pub snapshots: usize,
    pub errors: usize,
    pub event_log: usize,
}

fn cutoff(days: u32) -> String {
    (chrono::Utc::now() - chrono::Duration::days(i64::from(days)))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

const BATCH: i64 = 2000;

/// Runs `DELETE ... WHERE rowid IN (<select> LIMIT BATCH)` until nothing is left.
async fn delete_batched(
    db: &Db,
    table: &'static str,
    select_ids: String,
    arg: String,
) -> Result<usize> {
    let mut total = 0usize;
    loop {
        let sql = format!("DELETE FROM {table} WHERE rowid IN ({select_ids} LIMIT {BATCH})");
        let a = arg.clone();
        let n = db
            .write(move |t| t.execute(&sql, [&a]).map_err(err))
            .await?;
        total = total.saturating_add(n);
        if (n as i64) < BATCH {
            return Ok(total);
        }
        tokio::task::yield_now().await;
    }
}

/// Deletes rows older than the policy allows.
pub async fn prune(db: &Db, policy: &RetentionPolicy) -> Result<RetentionReport> {
    let mut rep = RetentionReport::default();
    let rules: [(&'static str, &str, u32); 6] = [
        ("job_logs", "ts", policy.job_logs_days),
        ("model_call_log", "called_at", policy.model_call_log_days),
        ("notifications", "created_at", policy.notifications_days),
        ("system_metrics", "ts", policy.system_metrics_days),
        ("audit_log", "ts", policy.audit_log_days),
        ("event_log", "ts", policy.event_log_days),
    ];
    for (table, col, days) in rules {
        if days == 0 {
            continue;
        }
        let n = delete_batched(
            db,
            table,
            format!("SELECT rowid FROM {table} WHERE {col} < ?1"),
            cutoff(days),
        )
        .await?;
        match table {
            "job_logs" => rep.job_logs = n,
            "model_call_log" => rep.model_call_log = n,
            "notifications" => rep.notifications = n,
            "system_metrics" => rep.system_metrics = n,
            "event_log" => rep.event_log = n,
            _ => rep.audit_log = n,
        }
    }
    if policy.job_runs_days > 0 {
        let keep = policy.job_runs_keep_per_site;
        rep.job_runs = delete_batched(
            db,
            "job_runs",
            format!(
                "SELECT j.rowid FROM job_runs j
                  WHERE j.status IN ('done','failed','cancelled') AND j.created_at < ?1
                    AND j.id NOT IN (SELECT last_job_run_id FROM site_schedules WHERE last_job_run_id IS NOT NULL)
                    AND j.id NOT IN (
                      SELECT id FROM (
                        SELECT id, row_number() OVER (
                          PARTITION BY coalesce(site_id,''), kind ORDER BY created_at DESC, id DESC) AS rn
                        FROM job_runs WHERE status IN ('done','failed','cancelled'))
                      WHERE rn <= {keep})"
            ),
            cutoff(policy.job_runs_days),
        )
        .await?;
    }
    if policy.snapshots_days > 0 {
        rep.snapshots = delete_batched(
            db,
            "snapshots",
            "SELECT s.rowid FROM snapshots s
              WHERE s.taken_at < ?1
                AND NOT EXISTS (SELECT 1 FROM link_edges e WHERE e.snapshot_id = s.id AND e.status = 'applied')
                AND NOT EXISTS (SELECT 1 FROM inject_journal j WHERE j.snapshot_id = s.id)"
                .to_string(),
            cutoff(policy.snapshots_days),
        )
        .await?;
    }
    if policy.errors_days > 0 {
        rep.errors = delete_batched(
            db,
            "errors",
            "SELECT rowid FROM errors WHERE resolved_at IS NOT NULL AND last_seen < ?1".to_string(),
            cutoff(policy.errors_days),
        )
        .await?;
    }
    Ok(rep)
}

/// `PRAGMA optimize`, incremental vacuum (effective only with `auto_vacuum=INCREMENTAL`), optional
/// full `ANALYZE`, and a WAL checkpoint. Runs on the writer connection.
pub async fn maintenance(db: &Db, analyze: bool) -> Result<()> {
    db.with_writer(move |c| {
        if analyze {
            c.execute_batch("ANALYZE").map_err(err)?;
        }
        c.execute_batch("PRAGMA optimize; PRAGMA incremental_vacuum;")
            .map_err(err)?;
        let _ = c.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        Ok(())
    })
    .await
}

/// Daily loop: prune + maintenance (ANALYZE weekly). Must be called inside a tokio runtime.
/// `serve` no longer calls this: the job scheduler enqueues the `maintenance` job daily
/// (`monitor.maintenance_cron`), which runs the same prune + maintenance. Kept for embedders.
pub fn spawn_daily(cfg: Arc<Config>, db: Db) -> tokio::task::JoinHandle<()> {
    let db0 = db.clone();
    crate::supervise::spawn("retention", Some(db0), move || {
        let (cfg, db) = (cfg.clone(), db.clone());
        async move {
            // first run shortly after boot, then every 24 h
            tokio::time::sleep(Duration::from_secs(300)).await;
            let mut day: u64 = 0;
            loop {
                let policy = RetentionPolicy::from_config(&cfg);
                match prune(&db, &policy).await {
                    Ok(r) => tracing::info!(?r, "retention prune"),
                    Err(e) => tracing::warn!(error = %e, "retention prune failed"),
                }
                if let Err(e) = maintenance(&db, day.is_multiple_of(7)).await {
                    tracing::warn!(error = %e, "db maintenance failed");
                }
                day = day.wrapping_add(1);
                tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn prunes_old_rows_only() {
        let dir = std::env::temp_dir().join(format!("sf-ops-ret-{}", sf_core::new_id()));
        let db = Db::open(&dir.join("t.db")).unwrap();
        db.write(|t| {
            t.execute_batch(
                "INSERT INTO audit_log(ts, actor, action) VALUES('2000-01-01T00:00:00.000Z','a','x');
                 INSERT INTO audit_log(actor, action) VALUES('a','y');
                 INSERT INTO event_log(ts, source, actor, action) VALUES('2000-01-01T00:00:00.000Z','api','a','x');
                 INSERT INTO event_log(source, actor, action) VALUES('api','a','y');
                 INSERT INTO notifications(event, title, created_at) VALUES('job.done','t','2000-01-01T00:00:00.000Z');
                 INSERT INTO system_metrics(ts,cpu_pct,proc_cpu_pct,mem_used_mb,mem_total_mb,proc_rss_mb)
                   VALUES('2000-01-01T00:00:00.000Z',1,1,1,1,1);",
            )
            .map_err(err)
        })
        .await
        .unwrap();
        let rep = prune(&db, &RetentionPolicy::from_config(&Config::default()))
            .await
            .unwrap();
        assert_eq!(
            rep,
            RetentionReport {
                audit_log: 1,
                event_log: 1,
                notifications: 1,
                system_metrics: 1,
                ..Default::default()
            }
        );
        // 0 = keep forever
        db.write(|t| {
            t.execute_batch(
                "INSERT INTO event_log(ts, source, actor, action) VALUES('2000-01-01T00:00:00.000Z','api','a','z');",
            )
            .map_err(err)
        })
        .await
        .unwrap();
        let mut keep = Config::default();
        keep.monitor.event_log_retain_days = 0;
        let rep = prune(&db, &RetentionPolicy::from_config(&keep))
            .await
            .unwrap();
        assert_eq!(rep.event_log, 0);
        maintenance(&db, true).await.unwrap();
        db.close();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// G2-3: job_runs, snapshots and resolved errors are pruned; the newest runs per site/kind,
    /// snapshots backing applied edges and open errors are kept.
    #[tokio::test(flavor = "multi_thread")]
    async fn prunes_job_runs_snapshots_and_errors() {
        let dir = std::env::temp_dir().join(format!("sf-ops-ret2-{}", sf_core::new_id()));
        let db = Db::open(&dir.join("t.db")).unwrap();
        db.write(|t| {
            t.execute_batch(
                "INSERT INTO sites(id,domain,api_key) VALUES('s1','ex.com','k');
                 INSERT INTO posts(id,site_id,url,title) VALUES('p1','s1','https://ex.com/a','A'),('p2','s1','https://ex.com/b','B');
                 INSERT INTO job_runs(id,site_id,kind,status,created_at) VALUES
                   ('j1','s1','crawl','done','2000-01-01T00:00:00.000Z'),
                   ('j2','s1','crawl','done','2000-01-02T00:00:00.000Z'),
                   ('j3','s1','crawl','failed','2000-01-03T00:00:00.000Z'),
                   ('j4','s1','plan','done','2000-01-01T00:00:00.000Z'),
                   ('j5','s1','crawl','running','2000-01-01T00:00:00.000Z');
                 INSERT INTO job_logs(job_run_id,message) VALUES('j1','x');
                 INSERT INTO snapshots(id,post_id,content_html,taken_at) VALUES
                   ('sn1','p1','<p>a</p>','2000-01-01T00:00:00.000Z'),
                   ('sn2','p1','<p>b</p>','2000-01-01T00:00:00.000Z'),
                   ('sn3','p1','<p>c</p>','2000-01-01T00:00:00.000Z');
                 INSERT INTO link_edges(id,site_id,source_post_id,target_post_id,anchor,status,snapshot_id)
                   VALUES('e1','s1','p1','p2','b','applied','sn1'),('e2','s1','p1','p2','bb','reverted','sn2');
                 INSERT INTO errors(fingerprint,source,message,last_seen,resolved_at) VALUES
                   ('f1','x','old resolved','2000-01-01T00:00:00.000Z','2000-01-02T00:00:00.000Z'),
                   ('f2','x','old open','2000-01-01T00:00:00.000Z',NULL);",
            )
            .map_err(err)
        })
        .await
        .unwrap();
        let mut cfg = Config::default();
        cfg.monitor.job_runs_keep_per_site = 1;
        let rep = prune(&db, &RetentionPolicy::from_config(&cfg))
            .await
            .unwrap();
        assert_eq!(
            (rep.job_runs, rep.snapshots, rep.errors),
            (2, 2, 1),
            "{rep:?}"
        );
        let left: Vec<String> = db
            .read(|c| {
                let mut st = c
                    .prepare(
                        "SELECT id FROM job_runs UNION ALL SELECT id FROM snapshots
                         UNION ALL SELECT fingerprint FROM errors ORDER BY 1",
                    )
                    .map_err(err)?;
                let v = st
                    .query_map([], |r| r.get(0))
                    .map_err(err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(err)?;
                Ok(v)
            })
            .await
            .unwrap();
        // newest crawl (j3) and the only plan (j4) kept, running j5 untouched; sn1 backs an applied edge
        assert_eq!(left, vec!["f2", "j3", "j4", "j5", "sn1"]);
        db.close();
        let _ = std::fs::remove_dir_all(dir);
    }
}
