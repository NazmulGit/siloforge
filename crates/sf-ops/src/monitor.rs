//! Resource monitor.
//!
//! * A background task samples the machine every `monitor.sample_secs` (sysinfo): machine CPU %,
//!   this process' CPU % (normalised to the whole machine), RAM used/total, process RSS, free disk on
//!   the volume holding `SILOFORGE_HOME`, and job counts (`job_runs` running/queued).
//! * Raw samples stay in an in-memory ring buffer (last hour). Every minute the samples of the previous
//!   minute are averaged into one `system_metrics` row; rows older than `monitor.retain_days` are pruned.
//! * Threshold alerts (CPU, RAM, disk; checked on each 1-minute aggregate) become `system.alert`
//!   notifications, debounced to one per kind per 30 minutes.
//! * [`JobMeter`] measures one job (peak RSS, CPU) for `job_runs.resources`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rusqlite::params;
use serde::Serialize;
use serde_json::{Value, json};
use sf_core::{Config, Result};
use sf_store::{Db, err, row_to_json};
use sysinfo::{Disks, Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::notify::{Notification, Notifier, events};

const MB: u64 = 1024 * 1024;
const RAW_WINDOW: Duration = Duration::from_secs(3600);
const ALERT_DEBOUNCE: Duration = Duration::from_secs(30 * 60);

/// One raw sample.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Sample {
    pub ts: String,
    /// Whole-machine CPU, 0..100.
    pub cpu_pct: f32,
    /// This process, as a share of the whole machine, 0..100.
    pub proc_cpu_pct: f32,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    pub proc_rss_mb: u64,
    pub disk_free_mb: Option<u64>,
    pub jobs_running: i64,
    pub jobs_queued: i64,
}

/// Static machine facts plus configured budgets.
#[derive(Debug, Clone, Serialize)]
pub struct Capacity {
    pub physical_cores: usize,
    pub logical_cpus: usize,
    /// `resources.cpu_threads` resolved (rayon pool size).
    pub cpu_threads: usize,
    pub max_concurrent_jobs: usize,
    pub mem_total_mb: u64,
    pub mem_available_mb: u64,
    /// `resources.vector_cache_mb`.
    pub vector_cache_mb: usize,
    /// Heuristic: min(logical/4 rounded up, (available RAM - vector cache) / 512 MB), clamped 1..=8.
    pub recommended_max_concurrent_jobs: usize,
    pub gpu: Option<String>,
    pub os: Option<String>,
    pub hostname: Option<String>,
}

struct Sampler {
    sys: System,
    pid: Option<Pid>,
    home: PathBuf,
    ncpu: usize,
}

impl Sampler {
    fn new(home: &Path) -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let pid = sysinfo::get_current_pid().ok();
        let ncpu = sys.cpus().len().max(1);
        let mut s = Self {
            sys,
            pid,
            home: std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf()),
            ncpu,
        };
        s.refresh_proc();
        s
    }

    fn refresh_proc(&mut self) {
        if let Some(pid) = self.pid {
            self.sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                false, // "true" drops our own pid on the 2nd refresh (sysinfo 0.37 on Windows)
                ProcessRefreshKind::nothing().with_cpu().with_memory(),
            );
        }
    }

    /// Takes one sample (blocking, a few ms). Job counts are filled in by the caller.
    fn sample(&mut self) -> Sample {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.refresh_proc();
        let (proc_cpu, rss) = self
            .pid
            .and_then(|p| self.sys.process(p))
            .map(|p| (p.cpu_usage() / self.ncpu as f32, p.memory()))
            .unwrap_or((0.0, 0));
        Sample {
            ts: sf_core::now_rfc3339(),
            cpu_pct: finite(self.sys.global_cpu_usage()).clamp(0.0, 100.0),
            proc_cpu_pct: finite(proc_cpu).clamp(0.0, 100.0),
            mem_used_mb: self.sys.used_memory() / MB,
            mem_total_mb: self.sys.total_memory() / MB,
            proc_rss_mb: rss / MB,
            disk_free_mb: disk_free_mb(&self.home),
            jobs_running: 0,
            jobs_queued: 0,
        }
    }
}

fn finite(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

/// Free space (MB) on the disk whose mount point is the longest prefix of `path`.
pub fn disk_free_mb(path: &Path) -> Option<u64> {
    let disks = Disks::new_with_refreshed_list();
    let raw = path.to_string_lossy().replace('/', "\\");
    // canonicalize() on Windows yields a verbatim `\\?\D:\...` path
    let path_s = raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_lowercase();
    disks
        .list()
        .iter()
        .filter(|d| {
            let mp = d.mount_point().to_string_lossy().to_lowercase();
            path.starts_with(d.mount_point()) || path_s.starts_with(&mp.replace('/', "\\"))
        })
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space() / MB)
}

#[derive(Default)]
struct AlertState {
    last: Option<Instant>,
}

struct Inner {
    db: Db,
    cfg: Arc<Config>,
    notifier: Option<Notifier>,
    sampler: Mutex<Option<Sampler>>,
    ring: Mutex<VecDeque<Sample>>,
    minute: Mutex<(String, Vec<Sample>)>,
    alerts: Mutex<[AlertState; 3]>,
    gpu: Mutex<Option<String>>,
    capacity_cache: Mutex<Option<(Instant, Capacity)>>,
}

/// Cheap-to-clone monitor handle. Register it as a service (`AppState::services`).
#[derive(Clone)]
pub struct Monitor {
    inner: Arc<Inner>,
}

impl Monitor {
    /// Creates a monitor without starting the background task (tests, CLI `doctor`).
    pub fn new(cfg: Arc<Config>, db: Db, notifier: Option<Notifier>) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                cfg,
                notifier,
                sampler: Mutex::new(None),
                ring: Mutex::new(VecDeque::new()),
                minute: Mutex::new((String::new(), Vec::new())),
                alerts: Mutex::new(Default::default()),
                gpu: Mutex::new(None),
                capacity_cache: Mutex::new(None),
            }),
        }
    }

    /// Creates the monitor and spawns its sampling loop (and a one-off GPU name probe on Windows).
    /// Must be called inside a tokio runtime.
    pub fn start(cfg: Arc<Config>, db: Db, notifier: Option<Notifier>) -> Self {
        let m = Self::new(cfg, db, notifier);
        let me = m.clone();
        tokio::spawn(async move {
            let gpu = detect_gpu().await;
            *me.inner.gpu.lock() = gpu;
        });
        let me0 = m.clone();
        let db = m.inner.db.clone();
        // R8: supervised (a panic restarts the sampler instead of silently ending monitoring).
        crate::supervise::spawn("monitor", Some(db), move || {
            let me = me0.clone();
            async move {
                let every = Duration::from_secs(me.inner.cfg.monitor.sample_secs.clamp(1, 3600));
                let mut tick = tokio::time::interval(every);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut last_prune = Instant::now() - Duration::from_secs(7200);
                loop {
                    tick.tick().await;
                    if let Err(e) = me.sample_now().await {
                        tracing::debug!(error = %e, "monitor sample failed");
                    }
                    if last_prune.elapsed() > Duration::from_secs(3600) {
                        last_prune = Instant::now();
                        if let Err(e) = me.prune().await {
                            tracing::warn!(error = %e, "system_metrics prune failed");
                        }
                    }
                }
            }
        });
        m
    }

    /// Takes one sample now: records it in the ring buffer, rolls the minute aggregate and checks alerts.
    pub async fn sample_now(&self) -> Result<Sample> {
        let inner = self.inner.clone();
        let mut s = tokio::task::spawn_blocking(move || {
            let mut guard = inner.sampler.lock();
            let sampler = guard.get_or_insert_with(|| Sampler::new(&inner.cfg.home));
            sampler.sample()
        })
        .await
        .map_err(|e| sf_core::Error::internal(format!("sampler: {e}")))?;
        let (running, queued) = job_counts(&self.inner.db).await.unwrap_or((0, 0));
        s.jobs_running = running;
        s.jobs_queued = queued;
        self.push(s.clone()).await?;
        Ok(s)
    }

    async fn push(&self, s: Sample) -> Result<()> {
        {
            let mut ring = self.inner.ring.lock();
            ring.push_back(s.clone());
            let cap = (RAW_WINDOW.as_secs() / self.inner.cfg.monitor.sample_secs.max(1)) as usize;
            while ring.len() > cap.max(12) {
                ring.pop_front();
            }
        }
        let minute_key = s.ts.get(..16).unwrap_or("").to_string(); // YYYY-MM-DDTHH:MM
        let flush = {
            let mut m = self.inner.minute.lock();
            if m.0 == minute_key {
                m.1.push(s.clone());
                None
            } else {
                let prev = std::mem::replace(&mut *m, (minute_key, vec![s.clone()]));
                (!prev.1.is_empty()).then_some(prev)
            }
        };
        if let Some((key, samples)) = flush
            && let Some(agg) = aggregate(&samples)
        {
            let ts = format!("{key}:00.000Z");
            self.persist(&ts, &agg).await?;
            self.check_alerts(&agg);
        }
        Ok(())
    }

    async fn persist(&self, ts: &str, a: &Sample) -> Result<()> {
        let (ts, a) = (ts.to_string(), a.clone());
        self.inner
            .db
            .write(move |t| {
                t.execute(
                    "INSERT OR REPLACE INTO system_metrics(ts, cpu_pct, proc_cpu_pct, mem_used_mb, mem_total_mb,
                       proc_rss_mb, jobs_running, jobs_queued, disk_free_mb) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![
                        ts,
                        a.cpu_pct as f64,
                        a.proc_cpu_pct as f64,
                        a.mem_used_mb as i64,
                        a.mem_total_mb as i64,
                        a.proc_rss_mb as i64,
                        a.jobs_running,
                        a.jobs_queued,
                        a.disk_free_mb.map(|d| d as i64)
                    ],
                )
                .map_err(err)?;
                Ok(())
            })
            .await
    }

    fn check_alerts(&self, a: &Sample) {
        let m = &self.inner.cfg.monitor;
        let mem_pct = if a.mem_total_mb > 0 {
            a.mem_used_mb as f32 * 100.0 / a.mem_total_mb as f32
        } else {
            0.0
        };
        let checks = [
            (
                m.alert_cpu_pct > 0.0 && a.cpu_pct >= m.alert_cpu_pct,
                "cpu",
                format!(
                    "CPU at {:.0}% (threshold {:.0}%)",
                    a.cpu_pct, m.alert_cpu_pct
                ),
            ),
            (
                m.alert_mem_pct > 0.0 && mem_pct >= m.alert_mem_pct,
                "memory",
                format!(
                    "RAM at {mem_pct:.0}% ({} / {} MB, threshold {:.0}%)",
                    a.mem_used_mb, a.mem_total_mb, m.alert_mem_pct
                ),
            ),
            (
                m.alert_disk_free_mb > 0
                    && a.disk_free_mb.is_some_and(|d| d < m.alert_disk_free_mb),
                "disk",
                format!(
                    "Free disk {} MB below {} MB",
                    a.disk_free_mb.unwrap_or(0),
                    m.alert_disk_free_mb
                ),
            ),
        ];
        let mut fire = Vec::new();
        {
            let mut st = self.inner.alerts.lock();
            for (i, (bad, kind, msg)) in checks.into_iter().enumerate() {
                let Some(s) = st.get_mut(i) else { continue };
                if !bad {
                    continue;
                }
                // at most one alert per kind per debounce window (also while it stays bad)
                if s.last.is_none_or(|t| t.elapsed() >= ALERT_DEBOUNCE) {
                    s.last = Some(Instant::now());
                    fire.push((kind, msg));
                }
            }
        }
        for (kind, msg) in fire {
            tracing::warn!(kind, %msg, "system alert");
            if let Some(n) = &self.inner.notifier {
                n.notify(
                    Notification::new(events::SYSTEM_ALERT, format!("SiloForge {kind} alert"), msg)
                        .data(json!({"kind": kind, "sample": a})),
                );
            }
        }
    }

    /// Raw samples currently in memory (oldest first, at most ~1 h).
    pub fn recent(&self) -> Vec<Sample> {
        self.inner.ring.lock().iter().cloned().collect()
    }

    /// Latest raw sample, if any.
    pub fn latest(&self) -> Option<Sample> {
        self.inner.ring.lock().back().cloned()
    }

    /// Machine capacity and configured budgets (cached for 60 s).
    pub fn capacity(&self) -> Capacity {
        if let Some((t, c)) = self.inner.capacity_cache.lock().as_ref()
            && t.elapsed() < Duration::from_secs(60)
        {
            let mut c = c.clone();
            c.gpu = self.inner.gpu.lock().clone();
            return c;
        }
        let c = compute_capacity(&self.inner.cfg, self.inner.gpu.lock().clone());
        *self.inner.capacity_cache.lock() = Some((Instant::now(), c.clone()));
        c
    }

    /// Stored 1-minute aggregates since `since` (RFC 3339). `bucket_chars` groups by a `ts` prefix
    /// (16 = minute, 15 = 10 minutes, 13 = hour) and averages each group.
    pub async fn history(&self, since: &str, bucket_chars: usize) -> Result<Vec<Value>> {
        metrics_history(&self.inner.db, since, bucket_chars).await
    }

    /// Deletes `system_metrics` rows older than `monitor.retain_days`.
    pub async fn prune(&self) -> Result<usize> {
        let days = i64::from(self.inner.cfg.monitor.retain_days.max(1));
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(days))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        self.inner
            .db
            .write(move |t| {
                t.execute("DELETE FROM system_metrics WHERE ts < ?1", [&cutoff])
                    .map_err(err)
            })
            .await
    }
}

fn aggregate(samples: &[Sample]) -> Option<Sample> {
    let last = samples.last()?;
    let n = samples.len() as f64;
    let avg = |f: &dyn Fn(&Sample) -> f64| samples.iter().map(f).sum::<f64>() / n;
    Some(Sample {
        ts: last.ts.clone(),
        cpu_pct: avg(&|s| f64::from(s.cpu_pct)) as f32,
        proc_cpu_pct: avg(&|s| f64::from(s.proc_cpu_pct)) as f32,
        mem_used_mb: avg(&|s| s.mem_used_mb as f64).round() as u64,
        mem_total_mb: last.mem_total_mb,
        proc_rss_mb: samples.iter().map(|s| s.proc_rss_mb).max().unwrap_or(0),
        disk_free_mb: samples.iter().filter_map(|s| s.disk_free_mb).min(),
        jobs_running: samples.iter().map(|s| s.jobs_running).max().unwrap_or(0),
        jobs_queued: samples.iter().map(|s| s.jobs_queued).max().unwrap_or(0),
    })
}

/// Same query as `JobQueue::counts` (sf-ops does not depend on sf-jobs, so the runner can use sf-ops).
pub async fn job_counts(db: &Db) -> Result<(i64, i64)> {
    db.read(|c| {
        c.query_row(
            "SELECT sum(status='running'), sum(status='queued') FROM job_runs WHERE status IN ('running','queued')",
            [],
            |r| Ok((r.get::<_, Option<i64>>(0)?.unwrap_or(0), r.get::<_, Option<i64>>(1)?.unwrap_or(0))),
        )
        .map_err(err)
    })
    .await
}

/// Reads `system_metrics` since `since`, grouped by the first `bucket_chars` chars of `ts`.
pub async fn metrics_history(db: &Db, since: &str, bucket_chars: usize) -> Result<Vec<Value>> {
    let since = since.to_string();
    let b = bucket_chars.clamp(10, 24) as i64;
    db.read(move |c| {
        let mut stmt = c
            .prepare(
                "SELECT min(ts) AS ts, round(avg(cpu_pct),1) AS cpu_pct, round(avg(proc_cpu_pct),1) AS proc_cpu_pct,
                   CAST(avg(mem_used_mb) AS INTEGER) AS mem_used_mb, max(mem_total_mb) AS mem_total_mb,
                   max(proc_rss_mb) AS proc_rss_mb, max(jobs_running) AS jobs_running, max(jobs_queued) AS jobs_queued,
                   min(disk_free_mb) AS disk_free_mb
                 FROM system_metrics WHERE ts >= ?1 GROUP BY substr(ts, 1, ?2) ORDER BY ts",
            )
            .map_err(err)?;
        let rows = stmt
            .query_map(params![since, b], |r| Ok(row_to_json(r, &[], &[])))
            .map_err(err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(err)?;
        Ok(rows)
    })
    .await
}

/// Computes the capacity view (blocking for a few ms; cached by `Monitor::capacity`).
pub fn compute_capacity(cfg: &Config, gpu: Option<String>) -> Capacity {
    let mut sys = System::new();
    sys.refresh_memory();
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let physical = System::physical_core_count().unwrap_or(logical.div_ceil(2).max(1));
    let total = sys.total_memory() / MB;
    let avail = sys.available_memory() / MB;
    let by_cpu = logical.div_ceil(4).max(1);
    let by_ram = (avail.saturating_sub(cfg.resources.vector_cache_mb as u64) / 512) as usize;
    Capacity {
        physical_cores: physical,
        logical_cpus: logical,
        cpu_threads: cfg.cpu_threads(),
        max_concurrent_jobs: cfg.resources.max_concurrent_jobs,
        mem_total_mb: total,
        mem_available_mb: avail,
        vector_cache_mb: cfg.resources.vector_cache_mb,
        recommended_max_concurrent_jobs: by_cpu.min(by_ram).clamp(1, 8),
        gpu,
        os: System::long_os_version(),
        hostname: System::host_name(),
    }
}

/// Best-effort GPU name (Windows: CIM query through PowerShell, 10 s timeout). None elsewhere.
pub async fn detect_gpu() -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let fut = tokio::process::Command::new(crate::notify::powershell_exe())
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-CimInstance Win32_VideoController | Select-Object -ExpandProperty Name) -join '; '",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(10), fut)
        .await
        .ok()?
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then(|| s.chars().take(300).collect())
}

// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct MeterStats {
    peak_rss: u64,
    cpu_sum: f64,
    cpu_n: u64,
    cpu_ms_start: Option<u64>,
    cpu_ms_last: Option<u64>,
}

/// Per-job resource meter. Samples this process once per second on a small background thread.
/// Values are process-wide while the job runs (concurrent jobs share the process).
///
/// ```ignore
/// let m = JobMeter::start();
/// /* run the job */
/// queue.finish(id, stats, m.finish()).await?;
/// ```
pub struct JobMeter {
    started: Instant,
    stop: Arc<AtomicBool>,
    stats: Arc<Mutex<MeterStats>>,
    thread: Option<std::thread::Thread>,
}

impl JobMeter {
    /// Starts measuring (spawns the sampler thread; never fails — metrics are just empty if it can't).
    pub fn start() -> Self {
        Self::start_every(Duration::from_secs(1))
    }

    /// Like `start` with a custom sampling period (tests).
    pub fn start_every(period: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Mutex::new(MeterStats::default()));
        let (s2, st2) = (stop.clone(), stats.clone());
        let handle = std::thread::Builder::new()
            .name("sf-job-meter".into())
            .spawn(move || {
                let Ok(pid) = sysinfo::get_current_pid() else {
                    return;
                };
                let mut sys = System::new();
                let ncpu = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1) as f64;
                let kind = ProcessRefreshKind::nothing().with_cpu().with_memory();
                let mut first = true;
                while !s2.load(Ordering::Relaxed) {
                    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), false, kind);
                    if let Some(p) = sys.process(pid) {
                        let mut st = st2.lock();
                        st.peak_rss = st.peak_rss.max(p.memory());
                        let cpu_ms = p.accumulated_cpu_time();
                        if st.cpu_ms_start.is_none() {
                            st.cpu_ms_start = Some(cpu_ms);
                        }
                        st.cpu_ms_last = Some(cpu_ms);
                        if !first {
                            // first cpu_usage reading has no baseline
                            st.cpu_sum += f64::from(finite(p.cpu_usage())) / ncpu;
                            st.cpu_n += 1;
                        }
                    }
                    first = false;
                    std::thread::park_timeout(period);
                }
            })
            .ok();
        Self {
            started: Instant::now(),
            stop,
            stats,
            thread: handle.map(|h| h.thread().clone()),
        }
    }

    /// Stops measuring and returns `{peak_rss_mb, avg_cpu_pct, cpu_secs, wall_ms}`.
    pub fn finish(mut self) -> Value {
        self.halt();
        let mut st = self.stats.lock();
        if st.cpu_ms_last.is_none() {
            // very short job: the sampler thread never ran; take one reading inline
            if let Ok(pid) = sysinfo::get_current_pid() {
                let mut sys = System::new();
                sys.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[pid]),
                    false,
                    ProcessRefreshKind::nothing().with_cpu().with_memory(),
                );
                if let Some(p) = sys.process(pid) {
                    st.peak_rss = st.peak_rss.max(p.memory());
                }
            }
        }
        let cpu_secs = match (st.cpu_ms_start, st.cpu_ms_last) {
            (Some(a), Some(b)) => b.saturating_sub(a) as f64 / 1000.0,
            _ => 0.0,
        };
        let avg = if st.cpu_n > 0 {
            st.cpu_sum / st.cpu_n as f64
        } else {
            0.0
        };
        json!({
            "peak_rss_mb": st.peak_rss / MB,
            "avg_cpu_pct": (avg * 10.0).round() / 10.0,
            "cpu_secs": (cpu_secs * 1000.0).round() / 1000.0,
            "wall_ms": self.started.elapsed().as_millis() as u64,
        })
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.unpark();
        }
    }
}

impl Drop for JobMeter {
    fn drop(&mut self) {
        self.halt();
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn sample_sanity_and_aggregate() {
        let dir = std::env::temp_dir().join(format!("sf-ops-mon-{}", sf_core::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("t.db")).unwrap();
        let mut cfg = Config::default();
        cfg.home = dir.clone();
        cfg.monitor.alert_disk_free_mb = 0;
        cfg.monitor.alert_cpu_pct = 0.0;
        cfg.monitor.alert_mem_pct = 0.0;
        let m = Monitor::new(Arc::new(cfg), db.clone(), None);
        let s = m.sample_now().await.unwrap();
        assert!(s.mem_total_mb > 0 && s.mem_used_mb <= s.mem_total_mb);
        assert!((0.0..=100.0).contains(&s.cpu_pct));
        assert!(s.proc_rss_mb > 0);
        assert!(s.disk_free_mb.is_some_and(|d| d > 0), "{s:?}");
        assert_eq!(m.recent().len(), 1);
        let cap = m.capacity();
        assert!(cap.logical_cpus >= 1 && cap.recommended_max_concurrent_jobs >= 1);

        // a minute rollover persists one aggregate row
        let mut a = s.clone();
        a.ts = "2026-01-01T00:00:10.000Z".into();
        let mut b = s.clone();
        b.ts = "2026-01-01T00:00:40.000Z".into();
        b.proc_rss_mb = a.proc_rss_mb + 100;
        let mut c = s.clone();
        c.ts = "2026-01-01T00:01:05.000Z".into();
        // reset the current minute so ordering is deterministic
        *m.inner.minute.lock() = (String::new(), Vec::new());
        m.push(a).await.unwrap();
        m.push(b.clone()).await.unwrap();
        m.push(c).await.unwrap();
        let h = m.history("2026-01-01T00:00:00.000Z", 16).await.unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0]["ts"], "2026-01-01T00:00:00.000Z");
        assert_eq!(h[0]["proc_rss_mb"], b.proc_rss_mb);
        assert_eq!(m.prune().await.unwrap(), 1); // 2026-01-01 is older than 14 days
        db.close();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn alerts_debounce() {
        let dir = std::env::temp_dir().join(format!("sf-ops-mon-{}", sf_core::new_id()));
        let db = Db::open(&dir.join("t.db")).unwrap();
        let mut cfg = Config::default();
        cfg.home = dir.clone();
        cfg.monitor.alert_cpu_pct = 50.0;
        let key = Arc::new([1u8; 32]);
        let n = Notifier::start(db.clone(), key, Default::default());
        let m = Monitor::new(Arc::new(cfg), db.clone(), Some(n));
        let s = Sample {
            ts: sf_core::now_rfc3339(),
            cpu_pct: 90.0,
            proc_cpu_pct: 1.0,
            mem_used_mb: 1,
            mem_total_mb: 100,
            proc_rss_mb: 1,
            disk_free_mb: Some(100_000),
            jobs_running: 0,
            jobs_queued: 0,
        };
        m.check_alerts(&s);
        m.check_alerts(&s); // debounced
        let mut ok = false;
        for _ in 0..100 {
            let rows = crate::notify::list_notifications(&db, false, 10)
                .await
                .unwrap();
            if !rows.is_empty() {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0]["event"], "system.alert");
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ok);
        db.close();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn job_meter_reports() {
        let m = JobMeter::start_every(Duration::from_millis(20));
        let mut x = 0u64;
        for i in 0..3_000_000u64 {
            x = x.wrapping_add(i * i);
        }
        std::thread::sleep(Duration::from_millis(80));
        let v = m.finish();
        assert!(x > 0);
        assert!(v["peak_rss_mb"].as_u64().unwrap() > 0);
        assert!(v["wall_ms"].as_u64().unwrap() >= 80);
        assert!(v["cpu_secs"].as_f64().unwrap() >= 0.0);
    }
}
