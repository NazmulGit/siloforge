//! v2.2 Link Runs end to end through the real router + job runner against a stub WordPress
//! (never a real site): create a run (in_text + semantic, mode opportunity) -> ready -> items
//! grouped by post -> deploy -> applied + verified -> a post is edited so one link is gone
//! (not_present) -> repair -> applied -> undo run -> every post byte-identical to the original.
//! Plus ownership (404 for another user), status conflicts (409) and the Auto-Linker settings.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sf_core::Config;
use sf_core::crypto::{sign_jwt, user_claims};
use sf_core::secrets::SiteSecrets;
use sf_embed::{ChainOptions, EmbedChain, EmbedFuture, Embedder, EmbedderKind};
use sf_jobs::{JobContext, Runner, RunnerOptions};
use sf_server::AppState;
use sf_store::Db;
use tower::ServiceExt;

const SECRET: &str = "test-jwt-secret-0123456789abcdef";

// ------------------------------------------------------------------ stub WordPress

#[derive(Clone)]
struct Post {
    id: u64,
    slug: String,
    title: String,
    raw: String,
    modified: u32,
}

struct Wp {
    base: String,
    posts: BTreeMap<u64, Post>,
}
type W = Arc<Mutex<Wp>>;

fn rendered(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(i) = rest.find("<!--") {
        out.push_str(&rest[..i]);
        match rest[i..].find("-->") {
            Some(j) => rest = &rest[i + j + 3..],
            None => rest = "",
        }
    }
    out.push_str(rest);
    out
}

fn post_json(base: &str, p: &Post, edit: bool) -> Value {
    let mut content = json!({"rendered": rendered(&p.raw)});
    if edit {
        content["raw"] = json!(p.raw);
    }
    json!({
        "id": p.id, "slug": p.slug, "link": format!("{base}/{}/", p.slug),
        "title": {"rendered": p.title}, "content": content, "excerpt": {"rendered": ""},
        "type": "post", "status": "publish", "date_gmt": "2026-01-01T00:00:00",
        "modified_gmt": format!("2026-10-01T00:{:02}:{:02}", p.modified / 60 % 60, p.modified % 60),
    })
}

fn authed(h: &HeaderMap) -> bool {
    h.get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("Basic "))
}

async fn list_posts(
    State(st): State<W>,
    Query(q): Query<HashMap<String, String>>,
    h: HeaderMap,
) -> Response {
    let edit = q.get("context").map(String::as_str) == Some("edit");
    if edit && !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let page: usize = q.get("page").and_then(|v| v.parse().ok()).unwrap_or(1);
    let s = st.lock();
    if page > 1 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"code": "rest_post_invalid_page_number"})),
        )
            .into_response();
    }
    let items: Vec<Value> = s
        .posts
        .values()
        .map(|p| post_json(&s.base, p, edit))
        .collect();
    let mut hm = HeaderMap::new();
    hm.insert("x-wp-totalpages", "1".parse().unwrap());
    hm.insert("x-wp-total", items.len().to_string().parse().unwrap());
    (hm, Json(Value::Array(items))).into_response()
}

async fn list_pages() -> Response {
    let mut h = HeaderMap::new();
    h.insert("x-wp-totalpages", "0".parse().unwrap());
    (h, Json(json!([]))).into_response()
}

async fn get_post(State(st): State<W>, Path(id): Path<u64>, h: HeaderMap) -> Response {
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let s = st.lock();
    match s.posts.get(&id) {
        Some(p) => Json(post_json(&s.base, p, true)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn update_post(
    State(st): State<W>,
    Path(id): Path<u64>,
    h: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !authed(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut s = st.lock();
    let base = s.base.clone();
    let Some(p) = s.posts.get_mut(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(c) = body.get("content").and_then(Value::as_str) {
        p.raw = c.to_string();
    }
    p.modified += 1;
    Json(post_json(&base, p, true)).into_response()
}

async fn public_page(State(st): State<W>, Path(slug): Path<String>) -> Response {
    let s = st.lock();
    match s.posts.values().find(|p| p.slug == slug) {
        Some(p) => Html(format!(
            "<html><body><article class=\"entry-content\">{}</article></body></html>",
            rendered(&p.raw)
        ))
        .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn para(t: &str) -> String {
    format!("<!-- wp:paragraph -->\n<p>{t}</p>\n<!-- /wp:paragraph -->\n\n")
}

fn posts() -> Vec<Post> {
    let mk = |id: u64, slug: &str, title: &str, ps: &[&str]| Post {
        id,
        slug: slug.into(),
        title: title.into(),
        raw: format!(
            "<!-- wp:heading -->\n<h2>{title}</h2>\n<!-- /wp:heading -->\n\n{}",
            ps.iter().map(|p| para(p)).collect::<String>()
        ),
        modified: 1,
    };
    vec![
        mk(
            1,
            "bike-locks",
            "Best Bike Locks for City Riders",
            &[
                "A heavy u-lock is the heart of good bike locks, and every city rider should pair it with bright bike lights for night riding.",
                "Before locking up, many riders first clean the bike chain so the drivetrain stays quiet and the bike chain cleaning routine stays short.",
                "Thieves target weak cable locks, so serious riders combine a folding lock with sturdy floor bike pumps kept safely at home.",
            ],
        ),
        mk(
            2,
            "bike-chain-cleaning",
            "How to Clean a Bike Chain",
            &[
                "Regular bike chain cleaning keeps shifting smooth, and a clean drivetrain also makes mountain bike tires feel faster on long climbs.",
                "Dry the chain with a rag, add a light lubricant and check your road bike saddles for loose bolts while you wait for the lube.",
            ],
        ),
        mk(
            3,
            "mountain-bike-tires",
            "Mountain Bike Tires Explained",
            &[
                "Wide mountain bike tires grip loose gravel, but they need the right pressure, so keep floor bike pumps with a precise gauge in the garage.",
                "Riders who stay out after sunset should mount bike lights for night riding before the trail gets dark and technical.",
            ],
        ),
        mk(
            4,
            "road-bike-saddles",
            "Choosing Road Bike Saddles",
            &[
                "Comfortable road bike saddles reduce numbness on long rides, and a proper fit matters more than padding or price for most riders.",
                "Commuters who leave their bikes outside should protect expensive road bike saddles with good bike locks and a saddle cable.",
            ],
        ),
        mk(
            5,
            "bike-lights",
            "Bike Lights for Night Riding",
            &[
                "Bright bike lights for night riding make you visible to drivers, and rechargeable lamps last for weeks of city commuting.",
                "Store your lights indoors with your floor bike pumps so the batteries stay warm during the cold winter months.",
            ],
        ),
        mk(
            6,
            "bike-pumps",
            "Floor Bike Pumps Compared",
            &[
                "Good floor bike pumps have a stable base and an accurate gauge, which matters for skinny road tires and wide mountain bike tires alike.",
                "Keep the pump next to your chain lube so bike chain cleaning and tire checks become one quick routine before every ride.",
            ],
        ),
    ]
}

async fn start_wp() -> (String, W) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let state: W = Arc::new(Mutex::new(Wp {
        base: base.clone(),
        posts: posts().into_iter().map(|p| (p.id, p)).collect(),
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

// ------------------------------------------------------------------ fake embedder

struct Fake;

fn fake_vector(text: &str) -> Vec<f32> {
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

impl Embedder for Fake {
    fn id(&self) -> &str {
        "fake"
    }
    fn kind(&self) -> EmbedderKind {
        EmbedderKind::Remote
    }
    fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(async move { Ok(texts.iter().map(|t| fake_vector(t)).collect()) })
    }
}

// ------------------------------------------------------------------ harness

struct H {
    app: Router,
    u1: String,
    u2: String,
    dir: std::path::PathBuf,
    db: Db,
}

impl H {
    async fn call(
        &self,
        method: &str,
        path: &str,
        tok: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {tok}"));
        let body = match body {
            Some(v) => {
                b = b.header(header::CONTENT_TYPE, "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let res = self
            .app
            .clone()
            .oneshot(b.body(body).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 64 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Poll the run until `done(run)` (60 s max).
    async fn wait(&self, run: &str, done: impl Fn(&Value) -> bool) -> Value {
        let t = Instant::now();
        loop {
            let (s, r) = self
                .call("GET", &format!("/api/v2/link-runs/{run}"), &self.u1, None)
                .await;
            assert_eq!(s, StatusCode::OK, "{r}");
            if done(&r) {
                return r;
            }
            assert!(
                t.elapsed() < Duration::from_secs(60),
                "timed out waiting: {r}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn idle(&self, run: &str) -> Value {
        self.wait(run, |r| r["job_running"] == false).await
    }

    async fn items(&self, run: &str) -> Vec<Value> {
        let (s, v) = self
            .call(
                "GET",
                &format!("/api/v2/link-runs/{run}/items?limit=1000"),
                &self.u1,
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK);
        v["items"].as_array().unwrap().clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn link_run_e2e_through_the_api() {
    let (base, wp) = start_wp().await;
    let original: HashMap<u64, String> = wp
        .lock()
        .posts
        .values()
        .map(|p| (p.id, p.raw.clone()))
        .collect();
    let dir = std::env::temp_dir().join(format!("sf-lr-e2e-{}", sf_core::new_id()));
    let db = Db::open(&dir.join("t.db")).unwrap();
    let mut cfg = Config {
        home: dir.clone(),
        ..Default::default()
    };
    cfg.auth.jwt_secret = SECRET.into();
    let st = AppState::new(cfg, db.clone(), [7u8; 32]);
    let site_id = sf_core::new_id();
    let secrets = SiteSecrets {
        wp_user: Some("admin".into()),
        wp_app_password: Some("abcd efgh ijkl".into()),
        gsc_credentials: None,
    }
    .seal(&st.secret_key)
    .unwrap();
    {
        let (sid, b) = (site_id.clone(), base.clone());
        db.write(move |t| {
            t.execute_batch(
                "INSERT INTO users(id,email,password_hash,role) VALUES('u1','u1@x.io','x','user');
                 INSERT INTO users(id,email,password_hash,role) VALUES('u2','u2@x.io','x','user');",
            )
            .map_err(sf_store::err)?;
            t.execute(
                "INSERT INTO sites(id, domain, wp_url, mode, api_key, settings, secrets_enc, user_id)
                 VALUES(?1, ?2, ?3, 'wp_rest', ?4, '{\"allow_private_host\": true}', ?5, 'u1')",
                rusqlite::params![sid, b.trim_start_matches("http://"), b, format!("k_{sid}"), secrets],
            )
            .map_err(sf_store::err)?;
            Ok(())
        })
        .await
        .unwrap();
    }
    // job runner on the same DB + queue
    let embed = Arc::new(EmbedChain::new(
        vec![Arc::new(Fake)],
        ChainOptions::default(),
    ));
    let mut ctx = JobContext::new(
        st.cfg.clone(),
        db.clone(),
        st.jobs.clone(),
        embed,
        st.secret_key.clone(),
    );
    ctx.http.politeness_delay = Duration::ZERO;
    ctx.http.allow_private = true;
    ctx.http.max_retries = 0;
    ctx.verify.attempts = 1;
    ctx.verify.backoff = Duration::from_millis(1);
    let ctx = Arc::new(ctx);
    let mut ro = RunnerOptions::from_config(&ctx.cfg);
    ro.follow_live = false;
    ro.max_concurrent = 2;
    ro.poll = Duration::from_millis(50);
    ro.heartbeat = Duration::from_millis(200);
    let runner = Runner::start_with(ctx.clone(), ro);
    let tok = |id: &str| sign_jwt(SECRET, &user_claims("user", id, 0)).unwrap();
    let h = H {
        app: sf_server::router(st.clone(), None),
        u1: tok("u1"),
        u2: tok("u2"),
        dir: dir.clone(),
        db: db.clone(),
    };

    // ---- opportunities
    let (s, o) = h
        .call(
            "GET",
            &format!("/api/v2/sites/{site_id}/opportunities"),
            &h.u1,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{o}");
    assert!(o["counts"].get("semantic_pairs").is_some() && o["by_strategy"].is_object());
    assert_eq!(o["last_crawl_at"], Value::Null);
    // f32 threshold reported as typed (was 0.550000011920929)
    assert_eq!(o["by_strategy"]["semantic"]["threshold"], json!(0.55));
    let (s, _) = h
        .call(
            "GET",
            &format!("/api/v2/sites/{site_id}/opportunities"),
            &h.u2,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // ---- create + run to ready
    let options = json!({
        "crawl": "always", "mode": "opportunity",
        "strategies": {"in_text": true, "contextual": false, "semantic": true, "keyword_rules": false,
                       "silo": false, "orphan_rescue": true, "money_pages": false, "fix_broken": false},
        "filters": {"min_score": 0.0, "max_total": 100, "max_new_per_post": 3, "max_links_per_post": 10,
                    "anchor_types": ["exact","partial","lsi","branded","generic","naked","cta"], "max_exact_share": 1.0},
    });
    let (s, bad) = h
        .call(
            "POST",
            &format!("/api/v2/sites/{site_id}/link-runs"),
            &h.u1,
            Some(json!({"options": {"mode": "zigzag"}})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
    let (s, c) = h
        .call(
            "POST",
            &format!("/api/v2/sites/{site_id}/link-runs"),
            &h.u1,
            Some(json!({"name": "E2E", "options": options})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    let run = c["run"]["id"].as_str().unwrap().to_string();
    assert!(c["job_run_id"].is_string());
    let (s, _) = h
        .call(
            "POST",
            &format!("/api/v2/sites/{site_id}/link-runs"),
            &h.u1,
            Some(json!({"options": options})),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "one active run per site");
    let r = h
        .wait(&run, |r| r["status"] == "ready" || r["status"] == "failed")
        .await;
    assert_eq!(r["status"], "ready", "{r}");
    assert_eq!(r["progress"]["pct"], 100);
    let (_, job) = h
        .call(
            "GET",
            &format!("/api/v2/jobs/{}", r["plan_job_id"].as_str().unwrap()),
            &h.u1,
            None,
        )
        .await;
    let (_, logs) = h
        .call(
            "GET",
            &format!("/api/v2/jobs/{}/logs", r["plan_job_id"].as_str().unwrap()),
            &h.u1,
            None,
        )
        .await;
    assert!(
        r["stats"]["opportunities"].as_i64().unwrap() >= 2,
        "{r}
{job}
{logs}"
    );
    // another user cannot see it
    let (s, _) = h
        .call("GET", &format!("/api/v2/link-runs/{run}"), &h.u2, None)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, l) = h
        .call(
            "GET",
            &format!("/api/v2/sites/{site_id}/link-runs"),
            &h.u1,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(l["runs"][0]["id"], run.as_str());

    // ---- items grouped by post
    let (s, g) = h
        .call(
            "GET",
            &format!("/api/v2/link-runs/{run}/items?group=post"),
            &h.u1,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{g}");
    let groups = g["posts"].as_array().unwrap();
    assert!(!groups.is_empty());
    for gp in groups {
        for l in gp["links"].as_array().unwrap() {
            assert_eq!(l["source"]["id"], gp["post"]["id"]);
            assert_eq!(l["strategy"], "semantic");
            assert_eq!(l["insert_module"], "native");
            assert!(l["open_url"].as_str().unwrap().contains("#:~:text="));
            assert!(l["alternates"].is_array());
        }
    }
    let (s, _) = h
        .call(
            "GET",
            &format!("/api/v2/link-runs/{run}/items?status=bogus"),
            &h.u1,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // edit one anchor with a non-existent alternate -> 400; untick + re-tick round trip
    let items = h.items(&run).await;
    let e0 = items[0]["edge_id"].as_str().unwrap().to_string();
    let (s, _) = h
        .call(
            "PATCH",
            &format!("/api/v2/link-runs/{run}/items/{e0}"),
            &h.u1,
            Some(json!({"alternate": "no such anchor"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // an in-text run refuses an anchor that is not in the paragraph (E2E 2.2) ...
    let (s, b) = h
        .call(
            "PATCH",
            &format!("/api/v2/link-runs/{run}/items/{e0}"),
            &h.u1,
            Some(json!({"anchor": "Totally Unrelated Phrase Xyz"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    // ... and the PATCH response carries the edited item itself
    let a0 = items[0]["anchor"].as_str().unwrap().to_string();
    let (s, b) = h
        .call(
            "PATCH",
            &format!("/api/v2/link-runs/{run}/items/{e0}"),
            &h.u1,
            Some(json!({"anchor": format!("  {a0} ")})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["item"]["edge_id"], json!(e0), "{b}");
    assert_eq!(b["item"]["anchor"], json!(a0), "{b}");
    // "By" in Runs history shows a name, not the actor id
    assert!(b["run"]["created_by_name"].is_string(), "{b}");
    let (s, sel) = h
        .call(
            "POST",
            &format!("/api/v2/link-runs/{run}/select"),
            &h.u1,
            Some(json!({"edge_ids": [e0], "deselect": true})),
        )
        .await;
    assert_eq!((s, sel["changed"].clone()), (StatusCode::OK, json!(1)));
    let (s, _) = h
        .call(
            "POST",
            &format!("/api/v2/link-runs/{run}/select"),
            &h.u1,
            Some(json!({"all": true})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);

    // ---- deploy -> applied + verified
    let (s, d) = h
        .call(
            "POST",
            &format!("/api/v2/link-runs/{run}/deploy"),
            &h.u1,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{d}");
    let (s, _) = h
        .call(
            "POST",
            &format!("/api/v2/link-runs/{run}/deploy"),
            &h.u1,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "deploy needs ready");
    let r = h
        .wait(&run, |r| {
            r["job_running"] == false && r["status"] != "deploying"
        })
        .await;
    assert!(
        r["status"] == "deployed" || r["status"] == "partially_deployed",
        "{r}"
    );
    let items = h.items(&run).await;
    let applied: Vec<&Value> = items.iter().filter(|i| i["status"] == "applied").collect();
    assert!(applied.len() >= 2, "{items:?}");
    for a in &applied {
        assert_eq!(a["verify"]["present"], true, "{a}");
        assert!(a["live_url"].as_str().unwrap().starts_with(&base));
    }

    // ---- the editor removes one link -> verify -> not_present
    let victim = applied[0].clone();
    let (vid, src_url, turl) = (
        victim["edge_id"].as_str().unwrap().to_string(),
        victim["source"]["url"].as_str().unwrap().to_string(),
        victim["target"]["url"].as_str().unwrap().to_string(),
    );
    {
        let mut w = wp.lock();
        let p = w
            .posts
            .values_mut()
            .find(|p| src_url.contains(&format!("/{}/", p.slug)))
            .unwrap();
        let open = format!("<a href=\"{turl}\">");
        let at = p.raw.find(&open).unwrap();
        let close = at + p.raw[at..].find("</a>").unwrap();
        let inner = p.raw[at + open.len()..close].to_string();
        p.raw = format!("{}{}{}", &p.raw[..at], inner, &p.raw[close + 4..]);
        p.modified += 9;
    }
    let (s, v) = h
        .call(
            "POST",
            &format!("/api/v2/link-runs/{run}/verify"),
            &h.u1,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let r = h.idle(&run).await;
    assert!(r["verify_job_id"].is_string());
    let it = h
        .items(&run)
        .await
        .into_iter()
        .find(|i| i["edge_id"] == vid.as_str())
        .unwrap();
    assert_eq!(it["status"], "not_present", "{it}");

    // ---- repair -> applied again
    let (s, _) = h
        .call(
            "POST",
            &format!("/api/v2/link-runs/{run}/repair"),
            &h.u1,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let r = h
        .wait(&run, |r| {
            r["job_running"] == false && r["status"] != "deploying"
        })
        .await;
    assert!(r["repair_job_id"].is_string());
    let it = h
        .items(&run)
        .await
        .into_iter()
        .find(|i| i["edge_id"] == vid.as_str())
        .unwrap();
    assert_eq!(it["status"], "applied", "{it}");
    assert_eq!(it["verify"]["present"], true);
    assert!(r["stats"]["repaired"].as_i64().unwrap() >= 1, "{r}");

    // ---- undo the run -> byte-identical content
    let (s, _) = h
        .call(
            "POST",
            &format!("/api/v2/link-runs/{run}/undo"),
            &h.u1,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let r = h
        .wait(&run, |r| {
            r["job_running"] == false && r["status"] != "deploying"
        })
        .await;
    assert_eq!(r["status"], "undone", "{r}");
    for (id, raw) in &original {
        assert_eq!(
            &wp.lock().posts.get(id).unwrap().raw,
            raw,
            "post {id} differs after undo"
        );
    }
    let (s, _) = h
        .call(
            "POST",
            &format!("/api/v2/link-runs/{run}/cancel"),
            &h.u1,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // ---- Auto-Linker settings: v1 PATCH whitelist + v2 endpoints keep the schedule in sync
    let (s, b) = h
        .call(
            "PATCH",
            &format!("/api/v1/sites/{site_id}"),
            &h.u1,
            Some(json!({"settings": {"auto_linker": {"enabled": true, "cron": "nope"}}})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    let (s, b) = h
        .call(
            "PATCH",
            &format!("/api/v1/sites/{site_id}"),
            &h.u1,
            Some(json!({"settings": {"auto_linker": {"enabled": true, "cron": "0 4 * * *"}}})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, a) = h
        .call(
            "GET",
            &format!("/api/v2/sites/{site_id}/auto-linker"),
            &h.u1,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(a["auto_linker"]["enabled"], true);
    assert_eq!(a["schedule"]["active"], true);
    assert_eq!(a["schedule"]["cron"], "0 4 * * *");
    let (s, a) = h
        .call(
            "PUT",
            &format!("/api/v2/sites/{site_id}/auto-linker"),
            &h.u1,
            Some(json!({"enabled": false})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{a}");
    assert_eq!(a["schedule"]["active"], false);
    let (s, _) = h
        .call(
            "PUT",
            &format!("/api/v2/sites/{site_id}/auto-linker"),
            &h.u2,
            Some(json!({"enabled": true})),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = h
        .call(
            "POST",
            &format!("/api/v2/link-runs/{}/deploy", sf_core::new_id()),
            &h.u1,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = h
        .call("GET", "/api/v2/link-runs/../../x", &h.u1, None)
        .await;
    assert!(s == StatusCode::NOT_FOUND || s == StatusCode::BAD_REQUEST);

    runner.shutdown(Duration::from_secs(5)).await;
    h.db.close();
    let _ = std::fs::remove_dir_all(&h.dir);
}
