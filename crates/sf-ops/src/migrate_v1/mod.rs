//! v1 Postgres -> v2 SQLite importer.
//!
//! * Reads the LIVE v1 database without a host port: `docker exec siloforge_pg psql ... -c "COPY (SELECT ...)
//!   TO STDOUT WITH (FORMAT csv, HEADER, NULL '\N')"`, every session `SET default_transaction_read_only = on`
//!   ([`source::DockerSource`]). Nothing is ever written to v1.
//! * Or reads a directory of CSVs ([`source::DirSource`]) written by [`export_v1_to_dir`] on another PC.
//! * Mapping ([`tables::TABLES`]): uuids kept, timestamps -> RFC 3339 UTC, jsonb -> TEXT, booleans -> 0/1,
//!   pgvector text -> f32 LE BLOB, `posts.chunked_hash = content_hash` (unchanged posts are not re-chunked),
//!   `sites.settings.{wp_user, wp_app_password, gsc.credentials}` -> encrypted `sites.secrets_enc`,
//!   bcrypt hashes kept. v1 jobs still `queued`/`running` become `failed` (their BullMQ payload is gone).
//! * Rows go in batched transactions (`batch_size`, also capped at ~32 MB) via `Db::write_blocking`, so
//!   call these functions from a plain thread (CLI or `spawn_blocking`), never from async code.
//! * The posts <-> clusters FK cycle: clusters are inserted with `pillar_post_id = NULL`, then linked after
//!   posts. Inserts are `INSERT OR IGNORE` (re-runs are idempotent); rows violating a foreign key (e.g. a
//!   `--tables` subset without parents) are skipped and counted.

pub mod source;
pub mod tables;

use std::collections::HashSet;
use std::fmt;
use std::io::Read;
use std::path::Path;

use rusqlite::types::Value as SqlValue;
use serde::Serialize;
use sf_core::secrets::SiteSecrets;
use sf_core::types::{blob_to_vec, parse_pgvector, vec_to_blob};
use sf_core::{EMBED_DIM, Error, Result};
use sf_store::{Db, err};

pub use source::{DirSource, DockerSource, NULL_MARK, Source};
use tables::{Scope, TABLES, Table, Ty};

/// Max bytes buffered per batch before a flush (protects RAM on big `posts.content_html`).
const BATCH_BYTES: usize = 32 << 20;

#[derive(Clone)]
pub struct MigrateOptions {
    /// Only these tables (v1 names). None = all.
    pub tables: Option<Vec<String>>,
    /// Only this site (uuid). Global tables (users, model_*) are still imported in full.
    pub site: Option<String>,
    /// Count only, write nothing.
    pub dry_run: bool,
    /// Rows per transaction (default 5000).
    pub batch_size: usize,
    /// AES key for `sites.secrets_enc` (`sf_core::crypto::derive_secret_key(cfg.encryption_master())`).
    pub secret_key: [u8; 32],
}

impl MigrateOptions {
    pub fn new(secret_key: [u8; 32]) -> Self {
        Self {
            tables: None,
            site: None,
            dry_run: false,
            batch_size: 5000,
            secret_key,
        }
    }

    fn wants(&self, table: &str) -> bool {
        self.tables
            .as_ref()
            .is_none_or(|t| t.iter().any(|x| x == table))
    }

    fn validate(&self) -> Result<()> {
        if let Some(s) = &self.site
            && !is_uuid(s)
        {
            return Err(Error::bad("--site must be a site uuid"));
        }
        if let Some(t) = &self.tables {
            for name in t {
                if tables::find(name).is_none() {
                    return Err(Error::bad(format!("unknown table '{name}'")));
                }
            }
        }
        Ok(())
    }
}

/// Progress event (after each batch and at the end of each table).
#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub table: String,
    pub rows: u64,
    pub inserted: u64,
    pub done: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TableReport {
    pub table: String,
    /// Rows read from the source (after the site filter).
    pub source_rows: u64,
    pub inserted: u64,
    /// Already present / unique or CHECK conflict (`INSERT OR IGNORE`).
    pub ignored: u64,
    /// Rejected by a foreign key.
    pub skipped: u64,
    /// Vectors that were not valid 384-d pgvector text (stored as NULL, will be re-embedded).
    pub invalid_vectors: u64,
    /// Present in the source? (missing tables are skipped).
    pub present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MigrateReport {
    pub source: String,
    pub dry_run: bool,
    pub tables: Vec<TableReport>,
    pub pillars_linked: u64,
    pub secrets_moved: u64,
}

impl fmt::Display for MigrateReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "v1 migration from {}{}",
            self.source,
            if self.dry_run { " (dry run)" } else { "" }
        )?;
        writeln!(
            f,
            "{:<22} {:>10} {:>10} {:>8} {:>8} {:>8}",
            "table", "source", "inserted", "ignored", "skipped", "badvec"
        )?;
        for t in &self.tables {
            if !t.present {
                writeln!(f, "{:<22} (not in source)", t.table)?;
                continue;
            }
            writeln!(
                f,
                "{:<22} {:>10} {:>10} {:>8} {:>8} {:>8}{}",
                t.table,
                t.source_rows,
                t.inserted,
                t.ignored,
                t.skipped,
                t.invalid_vectors,
                t.first_error
                    .as_deref()
                    .map(|e| format!("  first error: {e}"))
                    .unwrap_or_default()
            )?;
        }
        write!(
            f,
            "cluster pillars linked: {}, sites with secrets moved: {}",
            self.pillars_linked, self.secrets_moved
        )
    }
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// SQL WHERE clause for the docker source (site id already validated as a uuid).
fn sql_filter(t: &Table, site: Option<&str>) -> Option<String> {
    let s = site?;
    match t.scope {
        Scope::Global => None,
        Scope::SelfId => Some(format!("id = '{s}'")),
        Scope::Site => Some(format!("site_id = '{s}'")),
        Scope::SiteOrNull => Some(format!("(site_id IS NULL OR site_id = '{s}')")),
        Scope::Post => Some(format!(
            "post_id IN (SELECT id FROM posts WHERE site_id = '{s}')"
        )),
    }
}

// ---------------------------------------------------------------------------------------------
// Value conversion.

/// Postgres timestamp text (`2026-10-04 12:34:56.123456+00`, `+05:30`, RFC 3339, or naive = UTC) ->
/// RFC 3339 UTC with milliseconds.
pub fn pg_ts_to_rfc3339(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let fmt =
        |d: chrono::DateTime<chrono::Utc>| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(fmt(d.to_utc()));
    }
    let mut t = s.replacen(' ', "T", 1);
    // "+00" / "-05" -> "+00:00"
    let b = t.as_bytes();
    let n = b.len();
    if n >= 3
        && t.contains('T')
        && matches!(b.get(n - 3), Some(b'+' | b'-'))
        && b.get(n - 2).is_some_and(u8::is_ascii_digit)
        && b.get(n - 1).is_some_and(u8::is_ascii_digit)
    {
        t.push_str(":00");
    }
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(&t) {
        return Some(fmt(d.to_utc()));
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(n) = chrono::NaiveDateTime::parse_from_str(&t, f) {
            return Some(fmt(n.and_utc()));
        }
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|n| fmt(n.and_utc()))
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "t" | "true" | "1" | "y" | "yes" | "on" => Some(true),
        "f" | "false" | "0" | "n" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Converts one CSV cell. `invalid_vec` is set when a non-empty vector could not be used.
pub fn convert(raw: &str, ty: Ty, invalid_vec: &mut bool) -> SqlValue {
    let is_null = raw == NULL_MARK || (raw.is_empty() && ty != Ty::Text);
    if is_null {
        return if ty.nullable() {
            SqlValue::Null
        } else {
            default_for(ty)
        };
    }
    match ty {
        Ty::Text | Ty::NText => SqlValue::Text(raw.to_string()),
        Ty::Int | Ty::NInt => raw
            .trim()
            .parse::<i64>()
            .ok()
            .or_else(|| {
                raw.trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite())
                    .map(|f| f as i64)
            })
            .map(SqlValue::Integer)
            .unwrap_or_else(|| fallback(ty)),
        Ty::Real | Ty::NReal => raw
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(SqlValue::Real)
            .unwrap_or_else(|| fallback(ty)),
        Ty::Bool | Ty::NBool => parse_bool(raw)
            .map(|b| SqlValue::Integer(i64::from(b)))
            .unwrap_or_else(|| fallback(ty)),
        Ty::JsonObj | Ty::JsonArr => {
            if serde_json::from_str::<serde_json::Value>(raw).is_ok() {
                SqlValue::Text(raw.to_string())
            } else {
                default_for(ty)
            }
        }
        Ty::Ts | Ty::NTs => pg_ts_to_rfc3339(raw)
            .map(SqlValue::Text)
            .unwrap_or_else(|| SqlValue::Text(raw.to_string())),
        Ty::Vector => match parse_pgvector(raw) {
            Some(v) if v.len() == EMBED_DIM && v.iter().all(|x| x.is_finite()) => {
                SqlValue::Blob(vec_to_blob(&v))
            }
            _ => {
                *invalid_vec = true;
                SqlValue::Null
            }
        },
    }
}

fn fallback(ty: Ty) -> SqlValue {
    if ty.nullable() {
        SqlValue::Null
    } else {
        default_for(ty)
    }
}

fn default_for(ty: Ty) -> SqlValue {
    match ty {
        Ty::Text | Ty::NText => SqlValue::Text(String::new()),
        Ty::Int | Ty::NInt | Ty::Bool | Ty::NBool => SqlValue::Integer(0),
        Ty::Real | Ty::NReal => SqlValue::Real(0.0),
        Ty::JsonObj => SqlValue::Text("{}".into()),
        Ty::JsonArr => SqlValue::Text("[]".into()),
        Ty::Ts | Ty::NTs => SqlValue::Text(sf_core::now_rfc3339()),
        Ty::Vector => SqlValue::Null,
    }
}

/// Maps a source header onto the table spec: (csv index, column name, type) for shared columns.
fn plan_columns(t: &Table, header: &[String]) -> Vec<(usize, &'static str, Ty)> {
    t.cols
        .iter()
        .filter_map(|(name, ty)| {
            header
                .iter()
                .position(|h| h == name)
                .map(|i| (i, *name, *ty))
        })
        .collect()
}

/// Per-table row transformer (special cases live here).
struct RowMapper<'a> {
    table: &'a Table,
    cols: Vec<(usize, &'static str, Ty)>,
    key: [u8; 32],
    /// Output column names (spec columns + derived ones).
    out_cols: Vec<&'static str>,
}

fn idx_of(cols: &[(usize, &'static str, Ty)], name: &str) -> Option<usize> {
    cols.iter().position(|(_, n, _)| *n == name)
}

/// Output of mapping one row.
struct Mapped {
    values: Vec<SqlValue>,
    invalid_vec: bool,
    secrets_moved: bool,
    pillar: Option<(String, String)>,
}

impl<'a> RowMapper<'a> {
    fn new(table: &'a Table, header: &[String], key: [u8; 32]) -> Self {
        let cols = plan_columns(table, header);
        let mut out_cols: Vec<&'static str> = cols.iter().map(|c| c.1).collect();
        match table.name {
            "sites" if idx_of(&cols, "settings").is_some() => out_cols.push("secrets_enc"),
            "posts" if idx_of(&cols, "content_hash").is_some() => out_cols.push("chunked_hash"),
            _ => {}
        }
        Self {
            table,
            cols,
            key,
            out_cols,
        }
    }

    fn cell<'r>(&self, rec: &'r csv::ByteRecord, i: usize) -> std::borrow::Cow<'r, str> {
        String::from_utf8_lossy(rec.get(i).unwrap_or(b""))
    }

    fn map(&self, rec: &csv::ByteRecord) -> Result<Mapped> {
        let mut invalid_vec = false;
        let mut values: Vec<SqlValue> = self
            .cols
            .iter()
            .map(|(i, _, ty)| convert(&self.cell(rec, *i), *ty, &mut invalid_vec))
            .collect();
        let mut secrets_moved = false;
        let mut pillar = None;
        let pos = |n: &str| idx_of(&self.cols, n);
        match self.table.name {
            "sites" => {
                if let Some(si) = pos("settings") {
                    let mut settings: serde_json::Value = match values.get(si) {
                        Some(SqlValue::Text(s)) => serde_json::from_str(s).unwrap_or_default(),
                        _ => serde_json::Value::Object(Default::default()),
                    };
                    if !settings.is_object() {
                        settings = serde_json::Value::Object(Default::default());
                    }
                    let mut sec = SiteSecrets::default();
                    secrets_moved = sec.absorb_from_settings(&mut settings);
                    if let Some(v) = values.get_mut(si) {
                        *v = SqlValue::Text(settings.to_string());
                    }
                    values.push(SqlValue::Text(sec.seal(&self.key)?));
                }
            }
            "posts" => {
                if let Some(ci) = pos("content_hash") {
                    values.push(values.get(ci).cloned().unwrap_or(SqlValue::Null));
                }
            }
            "clusters" => {
                if let (Some(pi), Some(ii)) = (pos("pillar_post_id"), pos("id"))
                    && let (Some(SqlValue::Text(pid)), Some(SqlValue::Text(id))) =
                        (values.get(pi).cloned(), values.get(ii).cloned())
                {
                    pillar = Some((id, pid));
                    if let Some(v) = values.get_mut(pi) {
                        *v = SqlValue::Null;
                    }
                }
            }
            "job_runs" => {
                if let Some(si) = pos("status")
                    && matches!(values.get(si), Some(SqlValue::Text(s)) if s == "queued" || s == "running")
                {
                    if let Some(v) = values.get_mut(si) {
                        *v = SqlValue::Text("failed".into());
                    }
                    if let Some(ei) = pos("error") {
                        let prev = match values.get(ei) {
                            Some(SqlValue::Text(e)) if !e.is_empty() => format!("{e}; "),
                            _ => String::new(),
                        };
                        if let Some(v) = values.get_mut(ei) {
                            *v = SqlValue::Text(format!("{prev}unfinished in v1 at migration"));
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(Mapped {
            values,
            invalid_vec,
            secrets_moved,
            pillar,
        })
    }

    /// Value of a spec column in a mapped row (for the site filter).
    fn text_of<'v>(&self, m: &'v Mapped, name: &str) -> Option<&'v str> {
        let i = idx_of(&self.cols, name)?;
        match m.values.get(i) {
            Some(SqlValue::Text(s)) => Some(s.as_str()),
            _ => None,
        }
    }
}

/// Rust-side site filter (always applied; the docker source also filters in SQL).
fn keep_row(
    mapper: &RowMapper<'_>,
    m: &Mapped,
    site: Option<&str>,
    posts: &HashSet<String>,
) -> bool {
    let Some(site) = site else { return true };
    match mapper.table.scope {
        Scope::Global => true,
        Scope::SelfId => mapper.text_of(m, "id") == Some(site),
        Scope::Site => mapper.text_of(m, "site_id") == Some(site),
        Scope::SiteOrNull => mapper.text_of(m, "site_id").is_none_or(|s| s == site),
        Scope::Post => mapper
            .text_of(m, "post_id")
            .is_some_and(|p| posts.contains(p)),
    }
}

#[derive(Default)]
struct BatchResult {
    inserted: u64,
    ignored: u64,
    skipped: u64,
    first_error: Option<String>,
}

fn write_batch(
    db: &Db,
    table: &'static str,
    cols: &[&'static str],
    rows: Vec<Vec<SqlValue>>,
) -> Result<BatchResult> {
    let sql = format!(
        "INSERT OR IGNORE INTO {table}({}) VALUES({})",
        cols.join(","),
        sf_store::placeholders(cols.len())
    );
    db.write_blocking(move |t| {
        let mut stmt = t.prepare_cached(&sql).map_err(err)?;
        let mut r = BatchResult::default();
        for row in rows {
            match stmt.execute(rusqlite::params_from_iter(row.iter())) {
                Ok(1) => r.inserted += 1,
                Ok(_) => r.ignored += 1,
                Err(rusqlite::Error::SqliteFailure(f, msg))
                    if f.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    r.skipped += 1;
                    if r.first_error.is_none() {
                        r.first_error = Some(msg.unwrap_or_else(|| "constraint".into()));
                    }
                }
                Err(e) => return Err(err(e)),
            }
        }
        Ok(r)
    })
}

fn csv_reader(r: Box<dyn Read + Send>) -> csv::Reader<Box<dyn Read + Send>> {
    csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .buffer_capacity(1 << 20)
        .from_reader(r)
}

fn site_posts_in_db(db: &Db, site: &str) -> Result<HashSet<String>> {
    let site = site.to_string();
    db.read_blocking(move |c| {
        let mut stmt = c
            .prepare("SELECT id FROM posts WHERE site_id=?1")
            .map_err(err)?;
        let ids = stmt
            .query_map([&site], |r| r.get::<_, String>(0))
            .map_err(err)?
            .collect::<rusqlite::Result<HashSet<_>>>()
            .map_err(err)?;
        Ok(ids)
    })
}

/// Imports v1 data from `src` into `db`. Blocking; call from a plain thread.
pub fn import(
    src: &dyn Source,
    db: &Db,
    opts: &MigrateOptions,
    progress: &(dyn Fn(&Progress) + Sync),
) -> Result<MigrateReport> {
    opts.validate()?;
    let site = opts.site.as_deref();
    let batch = opts.batch_size.clamp(1, 100_000);
    let mut report = MigrateReport {
        source: src.describe(),
        dry_run: opts.dry_run,
        ..Default::default()
    };
    let mut posts: HashSet<String> = HashSet::new();
    let mut pillars: Vec<(String, String)> = Vec::new();
    for t in TABLES.iter().filter(|t| opts.wants(t.name)) {
        let mut tr = TableReport {
            table: t.name.to_string(),
            ..Default::default()
        };
        let Some(header) = src.columns(t.name)? else {
            tracing::warn!(table = t.name, "table not in v1 source; skipped");
            report.tables.push(tr);
            continue;
        };
        tr.present = true;
        let filter = sql_filter(t, site);
        if opts.dry_run
            && let Some(n) = src.count(t.name, filter.as_deref())?
        {
            tr.source_rows = n;
            progress(&Progress {
                table: t.name.into(),
                rows: n,
                inserted: 0,
                done: true,
            });
            report.tables.push(tr);
            continue;
        }
        if t.scope == Scope::Post
            && let Some(s) = site
            && !opts.dry_run
        {
            posts.extend(site_posts_in_db(db, s)?);
        }
        let shared: Vec<String> = t
            .cols
            .iter()
            .filter(|(n, _)| header.iter().any(|h| h == n))
            .map(|(n, _)| n.to_string())
            .collect();
        if shared.is_empty() {
            report.tables.push(tr);
            continue;
        }
        let mut rdr = csv_reader(src.open(t.name, &shared, filter.as_deref())?);
        let hdr: Vec<String> = rdr
            .byte_headers()
            .map_err(|e| Error::bad(format!("{}: bad CSV header: {e}", t.name)))?
            .iter()
            .map(|c| String::from_utf8_lossy(c).trim().to_string())
            .collect();
        let mapper = RowMapper::new(t, &hdr, opts.secret_key);
        let out_cols = mapper.out_cols.clone();
        let mut buf: Vec<Vec<SqlValue>> = Vec::with_capacity(batch.min(10_000));
        let mut buf_bytes = 0usize;
        let mut rec = csv::ByteRecord::new();
        let flush = |buf: &mut Vec<Vec<SqlValue>>, tr: &mut TableReport| -> Result<()> {
            if buf.is_empty() {
                return Ok(());
            }
            let rows = std::mem::take(buf);
            let r = write_batch(db, t.name, &out_cols, rows)?;
            tr.inserted += r.inserted;
            tr.ignored += r.ignored;
            tr.skipped += r.skipped;
            if tr.first_error.is_none() {
                tr.first_error = r.first_error;
            }
            Ok(())
        };
        loop {
            match rdr.read_byte_record(&mut rec) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    return Err(Error::bad(format!(
                        "{}: CSV read failed after {} rows: {e}",
                        t.name, tr.source_rows
                    )));
                }
            }
            let m = mapper.map(&rec)?;
            if !keep_row(&mapper, &m, site, &posts) {
                continue;
            }
            tr.source_rows += 1;
            if m.invalid_vec {
                tr.invalid_vectors += 1;
            }
            if m.secrets_moved {
                report.secrets_moved += 1;
            }
            if t.name == "posts"
                && let Some(id) = mapper.text_of(&m, "id")
            {
                posts.insert(id.to_string());
            }
            if let Some(p) = m.pillar {
                pillars.push(p);
            }
            if opts.dry_run {
                continue;
            }
            buf_bytes += rec.as_slice().len();
            buf.push(m.values);
            if buf.len() >= batch || buf_bytes >= BATCH_BYTES {
                flush(&mut buf, &mut tr)?;
                buf_bytes = 0;
                progress(&Progress {
                    table: t.name.into(),
                    rows: tr.source_rows,
                    inserted: tr.inserted,
                    done: false,
                });
            }
        }
        if !opts.dry_run {
            flush(&mut buf, &mut tr)?;
        }
        progress(&Progress {
            table: t.name.into(),
            rows: tr.source_rows,
            inserted: tr.inserted,
            done: true,
        });
        tracing::info!(
            table = t.name,
            rows = tr.source_rows,
            inserted = tr.inserted,
            "v1 table imported"
        );
        report.tables.push(tr);
    }
    if !opts.dry_run && !pillars.is_empty() {
        report.pillars_linked = db.write_blocking(move |t| {
            let mut stmt = t
                .prepare(
                    "UPDATE clusters SET pillar_post_id=?2 WHERE id=?1 AND EXISTS(SELECT 1 FROM posts WHERE id=?2)",
                )
                .map_err(err)?;
            let mut n = 0u64;
            for (id, pid) in &pillars {
                n += stmt.execute([id, pid]).map_err(err)? as u64;
            }
            Ok(n)
        })?;
    }
    Ok(report)
}

/// Imports from the live container with default names (`siloforge_pg`, user/db `siloforge`).
pub fn import_from_docker(
    db: &Db,
    opts: &MigrateOptions,
    progress: &(dyn Fn(&Progress) + Sync),
) -> Result<MigrateReport> {
    let src = DockerSource::default();
    src.check()?;
    import(&src, db, opts, progress)
}

/// Imports from a directory written by [`export_v1_to_dir`].
pub fn import_from_dir(
    dir: &Path,
    db: &Db,
    opts: &MigrateOptions,
    progress: &(dyn Fn(&Progress) + Sync),
) -> Result<MigrateReport> {
    if !dir.is_dir() {
        return Err(Error::not_found(format!(
            "directory not found: {}",
            dir.display()
        )));
    }
    import(&DirSource::new(dir), db, opts, progress)
}

/// Exports every known v1 table to `<dir>/<table>.csv` (header, `\N` = NULL) for offline migration.
/// Returns (table, bytes written). Read-only on v1.
pub fn export_v1_to_dir(
    src: &DockerSource,
    dir: &Path,
    site: Option<&str>,
) -> Result<Vec<(String, u64)>> {
    if let Some(s) = site
        && !is_uuid(s)
    {
        return Err(Error::bad("--site must be a site uuid"));
    }
    std::fs::create_dir_all(dir)?;
    let mut out = Vec::new();
    for t in TABLES {
        let Some(header) = src.columns(t.name)? else {
            continue;
        };
        let shared: Vec<String> = t
            .cols
            .iter()
            .filter(|(n, _)| header.iter().any(|h| h == n))
            .map(|(n, _)| n.to_string())
            .collect();
        if shared.is_empty() {
            continue;
        }
        let filter = sql_filter(t, site);
        let part = dir.join(format!("{}.csv.part", t.name));
        let mut r = src.open(t.name, &shared, filter.as_deref())?;
        let mut w = std::io::BufWriter::new(std::fs::File::create(&part)?);
        let n = std::io::copy(&mut r, &mut w)?;
        std::io::Write::flush(&mut w)?;
        drop(w);
        std::fs::rename(&part, dir.join(format!("{}.csv", t.name)))?;
        tracing::info!(table = t.name, bytes = n, "exported");
        out.push((t.name.to_string(), n));
    }
    let manifest = serde_json::json!({
        "format": "siloforge-v1-csv",
        "exported_at": sf_core::now_rfc3339(),
        "site": site,
        "null_marker": NULL_MARK,
        "tables": out.iter().map(|(t, b)| serde_json::json!({"table": t, "bytes": b})).collect::<Vec<_>>(),
    });
    std::fs::write(
        dir.join("_export.json"),
        serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
    )?;
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Verification.

#[derive(Debug, Clone, Serialize)]
pub struct CountCheck {
    pub table: String,
    pub source: u64,
    pub target: u64,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    pub counts: Vec<CountCheck>,
    pub sampled_embeddings: usize,
    pub missing_in_source: usize,
    pub min_cosine: f64,
    pub embeddings_ok: bool,
    pub ok: bool,
}

impl fmt::Display for VerifyReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{:<22} {:>10} {:>10}  status", "table", "v1", "v2")?;
        for c in &self.counts {
            writeln!(
                f,
                "{:<22} {:>10} {:>10}  {}",
                c.table,
                c.source,
                c.target,
                if c.ok { "ok" } else { "MISMATCH" }
            )?;
        }
        writeln!(
            f,
            "embeddings: {} sampled, {} missing in v1, min cosine {:.7} -> {}",
            self.sampled_embeddings,
            self.missing_in_source,
            self.min_cosine,
            if self.embeddings_ok { "ok" } else { "FAIL" }
        )?;
        write!(f, "overall: {}", if self.ok { "PASS" } else { "FAIL" })
    }
}

fn target_count(db: &Db, t: &Table, site: Option<&str>) -> Result<u64> {
    let (wh, bind): (String, Option<String>) = match (site, t.scope) {
        (None, _) | (_, Scope::Global) => (String::new(), None),
        (Some(s), Scope::SelfId) => (" WHERE id=?1".into(), Some(s.into())),
        (Some(s), Scope::Site) => (" WHERE site_id=?1".into(), Some(s.into())),
        (Some(s), Scope::SiteOrNull) => (
            " WHERE site_id IS NULL OR site_id=?1".into(),
            Some(s.into()),
        ),
        (Some(s), Scope::Post) => (
            " WHERE post_id IN (SELECT id FROM posts WHERE site_id=?1)".into(),
            Some(s.into()),
        ),
    };
    let sql = format!("SELECT count(*) FROM {}{wh}", t.name);
    db.read_blocking(move |c| {
        let n: i64 = match &bind {
            Some(b) => c.query_row(&sql, [b], |r| r.get(0)),
            None => c.query_row(&sql, [], |r| r.get(0)),
        }
        .map_err(err)?;
        Ok(n.max(0) as u64)
    })
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        dot += f64::from(*x) * f64::from(*y);
        na += f64::from(*x) * f64::from(*x);
        nb += f64::from(*y) * f64::from(*y);
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Compares per-table row counts between source and target (honouring `opts.tables` / `opts.site`)
/// and checks `sample` random paragraph embeddings (cosine must be >= 0.999999). Blocking.
pub fn verify(
    src: &dyn Source,
    db: &Db,
    opts: &MigrateOptions,
    sample: usize,
) -> Result<VerifyReport> {
    opts.validate()?;
    let site = opts.site.as_deref();
    let mut counts = Vec::new();
    for t in TABLES.iter().filter(|t| opts.wants(t.name)) {
        let Some(_) = src.columns(t.name)? else {
            continue;
        };
        let filter = sql_filter(t, site);
        let source = match src.count(t.name, filter.as_deref())? {
            Some(n) => n,
            None => {
                // stream and apply the Rust-side filter.
                // Post-scoped tables need the site's posts: count against the target's post set.
                let posts = match (site, t.scope) {
                    (Some(s), Scope::Post) => site_posts_in_db(db, s)?,
                    _ => HashSet::new(),
                };
                count_stream(src, t, site, &posts, opts.secret_key)?
            }
        };
        let target = target_count(db, t, site)?;
        counts.push(CountCheck {
            table: t.name.to_string(),
            source,
            target,
            ok: source == target,
        });
    }
    // embeddings sample
    let sample = sample.clamp(0, 1000) as i64;
    let site_s = site.map(str::to_string);
    let picked: Vec<(String, Vec<u8>)> = db.read_blocking(move |c| {
        let mut sql =
            String::from("SELECT id, embedding FROM paragraphs WHERE embedding IS NOT NULL");
        if site_s.is_some() {
            sql.push_str(" AND post_id IN (SELECT id FROM posts WHERE site_id=?2)");
        }
        sql.push_str(" ORDER BY random() LIMIT ?1");
        let mut stmt = c.prepare(&sql).map_err(err)?;
        let map = |r: &rusqlite::Row<'_>| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?));
        let rows = match &site_s {
            Some(s) => stmt.query_map(rusqlite::params![sample, s], map),
            None => stmt.query_map(rusqlite::params![sample], map),
        }
        .map_err(err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(err)?;
        Ok(rows)
    })?;
    let mut min_cos = 1.0f64;
    let mut found = 0usize;
    if !picked.is_empty() && src.columns("paragraphs")?.is_some() {
        let ids: Vec<&str> = picked
            .iter()
            .map(|(id, _)| id.as_str())
            .filter(|id| is_uuid(id))
            .collect();
        let filter = (!ids.is_empty()).then(|| {
            format!(
                "id IN ({})",
                ids.iter()
                    .map(|i| format!("'{i}'"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        });
        let want: std::collections::HashMap<&str, &Vec<u8>> =
            picked.iter().map(|(i, b)| (i.as_str(), b)).collect();
        let mut rdr = csv_reader(src.open(
            "paragraphs",
            &["id".to_string(), "embedding".to_string()],
            filter.as_deref(),
        )?);
        let hdr: Vec<String> = rdr
            .byte_headers()
            .map_err(|e| Error::bad(format!("paragraphs header: {e}")))?
            .iter()
            .map(|c| String::from_utf8_lossy(c).trim().to_string())
            .collect();
        let (Some(ii), Some(ei)) = (
            hdr.iter().position(|h| h == "id"),
            hdr.iter().position(|h| h == "embedding"),
        ) else {
            return Err(Error::bad("paragraphs source lacks id/embedding"));
        };
        let mut rec = csv::ByteRecord::new();
        while rdr
            .read_byte_record(&mut rec)
            .map_err(|e| Error::bad(format!("paragraphs csv: {e}")))?
        {
            let id = String::from_utf8_lossy(rec.get(ii).unwrap_or(b""));
            let Some(blob) = want.get(id.as_ref()) else {
                continue;
            };
            let src_vec = parse_pgvector(&String::from_utf8_lossy(rec.get(ei).unwrap_or(b"")));
            let dst_vec = blob_to_vec(blob);
            let c = match (src_vec, dst_vec) {
                (Some(a), Some(b)) => cosine(&a, &b),
                _ => 0.0,
            };
            min_cos = min_cos.min(c);
            found += 1;
        }
    }
    let missing = picked.len().saturating_sub(found);
    let embeddings_ok = missing == 0 && min_cos >= 0.999_999;
    let ok = embeddings_ok && counts.iter().all(|c| c.ok);
    Ok(VerifyReport {
        counts,
        sampled_embeddings: picked.len(),
        missing_in_source: missing,
        min_cosine: if picked.is_empty() { 1.0 } else { min_cos },
        embeddings_ok,
        ok,
    })
}

fn count_stream(
    src: &dyn Source,
    t: &Table,
    site: Option<&str>,
    posts: &HashSet<String>,
    key: [u8; 32],
) -> Result<u64> {
    let header = src.columns(t.name)?.unwrap_or_default();
    let shared: Vec<String> = t
        .cols
        .iter()
        .filter(|(n, _)| header.iter().any(|h| h == n))
        .map(|(n, _)| n.to_string())
        .collect();
    let mut rdr = csv_reader(src.open(t.name, &shared, sql_filter(t, site).as_deref())?);
    let hdr: Vec<String> = rdr
        .byte_headers()
        .map_err(|e| Error::bad(format!("{}: {e}", t.name)))?
        .iter()
        .map(|c| String::from_utf8_lossy(c).trim().to_string())
        .collect();
    let mapper = RowMapper::new(t, &hdr, key);
    let mut rec = csv::ByteRecord::new();
    let mut n = 0u64;
    while rdr
        .read_byte_record(&mut rec)
        .map_err(|e| Error::bad(format!("{}: {e}", t.name)))?
    {
        if site.is_none() {
            n += 1;
            continue;
        }
        let m = mapper.map(&rec)?;
        if keep_row(&mapper, &m, site, posts) {
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests;
