//! Backup and restore.
//!
//! An archive is a zip (deflate) holding:
//! * `siloforge.db`: a consistent snapshot taken with `VACUUM INTO` on a separate read connection
//!   (no writer stall, WAL content included);
//! * `siloforge.toml` and `secrets.env` (when present). `secrets.env` holds the master keys
//!   (`JWT_SECRET`, `SILOFORGE_ENCRYPTION_KEY`, provider keys), so it is only archived when the
//!   archive is encrypted or [`BackupOptions::include_secrets`] is set explicitly (M-02). Otherwise
//!   the manifest records `secrets_excluded: true` and a restore must be given the secrets
//!   separately (keep a copy of `secrets.env` or the passphrase in a password manager);
//! * `manifest.json`: [`Manifest`] with the app/schema version, instance id, hostname and the sha256 +
//!   size of every file.
//!
//! With a passphrase the zip is wrapped in the streaming AES-256-GCM format described in [`crypt`]
//! (`*.zip.sfenc`). Archives go to `cfg.backups_dir()`, are catalogued in `backups`, and are pruned to
//! `backup.keep`. [`restore`] verifies everything first, refuses a schema newer than this binary and a
//! running instance (unless forced), then swaps files in atomically (temp file + rename), keeping the
//! previous files as `<name>.pre-restore-<ts>`.

pub mod crypt;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sf_core::{Config, Error, Result};
use sf_store::{Db, err, row_to_json};
use sha2::{Digest, Sha256};

use crate::notify::{Notification, Notifier, events};

pub const DB_FILE: &str = "siloforge.db";
pub const CONFIG_FILE: &str = "siloforge.toml";
pub const SECRETS_FILE: &str = "secrets.env";
pub const MANIFEST_FILE: &str = "manifest.json";
/// Manifest format version.
pub const FORMAT: u32 = 1;
/// Manifest note added when `secrets.env` was left out of an unencrypted archive.
pub const SECRETS_EXCLUDED_NOTE: &str =
    "secrets.env excluded (unencrypted backup); provide secrets.env on restore";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackupKind {
    /// Database + config + secrets.
    #[default]
    Full,
    /// Database only.
    Db,
    /// Config + secrets only.
    Config,
}

impl BackupKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Db => "db",
            Self::Config => "config",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "full" => Some(Self::Full),
            "db" => Some(Self::Db),
            "config" => Some(Self::Config),
            _ => None,
        }
    }
    fn has_db(&self) -> bool {
        !matches!(self, Self::Config)
    }
    fn has_config(&self) -> bool {
        !matches!(self, Self::Db)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BackupOptions {
    #[serde(default)]
    pub kind: BackupKind,
    /// Encrypt the archive with this passphrase (None = plain zip).
    #[serde(default)]
    pub passphrase: Option<String>,
    #[serde(default)]
    pub note: String,
    /// Archive `secrets.env` even without a passphrase (plaintext master keys). Off by default.
    #[serde(default)]
    pub include_secrets: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub format: u32,
    pub app_version: String,
    /// `schema_version` of the snapshot (0 for config-only archives).
    pub schema_version: i64,
    pub created_at: String,
    pub instance_id: String,
    pub hostname: String,
    pub kind: BackupKind,
    #[serde(default)]
    pub note: String,
    pub files: BTreeMap<String, FileEntry>,
    /// True when `secrets.env` existed but was left out because the archive is unencrypted.
    /// Restoring such an archive needs the secrets (JWT/encryption keys) provided separately.
    #[serde(default)]
    pub secrets_excluded: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackupInfo {
    pub id: String,
    pub path: PathBuf,
    pub kind: BackupKind,
    pub size_bytes: u64,
    /// sha256 of the archive file itself.
    pub sha256: String,
    pub encrypted: bool,
    pub app_version: String,
    pub schema_version: i64,
    pub created_at: String,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreReport {
    pub manifest: Manifest,
    pub restored: Vec<String>,
    /// Files that were replaced, kept under their `.pre-restore-<ts>` names.
    pub previous: Vec<PathBuf>,
    /// Human-readable follow-ups (e.g. secrets that must be provided by hand).
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Hashing writer adapter.
struct HashWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    n: u64,
}

impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let k = self.inner.write(buf)?;
        if let Some(done) = buf.get(..k) {
            self.hasher.update(done);
        }
        self.n += k as u64;
        Ok(k)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn hash_file(path: &Path) -> Result<FileEntry> {
    let mut f = std::fs::File::open(path)?;
    let mut w = HashWriter {
        inner: std::io::sink(),
        hasher: Sha256::new(),
        n: 0,
    };
    std::io::copy(&mut f, &mut w)?;
    Ok(FileEntry {
        sha256: hex::encode(w.hasher.finalize()),
        size: w.n,
    })
}

fn zip_err(e: zip::result::ZipError) -> Error {
    Error::bad(format!("backup archive is invalid: {e}"))
}

/// Snapshot of the live database into `dest` (must not exist) via `VACUUM INTO`. Returns its schema version.
pub fn snapshot_db(db_path: &Path, dest: &Path) -> Result<i64> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(err)?;
    conn.busy_timeout(std::time::Duration::from_secs(30))
        .map_err(err)?;
    let dest_s = dest.to_string_lossy().to_string();
    conn.execute("VACUUM INTO ?1", [&dest_s]).map_err(err)?;
    drop(conn);
    let snap = rusqlite::Connection::open_with_flags(dest, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(err)?;
    sf_store::migrate::current_version(&snap)
}

fn ts_compact() -> String {
    chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

/// Builds an archive in `out_dir` (blocking). Returns the final path, manifest and archive hash entry.
pub fn create_archive(
    cfg: &Config,
    out_dir: &Path,
    opts: &BackupOptions,
    instance_id: &str,
) -> Result<(PathBuf, Manifest, FileEntry)> {
    std::fs::create_dir_all(out_dir)?;
    // The tail of a UUIDv7 is random; its head is the timestamp (identical for backups taken
    // within the same minute, which used to make their file names collide).
    let short = sf_core::new_id().replace('-', "");
    let short = short.get(short.len().saturating_sub(8)..).unwrap_or("x");
    let tmp = out_dir.join(format!(".tmp-{short}"));
    std::fs::create_dir_all(&tmp)?;
    let res = build(cfg, out_dir, &tmp, short, opts, instance_id);
    let _ = std::fs::remove_dir_all(&tmp);
    res
}

fn build(
    cfg: &Config,
    out_dir: &Path,
    tmp: &Path,
    short: &str,
    opts: &BackupOptions,
    instance_id: &str,
) -> Result<(PathBuf, Manifest, FileEntry)> {
    let mut sources: Vec<(&str, PathBuf)> = Vec::new();
    let mut schema_version = 0;
    if opts.kind.has_db() {
        let snap = tmp.join(DB_FILE);
        schema_version = snapshot_db(&cfg.db_path(), &snap)?;
        sources.push((DB_FILE, snap));
    }
    let encrypted = opts.passphrase.as_deref().is_some_and(|p| !p.is_empty());
    let mut secrets_excluded = false;
    if opts.kind.has_config() {
        for name in [CONFIG_FILE, SECRETS_FILE] {
            let p = cfg.home.join(name);
            if !p.is_file() {
                continue;
            }
            if name == SECRETS_FILE && !encrypted && !opts.include_secrets {
                // M-02: never put the master keys into a plaintext archive by default.
                secrets_excluded = true;
                tracing::warn!(
                    "backup is unencrypted: secrets.env left out (set SILOFORGE_BACKUP_PASSPHRASE or pass include_secrets); a restore will need the secrets provided separately"
                );
                continue;
            }
            sources.push((name, p));
        }
    }
    let zip_path = tmp.join("archive.zip");
    let mut zw = zip::ZipWriter::new(std::fs::File::create(&zip_path)?);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    let mut files = BTreeMap::new();
    for (name, path) in &sources {
        zw.start_file(*name, options).map_err(zip_err)?;
        let mut hw = HashWriter {
            inner: &mut zw,
            hasher: Sha256::new(),
            n: 0,
        };
        std::io::copy(&mut std::fs::File::open(path)?, &mut hw)?;
        let entry = FileEntry {
            sha256: hex::encode(hw.hasher.finalize()),
            size: hw.n,
        };
        files.insert(name.to_string(), entry);
    }
    let manifest = Manifest {
        format: FORMAT,
        app_version: sf_core::VERSION.to_string(),
        schema_version,
        created_at: sf_core::now_rfc3339(),
        instance_id: instance_id.to_string(),
        hostname: sysinfo::System::host_name().unwrap_or_default(),
        kind: opts.kind,
        note: if secrets_excluded {
            let n: String = opts.note.chars().take(400).collect();
            let sep = if n.is_empty() { "" } else { " " };
            format!("{n}{sep}[{SECRETS_EXCLUDED_NOTE}]")
        } else {
            opts.note.chars().take(500).collect()
        },
        files,
        secrets_excluded,
    };
    zw.start_file(MANIFEST_FILE, options).map_err(zip_err)?;
    zw.write_all(&serde_json::to_vec_pretty(&manifest).map_err(Error::internal)?)?;
    zw.finish().map_err(zip_err)?;

    let base = format!("siloforge-{}-{}-{short}", ts_compact(), opts.kind.as_str());
    let final_path = match opts.passphrase.as_deref().filter(|p| !p.is_empty()) {
        Some(pass) => {
            let out = out_dir.join(format!("{base}.zip.sfenc"));
            let part = out_dir.join(format!("{base}.zip.sfenc.part"));
            let w = std::io::BufWriter::new(std::fs::File::create(&part)?);
            let r = std::io::BufReader::new(std::fs::File::open(&zip_path)?);
            if let Err(e) = crypt::encrypt_stream(r, w, pass, crypt::CHUNK) {
                let _ = std::fs::remove_file(&part);
                return Err(e);
            }
            std::fs::rename(&part, &out)?;
            out
        }
        None => {
            let out = out_dir.join(format!("{base}.zip"));
            std::fs::rename(&zip_path, &out)?;
            out
        }
    };
    let entry = hash_file(&final_path)?;
    Ok((final_path, manifest, entry))
}

/// Opens an archive as a zip, decrypting into a sibling temp file when needed. The returned guard
/// deletes the temp file.
struct Opened {
    zip: zip::ZipArchive<std::fs::File>,
    tmp: Option<PathBuf>,
}

impl Drop for Opened {
    fn drop(&mut self) {
        if let Some(t) = self.tmp.take() {
            let _ = std::fs::remove_file(t);
        }
    }
}

fn open_archive(path: &Path, passphrase: Option<&str>, tmp_dir: &Path) -> Result<Opened> {
    if !path.is_file() {
        return Err(Error::not_found(format!(
            "backup file not found: {}",
            path.display()
        )));
    }
    let (file, tmp) = if crypt::is_encrypted(path) {
        let pass = passphrase
            .filter(|p| !p.is_empty())
            .ok_or_else(|| Error::bad("this backup is encrypted; a passphrase is required"))?;
        std::fs::create_dir_all(tmp_dir)?;
        let tmp = tmp_dir.join(format!(".sf-decrypt-{}.zip", sf_core::new_id()));
        let res = (|| {
            let r = std::io::BufReader::new(std::fs::File::open(path)?);
            let w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
            crypt::decrypt_stream(r, w, pass)?;
            Ok::<_, Error>(std::fs::File::open(&tmp)?)
        })();
        match res {
            Ok(f) => (f, Some(tmp)),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
    } else {
        (std::fs::File::open(path)?, None)
    };
    let mut opened = Opened {
        zip: zip::ZipArchive::new(file)
            .map_err(zip_err)
            .inspect_err(|_| {
                if let Some(t) = &tmp {
                    let _ = std::fs::remove_file(t);
                }
            })?,
        tmp: None,
    };
    opened.tmp = tmp;
    Ok(opened)
}

fn read_manifest(z: &mut zip::ZipArchive<std::fs::File>) -> Result<Manifest> {
    let mut f = z
        .by_name(MANIFEST_FILE)
        .map_err(|_| Error::bad("backup has no manifest.json"))?;
    let mut s = String::new();
    f.by_ref()
        .take(1 << 20)
        .read_to_string(&mut s)
        .map_err(|_| Error::bad("manifest.json unreadable"))?;
    let m: Manifest =
        serde_json::from_str(&s).map_err(|e| Error::bad(format!("manifest.json invalid: {e}")))?;
    if m.format > FORMAT {
        return Err(Error::bad(format!(
            "backup format v{} is newer than this binary supports (v{FORMAT})",
            m.format
        )));
    }
    Ok(m)
}

/// L-10: hard caps on restored sizes (a crafted archive must not fill the disk).
const MAX_SMALL_FILE: u64 = 1 << 20; // siloforge.toml / secrets.env
const MAX_DB_FILE: u64 = 256 << 30; // 256 GiB
/// Deflate cannot expand more than ~1032:1; anything claiming more is a bomb.
const MAX_INFLATE_RATIO: u64 = 1100;

fn check_entry_size(name: &str, manifest: u64, zip_size: u64, compressed: u64) -> Result<()> {
    let cap = if name == DB_FILE {
        MAX_DB_FILE
    } else {
        MAX_SMALL_FILE
    };
    if manifest > cap {
        return Err(Error::bad(format!(
            "{name}: {manifest} bytes is over the restore limit ({cap})"
        )));
    }
    if manifest != zip_size {
        return Err(Error::bad(format!(
            "{name}: manifest size {manifest} does not match the archive entry ({zip_size})"
        )));
    }
    if manifest > compressed.saturating_mul(MAX_INFLATE_RATIO).max(4096) {
        return Err(Error::bad(format!(
            "{name}: implausible compression ratio (zip bomb?)"
        )));
    }
    Ok(())
}

/// Copies one zip entry to `out`, checking size and sha256 against the manifest.
fn extract_checked<W: Write>(
    z: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
    want: &FileEntry,
    out: W,
) -> Result<()> {
    let mut f = z
        .by_name(name)
        .map_err(|_| Error::bad(format!("backup is missing {name}")))?;
    check_entry_size(name, want.size, f.size(), f.compressed_size())?;
    let mut hw = HashWriter {
        inner: out,
        hasher: Sha256::new(),
        n: 0,
    };
    std::io::copy(&mut f.by_ref().take(want.size.saturating_add(1)), &mut hw)
        .map_err(|e| Error::bad(format!("{name}: corrupted ({e})")))?;
    hw.flush()?;
    if hw.n != want.size || hex::encode(hw.hasher.finalize()) != want.sha256 {
        return Err(Error::bad(format!("{name}: checksum mismatch (tampered?)")));
    }
    Ok(())
}

/// Verifies an archive: manifest present and valid, every listed file present with matching sha256.
/// Blocking. Decrypts into the archive's directory when encrypted.
pub fn verify(path: &Path, passphrase: Option<&str>) -> Result<Manifest> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut o = open_archive(path, passphrase, dir)?;
    let m = read_manifest(&mut o.zip)?;
    if m.kind.has_db() && !m.files.contains_key(DB_FILE) {
        return Err(Error::bad("backup manifest lists no database"));
    }
    for (name, want) in &m.files {
        if ![DB_FILE, CONFIG_FILE, SECRETS_FILE].contains(&name.as_str()) {
            return Err(Error::bad(format!("unexpected file in manifest: {name}")));
        }
        extract_checked(&mut o.zip, name, want, std::io::sink())?;
    }
    Ok(m)
}

type RenameFn<'a> = &'a dyn Fn((&Path, &Path)) -> std::io::Result<()>;

/// Swaps staged `(name, tmp)` files into `home`: secrets first, config, database last. Existing
/// files are kept as `<name>.pre-restore-<ts>`. Every rename is recorded; on the first failure all
/// recorded renames are undone in reverse order, so the home is left exactly as it was (R12).
/// Returns (kept previous files, restored names).
fn swap_in(
    home: &Path,
    staged: &mut [(String, PathBuf)],
    ts: &str,
    rename: RenameFn<'_>,
) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let rank = |n: &str| match n {
        SECRETS_FILE => 0,
        CONFIG_FILE => 1,
        _ => 2,
    };
    staged.sort_by_key(|(n, _)| rank(n));
    let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut previous = Vec::new();
    let mut restored = Vec::new();
    let step = |from: &Path, to: &Path, done: &mut Vec<(PathBuf, PathBuf)>| {
        rename((from, to))?;
        done.push((from.to_path_buf(), to.to_path_buf()));
        Ok::<(), std::io::Error>(())
    };
    let res = (|| {
        for (name, tmp) in staged.iter() {
            let dest = home.join(name);
            if dest.exists() {
                let keep = home.join(format!("{name}.pre-restore-{ts}"));
                step(&dest, &keep, &mut done)?;
                previous.push(keep);
                if name == DB_FILE {
                    for sfx in ["-wal", "-shm"] {
                        let side = home.join(format!("{name}{sfx}"));
                        if side.exists() {
                            let to = home.join(format!("{name}.pre-restore-{ts}{sfx}"));
                            step(&side, &to, &mut done)?;
                        }
                    }
                }
            }
            step(tmp, &dest, &mut done)?;
            restored.push(name.clone());
        }
        Ok::<(), std::io::Error>(())
    })();
    if let Err(e) = res {
        let mut stuck = Vec::new();
        for (from, to) in done.iter().rev() {
            if let Err(re) = std::fs::rename(to, from) {
                stuck.push(format!("{} -> {}: {re}", to.display(), from.display()));
            }
        }
        tracing::error!(error = %e, ?stuck, "restore swap failed; previous files put back");
        let mut msg = format!("restore failed while swapping files ({e}); previous files restored");
        if !stuck.is_empty() {
            msg = format!(
                "restore failed while swapping files ({e}); could NOT put back: {}",
                stuck.join("; ")
            );
        }
        return Err(Error::internal(msg));
    }
    Ok((previous, restored))
}

/// Restores an archive into `target_home` (blocking). See the module docs for the safety rules.
pub fn restore(
    path: &Path,
    target_home: &Path,
    passphrase: Option<&str>,
    force: bool,
) -> Result<RestoreReport> {
    std::fs::create_dir_all(target_home)?;
    // R2: hold the instance lock for the whole restore so a `serve` can't start mid-swap.
    let _guard = if force {
        crate::instance::InstanceGuard::acquire(target_home).ok()
    } else {
        if let Some(pid) = crate::instance::running_instance(target_home) {
            return Err(Error::Conflict(format!(
                "SiloForge (pid {pid}) is running on {}; stop it first or pass force",
                target_home.display()
            )));
        }
        Some(
            crate::instance::InstanceGuard::acquire(target_home).map_err(|e| match e {
                crate::instance::AcquireError::Running(_) => Error::Conflict(format!(
                    "{e} ({}); stop it first or pass force",
                    target_home.display()
                )),
                crate::instance::AcquireError::Io(io) => Error::from(io),
            })?,
        )
    };
    let mut o = open_archive(path, passphrase, target_home)?;
    let m = read_manifest(&mut o.zip)?;
    let latest = sf_store::migrate::latest_version();
    if m.schema_version > latest {
        return Err(Error::bad(format!(
            "backup schema v{} is newer than this binary (v{latest}); upgrade SiloForge first",
            m.schema_version
        )));
    }
    // 1. extract everything to temp names, verifying hashes
    let mut staged: Vec<(String, PathBuf)> = Vec::new();
    let cleanup = |staged: &[(String, PathBuf)]| {
        for (_, p) in staged {
            let _ = std::fs::remove_file(p);
        }
    };
    for (name, want) in &m.files {
        if ![DB_FILE, CONFIG_FILE, SECRETS_FILE].contains(&name.as_str()) {
            cleanup(&staged);
            return Err(Error::bad(format!("unexpected file in manifest: {name}")));
        }
        let tmp = target_home.join(format!("{name}.restore-tmp"));
        let res = std::fs::File::create(&tmp)
            .map_err(Error::from)
            .and_then(|f| extract_checked(&mut o.zip, name, want, std::io::BufWriter::new(f)));
        staged.push((name.clone(), tmp));
        if let Err(e) = res {
            cleanup(&staged);
            return Err(e);
        }
    }
    if let Some((_, db_tmp)) = staged.iter().find(|(n, _)| n == DB_FILE) {
        let check = rusqlite::Connection::open_with_flags(db_tmp, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|c| c.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)));
        if check.as_deref() != Ok("ok") {
            cleanup(&staged);
            return Err(Error::bad("restored database failed the integrity check"));
        }
    }
    drop(o);
    // 2. swap in (secrets first, database last); any failure puts every file back (R12)
    let ts = ts_compact();
    let (previous, restored) = match swap_in(target_home, &mut staged, &ts, &|p| {
        std::fs::rename(p.0, p.1)
    }) {
        Ok(v) => v,
        Err(e) => {
            cleanup(&staged);
            return Err(e);
        }
    };
    tracing::info!(path = %path.display(), home = %target_home.display(), ?restored, "backup restored");
    let mut warnings = Vec::new();
    if m.secrets_excluded && !target_home.join(SECRETS_FILE).is_file() {
        let w = format!(
            "this backup does not contain {SECRETS_FILE}; copy the original {SECRETS_FILE} (JWT_SECRET, SILOFORGE_ENCRYPTION_KEY, provider keys) into {} before starting SiloForge, or stored WP/GSC credentials cannot be decrypted",
            target_home.display()
        );
        tracing::warn!("{w}");
        warnings.push(w);
    }
    Ok(RestoreReport {
        manifest: m,
        restored,
        previous,
        warnings,
    })
}

// ---------------------------------------------------------------------------------------------
// Catalogue + orchestration (async).

/// Returns (creating once) this instance's id from `kv`.
pub async fn instance_id(db: &Db) -> Result<String> {
    db.write(|t| {
        t.execute(
            "INSERT OR IGNORE INTO kv(key, value) VALUES('instance_id', ?1)",
            [sf_core::new_id()],
        )
        .map_err(err)?;
        t.query_row("SELECT value FROM kv WHERE key='instance_id'", [], |r| {
            r.get(0)
        })
        .map_err(err)
    })
    .await
}

async fn insert_running(db: &Db, id: &str, opts: &BackupOptions) -> Result<()> {
    let (id, kind, note) = (
        id.to_string(),
        opts.kind.as_str(),
        opts.note.chars().take(500).collect::<String>(),
    );
    db.write(move |t| {
        t.execute(
            "INSERT INTO backups(id, path, kind, status, note, app_version) VALUES(?1,'',?2,'running',?3,?4)",
            params![id, kind, note, sf_core::VERSION],
        )
        .map_err(err)?;
        Ok(())
    })
    .await
}

async fn run_backup(cfg: &Config, db: &Db, opts: BackupOptions, id: &str) -> Result<BackupInfo> {
    let iid = instance_id(db).await?;
    let (cfg2, opts2) = (cfg.clone(), opts.clone());
    let out_dir = cfg.backups_dir();
    let res = tokio::task::spawn_blocking(move || create_archive(&cfg2, &out_dir, &opts2, &iid))
        .await
        .map_err(|e| Error::internal(format!("backup task: {e}")))?;
    let id_s = id.to_string();
    match res {
        Ok((path, manifest, entry)) => {
            let info = BackupInfo {
                id: id_s.clone(),
                path: path.clone(),
                kind: opts.kind,
                size_bytes: entry.size,
                sha256: entry.sha256.clone(),
                encrypted: opts.passphrase.as_deref().is_some_and(|p| !p.is_empty()),
                app_version: manifest.app_version.clone(),
                schema_version: manifest.schema_version,
                created_at: manifest.created_at.clone(),
                note: manifest.note.clone(),
            };
            let i2 = info.clone();
            db.write(move |t| {
                t.execute(
                    "UPDATE backups SET path=?2, size_bytes=?3, sha256=?4, encrypted=?5, status='ok' WHERE id=?1",
                    params![
                        i2.id,
                        i2.path.to_string_lossy(),
                        i2.size_bytes as i64,
                        i2.sha256,
                        i2.encrypted as i64
                    ],
                )
                .map_err(err)?;
                Ok(())
            })
            .await?;
            if let Err(e) = prune(cfg, db).await {
                tracing::warn!(error = %e, "backup prune failed");
            }
            Ok(info)
        }
        Err(e) => {
            let msg = e.to_string();
            let _ = db
                .write(move |t| {
                    t.execute(
                        "UPDATE backups SET status='failed', note=CASE WHEN note='' THEN ?2 ELSE note || ' | ' || ?2 END WHERE id=?1",
                        params![id_s, msg.chars().take(500).collect::<String>()],
                    )
                    .map_err(err)?;
                    Ok(())
                })
                .await;
            Err(e)
        }
    }
}

/// Creates a backup now and records it (awaits completion).
pub async fn create(cfg: &Config, db: &Db, opts: BackupOptions) -> Result<BackupInfo> {
    let id = sf_core::new_id();
    insert_running(db, &id, &opts).await?;
    run_backup(cfg, db, opts, &id).await
}

/// Records a `running` backup row, then builds it in the background. Returns the backup id at once.
/// Sends `backup.done` / `backup.failed` when a notifier is given.
pub async fn start(
    cfg: Arc<Config>,
    db: Db,
    opts: BackupOptions,
    notifier: Option<Notifier>,
) -> Result<String> {
    let id = sf_core::new_id();
    insert_running(&db, &id, &opts).await?;
    let id2 = id.clone();
    tokio::spawn(async move {
        let res = run_backup(&cfg, &db, opts, &id2).await;
        notify_result(notifier.as_ref(), &res);
    });
    Ok(id)
}

fn notify_result(n: Option<&Notifier>, res: &Result<BackupInfo>) {
    let Some(n) = n else { return };
    let note = match res {
        Ok(i) => Notification::new(
            events::BACKUP_DONE,
            "SiloForge backup done",
            format!(
                "{} backup, {:.1} MB{}",
                i.kind.as_str(),
                i.size_bytes as f64 / 1_048_576.0,
                if i.encrypted { ", encrypted" } else { "" }
            ),
        )
        .data(serde_json::json!({"id": i.id, "path": i.path, "size_bytes": i.size_bytes})),
        Err(e) => Notification::new(
            events::BACKUP_FAILED,
            "SiloForge backup failed",
            e.to_string(),
        ),
    };
    n.notify(note);
}

const BACKUP_COLS: &str =
    "id, path, kind, size_bytes, sha256, encrypted, app_version, status, note, created_at";

pub async fn list(db: &Db, limit: usize) -> Result<Vec<Value>> {
    let limit = limit.clamp(1, 1000) as i64;
    db.read(move |c| {
        let mut stmt = c
            .prepare(&format!(
                "SELECT {BACKUP_COLS} FROM backups ORDER BY created_at DESC, id LIMIT ?1"
            ))
            .map_err(err)?;
        let rows = stmt
            .query_map([limit], |r| Ok(row_to_json(r, &[], &["encrypted"])))
            .map_err(err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(err)?;
        Ok(rows)
    })
    .await
}

pub async fn get(db: &Db, id: &str) -> Result<Option<Value>> {
    let id = id.to_string();
    db.read(move |c| {
        c.query_row(
            &format!("SELECT {BACKUP_COLS} FROM backups WHERE id=?1"),
            [&id],
            |r| Ok(row_to_json(r, &[], &["encrypted"])),
        )
        .optional()
        .map_err(err)
    })
    .await
}

/// Deletes the archive file (if under `cfg.backups_dir()` or recorded path exists) and the row.
pub async fn delete(db: &Db, id: &str) -> Result<bool> {
    let Some(row) = get(db, id).await? else {
        return Ok(false);
    };
    if let Some(p) = row
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
    {
        let p = PathBuf::from(p);
        if p.is_file() {
            tokio::fs::remove_file(&p).await?;
        }
    }
    let id = id.to_string();
    db.write(move |t| {
        Ok(t.execute("DELETE FROM backups WHERE id=?1", [&id])
            .map_err(err)?
            > 0)
    })
    .await
}

/// Keeps the newest `backup.keep` successful archives; deletes older files and rows.
pub async fn prune(cfg: &Config, db: &Db) -> Result<usize> {
    let keep = cfg.backup.keep.max(1) as i64;
    let old: Vec<(String, String)> = db
        .read(move |c| {
            let mut stmt = c
                .prepare(
                    "SELECT id, path FROM backups WHERE status='ok' ORDER BY created_at DESC, id LIMIT -1 OFFSET ?1",
                )
                .map_err(err)?;
            let rows = stmt
                .query_map([keep], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?;
    let mut n = 0;
    for (id, path) in old {
        let p = PathBuf::from(&path);
        if !path.is_empty()
            && p.is_file()
            && let Err(e) = tokio::fs::remove_file(&p).await
        {
            tracing::warn!(path, error = %e, "could not delete old backup");
            continue;
        }
        db.write(move |t| {
            t.execute("DELETE FROM backups WHERE id=?1", [&id])
                .map_err(err)?;
            Ok(())
        })
        .await?;
        n += 1;
    }
    Ok(n)
}

/// Passphrase for automatic backups: env var named by `backup.passphrase_env`, else `secrets.env`.
pub fn configured_passphrase(cfg: &Config) -> Option<String> {
    let var = cfg.backup.passphrase_env.trim();
    if var.is_empty() {
        return None;
    }
    if let Ok(v) = std::env::var(var)
        && !v.is_empty()
    {
        return Some(v);
    }
    let text = std::fs::read_to_string(cfg.home.join(SECRETS_FILE)).ok()?;
    sf_core::config::parse_env_file(&text)
        .into_iter()
        .find(|(k, v)| k == var && !v.is_empty())
        .map(|(_, v)| v)
}

/// Next run time (local time zone) of a 5- or 6-field cron expression.
pub fn next_run(cron: &str) -> Result<chrono::DateTime<chrono::Local>> {
    let c = croner::Cron::new(cron)
        .with_seconds_optional()
        .parse()
        .map_err(|e| Error::bad(format!("invalid backup.schedule '{cron}': {e}")))?;
    c.find_next_occurrence(&chrono::Local::now(), false)
        .map_err(|e| Error::bad(format!("backup.schedule has no next run: {e}")))
}

/// Spawns the automatic backup loop for `backup.schedule`. The schedule (and dir/keep/passphrase)
/// is re-read from the live config before every wait, so `PUT /api/v2/settings` applies without a
/// restart (v2.1); an empty or invalid schedule idles (re-checked every minute).
pub fn spawn_scheduler(
    cfg: impl Into<sf_core::LiveConfig>,
    db: Db,
    notifier: Option<Notifier>,
) -> Option<tokio::task::JoinHandle<()>> {
    let live: sf_core::LiveConfig = cfg.into();
    {
        let sched = live.get().backup.schedule.trim().to_string();
        if !sched.is_empty()
            && let Err(e) = next_run(&sched)
        {
            tracing::warn!(error = %e, "automatic backups paused until backup.schedule is fixed");
        }
    }
    // R8: supervised; a panic restarts the loop (it recomputes the next slot).
    let db0 = db.clone();
    Some(crate::supervise::spawn(
        "backup-scheduler",
        Some(db0),
        move || {
            let (live, db, notifier) = (live.clone(), db.clone(), notifier.clone());
            async move {
                loop {
                    let sched = live.get().backup.schedule.trim().to_string();
                    let next = if sched.is_empty() {
                        None
                    } else {
                        next_run(&sched).ok()
                    };
                    let Some(next) = next else {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                        continue;
                    };
                    // Sleep in <= 60 s steps so clock changes, sleep-resume and schedule edits
                    // are picked up.
                    let mut changed = false;
                    loop {
                        let wait = (next - chrono::Local::now()).num_seconds();
                        if wait <= 0 {
                            break;
                        }
                        tokio::time::sleep(
                            std::time::Duration::from_secs(wait.clamp(1, 60) as u64),
                        )
                        .await;
                        if live.get().backup.schedule.trim() != sched {
                            changed = true;
                            break;
                        }
                    }
                    if changed {
                        continue;
                    }
                    let cfg = live.get();
                    let opts = BackupOptions {
                        kind: BackupKind::Full,
                        passphrase: configured_passphrase(&cfg),
                        note: "scheduled".into(),
                        include_secrets: false,
                    };
                    tracing::info!("scheduled backup starting");
                    let res = create(&cfg, &db, opts).await;
                    if let Err(e) = &res {
                        tracing::error!(error = %e, "scheduled backup failed");
                    }
                    notify_result(notifier.as_ref(), &res);
                    // avoid double-firing within the same second
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        },
    ))
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    async fn home_with_data() -> (Config, Db) {
        let dir = std::env::temp_dir().join(format!("sf-ops-bk-{}", sf_core::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CONFIG_FILE), "[backup]\nkeep = 2\n").unwrap();
        std::fs::write(
            dir.join(SECRETS_FILE),
            "JWT_SECRET=abcdefghijklmnopqrstuvwxyz\n",
        )
        .unwrap();
        let mut cfg = Config::default();
        cfg.home = dir.clone();
        cfg.backup.keep = 2;
        let db = Db::open(&cfg.db_path()).unwrap();
        db.write(|t| {
            t.execute(
                "INSERT INTO users(id,email,password_hash,role) VALUES('u1','a@b.c','x','admin')",
                [],
            )
            .map_err(err)?;
            Ok(())
        })
        .await
        .unwrap();
        (cfg, db)
    }

    fn user_count(path: &Path) -> i64 {
        let c = rusqlite::Connection::open(path).unwrap();
        c.query_row("SELECT count(*) FROM users", [], |r| r.get(0))
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_verify_restore_roundtrip() {
        let (cfg, db) = home_with_data().await;
        // plain
        let plain = create(
            &cfg,
            &db,
            BackupOptions {
                note: "t1".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!plain.encrypted && plain.size_bytes > 0 && plain.schema_version >= 1);
        let m = verify(&plain.path, None).unwrap();
        // M-02: an unencrypted archive leaves the master keys out by default.
        assert!(m.files.contains_key(DB_FILE) && !m.files.contains_key(SECRETS_FILE));
        assert!(m.secrets_excluded);
        assert!(m.note.starts_with("t1 [") && m.note.contains("secrets.env excluded"));
        // ...unless explicitly asked for
        let with = create(
            &cfg,
            &db,
            BackupOptions {
                include_secrets: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mw = verify(&with.path, None).unwrap();
        assert!(mw.files.contains_key(SECRETS_FILE) && !mw.secrets_excluded);
        assert!(delete(&db, &with.id).await.unwrap());

        // encrypted
        let enc = create(
            &cfg,
            &db,
            BackupOptions {
                passphrase: Some("correct horse".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(enc.encrypted && enc.path.to_string_lossy().ends_with(".sfenc"));
        assert!(verify(&enc.path, None).is_err());
        assert!(verify(&enc.path, Some("wrong")).is_err());
        let me = verify(&enc.path, Some("correct horse")).unwrap();
        assert!(me.files.contains_key(SECRETS_FILE) && !me.secrets_excluded);

        // restore into a fresh home, and over an existing one
        let target = cfg.home.join("restored");
        let r = restore(&plain.path, &target, None, false).unwrap();
        assert_eq!(r.restored.len(), 2);
        assert!(r.previous.is_empty());
        assert!(r.warnings.iter().any(|w| w.contains("secrets.env")));
        assert_eq!(user_count(&target.join(DB_FILE)), 1);
        let r2 = restore(&enc.path, &target, Some("correct horse"), false).unwrap();
        assert_eq!(r2.restored.len(), 3);
        assert_eq!(r2.previous.len(), 2);
        assert!(r2.warnings.is_empty());
        assert!(r2.previous.iter().all(|p| p.exists()));
        assert!(restore(&enc.path, &target, Some("nope"), false).is_err());
        // no temp files left behind
        let leftovers: Vec<_> = std::fs::read_dir(&target)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.contains("restore-tmp") || n.contains("sf-decrypt")
            })
            .collect();
        assert!(leftovers.is_empty());

        // running instance refusal (our own pid file counts as running)
        let guard = crate::instance::InstanceGuard::acquire(&target).unwrap();
        assert!(matches!(
            restore(&plain.path, &target, None, false),
            Err(Error::Conflict(_))
        ));
        drop(guard);

        // prune keeps 2 newest
        let third = create(&cfg, &db, BackupOptions::default()).await.unwrap();
        let rows = list(&db, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(!plain.path.exists());
        assert!(third.path.exists());
        assert!(delete(&db, &third.id).await.unwrap());
        assert!(!third.path.exists());
        db.close();
        let _ = std::fs::remove_dir_all(&cfg.home);
    }

    /// R12: a failing rename (AV / indexer holding the DB) puts every swapped file back.
    #[test]
    fn swap_failure_rolls_back_every_file() {
        let home = std::env::temp_dir().join(format!("sf-ops-swap-{}", sf_core::new_id()));
        std::fs::create_dir_all(&home).unwrap();
        for (n, v) in [
            (SECRETS_FILE, "old-secrets"),
            (CONFIG_FILE, "old-config"),
            (DB_FILE, "old-db"),
        ] {
            std::fs::write(home.join(n), v).unwrap();
        }
        std::fs::write(home.join("siloforge.db-wal"), "old-wal").unwrap();
        let mut staged = Vec::new();
        for n in [DB_FILE, CONFIG_FILE, SECRETS_FILE] {
            let tmp = home.join(format!("{n}.restore-tmp"));
            std::fs::write(&tmp, format!("new-{n}")).unwrap();
            staged.push((n.to_string(), tmp));
        }
        let fail_db = |(from, to): (&Path, &Path)| {
            if to.ends_with(DB_FILE) {
                return Err(std::io::Error::other("locked by AV"));
            }
            std::fs::rename(from, to)
        };
        let e = swap_in(&home, &mut staged, "t1", &fail_db).unwrap_err();
        assert!(e.to_string().contains("previous files restored"), "{e}");
        // secrets/config were swapped first and are back to the old contents
        for (n, v) in [
            (SECRETS_FILE, "old-secrets"),
            (CONFIG_FILE, "old-config"),
            (DB_FILE, "old-db"),
            ("siloforge.db-wal", "old-wal"),
        ] {
            assert_eq!(std::fs::read_to_string(home.join(n)).unwrap(), v, "{n}");
        }
        // staged temp files are back in place for cleanup; no pre-restore copies left
        assert!(staged.iter().all(|(_, t)| t.exists()));
        let pre: Vec<_> = std::fs::read_dir(&home)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("pre-restore"))
            .collect();
        assert!(pre.is_empty());
        // and a clean run swaps in secrets -> config -> db
        let ok = |(from, to): (&Path, &Path)| std::fs::rename(from, to);
        let (prev, restored) = swap_in(&home, &mut staged, "t2", &ok).unwrap();
        assert_eq!(restored, vec![SECRETS_FILE, CONFIG_FILE, DB_FILE]);
        assert_eq!(prev.len(), 3);
        assert_eq!(
            std::fs::read_to_string(home.join(DB_FILE)).unwrap(),
            format!("new-{DB_FILE}")
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tampered_and_newer_schema_refused() {
        let (cfg, db) = home_with_data().await;
        let b = create(&cfg, &db, BackupOptions::default()).await.unwrap();
        let mut bytes = std::fs::read(&b.path).unwrap();
        let mid = bytes.len() / 3;
        for x in bytes.iter_mut().skip(mid).take(64) {
            *x ^= 0x5a;
        }
        let bad = cfg.home.join("bad.zip");
        std::fs::write(&bad, &bytes).unwrap();
        assert!(verify(&bad, None).is_err());
        assert!(restore(&bad, &cfg.home.join("t2"), None, false).is_err());
        assert!(!cfg.home.join("t2").join(DB_FILE).exists());
        std::fs::write(&bad, b"not a zip").unwrap();
        assert!(verify(&bad, None).is_err());

        // a database claiming a future schema
        db.write(|t| {
            t.execute(
                "INSERT INTO schema_version(version,name,checksum) VALUES(9999,'future','x')",
                [],
            )
            .map_err(err)?;
            Ok(())
        })
        .await
        .unwrap();
        let fut = create(&cfg, &db, BackupOptions::default()).await.unwrap();
        assert_eq!(fut.schema_version, 9999);
        verify(&fut.path, None).unwrap();
        let e = restore(&fut.path, &cfg.home.join("t3"), None, false).unwrap_err();
        assert!(e.to_string().contains("newer"), "{e}");
        // config-only kind
        let c = create(
            &cfg,
            &db,
            BackupOptions {
                kind: BackupKind::Config,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let m = verify(&c.path, None).unwrap();
        assert!(!m.files.contains_key(DB_FILE) && m.files.contains_key(CONFIG_FILE));
        assert!(next_run("17 3 * * *").is_ok());
        assert!(next_run("nope").is_err());
        db.close();
        let _ = std::fs::remove_dir_all(&cfg.home);
    }

    #[test]
    fn restore_entry_size_caps() {
        assert!(check_entry_size(DB_FILE, 10_000, 10_000, 2_000).is_ok());
        assert!(check_entry_size(CONFIG_FILE, 300, 300, 200).is_ok());
        assert!(check_entry_size(SECRETS_FILE, 2 << 20, 2 << 20, 1 << 20).is_err());
        assert!(check_entry_size(DB_FILE, MAX_DB_FILE + 1, MAX_DB_FILE + 1, MAX_DB_FILE).is_err());
        assert!(check_entry_size(DB_FILE, 10_000, 9_999, 5_000).is_err());
        assert!(check_entry_size(DB_FILE, 1 << 30, 1 << 30, 1_000).is_err());
    }
}
