//! Live configuration (v2.1): one shared, swappable [`Config`] snapshot.
//!
//! `PUT /api/v2/settings` validates a change, persists it and swaps the snapshot here. Services
//! that support live changes (job runner, scheduler, monitor, vector cache, crawl/WP client
//! factory, AI env fallback, embed chain) hold a [`LiveConfig`] and call [`LiveConfig::get`] on
//! each use instead of keeping a copy from boot. Reads are a cheap `Arc` clone.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::Config;

type Listener = Box<dyn Fn(&Arc<Config>) + Send + Sync>;

struct Inner {
    cur: RwLock<Arc<Config>>,
    generation: AtomicU64,
    listeners: RwLock<Vec<Listener>>,
}

/// Cheap-to-clone handle to the current configuration.
#[derive(Clone)]
pub struct LiveConfig {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LiveConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveConfig")
            .field("generation", &self.generation())
            .finish_non_exhaustive()
    }
}

impl LiveConfig {
    pub fn new(cfg: Config) -> Self {
        Self::from_arc(Arc::new(cfg))
    }

    pub fn from_arc(cfg: Arc<Config>) -> Self {
        Self {
            inner: Arc::new(Inner {
                cur: RwLock::new(cfg),
                generation: AtomicU64::new(0),
                listeners: RwLock::new(Vec::new()),
            }),
        }
    }

    /// The current snapshot (a poisoned lock still yields the value).
    pub fn get(&self) -> Arc<Config> {
        match self.inner.cur.read() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    /// Replace the snapshot, bump the generation and run the change listeners.
    pub fn set(&self, cfg: Config) {
        let new = Arc::new(cfg);
        match self.inner.cur.write() {
            Ok(mut g) => *g = new.clone(),
            Err(p) => *p.into_inner() = new.clone(),
        }
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        let listeners = match self.inner.listeners.read() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        for l in listeners.iter() {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| l(&new))).is_err() {
                tracing::error!("live config listener panicked");
            }
        }
    }

    /// Number of `set` calls so far (lets caches notice a change cheaply).
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::SeqCst)
    }

    /// Run `f` after every `set` with the new snapshot (keep it cheap; it runs on the caller).
    pub fn on_change(&self, f: impl Fn(&Arc<Config>) + Send + Sync + 'static) {
        match self.inner.listeners.write() {
            Ok(mut g) => g.push(Box::new(f)),
            Err(p) => p.into_inner().push(Box::new(f)),
        }
    }
}

impl From<Config> for LiveConfig {
    fn from(c: Config) -> Self {
        Self::new(c)
    }
}

impl From<Arc<Config>> for LiveConfig {
    fn from(c: Arc<Config>) -> Self {
        Self::from_arc(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn set_swaps_and_notifies() {
        let live = LiveConfig::new(Config::default());
        let seen = Arc::new(AtomicUsize::new(0));
        let s2 = seen.clone();
        live.on_change(move |c| {
            s2.store(c.resources.max_concurrent_jobs, Ordering::SeqCst);
        });
        let old = live.get();
        let mut c = (*old).clone();
        c.resources.max_concurrent_jobs = 7;
        live.set(c);
        assert_eq!(live.get().resources.max_concurrent_jobs, 7);
        assert_eq!(
            old.resources.max_concurrent_jobs, 3,
            "old snapshot untouched"
        );
        assert_eq!(seen.load(Ordering::SeqCst), 7);
        assert_eq!(live.generation(), 1);
        // a panicking listener never breaks set()
        live.on_change(|_| panic!("boom"));
        live.set(Config::default());
        assert_eq!(live.generation(), 2);
    }
}
