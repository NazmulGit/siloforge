//! Per-rung circuit breaker (keyed by model row id, or `env:<model>` for env rungs).
//!
//! closed --(N consecutive counted failures)--> open --(cooldown)--> half-open (one probe call)
//! half-open --success--> closed; half-open --failure--> open with a doubled cooldown (capped).
//! While open the rung is skipped, so a dead provider adds no latency to every call.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::types::BreakerView;

const MAX_COOLDOWN: Duration = Duration::from_secs(600);

#[derive(Debug, Default, Clone)]
struct State {
    consecutive: u32,
    open_until: Option<Instant>,
    trips: u32,
    probe_started: Option<Instant>,
    last_error: Option<String>,
}

/// What the breaker allows for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Permit {
    Allow,
    /// Half-open: this call is the single probe.
    Probe,
    /// Open: skip; value = time until a probe is allowed.
    Skip(Duration),
}

pub(crate) struct Breakers {
    map: Mutex<HashMap<String, State>>,
    threshold: u32,
    cooldown: Duration,
}

impl Breakers {
    pub(crate) fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            threshold: threshold.max(1),
            cooldown,
        }
    }

    pub(crate) fn permit(&self, key: &str) -> Permit {
        let now = Instant::now();
        let mut map = self.map.lock();
        let Some(st) = map.get_mut(key) else {
            return Permit::Allow;
        };
        let Some(until) = st.open_until else {
            return Permit::Allow;
        };
        if now < until {
            return Permit::Skip(until - now);
        }
        // Half-open: allow one probe at a time. A probe whose caller vanished (cancelled future)
        // is considered stale after one cooldown and another probe may start.
        if let Some(started) = st.probe_started
            && now.duration_since(started) < self.cooldown.max(Duration::from_secs(1))
        {
            return Permit::Skip(Duration::ZERO);
        }
        st.probe_started = Some(now);
        Permit::Probe
    }

    pub(crate) fn success(&self, key: &str) {
        self.map.lock().remove(key);
    }

    /// A failure that says something about provider health (transport, 5xx, 429, auth, bad body).
    pub(crate) fn failure(&self, key: &str, err: &str) {
        let now = Instant::now();
        let mut map = self.map.lock();
        let st = map.entry(key.to_string()).or_default();
        st.consecutive = st.consecutive.saturating_add(1);
        st.last_error = Some(crate::adapters::truncate(err, 300));
        if st.open_until.is_some() {
            // Failed half-open probe (or a straggler): re-open with a longer cooldown.
            st.trips = st.trips.saturating_add(1);
            let factor = 1u32 << st.trips.saturating_sub(1).min(5);
            let cd = self.cooldown.saturating_mul(factor).min(MAX_COOLDOWN);
            st.open_until = Some(now + cd);
            st.probe_started = None;
        } else if st.consecutive >= self.threshold {
            st.trips = 1;
            st.open_until = Some(now + self.cooldown);
            st.probe_started = None;
        }
    }

    /// A failure that is request-specific (400/413/422, empty reply): no health signal, but a
    /// half-open probe slot is released so the next call can probe.
    pub(crate) fn neutral(&self, key: &str) {
        if let Some(st) = self.map.lock().get_mut(key) {
            st.probe_started = None;
        }
    }

    pub(crate) fn is_open(&self, key: &str) -> bool {
        let now = Instant::now();
        self.map
            .lock()
            .get(key)
            .and_then(|s| s.open_until)
            .is_some_and(|u| now < u)
    }

    pub(crate) fn reset(&self, key: &str) {
        self.map.lock().remove(key);
    }

    pub(crate) fn view(&self, key: &str) -> BreakerView {
        let now = Instant::now();
        let map = self.map.lock();
        match map.get(key) {
            None => BreakerView {
                state: "closed".into(),
                consecutive_failures: 0,
                retry_in_ms: None,
                last_error: None,
            },
            Some(st) => {
                let (state, retry) = match st.open_until {
                    None => ("closed", None),
                    Some(u) if now < u => (
                        "open",
                        Some(u64::try_from((u - now).as_millis()).unwrap_or(u64::MAX)),
                    ),
                    Some(_) => ("half_open", None),
                };
                BreakerView {
                    state: state.into(),
                    consecutive_failures: st.consecutive,
                    retry_in_ms: retry,
                    last_error: st.last_error.clone(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_after_threshold_and_half_opens() {
        let b = Breakers::new(2, Duration::from_millis(30));
        assert_eq!(b.permit("k"), Permit::Allow);
        b.failure("k", "e1");
        assert_eq!(b.permit("k"), Permit::Allow);
        b.failure("k", "e2");
        assert!(matches!(b.permit("k"), Permit::Skip(_)));
        assert_eq!(b.view("k").state, "open");
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(b.permit("k"), Permit::Probe);
        // Second concurrent caller is skipped while the probe is in flight.
        assert!(matches!(b.permit("k"), Permit::Skip(_)));
        b.failure("k", "e3");
        assert!(b.is_open("k"));
        std::thread::sleep(Duration::from_millis(70));
        assert_eq!(b.permit("k"), Permit::Probe);
        b.success("k");
        assert_eq!(b.permit("k"), Permit::Allow);
        assert_eq!(b.view("k").state, "closed");
    }

    #[test]
    fn neutral_releases_probe() {
        let b = Breakers::new(1, Duration::from_millis(10));
        b.failure("k", "e");
        std::thread::sleep(Duration::from_millis(15));
        assert_eq!(b.permit("k"), Permit::Probe);
        b.neutral("k");
        assert_eq!(b.permit("k"), Permit::Probe);
    }
}
