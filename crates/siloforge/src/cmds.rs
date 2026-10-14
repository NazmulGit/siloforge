//! Operator subcommands.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rusqlite::OptionalExtension;
use sf_store::err;

use crate::serve::{init_logging, load};

// ---------------------------------------------------------------------------------------------
// Jobs wiring (runner + scheduler). Kept here so serve.rs stays readable.

pub struct JobsHandle {
    runner: sf_jobs::RunnerHandle,
    scheduler: sf_jobs::SchedulerHandle,
}

impl JobsHandle {
    pub async fn shutdown(self, grace: std::time::Duration) {
        self.scheduler.stop().await;
        self.runner.shutdown(grace).await;
    }
}

pub async fn start_jobs(
    state: &sf_server::AppState,
    ai: sf_ai::Ai,
    embed: Arc<sf_embed::EmbedChain>,
    notifier: sf_ops::notify::Notifier,
    errors: sf_ops::errors::ErrorTracker,
    monitor: sf_ops::monitor::Monitor,
) -> Result<JobsHandle> {
    let mut ctx = sf_jobs::JobContext::new(
        state.cfg.clone(),
        state.db.clone(),
        state.jobs.clone(),
        embed,
        state.secret_key.clone(),
    );
    ctx.ai = Some(Arc::new(ai));
    ctx.notifier = Some(notifier);
    ctx.errors = errors;
    ctx.monitor = Some(monitor);
    let ctx = Arc::new(ctx);
    let runner = sf_jobs::Runner::start(ctx.clone());
    let scheduler = sf_jobs::Scheduler::start(ctx);
    tracing::info!("job runner and scheduler started");
    Ok(JobsHandle { runner, scheduler })
}

// ---------------------------------------------------------------------------------------------

pub async fn mcp() -> Result<()> {
    // stdout is the protocol channel: logs go to stderr only.
    let cfg = sf_core::Config::default();
    let _ = init_logging(&cfg, true);
    sf_mcp::run_stdio(sf_mcp::McpConfig::from_env()).await
}

/// Name of the generated backup passphrase in `secrets.env` (`backup.passphrase_env` default).
const BACKUP_PASSPHRASE_KEY: &str = "SILOFORGE_BACKUP_PASSPHRASE";

const EXAMPLE_TOML: &str = include_str!("../../../siloforge.example.toml");

pub fn init(home: &Path, from_v1_env: Option<&Path>, force: bool) -> Result<()> {
    std::fs::create_dir_all(home).with_context(|| format!("create {}", home.display()))?;
    for d in ["logs", "backups", "models", "runtime"] {
        std::fs::create_dir_all(home.join(d))?;
    }
    let toml_path = home.join("siloforge.toml");
    if !toml_path.exists() {
        std::fs::write(&toml_path, EXAMPLE_TOML)?;
        println!("wrote {}", toml_path.display());
    }
    let secrets_path = home.join("secrets.env");
    if secrets_path.exists() && !force {
        println!(
            "{} exists (use --force to re-import; existing master keys are kept)",
            secrets_path.display()
        );
        return Ok(());
    }
    // G2-11: `--force` keeps the existing master keys (JWT_SECRET, SILOFORGE_ENCRYPTION_KEY,
    // the backup passphrase) and every other existing line: regenerating them would log every
    // user out and make the sealed secrets in an existing DB undecryptable. Imported v1 values
    // only fill or override the non-master keys.
    let existing: Vec<(String, String)> = if secrets_path.exists() {
        sf_core::config::parse_env_file(&std::fs::read_to_string(&secrets_path)?)
            .into_iter()
            .filter(|(_, v)| !v.is_empty())
            .collect()
    } else {
        Vec::new()
    };
    let mut lines: Vec<(String, String)> = Vec::new();
    if let Some(p) = from_v1_env {
        let text = std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
        let keep = [
            "JWT_SECRET",
            "ADMIN_EMAIL",
            "ADMIN_PASSWORD",
            "OPENROUTER_URL",
            "OPENROUTER_API_KEY",
            "OPENROUTER_API_KEY_T2",
            "EMBED_REMOTE_MODEL",
            "EMBED_CHAIN",
            "DARIO_URL",
            "DARIO_MODEL_PRIMARY",
            "DARIO_MODEL_FALLBACK",
            "GSC_CREDENTIALS_JSON",
        ];
        for (k, v) in sf_core::config::parse_env_file(&text) {
            if keep.contains(&k.as_str()) && !v.is_empty() {
                lines.push((k, v));
            }
        }
        println!("imported {} settings from {}", lines.len(), p.display());
    }
    let kept = merge_existing_secrets(&mut lines, existing);
    if kept > 0 {
        println!(
            "kept {kept} existing value(s) from secrets.env (master keys are never regenerated)"
        );
    }
    if !lines.iter().any(|(k, _)| k == "JWT_SECRET") {
        lines.push(("JWT_SECRET".into(), sf_core::crypto::random_hex(32)));
    }
    // A dedicated key for stored secrets (v1 coupled it to JWT_SECRET; v2 decouples them).
    if !lines.iter().any(|(k, _)| k == "SILOFORGE_ENCRYPTION_KEY") {
        lines.push((
            "SILOFORGE_ENCRYPTION_KEY".into(),
            sf_core::crypto::random_hex(32),
        ));
    }
    // M-02: backups (incl. scheduled ones) are encrypted by default with a generated passphrase.
    if !lines.iter().any(|(k, _)| k == BACKUP_PASSPHRASE_KEY) {
        lines.push((
            BACKUP_PASSPHRASE_KEY.into(),
            sf_core::crypto::random_hex(24),
        ));
    }
    let mut out = String::from(
        "# SiloForge secrets. Keep this file private; it is included in encrypted backups only.\n\
         # Keep a copy of SILOFORGE_BACKUP_PASSPHRASE outside this machine: restores need it.\n",
    );
    for (k, v) in &lines {
        out.push_str(&format!("{k}={v}\n"));
    }
    std::fs::write(&secrets_path, out)?;
    println!(
        "wrote {} ({} keys; values not shown)",
        secrets_path.display(),
        lines.len()
    );
    println!(
        "backups are encrypted with {BACKUP_PASSPHRASE_KEY} from that file; keep a copy of it \
         (e.g. in a password manager), a restore needs it"
    );
    println!("next: `siloforge migrate from-postgres` (optional), then `siloforge serve`");
    Ok(())
}

/// Keys an existing `secrets.env` always keeps on `init --force` (never overridden by an import).
const MASTER_KEYS: [&str; 3] = [
    "JWT_SECRET",
    "SILOFORGE_ENCRYPTION_KEY",
    BACKUP_PASSPHRASE_KEY,
];

/// Merge an existing `secrets.env` into freshly imported `lines`: master keys from `existing`
/// win, other existing keys are kept unless the import set them. Returns how many were kept.
fn merge_existing_secrets(
    lines: &mut Vec<(String, String)>,
    existing: Vec<(String, String)>,
) -> usize {
    let mut kept = 0;
    for (k, v) in existing {
        let pos = lines.iter().position(|(a, _)| *a == k);
        match pos {
            Some(i) if MASTER_KEYS.contains(&k.as_str()) => {
                if let Some(slot) = lines.get_mut(i) {
                    slot.1 = v;
                    kept += 1;
                }
            }
            Some(_) => {}
            None => {
                lines.push((k, v));
                kept += 1;
            }
        }
    }
    kept
}

pub async fn backup_create(
    home: &Path,
    kind: &str,
    passphrase: Option<String>,
    note: String,
    include_secrets: bool,
) -> Result<()> {
    let (cfg, db, _) = load(home.to_path_buf()).await?;
    let kind = sf_ops::backup::BackupKind::parse(kind).context("kind must be full|db|config")?;
    let passphrase = passphrase.or_else(|| sf_ops::backup::configured_passphrase(&cfg));
    let info = sf_ops::backup::create(
        &cfg,
        &db,
        sf_ops::backup::BackupOptions {
            kind,
            passphrase,
            note,
            include_secrets,
        },
    )
    .await?;
    if !info.encrypted && kind != sf_ops::backup::BackupKind::Db && !include_secrets {
        println!(
            "warning: backup is NOT encrypted, so secrets.env was left out (set SILOFORGE_BACKUP_PASSPHRASE or pass --include-secrets)"
        );
    }
    println!(
        "backup {} -> {} ({} bytes, encrypted: {})",
        info.id,
        info.path.display(),
        info.size_bytes,
        info.encrypted
    );
    db.close();
    Ok(())
}

pub async fn backup_list(home: &Path) -> Result<()> {
    let (_, db, _) = load(home.to_path_buf()).await?;
    for b in sf_ops::backup::list(&db, 100).await? {
        println!(
            "{}  {}  {:>12}  {}  {}",
            b["created_at"].as_str().unwrap_or(""),
            b["kind"].as_str().unwrap_or(""),
            b["size_bytes"],
            b["id"].as_str().unwrap_or(""),
            b["path"].as_str().unwrap_or("")
        );
    }
    db.close();
    Ok(())
}

pub fn backup_verify(file: &Path, passphrase: Option<String>) -> Result<()> {
    let m = sf_ops::backup::verify(file, passphrase.as_deref())?;
    println!(
        "OK: v{} schema {} created {} on {} ({} files)",
        m.app_version,
        m.schema_version,
        m.created_at,
        m.hostname,
        m.files.len()
    );
    Ok(())
}

/// G2-9: one-off offline `VACUUM` that also switches an existing database to
/// `auto_vacuum=INCREMENTAL` (new databases get it at creation), so the daily maintenance
/// `PRAGMA incremental_vacuum` actually returns free pages to the OS. Refuses while a server
/// (or any other SiloForge process) holds the data directory. Needs about the DB size in free disk.
pub fn vacuum(home: &Path) -> Result<()> {
    let _guard = sf_ops::instance::InstanceGuard::acquire(home)
        .map_err(|e| anyhow::anyhow!("{e}; stop the server first"))?;
    let path = home.join("siloforge.db");
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    let before = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let (mode_before, mode_after) = vacuum_file(&path)?;
    let after = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!(
        "vacuumed {}: {} MB -> {} MB, auto_vacuum {} -> {}",
        path.display(),
        before / 1_048_576,
        after / 1_048_576,
        mode_before,
        mode_after
    );
    Ok(())
}

fn vacuum_file(path: &Path) -> Result<(i64, i64)> {
    let c = rusqlite::Connection::open(path)?;
    c.busy_timeout(std::time::Duration::from_secs(10))?;
    let before: i64 = c.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    c.execute_batch("PRAGMA auto_vacuum=INCREMENTAL; VACUUM;")?;
    let after: i64 = c.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    let _ = c.pragma_update(None, "wal_checkpoint", "TRUNCATE");
    Ok((before, after))
}

pub fn restore(home: &Path, file: &Path, passphrase: Option<String>, force: bool) -> Result<()> {
    let r = sf_ops::backup::restore(file, home, passphrase.as_deref(), force)?;
    println!(
        "restored {} file(s) from backup v{} ({}) into {}",
        r.restored.len(),
        r.manifest.app_version,
        r.manifest.created_at,
        home.display()
    );
    for p in r.previous {
        println!("  previous copy kept: {}", p.display());
    }
    for w in r.warnings {
        println!("warning: {w}");
    }
    Ok(())
}

fn migrate_opts(
    key: [u8; 32],
    site: Option<String>,
    tables: Option<String>,
    dry_run: bool,
    refresh: bool,
) -> sf_ops::migrate_v1::MigrateOptions {
    let mut o = sf_ops::migrate_v1::MigrateOptions::new(key);
    o.site = site;
    o.tables = tables.map(|t| {
        t.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });
    o.dry_run = dry_run;
    o.refresh = refresh;
    o
}

fn print_progress(p: &sf_ops::migrate_v1::Progress) {
    if p.done {
        eprintln!(
            "  {:<22} {:>10} rows, {:>10} inserted",
            p.table, p.rows, p.inserted
        );
    }
}

pub async fn migrate_pg(
    home: &Path,
    site: Option<String>,
    tables: Option<String>,
    dry_run: bool,
    refresh: bool,
) -> Result<()> {
    let (_, db, key) = load(home.to_path_buf()).await?;
    let opts = migrate_opts(key, site, tables, dry_run, refresh);
    let db2 = db.clone();
    let report = tokio::task::spawn_blocking(move || {
        sf_ops::migrate_v1::import_from_docker(&db2, &opts, &print_progress)
    })
    .await??;
    println!("{report}");
    db.close();
    Ok(())
}

pub fn migrate_export(dir: &Path, site: Option<String>) -> Result<()> {
    let src = sf_ops::migrate_v1::DockerSource::default();
    for (t, bytes) in sf_ops::migrate_v1::export_v1_to_dir(&src, dir, site.as_deref())? {
        println!("{t:<22} {bytes:>12} bytes");
    }
    Ok(())
}

pub async fn migrate_import(
    home: &Path,
    dir: &Path,
    site: Option<String>,
    refresh: bool,
) -> Result<()> {
    let (_, db, key) = load(home.to_path_buf()).await?;
    let opts = migrate_opts(key, site, None, false, refresh);
    let (db2, dir2) = (db.clone(), dir.to_path_buf());
    let report = tokio::task::spawn_blocking(move || {
        sf_ops::migrate_v1::import_from_dir(&dir2, &db2, &opts, &print_progress)
    })
    .await??;
    println!("{report}");
    db.close();
    Ok(())
}

pub async fn migrate_verify(home: &Path, site: Option<String>, sample: usize) -> Result<()> {
    let (_, db, key) = load(home.to_path_buf()).await?;
    let opts = migrate_opts(key, site, None, false, false);
    let db2 = db.clone();
    let report = tokio::task::spawn_blocking(move || {
        let src = sf_ops::migrate_v1::DockerSource::default();
        sf_ops::migrate_v1::verify(&src, &db2, &opts, sample)
    })
    .await??;
    println!("{report}");
    db.close();
    if !report.ok {
        bail!("verification found differences");
    }
    Ok(())
}

pub async fn token_create(
    home: &Path,
    name: String,
    user: Option<String>,
    scopes: String,
    expires_days: Option<i64>,
) -> Result<()> {
    let (_, db, _) = load(home.to_path_buf()).await?;
    let scopes: Vec<String> = scopes
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    for s in &scopes {
        if !["*", "read", "write", "admin"].contains(&s.as_str()) {
            bail!("invalid scope '{s}' (use * | read | write | admin)");
        }
    }
    let (token, hash, prefix) = sf_core::crypto::new_api_token();
    let expires_days = expires_days.map(|d| d.clamp(1, 3650));
    let token_name = name.clone();
    let id = db
        .write(move |t| {
            let uid: Option<String> = match &user {
                Some(email) => t
                    .query_row("SELECT id FROM users WHERE email=?1", [email.to_lowercase()], |r| r.get(0))
                    .optional()
                    .map_err(err)?,
                None => t
                    .query_row(
                        "SELECT id FROM users WHERE role='admin' ORDER BY created_at LIMIT 1",
                        [],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(err)?,
            };
            let uid = uid.ok_or_else(|| sf_core::Error::not_found("user not found"))?;
            let id = sf_core::new_id();
            let expires_mod = expires_days.map(|d| format!("+{d} days"));
            t.execute(
                "INSERT INTO api_tokens(id,user_id,name,token_hash,prefix,scopes,expires_at)
                 VALUES(?1,?2,?3,?4,?5,?6, CASE WHEN ?7 IS NULL THEN NULL ELSE strftime('%Y-%m-%dT%H:%M:%fZ','now',?7) END)",
                rusqlite::params![id, uid, token_name, hash, prefix, serde_json::to_string(&scopes).unwrap_or_else(|_| "[]".into()), expires_mod],
            )
            .map_err(err)?;
            Ok(id)
        })
        .await?;
    println!("token id {id} ({name})");
    println!("{token}");
    eprintln!("Store it now; it is not shown again.");
    db.close();
    Ok(())
}

pub async fn token_list(home: &Path) -> Result<()> {
    let (_, db, _) = load(home.to_path_buf()).await?;
    let rows = db
        .read(|c| {
            let mut s = c
                .prepare(
                    "SELECT t.id, t.name, t.prefix, t.scopes, u.email, t.created_at, t.last_used_at, t.expires_at, t.revoked_at
                     FROM api_tokens t JOIN users u ON u.id=t.user_id ORDER BY t.created_at",
                )
                .map_err(err)?;
            let rows = s
                .query_map([], |r| {
                    Ok(format!(
                        "{}  {:<16} {}…  {:<18} {:<24} used:{} exp:{} {}",
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, Option<String>>(6)?.unwrap_or_else(|| "never".into()),
                        r.get::<_, Option<String>>(7)?.unwrap_or_else(|| "never".into()),
                        if r.get::<_, Option<String>>(8)?.is_some() { "REVOKED" } else { "" }
                    ))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?;
    for r in rows {
        println!("{r}");
    }
    db.close();
    Ok(())
}

pub async fn token_revoke(home: &Path, id_or_prefix: String) -> Result<()> {
    let (_, db, _) = load(home.to_path_buf()).await?;
    let n = db
        .write(move |t| {
            t.execute(
                "UPDATE api_tokens SET revoked_at=?2 WHERE (id=?1 OR prefix=?1) AND revoked_at IS NULL",
                rusqlite::params![id_or_prefix, sf_core::now_rfc3339()],
            )
            .map_err(err)
        })
        .await?;
    println!("revoked {n} token(s)");
    db.close();
    Ok(())
}

pub async fn user_set_password(
    home: &Path,
    email: String,
    password: String,
    create: bool,
) -> Result<()> {
    if password.len() < 8 {
        bail!("password must be at least 8 characters");
    }
    let (_, db, _) = load(home.to_path_buf()).await?;
    let hash =
        tokio::task::spawn_blocking(move || sf_core::crypto::hash_password(&password)).await??;
    let email = email.trim().to_lowercase();
    let msg = db
        .write(move |t| {
            let n = t
                .execute(
                    "UPDATE users SET password_hash=?2, token_version=token_version+1 WHERE email=?1",
                    rusqlite::params![email, hash],
                )
                .map_err(err)?;
            if n > 0 {
                return Ok("password updated; existing sessions revoked".to_string());
            }
            if !create {
                return Err(sf_core::Error::not_found("no such user (use --create to add an admin)"));
            }
            t.execute(
                "INSERT INTO users(id,email,password_hash,name,role) VALUES(?1,?2,?3,'Admin','admin')",
                rusqlite::params![sf_core::new_id(), email, hash],
            )
            .map_err(err)?;
            Ok("admin user created".to_string())
        })
        .await?;
    println!("{msg}");
    db.close();
    Ok(())
}

pub async fn user_list(home: &Path) -> Result<()> {
    let (_, db, _) = load(home.to_path_buf()).await?;
    let rows = db
        .read(|c| {
            let mut s = c
                .prepare("SELECT email, role, created_at FROM users ORDER BY created_at")
                .map_err(err)?;
            let rows = s
                .query_map([], |r| {
                    Ok(format!(
                        "{:<32} {:<6} {}",
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?
                    ))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?;
    for r in rows {
        println!("{r}");
    }
    db.close();
    Ok(())
}

pub async fn doctor(home: &Path) -> Result<()> {
    let mut problems = 0;
    let (cfg, db, _) = load(home.to_path_buf()).await?;
    println!(
        "siloforge {}  home {}",
        sf_core::VERSION,
        cfg.home.display()
    );
    match cfg.validate_for_serve() {
        Ok(()) => println!("[ok]   config"),
        Err(e) => {
            problems += 1;
            println!("[FAIL] config: {e}");
        }
    }
    let integrity: String = db
        .read(|c| {
            c.query_row("PRAGMA quick_check", [], |r| r.get(0))
                .map_err(err)
        })
        .await?;
    let schema = db.read(sf_store::migrate::current_version).await?;
    if integrity == "ok" {
        println!(
            "[ok]   database {} (schema v{schema})",
            cfg.db_path().display()
        );
    } else {
        problems += 1;
        println!("[FAIL] database integrity: {integrity}");
    }
    let counts: (i64, i64, i64) = db
        .read(|c| {
            c.query_row(
                "SELECT (SELECT count(*) FROM sites),(SELECT count(*) FROM posts),(SELECT count(*) FROM paragraphs)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(err)
        })
        .await?;
    println!(
        " sites {} · posts {} · paragraphs {}",
        counts.0, counts.1, counts.2
    );
    let chain = sf_embed::EmbedChain::from_config(&cfg.embed, &cfg.home, cfg.cpu_threads());
    for r in chain.status() {
        println!(
            " embed rung {:<8} {:<8} available:{} {}",
            r.id,
            r.device,
            r.available,
            r.last_error.unwrap_or_default()
        );
    }
    match chain.self_test().await {
        Ok(t) if t.ok => println!(
            "[ok]   embeddings via {} ({}) cosine {:.6} in {} ms",
            t.rung, t.device, t.cosine, t.ms
        ),
        Ok(t) => {
            problems += 1;
            println!(
                "[FAIL] embeddings via {} cosine {:.6} (expected ≥ 0.9999)",
                t.rung, t.cosine
            );
        }
        Err(e) => {
            problems += 1;
            println!(
                "[FAIL] embeddings: {e} (try `siloforge fetch-runtime` and `siloforge fetch-model`)"
            );
        }
    }
    for bind in &cfg.server.binds {
        match std::net::TcpListener::bind(bind) {
            Ok(_) => println!("[ok]   port {bind} free"),
            Err(_) => {
                println!("[info] port {bind} in use (a server is running, or v1 still holds it)")
            }
        }
    }
    if let Some(mb) = sf_ops::monitor::disk_free_mb(&cfg.home) {
        if mb < cfg.monitor.alert_disk_free_mb {
            problems += 1;
            println!("[FAIL] disk free {mb} MB");
        } else {
            println!("[ok]   disk free {mb} MB");
        }
    }
    db.close();
    if problems > 0 {
        bail!("{problems} problem(s) found");
    }
    Ok(())
}

pub async fn fetch_runtime(home: &Path, gpu: bool) -> Result<()> {
    let dest = home.join("runtime");
    for f in sf_embed::runtime::fetch_runtime(&dest, gpu).await? {
        println!("{f:?}");
    }
    Ok(())
}

pub async fn fetch_model(home: &Path) -> Result<()> {
    let dest = home.join("models").join("all-MiniLM-L6-v2");
    for f in sf_embed::runtime::fetch_model(&dest).await? {
        println!("{f:?}");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// M-02: `init` generates a backup passphrase so scheduled backups are encrypted.
    #[test]
    fn init_generates_backup_passphrase() {
        let home = std::env::temp_dir().join(format!("sf-init-{}", sf_core::new_id()));
        init(&home, None, false).unwrap();
        let text = std::fs::read_to_string(home.join("secrets.env")).unwrap();
        let env = sf_core::config::parse_env_file(&text);
        let pass = env
            .iter()
            .find(|(k, _)| k == BACKUP_PASSPHRASE_KEY)
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(pass.len(), 48);
        let cfg = sf_core::Config {
            home: home.clone(),
            ..Default::default()
        };
        assert_eq!(
            sf_ops::backup::configured_passphrase(&cfg).as_deref(),
            std::env::var(BACKUP_PASSPHRASE_KEY)
                .ok()
                .filter(|v| !v.is_empty())
                .as_deref()
                .or(Some(pass.as_str()))
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// G2-9: `vacuum` converts an existing auto_vacuum=NONE database and refuses while locked.
    #[test]
    fn vacuum_enables_incremental_auto_vacuum_and_respects_the_lock() {
        let home = std::env::temp_dir().join(format!("sf-vacuum-{}", sf_core::new_id()));
        std::fs::create_dir_all(&home).unwrap();
        let path = home.join("siloforge.db");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch("PRAGMA auto_vacuum=NONE; CREATE TABLE t(x); INSERT INTO t VALUES(1);")
                .unwrap();
        }
        {
            let _held = sf_ops::instance::InstanceGuard::acquire(&home).unwrap();
            assert!(vacuum(&home).is_err());
        }
        vacuum(&home).unwrap();
        let c = rusqlite::Connection::open(&path).unwrap();
        let mode: i64 = c.query_row("PRAGMA auto_vacuum", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, 2);
        let n: i64 = c
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        drop(c);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// G2-11: `init --force` never regenerates the master keys of an existing home.
    #[test]
    fn init_force_keeps_master_keys_and_other_lines() {
        let home = std::env::temp_dir().join(format!("sf-init-force-{}", sf_core::new_id()));
        init(&home, None, false).unwrap();
        let path = home.join("secrets.env");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(
            "DARIO_MODEL_PRIMARY=keep-me
EMBED_CHAIN=onnx
",
        );
        std::fs::write(&path, &text).unwrap();
        let before = sf_core::config::parse_env_file(&text);
        let v1 = home.join("v1.env");
        std::fs::write(
            &v1,
            "JWT_SECRET=v1-secret-should-not-win-000
EMBED_CHAIN=remote
",
        )
        .unwrap();
        init(&home, Some(&v1), true).unwrap();
        let after = sf_core::config::parse_env_file(&std::fs::read_to_string(&path).unwrap());
        let get = |env: &[(String, String)], k: &str| {
            env.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone())
        };
        for k in MASTER_KEYS {
            assert_eq!(get(&after, k), get(&before, k), "{k}");
            assert_eq!(after.iter().filter(|(a, _)| a == k).count(), 1, "{k}");
        }
        assert_eq!(
            get(&after, "DARIO_MODEL_PRIMARY").as_deref(),
            Some("keep-me")
        );
        assert_eq!(get(&after, "EMBED_CHAIN").as_deref(), Some("remote"));
        let _ = std::fs::remove_dir_all(&home);
    }
}
