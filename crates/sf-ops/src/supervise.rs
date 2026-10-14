//! Supervision for long-running background loops (R8).
//!
//! [`spawn`] runs a loop factory in its own task. When the loop panics, or returns while it was
//! expected to run forever, the supervisor records it in the error tracker, waits with exponential
//! backoff (1 s doubling up to 60 s) and starts a fresh loop. A loop that returns because its
//! service is shutting down signals that by returning [`Exit::Stop`]. Liveness of every supervised
//! service is kept in a process-wide registry ([`services`]) for `/health` and `/api/v2/system`.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::LazyLock;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use serde_json::json;
use sf_store::Db;

use crate::errors::ErrorTracker;

/// How a supervised loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Intentional stop (shutdown): do not restart.
    Stop,
    /// The loop gave up: restart it after a backoff.
    Restart,
}

/// Liveness of one supervised service.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ServiceState {
    pub running: bool,
    pub restarts: u64,
    pub last_error: Option<String>,
    pub last_error_at: Option<String>,
}

static REGISTRY: LazyLock<Mutex<BTreeMap<String, ServiceState>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Snapshot of every supervised service.
pub fn services() -> BTreeMap<String, ServiceState> {
    REGISTRY.lock().clone()
}

/// True when every supervised service that has not been stopped on purpose is running.
pub fn all_running() -> bool {
    REGISTRY.lock().values().all(|s| s.running)
}

fn set(name: &str, f: impl FnOnce(&mut ServiceState)) {
    let mut r = REGISTRY.lock();
    f(r.entry(name.to_string()).or_default());
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

/// Backoff policy (tests shorten it).
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub first: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            first: Duration::from_secs(1),
            max: Duration::from_secs(60),
        }
    }
}

/// Spawns `make()` under supervision. `db` (when given) receives an `errors` row per crash.
pub fn spawn<F, Fut>(name: &str, db: Option<Db>, make: F) -> tokio::task::JoinHandle<()>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Exit> + Send + 'static,
{
    spawn_with(name, db, Backoff::default(), make)
}

pub fn spawn_with<F, Fut>(
    name: &str,
    db: Option<Db>,
    backoff: Backoff,
    make: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Exit> + Send + 'static,
{
    let name = name.to_string();
    let errors = db.map(ErrorTracker::new);
    tokio::spawn(async move {
        let mut wait = backoff.first;
        loop {
            set(&name, |s| s.running = true);
            let started = std::time::Instant::now();
            let msg = match tokio::spawn(make()).await {
                Ok(Exit::Stop) => {
                    set(&name, |s| s.running = false);
                    tracing::info!(service = %name, "background service stopped");
                    return;
                }
                Ok(Exit::Restart) => "loop exited unexpectedly".to_string(),
                Err(e) if e.is_cancelled() => {
                    set(&name, |s| s.running = false);
                    return;
                }
                Err(e) => format!("panicked: {}", panic_text(e)),
            };
            tracing::error!(service = %name, error = %msg, "background service died; restarting");
            set(&name, |s| {
                s.running = false;
                s.restarts = s.restarts.saturating_add(1);
                s.last_error = Some(msg.clone());
                s.last_error_at = Some(sf_core::now_rfc3339());
            });
            if let Some(t) = &errors
                && let Err(e) = t
                    .record(&format!("service:{name}"), &msg, json!({"service": name}))
                    .await
            {
                tracing::warn!(error = %e, "could not record service failure");
            }
            // a loop that ran for a while starts the backoff over
            if started.elapsed() > backoff.max.saturating_mul(2) {
                wait = backoff.first;
            }
            tokio::time::sleep(wait).await;
            wait = wait.saturating_mul(2).min(backoff.max);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::panic)]
    async fn panicking_loop_is_restarted_and_recorded() {
        let dir = std::env::temp_dir().join(format!("sf-ops-sup-{}", sf_core::new_id()));
        let db = Db::open(&dir.join("t.db")).unwrap();
        let runs = Arc::new(AtomicU32::new(0));
        let r2 = runs.clone();
        let fast = Backoff {
            first: Duration::from_millis(5),
            max: Duration::from_millis(20),
        };
        let h = spawn_with("test-loop", Some(db.clone()), fast, move || {
            let r = r2.clone();
            async move {
                let n = r.fetch_add(1, Ordering::SeqCst);
                match n {
                    0 => panic!("first run blows up"),
                    1 => Exit::Restart,
                    _ => Exit::Stop,
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(10), h)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        let st = services().get("test-loop").cloned().unwrap();
        assert_eq!(st.restarts, 2);
        assert!(!st.running);
        let n: i64 = db
            .read(|c| {
                c.query_row(
                    "SELECT count FROM errors WHERE source='service:test-loop' AND message LIKE '%blows up%'",
                    [],
                    |r| r.get(0),
                )
                .map_err(sf_store::err)
            })
            .await
            .unwrap();
        assert_eq!(n, 1);
        db.close();
        let _ = std::fs::remove_dir_all(dir);
    }
}
