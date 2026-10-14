//! SiloForge jobs (WP J): durable queue (`queue`), the runner (`runner`), the cron scheduler
//! (`scheduler`), the shared job context (`ctx`) and every pipeline (`pipelines`).
//!
//! Wiring (binary):
//! ```ignore
//! sf_jobs::lower_compute_priority(&cfg); // before sf_engine::pool::init(cfg.cpu_threads())
//! let queue = JobQueue::new(db.clone());
//! let mut ctx = JobContext::new(cfg.clone(), db.clone(), queue.clone(), Arc::new(embed_chain), secret_key);
//! ctx.ai = Some(Arc::new(ai));
//! ctx.notifier = Some(notifier);
//! ctx.monitor = Some(monitor);
//! let ctx = Arc::new(ctx);
//! let runner = Runner::start(ctx.clone());
//! let scheduler = Scheduler::start(ctx.clone());
//! // ... on shutdown:
//! scheduler.stop().await;
//! runner.shutdown(Duration::from_secs(20)).await;
//! ```

// SQL row tuples are spelled out inline next to their SELECT; naming each one would hide the mapping.
#![allow(clippy::type_complexity)]

pub mod ctx;
pub mod pipelines;
pub mod queue;
pub mod runner;
pub mod scheduler;

pub use ctx::{Job, JobContext, JobError, JobResult, SiteVectors, VectorCache};
pub use queue::{ClaimedJob, Enqueued, JobQueue, NewJob};
pub use runner::{Runner, RunnerHandle, RunnerOptions, kind_timeout, lower_compute_priority};
pub use scheduler::{Scheduler, SchedulerHandle};
