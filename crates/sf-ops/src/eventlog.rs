//! Unified event log (contract §9): one append-only `event_log` table for API requests, MCP
//! calls, job lifecycle, auth, notifications and system events.
//!
//! * [`EventLog::record`] never blocks and never fails: it pushes onto a bounded queue; when the
//!   queue is full the event is dropped and counted ([`EventLog::dropped`]).
//! * A supervised writer task drains the queue and inserts batches in **one** `Db::write`
//!   transaction every second (or as soon as [`EventLogOptions::batch_max`] rows are waiting).
//! * [`EventLog::flush`] waits until everything recorded so far is written (call on shutdown).
//! * [`EventLog::disabled`] is a no-op handle for tests and CLI commands.
//! * `detail` goes through [`scrub`]: secret-looking keys are masked, strings/arrays/depth are
//!   capped and the whole JSON is size-capped. Callers must never put request bodies in it.
//!
//! Wiring (`siloforge serve`): `let log = EventLog::start(db.clone(), Default::default());`
//! then `state.services.insert(Arc::new(log.clone()))`, `ctx.events = Some(log.clone())`,
//! `queue.set_events(log.clone())`, `notifier.set_events(log.clone())`, and `log.flush().await`
//! before `db.close()`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Map, Value, json};
use sf_store::{Db, err};
use tokio::sync::{mpsc, oneshot};

/// `source` values.
pub mod source {
    pub const UI: &str = "ui";
    pub const API: &str = "api";
    pub const MCP: &str = "mcp";
    pub const JOB: &str = "job";
    pub const SYSTEM: &str = "system";
    pub const AUTH: &str = "auth";
    pub const NOTIFY: &str = "notify";
    /// v2.3: one row per AI call (tokens, cost, cache hits).
    pub const AI: &str = "ai";
    pub const ALL: &[&str] = &[UI, API, MCP, JOB, SYSTEM, AUTH, NOTIFY, AI];
}

/// `status` values.
pub mod status {
    pub const OK: &str = "ok";
    pub const ERROR: &str = "error";
    pub const DENIED: &str = "denied";
    pub const ALL: &[&str] = &[OK, ERROR, DENIED];
}

/// Max serialized size of one `detail` (bytes).
pub const DETAIL_MAX_BYTES: usize = 4096;
const STR_MAX: usize = 500;
const ARRAY_MAX: usize = 50;
const DEPTH_MAX: usize = 6;
const FIELD_MAX: usize = 300;
const MASK: &str = "***";

/// One event. Build with [`Event::new`] (or a shortcut) and the chainable setters.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub ts: String,
    pub source: String,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub site_id: Option<String>,
    pub status: String,
    pub http_status: Option<i64>,
    pub duration_ms: Option<i64>,
    /// Already scrubbed.
    pub detail: Option<Value>,
    pub user_id: Option<String>,
}

fn cap(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

fn cap_opt(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| cap(s, FIELD_MAX))
}

impl Event {
    pub fn new(source: &str, actor: &str, action: &str) -> Self {
        Self {
            ts: sf_core::now_rfc3339(),
            source: cap(source, 20),
            actor: cap(actor, FIELD_MAX),
            action: cap(action, FIELD_MAX),
            target: None,
            site_id: None,
            status: status::OK.into(),
            http_status: None,
            duration_ms: None,
            detail: None,
            user_id: None,
        }
    }
    /// `source=system, actor=system`.
    pub fn system(action: &str) -> Self {
        Self::new(source::SYSTEM, "system", action)
    }
    /// `source=job, actor=system`.
    pub fn job(action: &str) -> Self {
        Self::new(source::JOB, "system", action)
    }
    pub fn target(mut self, t: Option<&str>) -> Self {
        self.target = cap_opt(t);
        self
    }
    pub fn site(mut self, s: Option<&str>) -> Self {
        self.site_id = cap_opt(s);
        self
    }
    pub fn user(mut self, u: Option<&str>) -> Self {
        self.user_id = cap_opt(u);
        self
    }
    pub fn status(mut self, s: &str) -> Self {
        self.status = if status::ALL.contains(&s) {
            s.to_string()
        } else {
            status::ERROR.to_string()
        };
        self
    }
    /// `ok` when true, `error` otherwise.
    pub fn ok(self, ok: bool) -> Self {
        self.status(if ok { status::OK } else { status::ERROR })
    }
    pub fn http(mut self, code: u16) -> Self {
        self.http_status = Some(i64::from(code));
        self
    }
    pub fn duration(mut self, d: Duration) -> Self {
        self.duration_ms = Some(i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        self
    }
    /// Sets the detail JSON (scrubbed and size-capped here; `null` clears it).
    pub fn detail(mut self, v: Value) -> Self {
        self.detail = match v {
            Value::Null => None,
            v => Some(scrub(&v)),
        };
        self
    }
}

/// True when a JSON key names something secret (password, token, secret, key, auth header, ...).
pub fn is_secret_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    const PARTS: &[&str] = &[
        "password",
        "passwd",
        "passphrase",
        "secret",
        "token",
        "authorization",
        "cookie",
        "credential",
        "private",
        "signature",
    ];
    PARTS.iter().any(|p| k.contains(p))
        || k == "key"
        || k.ends_with("key")
        || k.ends_with("_key")
        || k.contains("key_")
        || k.contains("apikey")
        || k == "auth"
        || k == "body"
        || k == "content"
        || k == "html"
}

fn looks_secret_value(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with("sfk_")
        || t.len() >= 7
            && t.get(..7)
                .is_some_and(|p| p.eq_ignore_ascii_case("bearer "))
        || t.starts_with("eyJ") && t.matches('.').count() == 2
}

fn scrub_at(v: &Value, depth: usize) -> Value {
    match v {
        Value::String(s) if looks_secret_value(s) => Value::String(MASK.into()),
        Value::String(s) => Value::String(cap(s, STR_MAX)),
        Value::Array(a) if depth >= DEPTH_MAX => json!(format!("[{} items]", a.len())),
        Value::Array(a) => {
            let mut out: Vec<Value> = a
                .iter()
                .take(ARRAY_MAX)
                .map(|x| scrub_at(x, depth + 1))
                .collect();
            if a.len() > ARRAY_MAX {
                out.push(json!(format!("... {} more", a.len() - ARRAY_MAX)));
            }
            Value::Array(out)
        }
        Value::Object(o) if depth >= DEPTH_MAX => json!(format!("{{{} keys}}", o.len())),
        Value::Object(o) => {
            let mut out = Map::new();
            for (k, x) in o.iter().take(ARRAY_MAX) {
                let k2 = cap(k, 100);
                if is_secret_key(k) && !x.is_null() {
                    out.insert(k2, Value::String(MASK.into()));
                } else {
                    out.insert(k2, scrub_at(x, depth + 1));
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Masks secret-looking keys and values, caps strings/arrays/depth, and caps the serialized size
/// at [`DETAIL_MAX_BYTES`] (an oversized detail becomes `{truncated: true, preview}`).
pub fn scrub(v: &Value) -> Value {
    let out = scrub_at(v, 0);
    let text = out.to_string();
    if text.len() <= DETAIL_MAX_BYTES {
        return out;
    }
    let preview: String = text.chars().take(1000).collect();
    json!({ "truncated": true, "bytes": text.len(), "preview": preview })
}

/// Writer tuning.
#[derive(Debug, Clone)]
pub struct EventLogOptions {
    /// Bounded queue size; events beyond it are dropped (counted).
    pub queue_size: usize,
    /// Rows per transaction (a full batch is written immediately).
    pub batch_max: usize,
    /// Flush interval for a partial batch.
    pub interval: Duration,
}

impl Default for EventLogOptions {
    fn default() -> Self {
        Self {
            queue_size: 10_000,
            batch_max: 500,
            interval: Duration::from_secs(1),
        }
    }
}

enum Msg {
    Event(Box<Event>),
    Flush(oneshot::Sender<()>),
}

struct Inner {
    tx: mpsc::Sender<Msg>,
    dropped: AtomicU64,
    written: Arc<AtomicU64>,
}

/// Cheap-to-clone handle. A disabled handle ignores everything.
#[derive(Clone, Default)]
pub struct EventLog {
    inner: Option<Arc<Inner>>,
}

impl std::fmt::Debug for EventLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventLog")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

impl EventLog {
    /// No-op handle (tests, CLI).
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Starts the supervised writer. Must be called inside a tokio runtime.
    pub fn start(db: Db, opts: EventLogOptions) -> Self {
        let (tx, rx) = mpsc::channel::<Msg>(opts.queue_size.max(1));
        let written = Arc::new(AtomicU64::new(0));
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let (w2, db2) = (written.clone(), db.clone());
        crate::supervise::spawn("event-log", Some(db), move || {
            writer_loop(db2.clone(), rx.clone(), opts.clone(), w2.clone())
        });
        Self {
            inner: Some(Arc::new(Inner {
                tx,
                dropped: AtomicU64::new(0),
                written,
            })),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Queues one event. Never blocks; drops (and counts) the event when the queue is full.
    pub fn record(&self, ev: Event) {
        let Some(inner) = &self.inner else { return };
        if inner.tx.try_send(Msg::Event(Box::new(ev))).is_err() {
            let n = inner.dropped.fetch_add(1, Ordering::Relaxed);
            if n.is_power_of_two() {
                tracing::warn!(dropped = n + 1, "event log queue full; events dropped");
            }
        }
    }

    /// `source=system` event (settings changed, restart, backup done/failed, ...).
    pub fn record_system(&self, action: &str, ok: bool, detail: Value) {
        self.record(Event::system(action).ok(ok).detail(detail));
    }

    /// `source=auth` event.
    pub fn record_auth(
        &self,
        actor: &str,
        action: &str,
        status: &str,
        user_id: Option<&str>,
        target: Option<&str>,
        detail: Value,
    ) {
        self.record(
            Event::new(source::AUTH, actor, action)
                .status(status)
                .user(user_id)
                .target(target)
                .detail(detail),
        );
    }

    /// Events dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |i| i.dropped.load(Ordering::Relaxed))
    }

    /// Rows written so far.
    pub fn written(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |i| i.written.load(Ordering::Relaxed))
    }

    /// Waits (up to 10 s) until every event recorded before this call is written.
    pub async fn flush(&self) {
        let Some(inner) = &self.inner else { return };
        let (ack, rx) = oneshot::channel();
        let sent = tokio::time::timeout(Duration::from_secs(10), inner.tx.send(Msg::Flush(ack)));
        if !matches!(sent.await, Ok(Ok(()))) {
            tracing::warn!("event log flush could not be queued");
            return;
        }
        if tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .is_err()
        {
            tracing::warn!("event log flush timed out");
        }
    }
}

async fn writer_loop(
    db: Db,
    rx: Arc<tokio::sync::Mutex<mpsc::Receiver<Msg>>>,
    opts: EventLogOptions,
    written: Arc<AtomicU64>,
) -> crate::supervise::Exit {
    let mut rx = rx.lock().await;
    let batch_max = opts.batch_max.max(1);
    let mut buf: Vec<Event> = Vec::with_capacity(batch_max);
    let every = opts.interval.max(Duration::from_millis(10));
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            m = rx.recv() => match m {
                Some(Msg::Event(e)) => {
                    buf.push(*e);
                    if buf.len() >= batch_max {
                        write_batch(&db, &mut buf, &written).await;
                    }
                }
                Some(Msg::Flush(ack)) => {
                    write_batch(&db, &mut buf, &written).await;
                    let _ = ack.send(());
                }
                None => {
                    write_batch(&db, &mut buf, &written).await;
                    return crate::supervise::Exit::Stop;
                }
            },
            _ = tick.tick() => {
                if !buf.is_empty() {
                    write_batch(&db, &mut buf, &written).await;
                }
            }
        }
    }
}

async fn write_batch(db: &Db, buf: &mut Vec<Event>, written: &AtomicU64) {
    if buf.is_empty() {
        return;
    }
    let rows = std::mem::take(buf);
    let n = rows.len() as u64;
    let res = db
        .write(move |t| {
            let mut st = t
                .prepare_cached(
                    "INSERT INTO event_log(ts, source, actor, action, target, site_id, status,
                       http_status, duration_ms, detail, user_id)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                )
                .map_err(err)?;
            for e in rows {
                st.execute(rusqlite::params![
                    e.ts,
                    e.source,
                    e.actor,
                    e.action,
                    e.target,
                    e.site_id,
                    e.status,
                    e.http_status,
                    e.duration_ms,
                    e.detail.map(|d| d.to_string()),
                    e.user_id
                ])
                .map_err(err)?;
            }
            Ok(())
        })
        .await;
    match res {
        Ok(()) => {
            written.fetch_add(n, Ordering::Relaxed);
        }
        Err(e) => tracing::warn!(error = %e, rows = n, "event log batch write failed; dropped"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_db(tag: &str) -> (Db, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("sf-ops-ev-{tag}-{}", sf_core::new_id()));
        let db = Db::open(&dir.join("t.db")).unwrap();
        (db, dir)
    }

    async fn count(db: &Db) -> i64 {
        db.read(|c| {
            c.query_row("SELECT count(*) FROM event_log", [], |r| r.get(0))
                .map_err(err)
        })
        .await
        .unwrap()
    }

    #[test]
    fn scrub_masks_secrets_and_caps_size() {
        let v = json!({
            "name": "hermes", "password": "hunter2", "api_key": "k", "Authorization": "Bearer x",
            "nested": {"bot_token": "123:abc", "ok": 1, "deep": {"secret": "s"}},
            "list": (0..80).collect::<Vec<i32>>(), "bearer": "Bearer abc.def",
            "tok": "sfk_abcdef", "keyword": "bike lock", "body": "<p>post</p>",
            "long": "x".repeat(5000),
        });
        let s = scrub(&v);
        let text = s.to_string();
        for leak in ["hunter2", "123:abc", "sfk_abcdef", "Bearer", "<p>post</p>"] {
            assert!(!text.contains(leak), "{leak} leaked: {text}");
        }
        assert_eq!(s["name"], "hermes");
        assert_eq!(s["keyword"], "bike lock");
        assert_eq!(s["nested"]["ok"], 1);
        assert_eq!(s["nested"]["deep"]["secret"], "***");
        assert_eq!(s["list"].as_array().unwrap().len(), ARRAY_MAX + 1);
        assert_eq!(s["long"].as_str().unwrap().len(), STR_MAX);
        // whole-detail cap
        let big: Map<String, Value> = (0..50)
            .map(|i| (format!("f{i}"), json!("y".repeat(400))))
            .collect();
        let s = scrub(&Value::Object(big));
        assert_eq!(s["truncated"], true);
        assert!(s.to_string().len() < DETAIL_MAX_BYTES);
        // deep nesting never recurses without bound
        let mut deep = json!(1);
        for _ in 0..500 {
            deep = json!({ "a": deep });
        }
        let _ = scrub(&deep);
        assert!(Event::system("x").detail(Value::Null).detail.is_none());
        assert_eq!(Event::system("x").status("bogus").status, "error");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn batches_flush_on_interval_size_and_flush() {
        let (db, dir) = tmp_db("batch");
        let log = EventLog::start(
            db.clone(),
            EventLogOptions {
                queue_size: 1000,
                batch_max: 10,
                interval: Duration::from_secs(3600),
            },
        );
        for i in 0..25 {
            log.record(
                Event::new(source::API, "user:u1", &format!("POST /x/{i}"))
                    .site(Some("s1"))
                    .user(Some("u1"))
                    .http(200)
                    .detail(json!({"password": "p", "i": i})),
            );
        }
        // two full batches are written without waiting for the (1 h) interval
        for _ in 0..200 {
            if count(&db).await >= 20 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(count(&db).await, 20);
        log.flush().await;
        assert_eq!(count(&db).await, 25);
        assert_eq!(log.written(), 25);
        let detail: String = db
            .read(|c| {
                c.query_row(
                    "SELECT detail FROM event_log ORDER BY id LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .map_err(err)
            })
            .await
            .unwrap();
        assert!(!detail.contains("\"p\""), "{detail}");
        // interval flush
        let log2 = EventLog::start(
            db.clone(),
            EventLogOptions {
                interval: Duration::from_millis(50),
                ..Default::default()
            },
        );
        log2.record_system("system start", true, json!({"version": "x"}));
        for _ in 0..200 {
            if count(&db).await >= 26 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(count(&db).await, 26);
        db.close();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn overflow_drops_and_never_blocks() {
        let (db, dir) = tmp_db("overflow");
        let log = EventLog::start(
            db.clone(),
            EventLogOptions {
                queue_size: 4,
                batch_max: 1000,
                interval: Duration::from_secs(3600),
            },
        );
        let t0 = std::time::Instant::now();
        for i in 0..20_000 {
            log.record(Event::system(&format!("e{i}")));
        }
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert!(log.dropped() > 0);
        log.flush().await;
        let n = count(&db).await;
        assert_eq!(
            u64::try_from(n).unwrap() + log.dropped(),
            20_000,
            "written {n} dropped {}",
            log.dropped()
        );
        // disabled handle is a no-op
        let off = EventLog::disabled();
        off.record(Event::system("x"));
        off.flush().await;
        assert!(!off.is_enabled());
        assert_eq!(off.dropped(), 0);
        db.close();
        let _ = std::fs::remove_dir_all(dir);
    }
}
