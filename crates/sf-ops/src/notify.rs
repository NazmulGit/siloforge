//! Notifications: every event is stored in `notifications` and delivered to the enabled channels of
//! `notification_channels` that subscribe to it.
//!
//! * Callers use [`Notifier::notify`], which never blocks: it pushes onto a bounded mpsc queue and a
//!   background task does the delivery (with retries and exponential backoff).
//! * Rate limit: at most `rate_per_min` notifications per event type per minute; extra ones are dropped
//!   (and counted in [`Notifier::suppressed`]).
//! * Channel config is a JSON object encrypted with `sf_core::crypto::encrypt_secret` in `config_enc`.
//!   Secret fields (`bot_token`, `secret`, `token`, `headers`) are never returned; the API shows `"***"`.
//!
//! Channel kinds and their config:
//! | kind | config |
//! |---|---|
//! | `webhook` | `{url, headers?: {name: value}}` → POST JSON payload |
//! | `ntfy` | `{url (topic URL), token?}` → POST text body, `Title`/`Priority`/`Tags` headers |
//! | `telegram` | `{bot_token, chat_id, thread_id?, api_base?}` → Bot API `sendMessage` |
//! | `hermes` | `{url, secret?}` → POST JSON to a Hermes gateway webhook route; with a secret, signs with generic HMAC V2 (`X-Webhook-Timestamp`, `X-Webhook-Signature-V2` = hex HMAC-SHA256 of `"<ts>.<body>"`). SiloForge never needs the Telegram token. |
//! | `desktop` | `{}` → Windows toast via `powershell -NoProfile` (best effort, no-op elsewhere) |
//! | `log` | `{}` → `tracing` line |

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sf_core::crypto::{decrypt_secret, encrypt_secret};
use sf_core::{Error, Result};
use sf_store::{Db, err, row_to_json};
use tokio::sync::{Semaphore, mpsc};

/// Event names (wire strings). Channels subscribe to a list of these (or `"*"`).
pub mod events {
    pub const JOB_DONE: &str = "job.done";
    pub const JOB_FAILED: &str = "job.failed";
    pub const BACKUP_DONE: &str = "backup.done";
    pub const BACKUP_FAILED: &str = "backup.failed";
    pub const SYSTEM_ALERT: &str = "system.alert";
    pub const PLAN_READY: &str = "plan.ready";
    pub const INJECT_DONE: &str = "inject.done";
    pub const TEST: &str = "test";
    pub const ALL: &[&str] = &[
        JOB_DONE,
        JOB_FAILED,
        BACKUP_DONE,
        BACKUP_FAILED,
        SYSTEM_ALERT,
        PLAN_READY,
        INJECT_DONE,
    ];
}

/// Channel kinds accepted by `notification_channels.kind`.
pub const CHANNEL_KINDS: &[&str] = &["webhook", "ntfy", "telegram", "hermes", "desktop", "log"];
/// Config keys that hold secrets (masked on read, kept when PATCHed with `"***"`).
pub const SECRET_KEYS: &[&str] = &["bot_token", "secret", "token", "headers"];
const MASK: &str = "***";

/// One notification to send.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub event: String,
    /// `info` | `warn` | `error`.
    pub level: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub site_id: Option<String>,
    #[serde(default)]
    pub job_run_id: Option<String>,
}

impl Notification {
    pub fn new(event: &str, title: impl Into<String>, body: impl Into<String>) -> Self {
        let level = match event {
            events::JOB_FAILED | events::BACKUP_FAILED => "error",
            events::SYSTEM_ALERT => "warn",
            _ => "info",
        };
        Self {
            event: event.to_string(),
            level: level.to_string(),
            title: title.into(),
            body: body.into(),
            data: Value::Object(Default::default()),
            site_id: None,
            job_run_id: None,
        }
    }
    pub fn level(mut self, level: &str) -> Self {
        self.level = level.to_string();
        self
    }
    pub fn data(mut self, data: Value) -> Self {
        self.data = data;
        self
    }
    pub fn site(mut self, site_id: Option<&str>) -> Self {
        self.site_id = site_id.map(str::to_string);
        self
    }
    pub fn job(mut self, job_run_id: Option<&str>) -> Self {
        self.job_run_id = job_run_id.map(str::to_string);
        self
    }
}

/// Result of delivering to one channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Delivery {
    pub channel_id: String,
    pub kind: String,
    pub ok: bool,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub at: String,
}

/// Tunables (defaults suit production; tests shrink the backoff).
#[derive(Debug, Clone)]
pub struct NotifierOptions {
    pub max_attempts: u32,
    pub base_backoff: Duration,
    pub rate_per_min: u32,
    pub http_timeout: Duration,
    pub queue_size: usize,
    /// Deliveries in flight at once.
    pub concurrency: usize,
}

impl Default for NotifierOptions {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_backoff: Duration::from_secs(2),
            rate_per_min: 30,
            http_timeout: Duration::from_secs(15),
            queue_size: 1024,
            concurrency: 4,
        }
    }
}

/// A stored channel with its decrypted config (internal use; never serialize to clients as-is).
#[derive(Debug, Clone)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub config: Value,
    pub events: Vec<String>,
    pub enabled: bool,
    pub created_at: String,
}

impl Channel {
    fn wants(&self, event: &str) -> bool {
        event == events::TEST || self.events.iter().any(|e| e == "*" || e == event)
    }
    /// Client view: secret config values masked.
    pub fn public_json(&self) -> Value {
        json!({
            "id": self.id, "name": self.name, "kind": self.kind, "config": mask_config(&self.config),
            "events": self.events, "enabled": self.enabled, "created_at": self.created_at,
        })
    }
}

/// Replaces secret values with `"***"`.
pub fn mask_config(cfg: &Value) -> Value {
    let mut out = cfg.clone();
    if let Some(obj) = out.as_object_mut() {
        for k in SECRET_KEYS {
            if let Some(v) = obj.get_mut(*k)
                && !v.is_null()
            {
                *v = Value::String(MASK.into());
            }
        }
        for k in URL_KEYS {
            if let Some(Value::String(u)) = obj.get_mut(*k) {
                *u = mask_url(u);
            }
        }
    }
    out
}

/// Config keys holding URLs that can carry a secret (masked by [`mask_url`]).
pub const URL_KEYS: &[&str] = &["url", "api_base"];

/// Cheap-to-clone handle. Create with [`Notifier::start`] inside a tokio runtime.
#[derive(Clone)]
pub struct Notifier {
    tx: mpsc::Sender<Notification>,
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    key: Arc<[u8; 32]>,
    opts: NotifierOptions,
    http: reqwest::Client,
    rate: Mutex<HashMap<String, (Instant, u32)>>,
    suppressed: AtomicU64,
}

impl Notifier {
    /// Starts the background delivery task. `key` is the stored-secret AES key (`AppState::secret_key`).
    pub fn start(db: Db, key: Arc<[u8; 32]>, opts: NotifierOptions) -> Self {
        let (tx, rx) = mpsc::channel::<Notification>(opts.queue_size.max(1));
        let http = reqwest::Client::builder()
            .timeout(opts.http_timeout)
            // L-05: never follow a webhook redirect (custom headers would follow it to any host).
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!("SiloForge/{}", sf_core::VERSION))
            .build()
            .unwrap_or_default();
        let inner = Arc::new(Inner {
            db,
            key,
            opts,
            http,
            rate: Mutex::new(HashMap::new()),
            suppressed: AtomicU64::new(0),
        });
        let worker = inner.clone();
        // R8: supervised; the receiver is shared so a restarted loop keeps draining the queue.
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let sem = Arc::new(Semaphore::new(worker.opts.concurrency.max(1)));
        crate::supervise::spawn("notifier", Some(worker.db.clone()), move || {
            let (worker, rx, sem) = (worker.clone(), rx.clone(), sem.clone());
            async move {
                let mut rx = rx.lock().await;
                while let Some(n) = rx.recv().await {
                    let Ok(permit) = sem.clone().acquire_owned().await else {
                        break;
                    };
                    let w = worker.clone();
                    tokio::spawn(async move {
                        if let Err(e) = w.process(n).await {
                            tracing::warn!(error = %e, "notification processing failed");
                        }
                        drop(permit);
                    });
                }
                crate::supervise::Exit::Stop
            }
        });
        Self { tx, inner }
    }

    /// Queues a notification. Never blocks; returns false when rate-limited or the queue is full.
    pub fn notify(&self, n: Notification) -> bool {
        if !self.inner.allow(&n.event) {
            self.inner.suppressed.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(event = %n.event, "notification rate-limited");
            return false;
        }
        match self.tx.try_send(n) {
            Ok(()) => true,
            Err(_) => {
                self.inner.suppressed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("notification queue full; dropped");
                false
            }
        }
    }

    /// Notifications dropped by the rate limiter or a full queue since start.
    pub fn suppressed(&self) -> u64 {
        self.inner.suppressed.load(Ordering::Relaxed)
    }

    /// Sends a test message to one channel right now (awaits delivery, with retries) and stores it.
    pub async fn test_channel(&self, channel_id: &str) -> Result<Delivery> {
        let ch = get_channel(&self.inner.db, &self.inner.key, channel_id)
            .await?
            .ok_or_else(|| Error::not_found("channel not found"))?;
        let n = Notification::new(
            events::TEST,
            "SiloForge test notification",
            format!("Channel '{}' ({}) works.", ch.name, ch.kind),
        );
        let id = self.inner.store(&n).await?;
        let d = self.inner.deliver_with_retry(&ch, &n, id).await;
        self.inner
            .save_deliveries(id, std::slice::from_ref(&d))
            .await?;
        Ok(d)
    }

    pub fn db(&self) -> &Db {
        &self.inner.db
    }
    pub fn key(&self) -> &Arc<[u8; 32]> {
        &self.inner.key
    }
}

impl Inner {
    fn allow(&self, event: &str) -> bool {
        let limit = self.opts.rate_per_min.max(1);
        let mut map = self.rate.lock();
        if map.len() > 256 {
            map.retain(|_, (t, _)| t.elapsed() < Duration::from_secs(60));
        }
        let e = map.entry(event.to_string()).or_insert((Instant::now(), 0));
        if e.0.elapsed() >= Duration::from_secs(60) {
            *e = (Instant::now(), 0);
        }
        if e.1 >= limit {
            return false;
        }
        e.1 += 1;
        true
    }

    async fn store(&self, n: &Notification) -> Result<i64> {
        let n = n.clone();
        self.db
            .write(move |t| {
                t.execute(
                    "INSERT INTO notifications(event, level, title, body, data, site_id, job_run_id)
                     VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        n.event,
                        n.level,
                        n.title.chars().take(500).collect::<String>(),
                        n.body.chars().take(8000).collect::<String>(),
                        n.data.to_string(),
                        n.site_id,
                        n.job_run_id
                    ],
                )
                .map_err(err)?;
                Ok(t.last_insert_rowid())
            })
            .await
    }

    async fn save_deliveries(&self, id: i64, ds: &[Delivery]) -> Result<()> {
        let mut map = serde_json::Map::new();
        for d in ds {
            map.insert(
                d.channel_id.clone(),
                serde_json::to_value(d).unwrap_or(Value::Null),
            );
        }
        let delivered = Value::Object(map).to_string();
        self.db
            .write(move |t| {
                t.execute(
                    "UPDATE notifications SET delivered=?2 WHERE id=?1",
                    params![id, delivered],
                )
                .map_err(err)?;
                Ok(())
            })
            .await
    }

    async fn process(self: &Arc<Self>, n: Notification) -> Result<()> {
        let id = self.store(&n).await?;
        let channels = list_channels(&self.db, &self.key).await?;
        let targets: Vec<Channel> = channels
            .into_iter()
            .filter(|c| c.enabled && c.wants(&n.event))
            .collect();
        if targets.is_empty() {
            return Ok(());
        }
        let mut handles = Vec::with_capacity(targets.len());
        for ch in targets {
            let me = self.clone();
            let n = n.clone();
            handles.push(tokio::spawn(async move {
                me.deliver_with_retry(&ch, &n, id).await
            }));
        }
        let mut results = Vec::with_capacity(handles.len());
        for h in handles {
            if let Ok(d) = h.await {
                results.push(d);
            }
        }
        self.save_deliveries(id, &results).await
    }

    async fn deliver_with_retry(&self, ch: &Channel, n: &Notification, id: i64) -> Delivery {
        let max = self.opts.max_attempts.max(1);
        let mut attempt = 0u32;
        let mut last_err = None;
        while attempt < max {
            attempt += 1;
            match self.deliver_once(ch, n, id).await {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(Failure { msg, retryable }) => {
                    last_err = Some(msg);
                    if !retryable || attempt >= max {
                        break;
                    }
                    let factor = 1u32 << (attempt - 1).min(6);
                    tokio::time::sleep(self.opts.base_backoff.saturating_mul(factor)).await;
                }
            }
        }
        if let Some(e) = &last_err {
            tracing::warn!(channel = %ch.name, kind = %ch.kind, error = %e, "notification delivery failed");
        }
        Delivery {
            channel_id: ch.id.clone(),
            kind: ch.kind.clone(),
            ok: last_err.is_none(),
            attempts: attempt,
            error: last_err,
            at: sf_core::now_rfc3339(),
        }
    }

    async fn deliver_once(
        &self,
        ch: &Channel,
        n: &Notification,
        id: i64,
    ) -> std::result::Result<(), Failure> {
        let cfg = &ch.config;
        let s = |k: &str| cfg.get(k).and_then(Value::as_str).unwrap_or("").trim();
        let payload = json!({
            "id": id, "source": "siloforge", "event": n.event, "level": n.level, "title": n.title,
            "body": n.body, "text": format!("{}\n{}", n.title, n.body).trim().to_string(),
            "data": n.data, "site_id": n.site_id, "job_run_id": n.job_run_id,
            "created_at": sf_core::now_rfc3339(),
        });
        match ch.kind.as_str() {
            "log" => {
                tracing::info!(event = %n.event, level = %n.level, title = %n.title, body = %n.body, "notification");
                Ok(())
            }
            "desktop" => desktop_toast(&n.title, &n.body),
            "webhook" => {
                let url = need(s("url"), "url")?;
                metadata_guard(url).await?;
                let mut req = self.http.post(url).json(&payload);
                if let Some(h) = cfg.get("headers").and_then(Value::as_object) {
                    for (k, v) in h {
                        if let Some(v) = v.as_str() {
                            req = req.header(k.as_str(), v);
                        }
                    }
                }
                send(req).await
            }
            "hermes" => {
                let url = need(s("url"), "url")?;
                metadata_guard(url).await?;
                let body = payload.to_string();
                let mut req = self
                    .http
                    .post(url)
                    .header("content-type", "application/json")
                    .header("X-Request-ID", format!("siloforge-{id}-{}", ch.id));
                let secret = s("secret");
                if !secret.is_empty() {
                    let ts = sf_core::now_unix().to_string();
                    req = req.header("X-Webhook-Timestamp", &ts).header(
                        "X-Webhook-Signature-V2",
                        hermes_signature(secret, &ts, &body),
                    );
                }
                send(req.body(body)).await
            }
            "ntfy" => {
                let url = need(s("url"), "url")?;
                metadata_guard(url).await?;
                let priority = match n.level.as_str() {
                    "error" => "high",
                    "warn" => "default",
                    _ => "low",
                };
                let mut req = self
                    .http
                    .post(url)
                    .header("Title", ascii_header(&n.title))
                    .header("Priority", priority)
                    .header("Tags", n.event.replace('.', "_"))
                    .body(if n.body.is_empty() {
                        n.title.clone()
                    } else {
                        n.body.clone()
                    });
                let token = s("token");
                if !token.is_empty() {
                    req = req.bearer_auth(token);
                }
                send(req).await
            }
            "telegram" => {
                let token = need(s("bot_token"), "bot_token")?;
                let chat = cfg
                    .get("chat_id")
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .filter(|s| !s.is_empty() && s != "null")
                    .ok_or_else(|| Failure::fatal("config.chat_id missing"))?;
                let base = match s("api_base") {
                    "" => "https://api.telegram.org",
                    b => b,
                };
                let mut body = json!({
                    "chat_id": chat,
                    "text": format!("{}\n{}", n.title, n.body).trim().chars().take(4000).collect::<String>(),
                    "disable_web_page_preview": true,
                });
                if let Some(t) = cfg.get("thread_id").filter(|v| !v.is_null()) {
                    body["message_thread_id"] = t.clone();
                }
                metadata_guard(base).await?;
                let url = format!("{}/bot{}/sendMessage", base.trim_end_matches('/'), token);
                send(self.http.post(url).json(&body)).await
            }
            other => Err(Failure::fatal(format!("unknown channel kind {other}"))),
        }
    }
}

struct Failure {
    msg: String,
    retryable: bool,
}

impl Failure {
    fn fatal(m: impl Into<String>) -> Self {
        Self {
            msg: m.into(),
            retryable: false,
        }
    }
}

fn need<'a>(v: &'a str, name: &str) -> std::result::Result<&'a str, Failure> {
    if v.is_empty() {
        Err(Failure::fatal(format!("config.{name} missing")))
    } else {
        Ok(v)
    }
}

/// L-05: never deliver to a cloud-metadata endpoint (IP literal, well-known name, or a name
/// that resolves to one). Loopback / LAN stay allowed: the Hermes gateway lives on 127.0.0.1.
async fn metadata_guard(url: &str) -> std::result::Result<(), Failure> {
    let parsed =
        reqwest::Url::parse(url).map_err(|_| Failure::fatal("config.url is not a valid URL"))?;
    let host = parsed.host_str().unwrap_or("").to_string();
    if host.is_empty() {
        return Err(Failure::fatal("config.url has no host"));
    }
    if sf_core::net::is_metadata_host(&host) {
        return Err(Failure::fatal(
            "refusing to deliver to a cloud-metadata address",
        ));
    }
    let port = parsed.port_or_known_default().unwrap_or(80);
    let bare = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    if let Ok(addrs) = tokio::net::lookup_host((bare.as_str(), port)).await {
        for a in addrs {
            if sf_core::net::is_metadata_ip(a.ip()) {
                return Err(Failure::fatal(
                    "refusing to deliver: host resolves to a cloud-metadata address",
                ));
            }
        }
    }
    Ok(())
}

/// L-05: client view of a secret-bearing URL (Slack/Discord/ntfy tokens live in the path or
/// query): `scheme://host[:port]/***`. A URL with no path/query is shown as is.
pub fn mask_url(u: &str) -> String {
    let Ok(p) = reqwest::Url::parse(u) else {
        return MASK.to_string();
    };
    if (p.path().is_empty() || p.path() == "/") && p.query().is_none() {
        return u.to_string();
    }
    let host = p.host_str().unwrap_or("");
    match p.port() {
        Some(port) => format!("{}://{host}:{port}/{MASK}", p.scheme()),
        None => format!("{}://{host}/{MASK}", p.scheme()),
    }
}

/// Header values must be visible ASCII; non-ASCII is replaced.
fn ascii_header(s: &str) -> String {
    s.chars()
        .take(200)
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() {
                c
            } else {
                '?'
            }
        })
        .collect()
}

/// Sends a request; errors never contain the URL (it may hold a bot token).
async fn send(req: reqwest::RequestBuilder) -> std::result::Result<(), Failure> {
    match req.send().await {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => {
            let code = r.status().as_u16();
            Err(Failure {
                msg: format!("HTTP {code}"),
                retryable: code == 429 || code == 408 || code >= 500,
            })
        }
        Err(e) => Err(Failure {
            msg: format!("request failed: {}", e.without_url()),
            retryable: true,
        }),
    }
}

/// Hermes generic webhook HMAC V2: hex HMAC-SHA256 of `"<ts>.<body>"`.
pub fn hermes_signature(secret: &str, ts: &str, body: &str) -> String {
    let Ok(mut mac) = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()) else {
        return String::new();
    };
    mac.update(ts.as_bytes());
    mac.update(b".");
    mac.update(body.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Windows toast through PowerShell's WinRT bridge. Spawned detached; title/body travel via env vars
/// so they are never interpreted as script. G2-17: other platforms report a (non-retryable)
/// failure instead of a fake success.
fn desktop_toast(title: &str, body: &str) -> std::result::Result<(), Failure> {
    if !cfg!(windows) {
        return Err(Failure::fatal(DESKTOP_UNSUPPORTED));
    }
    const SCRIPT: &str = "$ErrorActionPreference='SilentlyContinue';\
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null;\
$t=[Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02);\
$x=$t.GetElementsByTagName('text');\
$x.Item(0).AppendChild($t.CreateTextNode($env:SF_TOAST_TITLE)) | Out-Null;\
$x.Item(1).AppendChild($t.CreateTextNode($env:SF_TOAST_BODY)) | Out-Null;\
$n=[Windows.UI.Notifications.ToastNotification]::new($t);\
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe').Show($n)";
    let res = std::process::Command::new(powershell_exe())
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            SCRIPT,
        ])
        .env(
            "SF_TOAST_TITLE",
            title.chars().take(200).collect::<String>(),
        )
        .env("SF_TOAST_BODY", body.chars().take(500).collect::<String>())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match res {
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::debug!(error = %e, "desktop toast unavailable");
            Err(Failure::fatal(format!("desktop toast unavailable: {e}")))
        }
    }
}

const DESKTOP_UNSUPPORTED: &str = "desktop notifications are only supported on Windows";

// ---------------------------------------------------------------------------------------------
// Channel CRUD (used by the v2 API).

/// Body for creating a channel.
#[derive(Debug, Clone, Deserialize)]
pub struct NewChannel {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub events: Option<Vec<String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Body for PATCHing a channel (absent fields unchanged; config is merged key by key, `"***"` keeps a secret).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChannelPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub config: Option<Value>,
    #[serde(default)]
    pub events: Option<Vec<String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

fn validate_events(ev: &[String]) -> Result<()> {
    if ev.len() > 32 {
        return Err(Error::bad("too many events"));
    }
    for e in ev {
        if e != "*" && !events::ALL.contains(&e.as_str()) {
            return Err(Error::bad(format!(
                "unknown event '{e}' (allowed: {}, *)",
                events::ALL.join(", ")
            )));
        }
    }
    Ok(())
}

fn validate_config(kind: &str, cfg: &Value) -> Result<()> {
    if !cfg.is_object() {
        return Err(Error::bad("config must be an object"));
    }
    let s = |k: &str| cfg.get(k).and_then(Value::as_str).unwrap_or("");
    let url_ok = |u: &str| u.starts_with("http://") || u.starts_with("https://");
    match kind {
        "webhook" | "ntfy" | "hermes" if !url_ok(s("url")) => {
            Err(Error::bad("config.url must be an http(s) URL"))
        }
        _ if URL_KEYS.iter().any(|k| {
            reqwest::Url::parse(s(k))
                .ok()
                .and_then(|u| u.host_str().map(sf_core::net::is_metadata_host))
                .unwrap_or(false)
        }) =>
        {
            Err(Error::bad(
                "config URL must not point at a cloud-metadata address",
            ))
        }
        "telegram" if s("bot_token").is_empty() || cfg.get("chat_id").is_none() => Err(Error::bad(
            "telegram needs config.bot_token and config.chat_id",
        )),
        k if !CHANNEL_KINDS.contains(&k) => Err(Error::bad(format!(
            "kind must be one of {}",
            CHANNEL_KINDS.join(", ")
        ))),
        _ => Ok(()),
    }
}

fn row_channel(key: &[u8; 32], r: &rusqlite::Row<'_>) -> rusqlite::Result<Channel> {
    let enc: String = r.get("config_enc")?;
    let config = decrypt_secret(key, &enc)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    let events: String = r.get("events")?;
    Ok(Channel {
        id: r.get("id")?,
        name: r.get("name")?,
        kind: r.get("kind")?,
        config,
        events: serde_json::from_str(&events).unwrap_or_default(),
        enabled: r.get::<_, i64>("enabled")? != 0,
        created_at: r.get("created_at")?,
    })
}

pub async fn list_channels(db: &Db, key: &Arc<[u8; 32]>) -> Result<Vec<Channel>> {
    let key = key.clone();
    db.read(move |c| {
        let mut stmt = c
            .prepare("SELECT * FROM notification_channels ORDER BY created_at, id")
            .map_err(err)?;
        let rows = stmt
            .query_map([], |r| row_channel(&key, r))
            .map_err(err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(err)?;
        Ok(rows)
    })
    .await
}

pub async fn get_channel(db: &Db, key: &Arc<[u8; 32]>, id: &str) -> Result<Option<Channel>> {
    let (key, id) = (key.clone(), id.to_string());
    db.read(move |c| {
        c.query_row(
            "SELECT * FROM notification_channels WHERE id=?1",
            [&id],
            |r| row_channel(&key, r),
        )
        .optional()
        .map_err(err)
    })
    .await
}

pub async fn create_channel(db: &Db, key: &Arc<[u8; 32]>, new: NewChannel) -> Result<Channel> {
    let name = new.name.trim().to_string();
    if name.is_empty() || name.len() > 100 {
        return Err(Error::bad("name is required (max 100 chars)"));
    }
    let config = if new.config.is_null() {
        json!({})
    } else {
        new.config
    };
    validate_config(&new.kind, &config)?;
    let events = new.events.unwrap_or_else(|| {
        [
            events::JOB_FAILED,
            events::JOB_DONE,
            events::BACKUP_DONE,
            events::BACKUP_FAILED,
            events::SYSTEM_ALERT,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    });
    validate_events(&events)?;
    let enc = encrypt_secret(key, &config.to_string())?;
    let id = sf_core::new_id();
    let (id2, kind, enabled) = (id.clone(), new.kind.clone(), new.enabled.unwrap_or(true));
    let ev = serde_json::to_string(&events).unwrap_or_else(|_| "[]".into());
    db.write(move |t| {
        t.execute(
            "INSERT INTO notification_channels(id, name, kind, config_enc, events, enabled) VALUES(?1,?2,?3,?4,?5,?6)",
            params![id2, name, kind, enc, ev, enabled as i64],
        )
        .map_err(err)?;
        Ok(())
    })
    .await?;
    get_channel(db, key, &id)
        .await?
        .ok_or_else(|| Error::internal("channel vanished"))
}

pub async fn update_channel(
    db: &Db,
    key: &Arc<[u8; 32]>,
    id: &str,
    patch: ChannelPatch,
) -> Result<Channel> {
    let mut ch = get_channel(db, key, id)
        .await?
        .ok_or_else(|| Error::not_found("channel not found"))?;
    if let Some(n) = patch.name {
        let n = n.trim().to_string();
        if n.is_empty() || n.len() > 100 {
            return Err(Error::bad("name is required (max 100 chars)"));
        }
        ch.name = n;
    }
    if let Some(cfg) = patch.config {
        let Some(src) = cfg.as_object() else {
            return Err(Error::bad("config must be an object"));
        };
        let mut merged = ch.config.as_object().cloned().unwrap_or_default();
        for (k, v) in src {
            if v.as_str() == Some(MASK) {
                continue; // masked secret echoed back: keep the stored value
            }
            if URL_KEYS.contains(&k.as_str())
                && let (Some(new), Some(Value::String(old))) = (v.as_str(), merged.get(k))
                && new != old.as_str()
                && new == mask_url(old)
            {
                continue; // masked URL echoed back: keep the stored URL
            }
            if v.is_null() {
                merged.remove(k);
            } else {
                merged.insert(k.clone(), v.clone());
            }
        }
        ch.config = Value::Object(merged);
        validate_config(&ch.kind, &ch.config)?;
    }
    if let Some(ev) = patch.events {
        validate_events(&ev)?;
        ch.events = ev;
    }
    if let Some(e) = patch.enabled {
        ch.enabled = e;
    }
    let enc = encrypt_secret(key, &ch.config.to_string())?;
    let ev = serde_json::to_string(&ch.events).unwrap_or_else(|_| "[]".into());
    let (id2, name, enabled) = (ch.id.clone(), ch.name.clone(), ch.enabled);
    db.write(move |t| {
        t.execute(
            "UPDATE notification_channels SET name=?2, config_enc=?3, events=?4, enabled=?5 WHERE id=?1",
            params![id2, name, enc, ev, enabled as i64],
        )
        .map_err(err)?;
        Ok(())
    })
    .await?;
    Ok(ch)
}

pub async fn delete_channel(db: &Db, id: &str) -> Result<bool> {
    let id = id.to_string();
    db.write(move |t| {
        Ok(
            t.execute("DELETE FROM notification_channels WHERE id=?1", [&id])
                .map_err(err)?
                > 0,
        )
    })
    .await
}

/// Lists stored notifications, newest first.
pub async fn list_notifications(db: &Db, unread_only: bool, limit: usize) -> Result<Vec<Value>> {
    let limit = limit.clamp(1, 1000) as i64;
    db.read(move |c| {
        let filter = if unread_only {
            "WHERE read_at IS NULL"
        } else {
            ""
        };
        let mut stmt = c
            .prepare(&format!(
                "SELECT id, event, level, title, body, data, site_id, job_run_id, delivered, read_at, created_at
                 FROM notifications {filter} ORDER BY id DESC LIMIT ?1"
            ))
            .map_err(err)?;
        let rows = stmt
            .query_map([limit], |r| Ok(row_to_json(r, &["data", "delivered"], &[])))
            .map_err(err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(err)?;
        Ok(rows)
    })
    .await
}

/// Marks notifications read (`ids = None` = all). Returns rows updated.
pub async fn mark_read(db: &Db, ids: Option<Vec<i64>>) -> Result<usize> {
    let now = sf_core::now_rfc3339();
    db.write(move |t| {
        let n = match ids {
            None => t
                .execute(
                    "UPDATE notifications SET read_at=?1 WHERE read_at IS NULL",
                    [&now],
                )
                .map_err(err)?,
            Some(ids) => {
                let mut n = 0;
                for id in ids.iter().take(10_000) {
                    n += t
                        .execute(
                            "UPDATE notifications SET read_at=?2 WHERE id=?1 AND read_at IS NULL",
                            params![id, now],
                        )
                        .map_err(err)?;
                }
                n
            }
        };
        Ok(n)
    })
    .await
}

/// Count of unread notifications.
pub async fn unread_count(db: &Db) -> Result<i64> {
    db.read(|c| {
        c.query_row(
            "SELECT count(*) FROM notifications WHERE read_at IS NULL",
            [],
            |r| r.get(0),
        )
        .map_err(err)
    })
    .await
}

/// I-03: absolute path of Windows PowerShell (`%SystemRoot%\\System32\\WindowsPowerShell\\v1.0`),
/// so a `powershell.exe` planted earlier in PATH or the working directory is never run.
pub(crate) fn powershell_exe() -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    root.join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use std::sync::atomic::AtomicUsize;

    #[derive(Clone, Default)]
    struct Stub {
        hits: Arc<AtomicUsize>,
        fail_first: usize,
        bodies: Arc<Mutex<Vec<(HeaderMap, String)>>>,
    }

    async fn hook(State(s): State<Stub>, headers: HeaderMap, body: String) -> StatusCode {
        let n = s.hits.fetch_add(1, Ordering::SeqCst);
        s.bodies.lock().push((headers, body));
        if n < s.fail_first {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::OK
        }
    }

    async fn stub_server(stub: Stub) -> String {
        let app = Router::new()
            .route("/hook", post(hook))
            .route("/bad", post(|| async { StatusCode::BAD_REQUEST }))
            .route("/botTOKEN123/sendMessage", post(hook))
            .with_state(stub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn setup() -> (Db, std::path::PathBuf, Arc<[u8; 32]>) {
        let dir = std::env::temp_dir().join(format!("sf-ops-notify-{}", sf_core::new_id()));
        let db = Db::open(&dir.join("t.db")).unwrap();
        (db, dir, Arc::new([7u8; 32]))
    }

    fn fast() -> NotifierOptions {
        NotifierOptions {
            max_attempts: 3,
            base_backoff: Duration::from_millis(10),
            rate_per_min: 3,
            ..Default::default()
        }
    }

    async fn wait_delivered(db: &Db) -> Value {
        for _ in 0..200 {
            let rows = list_notifications(db, false, 10).await.unwrap();
            if let Some(r) = rows.first()
                && r["delivered"].as_object().is_some_and(|m| !m.is_empty())
            {
                return r.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("not delivered");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn webhook_retry_and_hermes_signature() {
        let (db, dir, key) = setup();
        let stub = Stub {
            fail_first: 2,
            ..Default::default()
        };
        let base = stub_server(stub.clone()).await;
        let ch = create_channel(
            &db,
            &key,
            NewChannel {
                name: "hook".into(),
                kind: "webhook".into(),
                config: json!({"url": format!("{base}/hook"), "headers": {"x-k": "v"}}),
                events: Some(vec!["job.failed".into()]),
                enabled: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(ch.public_json()["config"]["headers"], "***");
        let n = Notifier::start(db.clone(), key.clone(), fast());
        assert!(
            n.notify(Notification::new(events::JOB_FAILED, "Crawl failed", "boom").job(Some("j1")))
        );
        let row = wait_delivered(&db).await;
        let d = &row["delivered"][&ch.id];
        assert_eq!(d["ok"], true);
        assert_eq!(d["attempts"], 3);
        assert_eq!(stub.hits.load(Ordering::SeqCst), 3);
        {
            let bodies = stub.bodies.lock();
            let (h, b) = bodies.last().unwrap();
            assert_eq!(h.get("x-k").unwrap(), "v");
            let v: Value = serde_json::from_str(b).unwrap();
            assert_eq!(v["event"], "job.failed");
            assert_eq!(v["job_run_id"], "j1");
        }
        // unsubscribed event: stored, but not delivered to the channel
        assert!(n.notify(Notification::new(events::JOB_DONE, "ok", "")));
        // rate limit: 3 per event per minute
        assert!(n.notify(Notification::new(events::JOB_DONE, "ok", "")));
        assert!(n.notify(Notification::new(events::JOB_DONE, "ok", "")));
        assert!(!n.notify(Notification::new(events::JOB_DONE, "ok", "")));
        assert_eq!(n.suppressed(), 1);

        // hermes channel signs with HMAC V2
        let stub2 = Stub::default();
        let base2 = stub_server(stub2.clone()).await;
        let h = create_channel(
            &db,
            &key,
            NewChannel {
                name: "hermes".into(),
                kind: "hermes".into(),
                config: json!({"url": format!("{base2}/hook"), "secret": "s3cr3t"}),
                events: Some(vec!["*".into()]),
                enabled: Some(true),
            },
        )
        .await
        .unwrap();
        let d = n.test_channel(&h.id).await.unwrap();
        assert!(d.ok, "{d:?}");
        let bodies = stub2.bodies.lock();
        let (hd, body) = bodies.last().unwrap();
        let ts = hd.get("x-webhook-timestamp").unwrap().to_str().unwrap();
        let sig = hd.get("x-webhook-signature-v2").unwrap().to_str().unwrap();
        assert_eq!(sig, hermes_signature("s3cr3t", ts, body));
        db.close();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn non_retryable_and_telegram_and_crud() {
        let (db, dir, key) = setup();
        let stub = Stub::default();
        let base = stub_server(stub.clone()).await;
        let n = Notifier::start(db.clone(), key.clone(), fast());
        let bad = create_channel(
            &db,
            &key,
            NewChannel {
                name: "bad".into(),
                kind: "webhook".into(),
                config: json!({"url": format!("{base}/bad")}),
                events: None,
                enabled: None,
            },
        )
        .await
        .unwrap();
        let d = n.test_channel(&bad.id).await.unwrap();
        assert!(!d.ok);
        assert_eq!(d.attempts, 1);
        assert_eq!(d.error.as_deref(), Some("HTTP 400"));

        let tg = create_channel(
            &db,
            &key,
            NewChannel {
                name: "tg".into(),
                kind: "telegram".into(),
                config: json!({"bot_token": "TOKEN123", "chat_id": -100123, "api_base": base}),
                events: None,
                enabled: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(tg.public_json()["config"]["bot_token"], "***");
        assert!(n.test_channel(&tg.id).await.unwrap().ok);
        let body = stub.bodies.lock().last().unwrap().1.clone();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["chat_id"], "-100123");

        // PATCH keeps masked secret, merges others
        let up = update_channel(
            &db,
            &key,
            &tg.id,
            ChannelPatch {
                config: Some(json!({"bot_token": "***", "chat_id": "42"})),
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(up.config["bot_token"], "TOKEN123");
        assert_eq!(up.config["chat_id"], "42");
        assert!(!up.enabled);
        // stored encrypted
        let raw: String = db
            .read(|c| {
                c.query_row(
                    "SELECT group_concat(config_enc) FROM notification_channels",
                    [],
                    |r| r.get(0),
                )
                .map_err(err)
            })
            .await
            .unwrap();
        assert!(!raw.contains("TOKEN123"));
        // validation
        assert!(
            create_channel(
                &db,
                &key,
                NewChannel {
                    name: "x".into(),
                    kind: "smtp".into(),
                    config: json!({}),
                    events: None,
                    enabled: None
                }
            )
            .await
            .is_err()
        );
        assert!(
            create_channel(
                &db,
                &key,
                NewChannel {
                    name: "x".into(),
                    kind: "log".into(),
                    config: json!({}),
                    events: Some(vec!["nope".into()]),
                    enabled: None
                }
            )
            .await
            .is_err()
        );
        assert!(delete_channel(&db, &tg.id).await.unwrap());
        assert_eq!(list_channels(&db, &key).await.unwrap().len(), 1);
        assert!(unread_count(&db).await.unwrap() >= 2);
        assert!(mark_read(&db, None).await.unwrap() >= 2);
        assert_eq!(unread_count(&db).await.unwrap(), 0);
        db.close();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn webhook_urls_are_masked_and_metadata_refused() {
        assert_eq!(
            mask_url("https://hooks.slack.com/services/T0/B0/xyzSECRET"),
            "https://hooks.slack.com/***"
        );
        assert_eq!(
            mask_url("http://127.0.0.1:8644/webhooks/siloforge?k=SECRET"),
            "http://127.0.0.1:8644/***"
        );
        assert_eq!(mask_url("https://ntfy.sh"), "https://ntfy.sh");
        let m =
            mask_config(&json!({"url": "https://discord.com/api/webhooks/1/TOKEN", "secret": "s"}));
        assert!(!m.to_string().contains("TOKEN") && m["secret"] == "***");
        for bad in [
            "http://169.254.169.254/latest/meta-data",
            "http://[fd00:ec2::254]/",
            "http://metadata.google.internal/computeMetadata/v1/",
        ] {
            assert!(
                validate_config("webhook", &json!({"url": bad})).is_err(),
                "{bad}"
            );
        }
        assert!(validate_config("hermes", &json!({"url": "http://127.0.0.1:8644/x"})).is_ok());
    }

    #[tokio::test]
    async fn metadata_guard_blocks_at_delivery_and_desktop_is_honest() {
        assert!(metadata_guard("http://169.254.169.254/").await.is_err());
        assert!(metadata_guard("http://127.0.0.1:9/").await.is_ok());
        if !cfg!(windows) {
            let e = desktop_toast("t", "b").err().map(|f| f.msg);
            assert_eq!(e.as_deref(), Some(DESKTOP_UNSUPPORTED));
        }
    }

    #[tokio::test]
    async fn masked_url_echo_keeps_stored_url() {
        let (db, _dir, key) = setup();
        let ch = create_channel(
            &db,
            &key,
            NewChannel {
                name: "w".into(),
                kind: "webhook".into(),
                config: json!({"url": "https://hooks.example.test/services/SECRET"}),
                events: Some(vec!["*".into()]),
                enabled: Some(true),
            },
        )
        .await
        .unwrap();
        let shown = ch.public_json()["config"]["url"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(shown, "https://hooks.example.test/***");
        let up = update_channel(
            &db,
            &key,
            &ch.id,
            ChannelPatch {
                name: None,
                config: Some(json!({"url": shown})),
                events: None,
                enabled: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            up.config["url"],
            "https://hooks.example.test/services/SECRET"
        );
    }
}
