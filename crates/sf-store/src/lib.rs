//! SQLite storage for SiloForge.
//!
//! * One **writer thread** owns the only write connection; every write closure runs inside an
//!   `IMMEDIATE` transaction, so writes are serialised without `SQLITE_BUSY` storms.
//! * A small **reader pool** (WAL mode lets readers run concurrently with the writer).
//! * Both are used from async code through `read`/`write`, which never block the tokio runtime.
//!
//! Query helpers live in the sub-modules; they take `&Connection` / `&Transaction` so they can be
//! composed inside one transaction.

pub mod migrate;
pub mod util;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;

use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use sf_core::{Error, Result};
use tokio::sync::{Semaphore, oneshot};

pub use rusqlite;
pub use util::*;

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

/// Cheap to clone handle to the database.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    readers: Mutex<Vec<Connection>>,
    reader_slots: Semaphore,
    writer: Mutex<Option<mpsc::Sender<WriteJob>>>,
}

const READERS: usize = 6;

fn configure(conn: &Connection, writer: bool) -> rusqlite::Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -32_768)?; // 32 MiB per connection
    conn.pragma_update(None, "mmap_size", 268_435_456_i64)?;
    if writer {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "wal_autocheckpoint", 2000)?;
    }
    Ok(())
}

fn db_err(e: rusqlite::Error) -> Error {
    match &e {
        rusqlite::Error::SqliteFailure(f, msg)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            let m = msg.clone().unwrap_or_default();
            if m.contains("UNIQUE") {
                Error::Conflict(format!("already exists ({m})"))
            } else {
                Error::BadRequest(format!("constraint failed ({m})"))
            }
        }
        rusqlite::Error::QueryReturnedNoRows => Error::NotFound("not found".into()),
        _ => Error::Db(e.to_string()),
    }
}

/// Converts rusqlite errors (use with `?` via `.map_err(sf_store::err)`).
pub fn err(e: rusqlite::Error) -> Error {
    db_err(e)
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".into())
}

/// Runs `f` and turns a panic into an error (R7: a panicking DB closure must not kill the writer
/// thread or leak a pooled reader connection).
fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => {
            let msg = panic_message(p.as_ref());
            tracing::error!(%msg, "database {what} closure panicked (rolled back)");
            Err(Error::internal(format!("database {what} panicked: {msg}")))
        }
    }
}

/// One IMMEDIATE transaction; commits on Ok, rolls back on Err or panic.
fn run_tx<T>(conn: &mut Connection, f: impl FnOnce(&Transaction) -> Result<T>) -> Result<T> {
    guarded("write", || {
        let t = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_err)?;
        let out = f(&t)?;
        t.commit().map_err(db_err)?;
        Ok(out)
    })
}

/// Returns a pooled reader to the pool when dropped, also while unwinding.
struct ReaderLease<'a> {
    pool: &'a Mutex<Vec<Connection>>,
    conn: Option<Connection>,
}

impl ReaderLease<'_> {
    fn conn(&self) -> Result<&Connection> {
        self.conn
            .as_ref()
            .ok_or_else(|| Error::internal("reader lease empty"))
    }
}

impl Drop for ReaderLease<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            self.pool.lock().push(c);
        }
    }
}

fn open_reader(path: &Path) -> Result<Connection> {
    let c = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(db_err)?;
    configure(&c, false).map_err(db_err)?;
    Ok(c)
}

impl Db {
    /// Opens (creating if needed) the database and applies pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut wconn = Connection::open(path).map_err(db_err)?;
        configure(&wconn, true).map_err(db_err)?;
        migrate::run(&mut wconn)?;

        let mut readers = Vec::with_capacity(READERS);
        for _ in 0..READERS {
            readers.push(open_reader(path)?);
        }

        let (tx, rx) = mpsc::channel::<WriteJob>();
        std::thread::Builder::new()
            .name("sf-db-writer".into())
            .spawn(move || {
                let mut conn = wconn;
                while let Ok(job) = rx.recv() {
                    // Backstop: write closures are already guarded; anything else that unwinds
                    // (with_writer jobs) must not end the thread (R7).
                    let res =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut conn)));
                    if let Err(p) = res {
                        tracing::error!(msg = %panic_message(p.as_ref()), "db writer job panicked; writer keeps running");
                        if !conn.is_autocommit() {
                            let _ = conn.execute_batch("ROLLBACK");
                        }
                    }
                }
                // Channel closed: final checkpoint so the -wal file is folded back.
                let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
            })
            .map_err(|e| Error::internal(format!("spawn writer: {e}")))?;

        Ok(Self {
            inner: Arc::new(Inner {
                path: path.to_path_buf(),
                readers: Mutex::new(readers),
                reader_slots: Semaphore::new(READERS),
                writer: Mutex::new(Some(tx)),
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Runs a read-only closure on a pooled connection (on the blocking pool).
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let _permit = self
            .inner
            .reader_slots
            .acquire()
            .await
            .map_err(|_| Error::internal("db closed"))?;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let conn = inner.readers.lock().pop();
            let Some(conn) = conn else {
                return Err(Error::internal("reader pool empty"));
            };
            let lease = ReaderLease {
                pool: &inner.readers,
                conn: Some(conn),
            };
            guarded("read", || f(lease.conn()?))
        })
        .await
        .map_err(|e| Error::internal(format!("read task: {e}")))?
    }

    /// Runs a closure inside one IMMEDIATE transaction on the writer thread. Commits on Ok, rolls
    /// back on Err; a panic in `f` rolls back and is returned as an error (the writer keeps running).
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction) -> Result<T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let job: WriteJob = Box::new(move |conn: &mut Connection| {
            let _ = tx.send(run_tx(conn, f));
        });
        self.send_job(job)?;
        rx.await.map_err(|_| Error::internal("writer dropped"))?
    }

    /// Runs a closure with the raw writer connection (no transaction): VACUUM, backup, checkpoints.
    pub async fn with_writer<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let job: WriteJob = Box::new(move |conn: &mut Connection| {
            let res = guarded("writer", || f(conn));
            if res.is_err() && !conn.is_autocommit() {
                let _ = conn.execute_batch("ROLLBACK");
            }
            let _ = tx.send(res);
        });
        self.send_job(job)?;
        rx.await.map_err(|_| Error::internal("writer dropped"))?
    }

    /// Blocking variant of `write` for sync contexts (CLI, migrator threads). Never call from async code.
    pub fn write_blocking<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction) -> Result<T> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let job: WriteJob = Box::new(move |conn: &mut Connection| {
            let _ = tx.send(run_tx(conn, f));
        });
        self.send_job(job)?;
        rx.recv().map_err(|_| Error::internal("writer dropped"))?
    }

    /// Blocking read for sync contexts. Never call from async code. Uses a pooled reader when one
    /// is free, else a short-lived read-only connection (never "reader pool empty" under load).
    pub fn read_blocking<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        if let Ok(_permit) = self.inner.reader_slots.try_acquire() {
            let conn = self.inner.readers.lock().pop();
            if let Some(conn) = conn {
                let lease = ReaderLease {
                    pool: &self.inner.readers,
                    conn: Some(conn),
                };
                return guarded("read", || f(lease.conn()?));
            }
        }
        let conn = open_reader(&self.inner.path)?;
        guarded("read", || f(&conn))
    }

    fn send_job(&self, job: WriteJob) -> Result<()> {
        let guard = self.inner.writer.lock();
        let Some(sender) = guard.as_ref() else {
            return Err(Error::internal("db closed"));
        };
        sender
            .send(job)
            .map_err(|_| Error::internal("writer thread gone"))
    }

    /// Stops accepting writes; the writer thread checkpoints and exits after draining queued jobs.
    pub fn close(&self) {
        self.inner.writer.lock().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db() -> (Db, PathBuf) {
        let dir = std::env::temp_dir().join(format!("sf-store-test-{}", sf_core::new_id()));
        let db = Db::open(&dir.join("t.db")).unwrap();
        (db, dir)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_migrate_write_read() {
        let (db, dir) = tmp_db();
        let id = sf_core::new_id();
        let id2 = id.clone();
        db.write(move |t| {
            t.execute(
                "INSERT INTO users(id,email,password_hash,role) VALUES(?1,'a@b.c','x','admin')",
                [&id2],
            )
            .map_err(err)?;
            Ok(())
        })
        .await
        .unwrap();
        let n: i64 = db
            .read(|c| {
                c.query_row("SELECT count(*) FROM users", [], |r| r.get(0))
                    .map_err(err)
            })
            .await
            .unwrap();
        assert_eq!(n, 1);
        // unique violation maps to Conflict and rolls back
        let dup = db
            .write(move |t| {
                t.execute(
                    "INSERT INTO users(id,email,password_hash) VALUES(?1,'a@b.c','x')",
                    [sf_core::new_id()],
                )
                .map_err(err)?;
                Ok(())
            })
            .await;
        assert!(matches!(dup, Err(Error::Conflict(_))));
        // reopen: migrations are idempotent
        db.close();
        drop(db);
        let db2 = Db::open(&dir.join("t.db")).unwrap();
        let v = db2.read(migrate::current_version).await.unwrap();
        assert!(v >= 1);
        db2.close();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// R7: panics in DB closures become errors; the writer thread and the reader pool survive.
    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::panic)]
    async fn panicking_closures_do_not_kill_writer_or_readers() {
        let (db, dir) = tmp_db();
        let r = db
            .write(|t| -> Result<()> {
                t.execute("INSERT INTO kv(key,value) VALUES('half','written')", [])
                    .map_err(err)?;
                panic!("boom in write");
            })
            .await;
        assert!(
            matches!(&r, Err(e) if e.to_string().contains("boom in write")),
            "{r:?}"
        );
        let r = db
            .with_writer(|c| -> Result<()> {
                c.execute_batch("BEGIN").map_err(err)?;
                panic!("boom in with_writer");
            })
            .await;
        assert!(r.is_err());
        for _ in 0..(READERS * 2) {
            let r = db.read(|_| -> Result<()> { panic!("boom in read") }).await;
            assert!(r.is_err());
        }
        // writes still work and the half-written row was rolled back
        db.write(|t| {
            t.execute("INSERT INTO kv(key,value) VALUES('after','ok')", [])
                .map_err(err)
        })
        .await
        .unwrap();
        let keys: Vec<String> = db
            .read(|c| {
                let mut st = c.prepare("SELECT key FROM kv ORDER BY key").map_err(err)?;
                let v = st
                    .query_map([], |r| r.get(0))
                    .map_err(err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(err)?;
                Ok(v)
            })
            .await
            .unwrap();
        assert_eq!(keys, vec!["after".to_string()]);
        // every reader went back to the pool; read_blocking works too
        assert_eq!(db.inner.readers.lock().len(), READERS);
        let n: i64 = db
            .read_blocking(|c| {
                c.query_row("SELECT count(*) FROM kv", [], |r| r.get(0))
                    .map_err(err)
            })
            .unwrap();
        assert_eq!(n, 1);
        db.close();
        let _ = std::fs::remove_dir_all(dir);
    }
}
