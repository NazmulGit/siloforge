//! `siloforge` — the single SiloForge binary.
//!
//! `serve` runs everything (API + UI + MCP-over-HTTP + jobs + scheduler + monitor + backups).
//! The other subcommands are operator tools: setup, MCP stdio for agents, backup/restore,
//! v1 migration, API tokens, diagnostics.

mod cmds;
mod serve;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "siloforge",
    version,
    about = "Self-hosted internal linking for WordPress"
)]
struct Cli {
    /// Data directory (db, config, logs, models, backups). Default: $SILOFORGE_HOME, else <exe dir>/data.
    #[arg(long, global = true, env = "SILOFORGE_HOME")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server (API, UI, MCP over HTTP, job runner, scheduler, monitor).
    Serve,
    /// MCP server over stdio for AI agents (Hermes). Talks to a running server via SILOFORGE_API_URL + SILOFORGE_TOKEN.
    Mcp,
    /// Create the data directory with a config file and freshly generated secrets.
    Init {
        /// Import secrets/settings from a v1 .env (JWT_SECRET, ADMIN_*, OPENROUTER_*, DARIO_*), keeping v1 tokens valid.
        #[arg(long)]
        from_v1_env: Option<PathBuf>,
        /// Overwrite an existing secrets.env.
        #[arg(long)]
        force: bool,
    },
    /// Backups of database + config + secrets.
    Backup {
        #[command(subcommand)]
        cmd: BackupCmd,
    },
    /// Restore a backup archive into the data directory (server must be stopped unless --force).
    /// Encrypted archives need the SILOFORGE_BACKUP_PASSPHRASE of the source install (keep a copy
    /// outside the backup). Unencrypted archives do not contain secrets.env: copy it in by hand.
    Restore {
        file: PathBuf,
        /// Passphrase for encrypted archives (or SILOFORGE_BACKUP_PASSPHRASE).
        #[arg(long, env = "SILOFORGE_BACKUP_PASSPHRASE")]
        passphrase: Option<String>,
        #[arg(long)]
        force: bool,
    },
    /// Import data from SiloForge v1 (Postgres in Docker, read-only) or a CSV export directory.
    Migrate {
        #[command(subcommand)]
        cmd: MigrateCmd,
    },
    /// Manage API tokens (for Hermes / scripts / MCP).
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
    /// User administration.
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// Check configuration, database, embeddings runtime, ports and disk.
    Doctor,
    /// Download ONNX Runtime (DirectML build on Windows) into <home>/runtime.
    FetchRuntime {
        /// CPU-only runtime (smaller, no GPU).
        #[arg(long)]
        cpu: bool,
    },
    /// Download the all-MiniLM-L6-v2 ONNX model + tokenizer into <home>/models.
    FetchModel,
    /// Print version information.
    Version,
}

#[derive(Subcommand)]
enum BackupCmd {
    /// Create a backup now.
    Create {
        /// full | db | config
        #[arg(long, default_value = "full")]
        kind: String,
        #[arg(long, env = "SILOFORGE_BACKUP_PASSPHRASE")]
        passphrase: Option<String>,
        #[arg(long, default_value = "")]
        note: String,
        /// Put secrets.env (master keys) into the archive even when it is not encrypted.
        #[arg(long)]
        include_secrets: bool,
    },
    /// List backups.
    List,
    /// Verify an archive's manifest and hashes.
    Verify {
        file: PathBuf,
        #[arg(long, env = "SILOFORGE_BACKUP_PASSPHRASE")]
        passphrase: Option<String>,
    },
}

#[derive(Subcommand)]
enum MigrateCmd {
    /// Import from the running v1 container (docker exec siloforge_pg psql ... COPY TO STDOUT).
    FromPostgres {
        /// Only this site (uuid).
        #[arg(long)]
        site: Option<String>,
        /// Comma-separated v1 table names.
        #[arg(long)]
        tables: Option<String>,
        #[arg(long)]
        dry_run: bool,
        /// Overwrite rows that changed in v1 since an earlier import (UPSERT by id; never deletes).
        #[arg(long)]
        refresh: bool,
    },
    /// Export v1 tables to CSV files (for migrating from another PC).
    Export {
        dir: PathBuf,
        #[arg(long)]
        site: Option<String>,
    },
    /// Import from a CSV export directory.
    Import {
        dir: PathBuf,
        #[arg(long)]
        site: Option<String>,
        /// Overwrite rows that changed since an earlier import (UPSERT by id; never deletes).
        #[arg(long)]
        refresh: bool,
    },
    /// Compare row counts and sample embeddings between v1 and this database.
    Verify {
        #[arg(long)]
        site: Option<String>,
        #[arg(long, default_value_t = 100)]
        sample: usize,
    },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Create a token (printed once).
    Create {
        #[arg(long)]
        name: String,
        /// Owner email (default: the first admin).
        #[arg(long)]
        user: Option<String>,
        /// Comma-separated: * | read | write | admin
        #[arg(long, default_value = "read,write")]
        scopes: String,
        #[arg(long)]
        expires_days: Option<i64>,
    },
    List,
    Revoke {
        id_or_prefix: String,
    },
}

#[derive(Subcommand)]
enum UserCmd {
    /// Set (or reset) a user's password; creates an admin if the email doesn't exist and --create is given.
    SetPassword {
        email: String,
        #[arg(long, env = "SILOFORGE_NEW_PASSWORD")]
        password: String,
        #[arg(long)]
        create: bool,
    },
    List,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let home = cli.home.unwrap_or_else(sf_core::config::default_home);
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("sf-rt")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to start runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let res = rt.block_on(async move {
        match cli.cmd {
            Cmd::Serve => serve::run(home).await,
            Cmd::Mcp => cmds::mcp().await,
            Cmd::Init { from_v1_env, force } => cmds::init(&home, from_v1_env.as_deref(), force),
            Cmd::Backup { cmd } => match cmd {
                BackupCmd::Create {
                    kind,
                    passphrase,
                    note,
                    include_secrets,
                } => cmds::backup_create(&home, &kind, passphrase, note, include_secrets).await,
                BackupCmd::List => cmds::backup_list(&home).await,
                BackupCmd::Verify { file, passphrase } => cmds::backup_verify(&file, passphrase),
            },
            Cmd::Restore {
                file,
                passphrase,
                force,
            } => cmds::restore(&home, &file, passphrase, force),
            Cmd::Migrate { cmd } => match cmd {
                MigrateCmd::FromPostgres {
                    site,
                    tables,
                    dry_run,
                    refresh,
                } => cmds::migrate_pg(&home, site, tables, dry_run, refresh).await,
                MigrateCmd::Export { dir, site } => cmds::migrate_export(&dir, site),
                MigrateCmd::Import { dir, site, refresh } => {
                    cmds::migrate_import(&home, &dir, site, refresh).await
                }
                MigrateCmd::Verify { site, sample } => {
                    cmds::migrate_verify(&home, site, sample).await
                }
            },
            Cmd::Token { cmd } => match cmd {
                TokenCmd::Create {
                    name,
                    user,
                    scopes,
                    expires_days,
                } => cmds::token_create(&home, name, user, scopes, expires_days).await,
                TokenCmd::List => cmds::token_list(&home).await,
                TokenCmd::Revoke { id_or_prefix } => cmds::token_revoke(&home, id_or_prefix).await,
            },
            Cmd::User { cmd } => match cmd {
                UserCmd::SetPassword {
                    email,
                    password,
                    create,
                } => cmds::user_set_password(&home, email, password, create).await,
                UserCmd::List => cmds::user_list(&home).await,
            },
            Cmd::Doctor => cmds::doctor(&home).await,
            Cmd::FetchRuntime { cpu } => cmds::fetch_runtime(&home, !cpu).await,
            Cmd::FetchModel => cmds::fetch_model(&home).await,
            Cmd::Version => {
                println!("siloforge {}", sf_core::VERSION);
                Ok(())
            }
        }
    });
    match res {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
