//! The one shared rayon pool for CPU-heavy engine work (AGENTS.md rule 8).
//!
//! Every parallel routine in `sf-engine` (`vector::batch_top_k`, `score::score_batch`, k-means
//! assignment, ...) uses rayon's *current* pool. Run them through [`install`] (or [`spawn_compute`] /
//! [`spawn_with_callback`]) so they land on this bounded pool instead of rayon's implicit global one.
//!
//! Lifecycle: call [`init`] once at startup with `Config::cpu_threads()`. If nobody calls it, the
//! first use initialises the pool with [`default_threads`] (logical CPUs / 4, at least 1).
//!
//! Bridging to async (sf-engine never depends on tokio). Two options for `sf-jobs`:
//! 1. Callback + tokio oneshot (no blocked tokio thread, preferred):
//!    ```ignore
//!    let (tx, rx) = tokio::sync::oneshot::channel();
//!    sf_engine::pool::spawn_with_callback(move || heavy(), move |r| { let _ = tx.send(r); });
//!    let result = rx.await??; // Result<R, ComputeError>
//!    ```
//! 2. `std::sync::mpsc` receiver awaited on the blocking pool:
//!    ```ignore
//!    let rx = sf_engine::pool::spawn_compute(move || heavy());
//!    let result = tokio::task::spawn_blocking(move || rx.recv()).await??;
//!    ```
//!
//! Panics inside a job are caught and reported as [`ComputeError::Panicked`]; they never take
//! down a pool thread or the process.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::sync::mpsc;

/// Upper bound on pool threads, whatever the config says.
pub const MAX_THREADS: usize = 256;

static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();

type StartHandler = Box<dyn Fn(usize) + Send + Sync>;
static START_HANDLER: OnceLock<StartHandler> = OnceLock::new();

/// Register a callback that runs on every compute thread when it starts (argument = thread
/// index), e.g. to lower the OS thread priority for bulk jobs. Must be called before the pool is
/// first built ([`init`] or first use); returns `false` when it is too late or already set.
pub fn set_thread_start_handler(f: impl Fn(usize) + Send + Sync + 'static) -> bool {
    if POOL.get().is_some() {
        return false;
    }
    START_HANDLER.set(Box::new(f)).is_ok()
}

/// Why a pool operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComputeError {
    /// [`init`] was called after the pool already existed (the first configuration wins).
    AlreadyInitialized { threads: usize },
    /// rayon could not start its threads.
    Build(String),
    /// The job panicked; the message is the panic payload when it was a string.
    Panicked(String),
}

impl std::fmt::Display for ComputeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyInitialized { threads } => {
                write!(f, "compute pool already initialised with {threads} threads")
            }
            Self::Build(e) => write!(f, "compute pool failed to start: {e}"),
            Self::Panicked(m) => write!(f, "compute job panicked: {m}"),
        }
    }
}

impl std::error::Error for ComputeError {}

/// Default size when [`init`] was never called: logical CPUs / 4, at least 1.
pub fn default_threads() -> usize {
    let logical = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    (logical / 4).max(1)
}

fn build(threads: usize) -> Result<rayon::ThreadPool, ComputeError> {
    let threads = threads.clamp(1, MAX_THREADS);
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("sf-compute-{i}"))
        .start_handler(|i| {
            if let Some(h) = START_HANDLER.get()
                && std::panic::catch_unwind(AssertUnwindSafe(|| h(i))).is_err()
            {
                tracing::warn!("compute thread start handler panicked");
            }
        })
        .panic_handler(|_| tracing::error!("panic escaped a compute job"))
        .build()
        .map_err(|e| ComputeError::Build(e.to_string()))
}

/// Configure the pool once. `threads` is clamped to `1..=MAX_THREADS`; pass
/// `Config::cpu_threads()`. A second call returns `AlreadyInitialized` and changes nothing.
pub fn init(threads: usize) -> Result<usize, ComputeError> {
    if let Some(p) = POOL.get() {
        return Err(ComputeError::AlreadyInitialized {
            threads: p.current_num_threads(),
        });
    }
    let pool = build(threads)?;
    let n = pool.current_num_threads();
    match POOL.set(pool) {
        Ok(()) => {
            tracing::info!(threads = n, "compute pool ready");
            Ok(n)
        }
        Err(_) => Err(ComputeError::AlreadyInitialized {
            threads: POOL.get().map_or(n, rayon::ThreadPool::current_num_threads),
        }),
    }
}

fn pool() -> Option<&'static rayon::ThreadPool> {
    if let Some(p) = POOL.get() {
        return Some(p);
    }
    match build(default_threads()) {
        Ok(p) => {
            let _ = POOL.set(p);
        }
        Err(e) => tracing::error!(error = %e, "compute pool init failed; running inline"),
    }
    POOL.get()
}

/// Number of threads in the pool (initialising it with the default size if needed).
pub fn threads() -> usize {
    pool().map_or(1, rayon::ThreadPool::current_num_threads)
}

/// Run `f` inside the pool and block the caller until it returns. Rayon parallel iterators
/// inside `f` use this pool. A panic in `f` propagates to the caller (same as calling `f`
/// directly); use [`try_install`] to catch it. Do not call from a tokio worker thread.
pub fn install<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    match pool() {
        Some(p) => p.install(f),
        None => f(),
    }
}

/// Like [`install`] but a panic in `f` becomes `ComputeError::Panicked`.
pub fn try_install<R: Send>(f: impl FnOnce() -> R + Send) -> Result<R, ComputeError> {
    install(move || catch_unwind(AssertUnwindSafe(f)).map_err(|p| panic_message(p.as_ref())))
}

/// Queue `f` on the pool without blocking. The result (or the panic) arrives on the returned
/// receiver exactly once. See the module docs for awaiting it from async code.
pub fn spawn_compute<R: Send + 'static>(
    f: impl FnOnce() -> R + Send + 'static,
) -> mpsc::Receiver<Result<R, ComputeError>> {
    let (tx, rx) = mpsc::sync_channel(1);
    spawn_with_callback(f, move |r| {
        let _ = tx.send(r);
    });
    rx
}

/// Queue `f` on the pool; `done` is called on a pool thread with the result. Keep `done` cheap
/// (e.g. send on a tokio oneshot).
pub fn spawn_with_callback<R: Send + 'static>(
    f: impl FnOnce() -> R + Send + 'static,
    done: impl FnOnce(Result<R, ComputeError>) + Send + 'static,
) {
    let job = move || {
        let r = catch_unwind(AssertUnwindSafe(f)).map_err(|p| panic_message(p.as_ref()));
        if catch_unwind(AssertUnwindSafe(move || done(r))).is_err() {
            tracing::error!("compute completion callback panicked");
        }
    };
    match pool() {
        Some(p) => p.spawn(job),
        None => job(),
    }
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> ComputeError {
    let msg = p
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string());
    ComputeError::Panicked(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayon::prelude::*;

    #[test]
    fn install_runs_parallel_work_on_the_pool() {
        let sum: u64 = install(|| (0..10_000u64).into_par_iter().sum());
        assert_eq!(sum, 49_995_000);
        assert!(threads() >= 1);
    }

    #[test]
    fn second_init_is_rejected_not_fatal() {
        let _ = threads(); // force lazy init
        assert!(matches!(
            init(3),
            Err(ComputeError::AlreadyInitialized { .. })
        ));
    }

    #[test]
    fn spawn_compute_delivers_result() {
        let rx = spawn_compute(|| 6 * 7);
        assert_eq!(rx.recv().ok(), Some(Ok(42)));
    }

    #[test]
    #[allow(clippy::panic)]
    fn panics_are_caught() {
        let rx = spawn_compute(|| -> u8 { panic!("boom") });
        match rx.recv() {
            Ok(Err(ComputeError::Panicked(m))) => assert!(m.contains("boom")),
            other => panic!("unexpected {other:?}"),
        }
        let r = try_install(|| -> u8 { panic!("again") });
        assert!(matches!(r, Err(ComputeError::Panicked(_))));
        // pool still works afterwards
        assert_eq!(install(|| 1 + 1), 2);
    }

    #[test]
    fn callback_api_works() {
        let (tx, rx) = mpsc::channel();
        spawn_with_callback(
            || "ok".to_string(),
            move |r| {
                let _ = tx.send(r);
            },
        );
        assert_eq!(rx.recv().ok(), Some(Ok("ok".to_string())));
    }
}
