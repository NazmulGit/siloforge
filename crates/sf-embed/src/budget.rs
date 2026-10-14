//! CPU budget for local inference (reliability audit R11).
//!
//! Local rungs run on a tokio blocking thread plus the backend's own worker threads (ORT intra-op
//! pool, candle's rayon pool). Those threads are not part of `sf_engine::pool`, so the compute
//! pool's size and BELOW_NORMAL priority did not apply to them. [`CpuBudget`] carries
//! `resources.cpu_threads` and `resources.low_priority_jobs` down to the rungs:
//!
//! * ORT: intra-op threads = `threads`; with `low_priority` spinning is off and every pool thread
//!   is created by [`LowPriorityThreads`] (below-normal priority).
//! * candle: inference runs inside a private rayon pool of `threads` workers, below normal when
//!   `low_priority`.
//! * The blocking thread that drives inference runs below normal for the duration of the call
//!   ([`PriorityGuard`]) and is restored before it goes back to tokio.

/// Crossplatform priority value that maps to Windows `BELOW_NORMAL` (same as the compute pool).
const BELOW_NORMAL: u8 = 30;
/// Crossplatform priority value that maps to Windows `NORMAL`.
const NORMAL: u8 = 50;

/// How much CPU local inference may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuBudget {
    /// Worker threads for one inference call (`resources.cpu_threads` resolved, at least 1).
    pub threads: usize,
    /// `resources.low_priority_jobs`: below-normal OS priority, no busy-wait spinning.
    pub low_priority: bool,
}

impl CpuBudget {
    pub fn new(threads: usize, low_priority: bool) -> Self {
        Self {
            threads: threads.max(1),
            low_priority,
        }
    }
}

impl Default for CpuBudget {
    fn default() -> Self {
        Self::new(1, false)
    }
}

fn set_priority(value: u8) -> bool {
    let Ok(v) = thread_priority::ThreadPriorityValue::try_from(value) else {
        return false;
    };
    match thread_priority::set_current_thread_priority(
        thread_priority::ThreadPriority::Crossplatform(v),
    ) {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!(error = ?e, "could not change inference thread priority");
            false
        }
    }
}

/// Lower the current thread to below-normal priority (best effort). Returns true on success.
pub fn lower_current_thread() -> bool {
    set_priority(BELOW_NORMAL)
}

/// Lowers the current thread's priority while alive (when `active`) and restores normal priority
/// on drop, so a tokio blocking thread is never handed back at reduced priority.
pub struct PriorityGuard {
    lowered: bool,
}

impl PriorityGuard {
    pub fn new(active: bool) -> Self {
        Self {
            lowered: active && lower_current_thread(),
        }
    }

    pub fn lowered(&self) -> bool {
        self.lowered
    }
}

impl Drop for PriorityGuard {
    fn drop(&mut self) {
        if self.lowered {
            set_priority(NORMAL);
        }
    }
}

/// ORT thread manager: spawns the intra-op pool's threads at below-normal priority.
#[cfg(feature = "onnx")]
#[derive(Debug, Default, Clone, Copy)]
pub struct LowPriorityThreads;

#[cfg(feature = "onnx")]
impl ort::environment::ThreadManager for LowPriorityThreads {
    type Thread = std::thread::JoinHandle<()>;

    fn create(&self, work: impl FnOnce() + Send + 'static) -> ort::Result<Self::Thread> {
        std::thread::Builder::new()
            .name("sf-onnx".into())
            .spawn(move || {
                lower_current_thread();
                work();
            })
            .map_err(|e| ort::Error::new(format!("spawn onnx thread: {e}")))
    }

    fn join(thread: Self::Thread) -> ort::Result<()> {
        thread
            .join()
            .map_err(|_| ort::Error::new("onnx thread panicked"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_has_at_least_one_thread() {
        assert_eq!(CpuBudget::new(0, true).threads, 1);
        assert_eq!(CpuBudget::new(6, false).threads, 6);
        assert!(!CpuBudget::default().low_priority);
    }

    #[test]
    fn guard_restores_priority_on_drop() {
        let h = std::thread::spawn(|| {
            let before = thread_priority::get_current_thread_priority().ok();
            {
                let g = PriorityGuard::new(true);
                if g.lowered() {
                    let during = thread_priority::get_current_thread_priority().ok();
                    assert_ne!(
                        during, before,
                        "priority should be lowered inside the guard"
                    );
                }
            }
            let after = thread_priority::get_current_thread_priority().ok();
            assert_eq!(after, before, "priority restored after the guard");
        });
        assert!(h.join().is_ok());
    }

    #[test]
    fn inactive_guard_changes_nothing() {
        let g = PriorityGuard::new(false);
        assert!(!g.lowered());
    }
}
