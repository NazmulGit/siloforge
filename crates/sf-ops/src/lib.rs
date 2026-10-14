//! SiloForge operations: resource monitor, notifications, error tracker, backup/restore, retention and
//! the v1 Postgres migrator.
//!
//! Wiring (done by the `siloforge serve` binary, inside the tokio runtime):
//! ```ignore
//! let _guard = sf_ops::instance::InstanceGuard::acquire(&cfg.home)?;
//! let notifier = sf_ops::notify::Notifier::start(db.clone(), state.secret_key.clone(), Default::default());
//! let monitor = sf_ops::monitor::Monitor::start(state.cfg.clone(), db.clone(), Some(notifier.clone()));
//! sf_ops::backup::spawn_scheduler(state.cfg.clone(), db.clone(), Some(notifier.clone()));
//! sf_ops::retention::spawn_daily(state.cfg.clone(), db.clone());
//! state.services.insert(Arc::new(notifier));
//! state.services.insert(Arc::new(monitor));
//! state.services.insert(Arc::new(sf_ops::errors::ErrorTracker::new(db.clone())));
//! ```
//! sf-ops does not depend on sf-jobs, so the job runner can use [`monitor::JobMeter`], the notifier and
//! the error tracker.

pub mod backup;
pub mod errors;
pub mod eventlog;
pub mod instance;
pub mod migrate_v1;
pub mod monitor;
pub mod notify;
pub mod retention;
pub mod supervise;
