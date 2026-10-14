//! `maintenance`: retention prune (`sf_ops::retention::prune`: job logs, call log, notifications,
//! metrics, audit log, finished job runs, unused snapshots, resolved errors), then `ANALYZE` +
//! `PRAGMA optimize` + WAL checkpoint. Queued daily by the scheduler (`monitor.maintenance_cron`).
//! Stats: `{pruned: {...}, job_runs, snapshots, analyzed}`.

use std::sync::Arc;

use serde_json::{Value, json};
use sf_ops::retention::{RetentionPolicy, maintenance, prune};

use crate::ctx::{Job, JobContext, JobResult};

pub async fn run(ctx: &Arc<JobContext>, job: &Job) -> JobResult<Value> {
    job.progress("prune", 10.0).await;
    let report = prune(&ctx.db, &RetentionPolicy::from_config(&ctx.live.get())).await?;
    job.checkpoint().await?;
    job.progress("optimize", 60.0).await;
    maintenance(&ctx.db, true).await?;
    job.info(format!("maintenance: {report:?}")).await;
    Ok(json!({
        "pruned": report,
        "job_runs": report.job_runs,
        "snapshots": report.snapshots,
        "analyzed": true,
        "progress": {"phase": "done", "pct": 100},
    }))
}
