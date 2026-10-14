//! Test harness: temp DB, a stub WordPress REST server (raw Gutenberg + rendered, `context=edit`,
//! POST updates with `modified_gmt`), a deterministic fake embedder and job helpers.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sf_core::Config;
use sf_core::secrets::SiteSecrets;
use sf_core::types::JobKind;
use sf_embed::{ChainOptions, EmbedChain, EmbedFuture, Embedder, EmbedderKind};
use sf_jobs::{Job, JobContext, JobQueue, NewJob};
use sf_store::{Db, err};

// ---------------------------------------------------------------------------------------------
// stub WordPress
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct StubPost {
    pub id: u64,
    pub slug: String,
    pub title: String,
    pub raw: String,
    pub modified: u32,
}

#[derive(Default)]
pub struct WpState {
    pub base: String,
    pub posts: BTreeMap<u64, StubPost>,
    /// post id -> remaining "bump modified after serving a GET" count (conflict simulation)
    pub bump_on_get: HashMap<u64, u32>,
    pub updates: u32,
    /// Apply the next N updates but answer 500 (WordPress committed, response lost: R4).
    pub fail_after_update: u32,
}

pub type Wp = Arc<Mutex<WpState>>;

pub fn rendered(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(i) = rest.find("<!--") {
        out.push_str(&rest[..i]);
        match rest[i..].find("-->") {
            Some(j) => rest = &rest[i + j + 3..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn modified_gmt(m: u32) -> String {
    format!(
        "2026-10-01T{:02}:{:02}:{:02}",
        m / 3600 % 24,
        m / 60 % 60,
        m % 60
    )
}

fn post_json(base: &str, p: &StubPost, edit: bool) -> Value {
    let mut content = json!({"rendered": rendered(&p.raw)});
    if edit {
        content["raw"] = json!(p.raw);
    }
    json!({
        "id": p.id, "slug": p.slug, "link": format!("{base}/{}/", p.slug),
        "title": {"rendered": p.title}, "content": content, "excerpt": {"rendered": ""},
        "type": "post", "status": "publish", "date_gmt": "2026-01-01T00:00:00",
        "modified_gmt": modified_gmt(p.modified),
    })
}

fn authed(h: &HeaderMap) -> bool {
    h.get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("Basic "))
}

async fn list_posts(
    State(st): State<Wp>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let edit = q.get("context").map(String::as_str) == Some("edit");
    if edit && !authed(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"code": "rest_forbidden_context"})),
        )
            .into_response();
    }
    let per: usize = q
        .get("per_page")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10)
        .max(1);
    let page: usize = q
        .get("page")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1);
    let s = st.lock();
    let all: Vec<&StubPost> = s.posts.values().collect();
    let total_pages = all.len().div_ceil(per).max(1);
    if page > total_pages {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"code": "rest_post_invalid_page_number"})),
        )
            .into_response();
    }
    let items: Vec<Value> = all
        .iter()
        .skip((page - 1) * per)
        .take(per)
        .map(|p| post_json(&s.base, p, edit))
        .collect();
    let mut h = HeaderMap::new();
    h.insert("x-wp-totalpages", total_pages.to_string().parse().unwrap());
    h.insert("x-wp-total", all.len().to_string().parse().unwrap());
    (h, Json(Value::Array(items))).into_response()
}

async fn list_pages() -> Response {
    let mut h = HeaderMap::new();
    h.insert("x-wp-totalpages", "0".parse().unwrap());
    (h, Json(json!([]))).into_response()
}

async fn get_post(State(st): State<Wp>, Path(id): Path<u64>, headers: HeaderMap) -> Response {
    if !authed(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut s = st.lock();
    let base = s.base.clone();
    let Some(p) = s.posts.get(&id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let body = post_json(&base, &p, true);
    let bump = s.bump_on_get.get(&id).copied().unwrap_or(0);
    if bump > 0 {
        s.bump_on_get.insert(id, bump - 1);
        if let Some(pm) = s.posts.get_mut(&id) {
            pm.modified += 1;
        }
    }
    Json(body).into_response()
}

async fn update_post(
    State(st): State<Wp>,
    Path(id): Path<u64>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !authed(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut s = st.lock();
    let base = s.base.clone();
    s.updates += 1;
    let Some(p) = s.posts.get_mut(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(c) = body.get("content").and_then(Value::as_str) {
        p.raw = c.to_string();
    }
    p.modified += 1;
    let out = post_json(&base, p, true);
    if s.fail_after_update > 0 {
        s.fail_after_update -= 1;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Json(out).into_response()
}

async fn public_page(State(st): State<Wp>, Path(slug): Path<String>) -> Response {
    let s = st.lock();
    match s.posts.values().find(|p| p.slug == slug) {
        Some(p) => Html(format!(
            "<html><head><title>{}</title></head><body><nav><a href=\"/\">Home</a></nav><article class=\"entry-content\">{}</article></body></html>",
            p.title,
            rendered(&p.raw)
        ))
        .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Start the stub; returns its base URL and shared state.
pub async fn start_wp(posts: Vec<StubPost>) -> (String, Wp) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://127.0.0.1:{}", addr.port());
    let mut stmap = BTreeMap::new();
    for p in posts {
        stmap.insert(p.id, p);
    }
    let state: Wp = Arc::new(Mutex::new(WpState {
        base: base.clone(),
        posts: stmap,
        ..Default::default()
    }));
    let app = Router::new()
        .route("/wp-json/wp/v2/posts", get(list_posts))
        .route("/wp-json/wp/v2/pages", get(list_pages))
        .route("/wp-json/wp/v2/posts/{id}", get(get_post).post(update_post))
        .route("/{slug}/", get(public_page))
        .with_state(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base, state)
}

fn para(text: &str) -> String {
    format!("<!-- wp:paragraph -->\n<p>{text}</p>\n<!-- /wp:paragraph -->\n\n")
}

/// Six bike posts with Gutenberg raw content. Post 2 already links to post 1.
pub fn bike_posts(base: &str) -> Vec<StubPost> {
    let mk = |id: u64, slug: &str, title: &str, paras: &[String]| StubPost {
        id,
        slug: slug.into(),
        title: title.into(),
        raw: format!(
            "<!-- wp:heading -->\n<h2>{title}</h2>\n<!-- /wp:heading -->\n\n{}",
            paras.concat()
        ),
        modified: 1,
    };
    vec![
        mk(1, "bike-locks", "Best Bike Locks for City Riders", &[
            para("A heavy u-lock is the heart of good bike locks, and every city rider should pair it with bright bike lights for night riding."),
            para("Before locking up, many riders first clean the bike chain so the drivetrain stays quiet and the bike chain cleaning routine stays short."),
            para("Thieves target weak cable locks, so serious riders combine a folding lock with sturdy floor bike pumps kept safely at home."),
        ]),
        mk(2, "bike-chain-cleaning", "How to Clean a Bike Chain", &[
            para("Regular bike chain cleaning keeps shifting smooth, and a clean drivetrain also makes mountain bike tires feel faster on long climbs."),
            para("Park your bike next to a rack and secure it with <a href=\"BASE/bike-locks/\">bike locks</a> while the degreaser soaks into every link."),
            para("Dry the chain with a rag, add a light lubricant and check your road bike saddles for loose bolts while you wait for the lube."),
        ]),
        mk(3, "mountain-bike-tires", "Mountain Bike Tires Explained", &[
            para("Wide mountain bike tires grip loose gravel, but they need the right pressure, so keep floor bike pumps with a precise gauge in the garage."),
            para("Muddy trails coat the drivetrain, which is why bike chain cleaning after every ride matters as much as choosing good mountain bike tires."),
            para("Riders who stay out after sunset should mount bike lights for night riding before the trail gets dark and technical."),
        ]),
        mk(4, "road-bike-saddles", "Choosing Road Bike Saddles", &[
            para("Comfortable road bike saddles reduce numbness on long rides, and a proper fit matters more than padding or price for most riders."),
            para("Commuters who leave their bikes outside should protect expensive road bike saddles with good bike locks and a saddle cable."),
            para("A quiet bike chain cleaning habit and well inflated tires make every saddle feel better on rough city roads."),
        ]),
        mk(5, "bike-lights", "Bike Lights for Night Riding", &[
            para("Bright bike lights for night riding make you visible to drivers, and rechargeable lamps last for weeks of city commuting."),
            para("Mount the front lamp on the handlebar and check that wide mountain bike tires do not block the beam on rough trail descents."),
            para("Store your lights indoors with your floor bike pumps so the batteries stay warm during the cold winter months."),
        ]),
        mk(6, "bike-pumps", "Floor Bike Pumps Compared", &[
            para("Good floor bike pumps have a stable base and an accurate gauge, which matters for skinny road tires and wide mountain bike tires alike."),
            para("Keep the pump next to your chain lube so bike chain cleaning and tire checks become one quick routine before every ride."),
            para("A pump with a long hose also helps when you set up road bike saddles and need to lean the bike against the wall."),
        ]),
    ]
    .into_iter()
    .map(|mut p| {
        p.raw = p.raw.replace("BASE", base);
        p
    })
    .collect()
}

// ---------------------------------------------------------------------------------------------
// stub AI (OpenAI-compatible): rewrites the paragraph by appending the requested link
// ---------------------------------------------------------------------------------------------

async fn stub_chat(Json(body): Json<Value>) -> Json<Value> {
    let user = body["messages"]
        .as_array()
        .and_then(|m| m.iter().rev().find(|x| x["role"] == "user"))
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string();
    let link = user
        .find("<a href=")
        .and_then(|i| {
            user[i..]
                .find("</a>")
                .map(|j| user[i..i + j + 4].to_string())
        })
        .unwrap_or_default()
        // link a slightly longer phrase than asked (still contains the anchor)
        .replace("</a>", " guide</a>");
    let para = user
        .split(
            "Paragraph:
",
        )
        .nth(1)
        .unwrap_or("")
        .trim()
        .to_string();
    let reply = match para.rfind("</p>") {
        Some(i) => format!(
            "```html
{} See also {link}.</p>
```",
            &para[..i]
        ),
        None => format!("{para} See also {link}."),
    };
    Json(json!({
        "id": "x", "object": "chat.completion", "model": "stub-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": reply}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 10}
    }))
}

/// Start the stub AI; returns its `/v1` base URL.
pub async fn start_ai() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route("/v1/chat/completions", axum::routing::post(stub_chat));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://127.0.0.1:{port}/v1")
}

// ---------------------------------------------------------------------------------------------
// fake embedder
// ---------------------------------------------------------------------------------------------

/// Hashed bag-of-words → 384-d unit vector (deterministic). Optional per-batch delay.
pub struct FakeEmbedder {
    pub delay: Duration,
}

pub fn fake_vector(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; 384];
    v[0] = 0.05;
    for w in text
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() > 2)
    {
        let mut h: u32 = 2166136261;
        for b in w.bytes() {
            h ^= u32::from(b);
            h = h.wrapping_mul(16777619);
        }
        v[(h % 383 + 1) as usize] += 1.0;
    }
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().map(|x| x / n).collect()
}

impl Embedder for FakeEmbedder {
    fn id(&self) -> &str {
        "fake"
    }
    fn kind(&self) -> EmbedderKind {
        EmbedderKind::Remote
    }
    fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(async move {
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            Ok(texts.iter().map(|t| fake_vector(t)).collect())
        })
    }
}

// ---------------------------------------------------------------------------------------------
// context + helpers
// ---------------------------------------------------------------------------------------------

pub struct Env {
    pub ctx: Arc<JobContext>,
    pub db: Db,
    pub dir: PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        self.db.close();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn test_dir(tag: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("sf-jobs-{tag}-{}", sf_core::new_id()))
}

pub fn env_with(tag: &str, embed_delay: Duration, batch: usize) -> Env {
    env_full(tag, embed_delay, batch, None)
}

/// Env whose LLM calls go to the stub AI at `ai_url` (OpenAI-compatible env fallback rung).
pub fn env_with_ai(tag: &str, ai_url: &str) -> Env {
    env_full(tag, Duration::ZERO, 16, Some(ai_url.to_string()))
}

/// Env with a custom embedder (no AI).
pub fn env_with_embedder(tag: &str, embedder: Arc<dyn Embedder>) -> Env {
    env_inner(tag, embedder, 16, None)
}

fn env_full(tag: &str, embed_delay: Duration, batch: usize, ai_url: Option<String>) -> Env {
    env_inner(
        tag,
        Arc::new(FakeEmbedder { delay: embed_delay }),
        batch,
        ai_url,
    )
}

fn env_inner(tag: &str, embedder: Arc<dyn Embedder>, batch: usize, ai_url: Option<String>) -> Env {
    env_cfg_inner(tag, embedder, batch, ai_url, |_| {})
}

/// Env with the stub AI at `ai_url` and a config tweak (`[ai]` settings).
#[allow(dead_code)]
pub fn env_ai_cfg(tag: &str, ai_url: &str, tweak: impl FnOnce(&mut Config)) -> Env {
    env_cfg_inner(
        tag,
        Arc::new(FakeEmbedder {
            delay: Duration::ZERO,
        }),
        16,
        Some(ai_url.to_string()),
        tweak,
    )
}

/// Default env with a config tweak (e.g. the install-wide GSC fallback).
#[allow(dead_code)]
pub fn env_cfg(tag: &str, tweak: impl FnOnce(&mut Config)) -> Env {
    env_cfg_inner(
        tag,
        Arc::new(FakeEmbedder {
            delay: Duration::ZERO,
        }),
        16,
        None,
        tweak,
    )
}

fn env_cfg_inner(
    tag: &str,
    embedder: Arc<dyn Embedder>,
    batch: usize,
    ai_url: Option<String>,
    tweak: impl FnOnce(&mut Config),
) -> Env {
    let dir = test_dir(tag);
    let db = Db::open(&dir.join("t.db")).unwrap();
    let mut cfg = Config {
        home: dir.clone(),
        ..Config::default()
    };
    cfg.auth.jwt_secret = "test-secret-0123456789".into();
    if let Some(url) = &ai_url {
        cfg.llm.fallback_url = url.clone();
        cfg.llm.fallback_models = vec!["stub-model".into()];
    }
    tweak(&mut cfg);
    let cfg = Arc::new(cfg);
    let embed = Arc::new(EmbedChain::new(
        vec![embedder],
        ChainOptions {
            batch_size: batch,
            ..ChainOptions::default()
        },
    ));
    let key = Arc::new(sf_core::crypto::derive_secret_key("test-master-key").unwrap());
    let mut ctx = JobContext::new(
        cfg.clone(),
        db.clone(),
        JobQueue::new(db.clone()),
        embed,
        key.clone(),
    );
    if ai_url.is_some() {
        // v2.3: the AI layer reads `[llm]` and `[ai]` from the live config (tests change it)
        ctx.ai = Some(Arc::new(
            sf_ai::Ai::with_live(db.clone(), key, ctx.live.clone()).unwrap(),
        ));
    }
    ctx.http.politeness_delay = Duration::ZERO;
    ctx.http.allow_private = true;
    ctx.http.max_retries = 0;
    ctx.verify.attempts = 1;
    ctx.verify.backoff = Duration::from_millis(1);
    ctx.link_check.interval_cap = 1000;
    ctx.link_check.interval = Duration::from_millis(10);
    Env {
        ctx: Arc::new(ctx),
        db,
        dir,
    }
}

pub fn env(tag: &str) -> Env {
    env_with(tag, Duration::ZERO, 16)
}

/// Insert a wp_rest site pointing at `base` (with an application password when `creds`).
pub async fn add_site(e: &Env, base: &str, creds: bool, settings: Value) -> String {
    let id = sf_core::new_id();
    let secrets = if creds {
        SiteSecrets {
            wp_user: Some("admin".into()),
            wp_app_password: Some("abcd efgh ijkl".into()),
            gsc_credentials: None,
        }
        .seal(&e.ctx.secret_key)
        .unwrap()
    } else {
        String::new()
    };
    let (i, b) = (id.clone(), base.to_string());
    e.db.write(move |t| {
        t.execute(
            "INSERT INTO sites(id, domain, wp_url, mode, api_key, settings, secrets_enc) VALUES(?1,?6,?2,'wp_rest',?3,?4,?5)",
            rusqlite::params![
                i,
                b,
                format!("sf_{i}"),
                settings.to_string(),
                secrets,
                b.trim_start_matches("http://").to_string()
            ],
        )
        .map_err(err)?;
        Ok(())
    })
    .await
    .unwrap();
    id
}

/// Enqueue + claim + run a pipeline inline; finishes the job row like the runner would.
pub async fn run_job(e: &Env, kind: JobKind, site: &str, payload: Value) -> Value {
    let mut nj = NewJob::new(kind.clone(), Some(site), payload);
    nj.dedupe = Some(false);
    let enq = e.ctx.queue.enqueue(nj).await.unwrap();
    let claimed = match e.ctx.queue.claim_next(true).await.unwrap() {
        Some(c) if c.id == enq.job_run_id => c,
        Some(other) => panic!("unexpected job claimed: {}", other.id),
        None => panic!("job not claimable"),
    };
    let job = Arc::new(Job::new(claimed, e.ctx.queue.clone()));
    let res = sf_jobs::pipelines::dispatch(e.ctx.clone(), job.clone()).await;
    match res {
        Ok(v) => {
            e.ctx
                .queue
                .finish(&job.id, v.clone(), json!({}))
                .await
                .unwrap();
            v
        }
        Err(err) => {
            let _ = e
                .ctx
                .queue
                .fail(&job.id, &err.to_string(), false, json!({}))
                .await;
            panic!("{kind} failed: {err}");
        }
    }
}

pub async fn q_i64(db: &Db, sql: &str, p: Vec<String>) -> i64 {
    let sql = sql.to_string();
    db.read(move |c| {
        c.query_row(&sql, rusqlite::params_from_iter(p.iter()), |r| r.get(0))
            .map_err(err)
    })
    .await
    .unwrap()
}

pub async fn q_rows(db: &Db, sql: &str, p: Vec<String>) -> Vec<Vec<Value>> {
    let sql = sql.to_string();
    db.read(move |c| {
        let mut st = c.prepare(&sql).map_err(err)?;
        let n = st.column_count();
        let rows = st
            .query_map(rusqlite::params_from_iter(p.iter()), |r| {
                let mut v = Vec::with_capacity(n);
                for i in 0..n {
                    let val: rusqlite::types::Value = r.get(i)?;
                    v.push(match val {
                        rusqlite::types::Value::Null => Value::Null,
                        rusqlite::types::Value::Integer(i) => json!(i),
                        rusqlite::types::Value::Real(f) => json!(f),
                        rusqlite::types::Value::Text(s) => json!(s),
                        rusqlite::types::Value::Blob(b) => json!(b.len()),
                    });
                }
                Ok(v)
            })
            .map_err(err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(err)?;
        Ok(rows)
    })
    .await
    .unwrap()
}

pub async fn exec(db: &Db, sql: &str, p: Vec<String>) -> usize {
    let sql = sql.to_string();
    db.write(move |t| {
        t.execute(&sql, rusqlite::params_from_iter(p.iter()))
            .map_err(err)
    })
    .await
    .unwrap()
}

pub fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}
