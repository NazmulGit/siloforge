//! System info, live stats and metric history.

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};
use sf_ops::monitor::{Monitor, Sample};
use sf_store::err;

use crate::auth::Admin;
use crate::{ApiError, ApiResult, AppState};

const MB: u64 = 1024 * 1024;

async fn failed_24h(st: &AppState) -> ApiResult<i64> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::hours(24))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    Ok(st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT count(*) FROM job_runs WHERE status='failed' AND coalesce(finished_at, created_at) >= ?1",
                [&cutoff],
                |r| r.get(0),
            )
            .map_err(err)
        })
        .await?)
}

/// Latest sample from the monitor (taking one now if the ring is empty); None without a monitor.
async fn current(st: &AppState) -> Option<Sample> {
    let m = st.services.get::<Monitor>()?;
    match m.latest() {
        Some(s) => Some(s),
        None => m.sample_now().await.ok(),
    }
}

/// `GET /system`: `{version, uptime_secs, capacity, current, jobs:{running, queued, failed_24h},
/// disk_free_mb, db_size_bytes}` plus build/db/config details (config without secrets).
pub async fn info(State(st): State<AppState>, _a: Admin) -> ApiResult<Json<Value>> {
    let schema = st.db.read(sf_store::migrate::current_version).await?;
    let db_bytes = tokio::fs::metadata(st.db.path())
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let monitor = st.services.get::<Monitor>();
    let capacity = {
        let (m, cfg) = (monitor.clone(), st.cfg.clone());
        tokio::task::spawn_blocking(move || match m {
            Some(m) => m.capacity(),
            None => sf_ops::monitor::compute_capacity(&cfg, None),
        })
        .await
        .ok()
        .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
    };
    let cur = current(&st).await;
    let (running, queued) = st.jobs.counts().await.unwrap_or((0, 0));
    let failed = failed_24h(&st).await?;
    let disk_free_mb = match cur.as_ref().and_then(|s| s.disk_free_mb) {
        Some(d) => Some(d),
        None => {
            let home = st.cfg.home.clone();
            tokio::task::spawn_blocking(move || sf_ops::monitor::disk_free_mb(&home))
                .await
                .ok()
                .flatten()
        }
    };
    // Config serialisation skips secret fields (`#[serde(skip_serializing)]` in sf-core).
    let cfg = serde_json::to_value(&*st.cfg).unwrap_or(Value::Null);
    let uptime = st.started.elapsed().as_secs();
    let started_at =
        chrono::Utc::now() - chrono::Duration::seconds(uptime.min(i64::MAX as u64) as i64);
    Ok(Json(json!({
        "version": sf_core::VERSION,
        "uptime_secs": uptime,
        "started_at": started_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "capacity": capacity,
        "current": cur,
        "jobs": { "running": running, "queued": queued, "failed_24h": failed },
        "disk_free_mb": disk_free_mb,
        "db_size_bytes": db_bytes,
        "build": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "debug": cfg!(debug_assertions),
            "pid": std::process::id(),
        },
        "home": st.cfg.home,
        "db": { "path": st.db.path(), "size_bytes": db_bytes, "schema_version": schema,
                "latest_schema_version": sf_store::migrate::latest_version() },
        "config": cfg,
        "services": {
            "monitor": monitor.is_some(),
            "notifier": st.services.get::<sf_ops::notify::Notifier>().is_some(),
            "background": sf_ops::supervise::services(),
        },
    })))
}

/// `GET /system/stats`: compact live gauges (05-ui-audit §5 shape).
pub async fn stats(State(st): State<AppState>, _a: Admin) -> ApiResult<Json<Value>> {
    let monitor = st.service::<Monitor>()?;
    let s = match monitor.latest() {
        Some(s) => s,
        None => monitor.sample_now().await?,
    };
    let failed = failed_24h(&st).await?;
    Ok(Json(json!({
        "ts": s.ts,
        "cpu_pct": s.cpu_pct,
        "proc_cpu_pct": s.proc_cpu_pct,
        "mem_used_bytes": s.mem_used_mb.saturating_mul(MB),
        "mem_total_bytes": s.mem_total_mb.saturating_mul(MB),
        "proc_rss_bytes": s.proc_rss_mb.saturating_mul(MB),
        "disk_free_bytes": s.disk_free_mb.map(|d| d.saturating_mul(MB)),
        "uptime_s": st.started.elapsed().as_secs(),
        "version": sf_core::VERSION,
        "queue": { "queued": s.jobs_queued, "running": s.jobs_running, "failed_24h": failed },
    })))
}

#[derive(Deserialize)]
pub struct MetricsQ {
    window: Option<String>,
}

/// `GET /system/metrics?window=1h|24h|7d` -> `{samples:[...], window, resolution}`. 1h = raw in-memory
/// samples (fallback: minute rows); 24h = 1-minute rows; 7d = 10-minute averages.
pub async fn metrics(
    State(st): State<AppState>,
    _a: Admin,
    Query(q): Query<MetricsQ>,
) -> ApiResult<Json<Value>> {
    let window = q.window.as_deref().unwrap_or("1h");
    let (hours, bucket) = match window {
        "1h" => (1, 16),
        "24h" => (24, 16),
        "7d" => (24 * 7, 15),
        _ => return Err(ApiError::bad("window must be 1h, 24h or 7d")),
    };
    if window == "1h"
        && let Some(m) = st.services.get::<Monitor>()
    {
        let raw = m.recent();
        if !raw.is_empty() {
            return Ok(Json(json!({
                "window": window, "resolution": "raw",
                "sample_secs": st.cfg.monitor.sample_secs, "samples": raw,
            })));
        }
    }
    let since = (chrono::Utc::now() - chrono::Duration::hours(hours))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let samples = sf_ops::monitor::metrics_history(&st.db, &since, bucket).await?;
    Ok(Json(json!({
        "window": window,
        "resolution": if bucket == 16 { "1m" } else { "10m" },
        "samples": samples,
    })))
}
