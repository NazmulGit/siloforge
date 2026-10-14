//! Job pipelines. Each `run(ctx, job)` returns the job's final `stats` (v1 shapes) and publishes
//! progress as `stats.progress = {phase, pct}` while it runs.

pub mod anchors;
pub mod audit;
pub mod backup;
pub mod chunk;
pub mod common;
pub mod crawl;
pub mod embed;
pub mod graph;
pub mod gsc;
pub mod ids;
pub mod inject;
pub mod keyword_link;
pub mod keywords;
pub mod link_health;
pub mod maintenance;
pub mod plan;
pub mod undo;

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value;
use sf_core::Error;
use sf_core::types::JobKind;

use crate::ctx::{Job, JobContext, JobError, JobResult};

/// A pipeline entry point (used for overrides in tests and custom kinds).
pub type PipelineFn =
    Arc<dyn Fn(Arc<JobContext>, Arc<Job>) -> BoxFuture<'static, JobResult<Value>> + Send + Sync>;

/// Run the built-in pipeline for `job.kind`.
pub async fn dispatch(ctx: Arc<JobContext>, job: Arc<Job>) -> JobResult<Value> {
    match job.kind {
        JobKind::Crawl => crawl::run(&ctx, &job).await,
        JobKind::Plan => plan::run(&ctx, &job).await,
        JobKind::Inject => inject::run(&ctx, &job).await,
        JobKind::Undo => undo::run(&ctx, &job).await,
        JobKind::KeywordLink => keyword_link::run(&ctx, &job).await,
        JobKind::LinkHealth => link_health::run(&ctx, &job).await,
        JobKind::Audit => audit::run(&ctx, &job).await,
        JobKind::Embed => embed::run(&ctx, &job).await,
        JobKind::Backup => backup::run(&ctx, &job).await,
        JobKind::Maintenance => maintenance::run(&ctx, &job).await,
        JobKind::Gsc => gsc::run(&ctx, &job).await,
        JobKind::Unknown(ref k) => Err(JobError::fatal(Error::bad(format!(
            "unknown job kind: {k}"
        )))),
    }
}
