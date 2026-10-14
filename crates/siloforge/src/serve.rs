//! `siloforge serve`: wires every service and runs until Ctrl-C / SIGTERM.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use sf_core::Config;
use sf_store::Db;

pub fn init_logging(
    cfg: &Config,
    to_stderr_only: bool,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;
    let filter =
        tracing_subscriber::EnvFilter::try_from_env("SILOFORGE_LOG").unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new("info,hyper=warn,reqwest=warn,ort=warn")
        });
    let stderr = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false);
    if to_stderr_only {
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(stderr)
            .try_init();
        return None;
    }
    let _ = std::fs::create_dir_all(cfg.logs_dir());
    let appender = tracing_appender::rolling::daily(cfg.logs_dir(), "siloforge.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let file = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .json();
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stderr)
        .with(file)
        .try_init();
    Some(guard)
}

pub async fn load(home: PathBuf) -> Result<(Config, Db, [u8; 32])> {
    std::fs::create_dir_all(&home).with_context(|| format!("create {}", home.display()))?;
    let cfg = Config::load(&home)?;
    let master = cfg.encryption_master().to_string();
    let key =
        tokio::task::spawn_blocking(move || sf_core::crypto::derive_secret_key(&master)).await??;
    let db = Db::open(&cfg.db_path())?;
    Ok((cfg, db, key))
}

/// Seeds the first admin from ADMIN_EMAIL / ADMIN_PASSWORD when the users table is empty (v1 parity).
async fn seed_admin(cfg: &Config, db: &Db) -> Result<()> {
    if cfg.auth.admin_password.is_empty() {
        return Ok(());
    }
    let email = cfg.auth.admin_email.trim().to_lowercase();
    let pw = cfg.auth.admin_password.clone();
    let hash = tokio::task::spawn_blocking(move || sf_core::crypto::hash_password(&pw)).await??;
    let created = db
        .write(move |t| {
            let n: i64 = t
                .query_row("SELECT count(*) FROM users", [], |r| r.get(0))
                .map_err(sf_store::err)?;
            if n > 0 {
                return Ok(false);
            }
            let id = sf_core::new_id();
            t.execute(
                "INSERT INTO users(id,email,password_hash,name,role) VALUES(?1,?2,?3,'Admin','admin')",
                rusqlite::params![id, email, hash],
            )
            .map_err(sf_store::err)?;
            t.execute("UPDATE sites SET user_id=?1 WHERE user_id IS NULL", [&id])
                .map_err(sf_store::err)?;
            Ok(true)
        })
        .await?;
    if created {
        tracing::info!("seeded first admin user from ADMIN_EMAIL");
    }
    Ok(())
}

pub async fn run(home: PathBuf) -> Result<()> {
    // R2: the exclusive instance lock is the very first thing (before the DB is even opened), so
    // a second `serve` on the same home fails fast without touching jobs or files.
    let _instance = sf_ops::instance::InstanceGuard::acquire(&home)
        .with_context(|| format!("cannot start on {}", home.display()))?;
    let (cfg, db, key) = load(home.clone()).await?;
    let _log_guard = init_logging(&cfg, false);
    cfg.validate_for_serve()?;
    if cfg.auth.encryption_key.is_empty() {
        // L-16: stored secrets fall back to a key derived from JWT_SECRET.
        tracing::warn!(
            "SILOFORGE_ENCRYPTION_KEY is not set: stored secrets are encrypted with a key derived from JWT_SECRET (rotating JWT_SECRET would make them unreadable); run `siloforge init`"
        );
    }
    tracing::info!(version = sf_core::VERSION, home = %cfg.home.display(), "starting SiloForge");

    // R2: bind every listener before any background service, runner or reaper starts, so a port
    // clash ends the process before it can touch the job queue.
    let mut listeners = Vec::new();
    for bind in &cfg.server.binds {
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .with_context(|| format!("bind {bind} (is v1 or another instance still running?)"))?;
        listeners.push((bind.clone(), listener));
    }

    if cfg.resources.low_priority_jobs && sf_jobs::lower_compute_priority(&cfg) {
        tracing::info!("compute threads run at below-normal priority");
    }
    let threads = sf_engine::pool::init(cfg.cpu_threads()).unwrap_or(1);
    tracing::info!(threads, "compute pool ready");
    seed_admin(&cfg, &db).await?;

    let state = sf_server::AppState::new(cfg.clone(), db.clone(), key);
    let cfg_arc = state.cfg.clone();

    // Services (each background loop is supervised: sf_ops::supervise, R8).
    let notifier =
        sf_ops::notify::Notifier::start(db.clone(), state.secret_key.clone(), Default::default());
    let monitor =
        sf_ops::monitor::Monitor::start(cfg_arc.clone(), db.clone(), Some(notifier.clone()));
    let ai = sf_ai::Ai::new(db.clone(), state.secret_key.clone(), cfg.llm.clone())
        .map_err(|e| anyhow::anyhow!("ai init: {e}"))?;
    let embed = Arc::new(sf_embed::EmbedChain::from_config_budget(
        &cfg.embed,
        &cfg.home,
        threads,
        cfg.resources.low_priority_jobs,
    ));
    let errors = sf_ops::errors::ErrorTracker::new(db.clone());
    state.services.insert(Arc::new(notifier.clone()));
    state.services.insert(Arc::new(monitor.clone()));
    state.services.insert(Arc::new(ai.clone()));
    state.services.insert(embed.clone());
    state.services.insert(Arc::new(errors.clone()));
    state
        .services
        .insert(Arc::new(sf_server::state::McpTools(sf_mcp::tools::catalogue().into_iter().map(|t| serde_json::json!({"name": t.name, "title": t.title, "description": t.description, "read_only": t.read_only, "destructive": t.destructive})).collect())));
    let _backup_task =
        sf_ops::backup::spawn_scheduler(cfg_arc.clone(), db.clone(), Some(notifier.clone()));
    // Retention + ANALYZE run as the daily `maintenance` job queued by the scheduler
    // (monitor.maintenance_cron, G2-3).

    // Jobs.
    let jobs = crate::cmds::start_jobs(&state, ai, embed, notifier, errors, monitor).await?;

    // HTTP: MCP-over-HTTP calls back into our own API on the first bind.
    let first_bind = cfg
        .server
        .binds
        .first()
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:8600".into());
    let mut mcp_cfg = sf_mcp::McpConfig::from_env();
    mcp_cfg.api_url = format!("http://{first_bind}");
    let app = sf_server::router(state.clone(), Some(sf_mcp::http_router(mcp_cfg)));

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut servers = Vec::new();
    for (bind, listener) in listeners {
        tracing::info!(%bind, "listening");
        let app = app.clone();
        let mut rx = stop_rx.clone();
        servers.push(tokio::spawn(async move {
            // ConnectInfo lets the auth rate limiter key on the client IP (M-03).
            let make = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
            let res = axum::serve(listener, make)
                .with_graceful_shutdown(async move {
                    let _ = rx.wait_for(|v| *v).await;
                })
                .await;
            if let Err(e) = res {
                tracing::error!(error = %e, "server error");
            }
        }));
    }

    shutdown_signal().await;
    tracing::info!("shutting down");
    let _ = stop_tx.send(true);
    jobs.shutdown(std::time::Duration::from_secs(20)).await;
    // R17: long MCP-over-HTTP calls (wait_for_job) must not hold the exit forever.
    for s in servers {
        if tokio::time::timeout(std::time::Duration::from_secs(10), s)
            .await
            .is_err()
        {
            tracing::warn!("HTTP drain timed out; closing open connections");
        }
    }
    db.close();
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    // R17: console close, logoff and system shutdown also stop gracefully on Windows (WAL
    // checkpoint, running jobs left for the boot reaper instead of a hard kill).
    #[cfg(windows)]
    let term = async {
        use tokio::signal::windows;
        let (close, logoff, shutdown) = (
            windows::ctrl_close(),
            windows::ctrl_logoff(),
            windows::ctrl_shutdown(),
        );
        tokio::select! {
            _ = async { match close { Ok(mut s) => { s.recv().await; } Err(_) => std::future::pending::<()>().await } } => {}
            _ = async { match logoff { Ok(mut s) => { s.recv().await; } Err(_) => std::future::pending::<()>().await } } => {}
            _ = async { match shutdown { Ok(mut s) => { s.recv().await; } Err(_) => std::future::pending::<()>().await } } => {}
        }
    };
    #[cfg(not(any(unix, windows)))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R2: a second `serve` on a home that is in use fails before it opens the database, binds a
    /// port or starts the runner/reaper.
    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::unwrap_used)]
    async fn second_instance_fails_before_touching_anything() {
        let home = std::env::temp_dir().join(format!("sf-serve-dup-{}", sf_core::new_id()));
        let _first = sf_ops::instance::InstanceGuard::acquire(&home).unwrap();
        let r = run(home.clone()).await;
        let msg = format!("{:#}", r.err().unwrap());
        assert!(msg.contains("another SiloForge process"), "{msg}");
        assert!(!home.join("siloforge.db").exists(), "db must not be opened");
        // and the first instance's pid file is untouched
        assert_eq!(
            sf_ops::instance::running_instance(&home),
            Some(std::process::id())
        );
        drop(_first);
        let _ = std::fs::remove_dir_all(home);
    }
}
