//! Integration tests for the /api/v1 contract (work packet S): every route group through the real
//! router with a temp SQLite database.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{Value, json};
use sf_core::Config;
use sf_core::crypto::{sign_jwt, user_claims};
use sf_server::AppState;
use sf_server::v1::providers::{BoxFut, ProbeService, Prober};
use sf_store::Db;
use tower::ServiceExt;

const SECRET: &str = "test-jwt-secret-0123456789abcdef";
const WP_PASS: &str = "abcd efgh ijkl mnop";
const ADMIN_PW: &str = "admin-password-1";
const USER_PW: &str = "user-a-password";

struct H {
    app: Router,
    st: AppState,
    dir: PathBuf,
    admin: String,
    user_a: String,
    user_b: String,
    admin_id: String,
    a_id: String,
    site_a: String,
    site_b: String,
    site_a_key: String,
    posts: Vec<String>,
    edges: Vec<String>,
    /// Every response body seen, for the "secrets never leak" sweep.
    bodies: std::sync::Mutex<Vec<String>>,
}

struct Resp {
    status: StatusCode,
    json: Value,
    text: String,
    headers: HeaderMap,
}

fn token(role: &str, id: &str, tv: i64) -> String {
    sign_jwt(SECRET, &user_claims(role, id, tv)).unwrap()
}

impl H {
    async fn new() -> Self {
        Self::with_cfg(|_| {}).await
    }

    async fn with_cfg(f: impl FnOnce(&mut Config)) -> Self {
        let dir = std::env::temp_dir().join(format!("sf-server-v1-{}", sf_core::new_id()));
        let db = Db::open(&dir.join("t.db")).unwrap();
        let mut cfg = Config {
            home: dir.clone(),
            ..Default::default()
        };
        cfg.auth.jwt_secret = SECRET.into();
        f(&mut cfg);
        let st = AppState::new(cfg, db, [7u8; 32]);
        let app = sf_server::router(st.clone(), None);
        let (admin_id, a_id, b_id) = (sf_core::new_id(), sf_core::new_id(), sf_core::new_id());
        let (site_a, site_b) = (sf_core::new_id(), sf_core::new_id());
        let site_a_key = sf_core::crypto::new_site_api_key();
        let posts: Vec<String> = (0..4).map(|_| sf_core::new_id()).collect();
        let edges: Vec<String> = (0..5).map(|_| sf_core::new_id()).collect();
        let admin_hash = sf_core::crypto::hash_password(ADMIN_PW).unwrap();
        let user_hash = sf_core::crypto::hash_password(USER_PW).unwrap();
        // Legacy v1-style site B row: plaintext credentials still in settings.
        let legacy =
            json!({"insert_modules": ["native"], "wp_user": "bob", "wp_app_password": WP_PASS})
                .to_string();
        {
            let (admin_id, a_id, b_id, site_a, site_b, key, posts, edges) = (
                admin_id.clone(),
                a_id.clone(),
                b_id.clone(),
                site_a.clone(),
                site_b.clone(),
                site_a_key.clone(),
                posts.clone(),
                edges.clone(),
            );
            st.db
                .write(move |t| {
                    let x = |sql: &str, p: &[&dyn rusqlite::ToSql]| {
                        t.execute(sql, p).map_err(sf_store::err).map(|_| ())
                    };
                    x("INSERT INTO users(id,email,password_hash,name,role) VALUES(?1,'admin@x.io',?2,'Admin','admin')",
                        &[&admin_id, &admin_hash])?;
                    x("INSERT INTO users(id,email,password_hash,name,role) VALUES(?1,'a@x.io',?2,'A','user')",
                        &[&a_id, &user_hash])?;
                    x("INSERT INTO users(id,email,password_hash,name,role) VALUES(?1,'b@x.io',?2,'B','user')",
                        &[&b_id, &user_hash])?;
                    x("INSERT INTO sites(id,domain,api_key,user_id,created_at) VALUES(?1,'a.example.com',?2,?3,'2026-01-01T00:00:00.000Z')",
                        &[&site_a, &key, &a_id])?;
                    x("INSERT INTO sites(id,domain,api_key,user_id,settings,created_at) VALUES(?1,'ev\"il\r\n.com','sf_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',?2,?3,'2026-01-02T00:00:00.000Z')",
                        &[&site_b, &b_id, &legacy])?;
                    let k1 = sf_core::new_id();
                    x("INSERT INTO clusters(id,site_id,label,source) VALUES(?1,?2,'Topic A','auto')", &[&k1, &site_a])?;
                    for (i, p) in posts.iter().enumerate() {
                        let url = format!("https://a.example.com/p{i}");
                        let pr = 1.0 - i as f64 * 0.2;
                        let orphan = i64::from(i == 3);
                        x("INSERT INTO posts(id,site_id,wp_post_id,url,title,pagerank,is_orphan,cluster_id,link_health_checked_at)
                           VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'2026-01-01T00:00:00.000Z')",
                          &[p, &site_a, &(i as i64 + 10), &url, &format!("Post {i}"), &pr, &orphan, &k1])?;
                    }
                    let g = sf_core::new_id();
                    x("INSERT INTO paragraphs(id,post_id,idx,text,hash) VALUES(?1,?2,2,'hello','h')", &[&g, &posts[0]])?;
                    // edges: 0 suggested, 1 approved (with paragraph), 2 applied, 3 suggested (same anchor, other target), 4 rejected
                    let rows: [(&str, &str, &str, usize, usize, &str); 5] = [
                        ("suggested", "alpha", "exact", 0, 1, "plan"),
                        ("approved", "beta", "partial", 0, 2, "plan"),
                        ("applied", "gamma", "exact", 1, 2, "existing"),
                        ("suggested", "alpha", "exact", 2, 3, "plan"),
                        ("rejected", "delta", "generic", 3, 0, "plan"),
                    ];
                    for (i, (status, anchor, at, s, d, origin)) in rows.iter().enumerate() {
                        let para: Option<&String> = (i == 1).then_some(&g);
                        let applied: Option<&str> = (*status == "applied").then_some("2026-02-01T10:00:00.000Z");
                        x("INSERT INTO link_edges(id,site_id,source_post_id,target_post_id,paragraph_id,anchor,anchor_type,
                             composite_score,status,origin,applied_at,score_breakdown)
                           VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'{\"semantic\":0.5}')",
                          &[&edges[i], &site_a, &posts[*s], &posts[*d], &para, anchor, at, &(0.9 - i as f64 * 0.1), status, origin, &applied])?;
                    }
                    x("INSERT INTO link_health(id,site_id,source_post_id,href,requested_url,scope,status_code,is_broken,
                         redirect_hops,redirect_chain,is_nofollow,anchor,failure_kind)
                       VALUES(?1,?2,?3,'http://ext.com/x','http://Ext.com:80/x','external',404,1,3,'[{\"url\":\"a\",\"status\":301,\"order\":0}]',1,'=cmd','not_found')",
                      &[&sf_core::new_id(), &site_a, &posts[0]])?;
                    x("INSERT INTO site_metrics_history(id,site_id,source,metrics,captured_at) VALUES(?1,?2,'audit','{\"orphans\":5,\"health_score\":60}','2026-01-01T00:00:00.000Z')",
                      &[&sf_core::new_id(), &site_a])?;
                    x("INSERT INTO site_metrics_history(id,site_id,source,metrics,captured_at) VALUES(?1,?2,'audit','{\"orphans\":3,\"health_score\":70}','2026-01-02T00:00:00.000Z')",
                      &[&sf_core::new_id(), &site_a])?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        Self {
            app,
            st,
            dir,
            admin: token("admin", &admin_id, 0),
            user_a: token("user", &a_id, 0),
            user_b: token("user", &b_id, 0),
            admin_id,
            a_id,
            site_a,
            site_b,
            site_a_key,
            posts,
            edges,
            bodies: Default::default(),
        }
    }

    async fn send(&self, req: Request<Body>) -> Resp {
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        self.bodies.lock().unwrap().push(text.clone());
        let json = serde_json::from_str(&text).unwrap_or(Value::Null);
        Resp {
            status,
            json,
            text,
            headers,
        }
    }

    async fn call(&self, method: &str, path: &str, tok: Option<&str>, body: Option<Value>) -> Resp {
        let mut b = Request::builder()
            .method(method)
            .uri(format!("/api/v1{path}"))
            .header("content-type", "application/json");
        if let Some(t) = tok {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        let body = body
            .map(|v| Body::from(v.to_string()))
            .unwrap_or_else(Body::empty);
        self.send(b.body(body).unwrap()).await
    }

    async fn raw(&self, method: &str, path: &str, tok: &str, ctype: &str, body: &str) -> Resp {
        let req = Request::builder()
            .method(method)
            .uri(format!("/api/v1{path}"))
            .header("content-type", ctype)
            .header("authorization", format!("Bearer {tok}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        self.send(req).await
    }

    async fn get(&self, path: &str, tok: &str) -> Resp {
        self.call("GET", path, Some(tok), None).await
    }
    async fn post(&self, path: &str, tok: &str, body: Value) -> Resp {
        self.call("POST", path, Some(tok), Some(body)).await
    }

    fn assert_no_secrets(&self) {
        for b in self.bodies.lock().unwrap().iter() {
            assert!(!b.contains(WP_PASS), "wp app password leaked: {b}");
            assert!(!b.contains("secrets_enc"), "secrets_enc leaked: {b}");
            assert!(!b.contains("api_key_enc"), "api_key_enc leaked: {b}");
            assert!(!b.contains("PRIVATE KEY"), "gsc private key leaked: {b}");
            assert!(!b.contains("sk-live-SECRET"), "provider key leaked: {b}");
        }
    }

    fn done(self) {
        self.assert_no_secrets();
        self.st.db.close();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn auth_flow() {
    let h = H::new().await;
    // login
    let r = h
        .call(
            "POST",
            "/auth/login",
            None,
            Some(json!({"email": " A@X.io ", "password": USER_PW})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["user"]["email"], "a@x.io");
    assert_eq!(r.json["user"]["role"], "user");
    let tok = r.json["token"].as_str().unwrap().to_string();
    let r = h
        .call(
            "POST",
            "/auth/login",
            None,
            Some(json!({"email": "a@x.io", "password": "nope-nope"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(r.json["error"].is_string());
    let r = h
        .call(
            "POST",
            "/auth/login",
            None,
            Some(json!({"email": "ghost@x.io", "password": "whatever1"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // me
    let r = h.get("/auth/me", &tok).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["id"], h.a_id.as_str());
    assert!(r.json.get("password_hash").is_none());
    assert_eq!(
        h.call("GET", "/auth/me", None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    // registration closed by default once users exist
    let r = h
        .call(
            "POST",
            "/auth/register",
            None,
            Some(json!({"email": "n@x.io", "password": "longenough"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // change password: wrong current, too short, ok -> old token revoked
    let r = h
        .post(
            "/auth/change-password",
            &tok,
            json!({"current_password": "bad", "new_password": "newpassword"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = h
        .post(
            "/auth/change-password",
            &tok,
            json!({"current_password": USER_PW, "new_password": "short"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = h
        .post(
            "/auth/change-password",
            &tok,
            json!({"current_password": USER_PW, "new_password": "brand-new-pass"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["ok"], true);
    let fresh = r.json["token"].as_str().unwrap().to_string();
    assert_eq!(
        h.get("/auth/me", &tok).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        h.get("/auth/me", &h.user_a).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(h.get("/auth/me", &fresh).await.status, StatusCode::OK);
    let r = h
        .call(
            "POST",
            "/auth/login",
            None,
            Some(json!({"email": "a@x.io", "password": "brand-new-pass"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    // handshake
    let r = h
        .call(
            "POST",
            "/auth/handshake",
            None,
            Some(json!({"api_key": h.site_a_key})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["site_id"], h.site_a.as_str());
    let first_tok = r.json["token"].as_str().unwrap().to_string();
    // L-02: the key is stored hashed (legacy rows are hashed in place), the plaintext key keeps
    // working, and tokens from before the hash stay valid (same fingerprint).
    let stored: String = {
        let id = h.site_a.clone();
        h.st.db
            .read(move |c| {
                c.query_row("SELECT api_key FROM sites WHERE id=?1", [&id], |r| r.get(0))
                    .map_err(sf_store::err)
            })
            .await
            .unwrap()
    };
    assert!(stored.starts_with("sha256:") && !stored.contains(h.site_a_key.as_str()));
    let r = h
        .call(
            "POST",
            "/auth/handshake",
            None,
            Some(json!({"api_key": h.site_a_key})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        h.get(&format!("/suggestions/{}", h.site_a), &first_tok)
            .await
            .status,
        StatusCode::OK
    );
    let r = h
        .call(
            "POST",
            "/auth/handshake",
            None,
            Some(json!({"api_key": stored})),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::UNAUTHORIZED,
        "the stored hash is not a key"
    );
    let r = h
        .call(
            "POST",
            "/auth/handshake",
            None,
            Some(json!({"api_key": "sf_nope"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // malformed bodies
    let r = h
        .call("POST", "/auth/login", None, Some(json!({"email": 5})))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/login")
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    assert_eq!(h.send(req).await.status, StatusCode::BAD_REQUEST);
    // rate limit: login has 10/min per IP; we used 5 + the malformed ones above
    let mut limited = false;
    for _ in 0..12 {
        let r = h
            .call(
                "POST",
                "/auth/login",
                None,
                Some(json!({"email": "x@x.io", "password": "y"})),
            )
            .await;
        if r.status == StatusCode::TOO_MANY_REQUESTS {
            limited = true;
            break;
        }
    }
    assert!(limited);
    h.done();
}

/// M-03: the auth limiter keys on the client IP (ConnectInfo), counts failures only, locks one
/// account after repeated failures, and keeps the plugin handshake separate from human login.
#[tokio::test(flavor = "multi_thread")]
async fn auth_rate_limit_per_ip_and_account() {
    let h = H::new().await;
    let from = |ip: &str, path: &str, body: Value| {
        let addr = std::net::SocketAddr::new(ip.parse().unwrap(), 40000);
        let mut req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1{path}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(addr));
        req
    };
    // successes are never counted
    for _ in 0..15 {
        let r = h
            .send(from(
                "10.0.0.1",
                "/auth/login",
                json!({"email": "admin@x.io", "password": ADMIN_PW}),
            ))
            .await;
        assert_eq!(r.status, StatusCode::OK);
    }
    // IP A burns its failure budget on many different (unknown) accounts
    let mut limited = false;
    for i in 0..12 {
        let r = h
            .send(from(
                "10.0.0.2",
                "/auth/login",
                json!({"email": format!("ghost{i}@x.io"), "password": "nope"}),
            ))
            .await;
        if r.status == StatusCode::TOO_MANY_REQUESTS {
            limited = true;
            assert!(i >= 10, "limited too early: {i}");
            break;
        }
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    assert!(limited);
    // ...which neither blocks another IP (loopback v4 vs v6 are separate too) nor the handshake
    for ip in ["10.0.0.3", "127.0.0.1", "::1"] {
        let r = h
            .send(from(
                ip,
                "/auth/login",
                json!({"email": "admin@x.io", "password": ADMIN_PW}),
            ))
            .await;
        assert_eq!(r.status, StatusCode::OK, "{ip}");
    }
    let r = h
        .send(from(
            "10.0.0.2",
            "/auth/handshake",
            json!({"api_key": h.site_a_key}),
        ))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    // per-account lockout: failures on one email from many IPs lock that email only
    for i in 0..5 {
        let r = h
            .send(from(
                &format!("10.1.0.{i}"),
                "/auth/login",
                json!({"email": "a@x.io", "password": "wrong-pass"}),
            ))
            .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    let r = h
        .send(from(
            "10.2.0.1",
            "/auth/login",
            json!({"email": "a@x.io", "password": USER_PW}),
        ))
        .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{}", r.text);
    assert!(r.text.contains("this account"));
    let r = h
        .send(from(
            "10.2.0.1",
            "/auth/login",
            json!({"email": "b@x.io", "password": USER_PW}),
        ))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    // handshake failures are limited per IP, separately from login
    for _ in 0..10 {
        let r = h
            .send(from(
                "10.3.0.1",
                "/auth/handshake",
                json!({"api_key": "sf_nope"}),
            ))
            .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    let r = h
        .send(from(
            "10.3.0.1",
            "/auth/handshake",
            json!({"api_key": "sf_nope"}),
        ))
        .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    let r = h
        .send(from(
            "10.3.0.1",
            "/auth/login",
            json!({"email": "admin@x.io", "password": ADMIN_PW}),
        ))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    h.done();
}

/// L-12: UI and API responses carry anti-framing / nosniff / referrer headers.
#[tokio::test(flavor = "multi_thread")]
async fn security_headers_on_ui_and_api() {
    let h = H::new().await;
    for path in ["/", "/sites/x", "/api/v1/auth/me"] {
        let req = Request::builder().uri(path).body(Body::empty()).unwrap();
        let r = h.send(req).await;
        let get = |n: &str| {
            r.headers
                .get(n)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(get("x-frame-options"), "DENY", "{path}");
        assert_eq!(get("x-content-type-options"), "nosniff", "{path}");
        assert_eq!(get("referrer-policy"), "no-referrer", "{path}");
        assert!(get("content-security-policy").contains("frame-ancestors 'none'"));
    }
    h.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn register_first_user_becomes_admin_and_open_registration() {
    let dir = std::env::temp_dir().join(format!("sf-server-v1-reg-{}", sf_core::new_id()));
    let db = Db::open(&dir.join("t.db")).unwrap();
    let mut cfg = Config::default();
    cfg.auth.jwt_secret = SECRET.into();
    let st = AppState::new(cfg, db, [1u8; 32]);
    let app = sf_server::router(st.clone(), None);
    let call = |body: Value| {
        let app = app.clone();
        async move {
            let res = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/auth/register")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let s = res.status();
            let b = axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .unwrap();
            (
                s,
                serde_json::from_slice::<Value>(&b).unwrap_or(Value::Null),
            )
        }
    };
    let (s, _) = call(json!({"email": "bad", "password": "longenough"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, v) =
        call(json!({"email": "First@X.io", "password": "longenough", "name": " F "})).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(v["user"]["role"], "admin");
    assert_eq!(v["user"]["name"], "F");
    assert!(v["token"].is_string());
    let (s, _) = call(json!({"email": "second@x.io", "password": "longenough"})).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    st.db.close();
    let _ = std::fs::remove_dir_all(&dir);

    let h = H::with_cfg(|c| c.auth.allow_registration = true).await;
    let r = h
        .call(
            "POST",
            "/auth/register",
            None,
            Some(json!({"email": "n@x.io", "password": "longenough"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.json["user"]["role"], "user");
    let r = h
        .call(
            "POST",
            "/auth/register",
            None,
            Some(json!({"email": "N@x.io", "password": "longenough"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = h
        .call(
            "POST",
            "/auth/register",
            None,
            Some(json!({"email": "m@x.io", "password": "short"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    h.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn sites_and_secrets() {
    let h = H::new().await;
    // list: scoped, counts present, no api_key
    let r = h.get("/sites", &h.user_a).await;
    assert_eq!(r.status, StatusCode::OK);
    let arr = r.json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["posts_count"], 4);
    assert_eq!(arr[0]["edge_counts"]["suggested"], 2);
    assert!(arr[0].get("api_key").is_none());
    assert!(!r.text.contains(&h.site_a_key));
    let r = h.get("/sites", &h.admin).await;
    assert_eq!(r.json.as_array().unwrap().len(), 2);
    // legacy row: credentials stripped, flag exposed
    let b = r
        .json
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == h.site_b.as_str())
        .unwrap()
        .clone();
    assert_eq!(b["has_wp_credentials"], true);
    assert_eq!(b["wp_user"], "bob");
    assert!(b["settings"].get("wp_app_password").is_none());
    assert_eq!(b["edge_counts"], json!({}));
    // create
    let r = h
        .post(
            "/sites",
            &h.user_a,
            json!({"domain": "https://new.example.org/", "mode": "sitemap"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.json["domain"], "new.example.org");
    let key = r.json["api_key"].as_str().unwrap().to_string();
    assert!(key.starts_with("sf_") && key.len() == 35);
    let new_id = r.json["id"].as_str().unwrap().to_string();
    let r = h
        .post("/sites", &h.user_a, json!({"domain": "new.example.org"}))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    // another user may register the same domain (no cross-tenant oracle)
    let r = h
        .post("/sites", &h.user_b, json!({"domain": "new.example.org"}))
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    for bad in [
        json!({"domain": ""}),
        json!({"domain": "a\"b"}),
        json!({"domain": "x.com", "mode": "ftp"}),
        json!({}),
        json!({"domain": "x.com", "wp_url": "javascript:x"}),
    ] {
        assert_eq!(
            h.post("/sites", &h.user_a, bad).await.status,
            StatusCode::BAD_REQUEST
        );
    }
    // get + ownership
    let r = h.get(&format!("/sites/{new_id}"), &h.user_a).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.json.get("api_key").is_none());
    assert_eq!(
        h.get(&format!("/sites/{new_id}"), &h.user_b).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.get(&format!("/sites/{new_id}"), &h.admin).await.status,
        StatusCode::OK
    );
    assert_eq!(
        h.get("/sites/not-a-uuid", &h.admin).await.status,
        StatusCode::NOT_FOUND
    );
    // patch: credentials sealed, whitelist enforced, shallow merge, null removes
    let r = h
        .call(
            "PATCH",
            &format!("/sites/{}", h.site_a),
            Some(&h.user_a),
            Some(json!({"settings": {"preset": "aggressive", "wp_user": "alice", "wp_app_password": WP_PASS,
                "insert_modules": ["native", "context"], "gsc": {"credentials": "x"}}})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["has_wp_credentials"], true);
    assert_eq!(r.json["wp_user"], "alice");
    assert_eq!(r.json["settings"]["preset"], "aggressive");
    assert!(r.json["settings"].get("wp_user").is_none());
    assert!(r.json["settings"].get("gsc").is_none());
    let r = h
        .call(
            "PATCH",
            &format!("/sites/{}", h.site_a),
            Some(&h.user_a),
            Some(json!({"settings": {"max_links_per_post": 7, "preset": null}, "wp_url": "https://a.example.com/"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["settings"]["max_links_per_post"], 7);
    assert!(r.json["settings"].get("preset").is_none());
    assert_eq!(
        r.json["settings"]["insert_modules"],
        json!(["native", "context"])
    );
    assert_eq!(r.json["wp_url"], "https://a.example.com");
    let (blob, settings): (String, String) = {
        let id = h.site_a.clone();
        h.st.db
            .read(move |c| {
                c.query_row(
                    "SELECT secrets_enc, settings FROM sites WHERE id=?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(sf_store::err)
            })
            .await
            .unwrap()
    };
    assert!(blob.starts_with("v1:") && !blob.contains("abcd"));
    assert!(!settings.contains(WP_PASS));
    let secrets = sf_core::secrets::SiteSecrets::load(&[7u8; 32], &blob).unwrap();
    assert_eq!(secrets.wp_app_password.as_deref(), Some(WP_PASS));
    for bad in [
        json!({"settings": {"evil": 1}}),
        json!({"settings": {"max_links_per_post": -1}}),
        json!({"settings": {"preset": "nuts"}}),
        json!({"settings": {"insert_modules": ["x"]}}),
        json!({"settings": "string"}),
        json!({"mode": "ftp"}),
    ] {
        let r = h
            .call(
                "PATCH",
                &format!("/sites/{}", h.site_a),
                Some(&h.user_a),
                Some(bad),
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        h.call(
            "PATCH",
            &format!("/sites/{}", h.site_a),
            Some(&h.user_b),
            Some(json!({}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    // legacy plaintext row is cleaned on first patch
    let r = h
        .call(
            "PATCH",
            &format!("/sites/{}", h.site_b),
            Some(&h.admin),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let s: String = {
        let id = h.site_b.clone();
        h.st.db
            .read(move |c| {
                c.query_row("SELECT settings FROM sites WHERE id=?1", [&id], |r| {
                    r.get(0)
                })
                .map_err(sf_store::err)
            })
            .await
            .unwrap()
    };
    assert!(!s.contains(WP_PASS));
    // L-01: a legacy v1 site token (no `tv`) works until the key is rotated, then never again
    let mut legacy = sf_core::crypto::site_claims(&h.site_a, &h.site_a_key);
    legacy.tv = None;
    let legacy_tok = sign_jwt(SECRET, &legacy).unwrap();
    let sugg = format!("/suggestions/{}", h.site_a);
    assert_eq!(h.get(&sugg, &legacy_tok).await.status, StatusCode::OK);
    // rotate key
    let r = h
        .post(
            &format!("/sites/{}/rotate-key", h.site_a),
            &h.user_a,
            json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let rotated = r.json["api_key"].as_str().unwrap().to_string();
    assert_ne!(rotated, h.site_a_key);
    assert_eq!(
        h.get(&sugg, &legacy_tok).await.status,
        StatusCode::UNAUTHORIZED
    );
    let r = h
        .call(
            "POST",
            "/auth/handshake",
            None,
            Some(json!({"api_key": h.site_a_key})),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // gsc
    let creds = json!({"type": "service_account", "client_email": "svc@p.iam", "private_key": "-----BEGIN PRIVATE KEY-----x"}).to_string();
    let path = format!("/sites/{}/gsc", h.site_a);
    let r = h
        .post(&path, &h.user_a, json!({"credentials_json": creds}))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["connected"], true);
    assert_eq!(r.json["client_email"], "svc@p.iam");
    let r = h.get(&path, &h.user_a).await;
    assert_eq!(r.json["connected"], true);
    assert!(r.json["saved_at"].is_string());
    assert_eq!(
        h.post(&path, &h.user_a, json!({"credentials_json": "nope"}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.post(&path, &h.user_a, json!({"credentials_json": "[1]"}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.post(&path, &h.user_a, json!({"credentials_json": "{\"a\":1}"}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let r = h.get(&format!("/sites/{}", h.site_a), &h.user_a).await;
    assert_eq!(r.json["settings"]["gsc"]["connected"], true);
    assert!(r.json["settings"]["gsc"].get("credentials").is_none());
    let r = h.get(&format!("/dashboard/{}", h.site_a), &h.user_a).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = h.call("DELETE", &path, Some(&h.user_a), None).await;
    assert_eq!(r.json, json!({"connected": false}));
    assert_eq!(h.get(&path, &h.user_a).await.json["connected"], false);
    assert_eq!(h.get(&path, &h.user_b).await.status, StatusCode::NOT_FOUND);
    h.done();
}

/// M-01: private / loopback / metadata site hosts need an admin plus an explicit allowance.
#[tokio::test(flavor = "multi_thread")]
async fn sites_private_hosts_need_admin_and_allowance() {
    let h = H::new().await;
    let bad = |r: &Resp, needle: &str| {
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text);
        assert!(r.text.contains(needle), "{needle} not in {}", r.text);
    };
    // non-admin: loopback / RFC1918 / default docker alias / metadata all refused
    for (body, needle) in [
        (json!({"domain": "127.0.0.1:6230"}), "Only an admin"),
        (
            json!({"domain": "ok.example", "wp_url": "http://10.1.2.3/"}),
            "Only an admin",
        ),
        (
            json!({"domain": "site01.example:8080"}),
            "Only an admin",
        ),
        (
            json!({"domain": "ok.example", "wp_url": "http://169.254.169.254/"}),
            "cloud-metadata",
        ),
        (
            json!({"domain": "ok.example", "allow_private_host": true}),
            "admin",
        ),
        (json!({"domain": "user@evil.example"}), "host name"),
        (json!({"domain": "evil.example/path"}), "host name"),
        (json!({"domain": "evil.example?x"}), "host name"),
    ] {
        bad(&h.post("/sites", &h.user_a, body).await, needle);
    }
    // admin without an allowance: refused with a pointer to the settings
    bad(
        &h.post("/sites", &h.admin, json!({"domain": "10.0.0.5"}))
            .await,
        "allow_private_host",
    );
    // admin + per-site allowance: ok, flag stored
    let r = h
        .post(
            "/sites",
            &h.admin,
            json!({"domain": "10.0.0.5", "allow_private_host": true}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.json["settings"]["allow_private_host"], true);
    let priv_id = r.json["id"].as_str().unwrap().to_string();
    // metadata is never exempt, not even for an admin with the allowance
    bad(
        &h.call(
            "PATCH",
            &format!("/sites/{priv_id}"),
            Some(&h.admin),
            Some(json!({"wp_url": "http://169.254.169.254/latest"})),
        )
        .await,
        "cloud-metadata",
    );
    // non-admin can neither flip the flag nor point wp_url at loopback
    bad(
        &h.call(
            "PATCH",
            &format!("/sites/{}", h.site_a),
            Some(&h.user_a),
            Some(json!({"settings": {"allow_private_host": true}})),
        )
        .await,
        "admin",
    );
    bad(
        &h.call(
            "PATCH",
            &format!("/sites/{}", h.site_a),
            Some(&h.user_a),
            Some(json!({"wp_url": "http://127.0.0.1:6230"})),
        )
        .await,
        "Only an admin",
    );
    h.done();

    // global crawl.allow_private_hosts=true: admin may, a user still may not
    let h = H::with_cfg(|c| c.crawl.allow_private_hosts = true).await;
    let r = h
        .post("/sites", &h.admin, json!({"domain": "127.0.0.1:9"}))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    bad(
        &h.post("/sites", &h.user_a, json!({"domain": "127.0.0.1:10"}))
            .await,
        "Only an admin",
    );
    h.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn enqueue_routes_and_jobs() {
    let h = H::new().await;
    let sa = h.site_a.clone();
    let r = h
        .post(&format!("/sites/{sa}/crawl"), &h.user_a, json!({}))
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert_eq!(r.json["deduplicated"], false);
    let crawl_id = r.json["job_run_id"].as_str().unwrap().to_string();
    // empty body is tolerated and deduplicates onto the queued crawl
    let r = h
        .call("POST", &format!("/sites/{sa}/crawl"), Some(&h.user_a), None)
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert_eq!(r.json["deduplicated"], true);
    assert_eq!(r.json["job_run_id"], crawl_id.as_str());
    let r = h
        .post(
            &format!("/sites/{sa}/plan"),
            &h.user_a,
            json!({"topology": "bidirectional"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    let r = h
        .post(
            &format!("/sites/{sa}/plan"),
            &h.user_a,
            json!({"topology": "zigzag"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = h
        .call(
            "POST",
            &format!("/sites/{sa}/plan"),
            Some(&h.user_a),
            Some(Value::Null),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    let r = h
        .post(&format!("/sites/{sa}/archive-plan"), &h.user_a, json!({}))
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    let r = h
        .post(&format!("/sites/{sa}/link-health"), &h.user_a, json!({}))
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    let r = h
        .post(&format!("/rules/keywords/{sa}/run"), &h.user_a, json!({}))
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert_eq!(
        h.post(&format!("/sites/{sa}/crawl"), &h.user_b, json!({}))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // payload stored for the pipelines
    let payload: String = {
        let id = crawl_id.clone();
        h.st.db
            .read(move |c| {
                c.query_row(
                    "SELECT requested_by FROM job_runs WHERE id=?1",
                    [&id],
                    |r| r.get(0),
                )
                .map_err(sf_store::err)
            })
            .await
            .unwrap()
    };
    assert_eq!(payload, format!("user:{}", h.a_id));
    // jobs list (newest first, no payload) + run lookup
    let r = h.get(&format!("/jobs/{sa}"), &h.user_a).await;
    assert_eq!(r.status, StatusCode::OK);
    let jobs = r.json.as_array().unwrap();
    assert!(jobs.len() >= 5);
    assert!(jobs[0].get("payload").is_none());
    for k in [
        "id",
        "kind",
        "status",
        "stats",
        "error",
        "created_at",
        "started_at",
        "finished_at",
    ] {
        assert!(jobs[0].get(k).is_some(), "missing {k}");
    }
    assert!(jobs.iter().any(|j| j["kind"] == "crawl"));
    let r = h.get(&format!("/jobs/{sa}?limit=2"), &h.user_a).await;
    assert_eq!(r.json.as_array().unwrap().len(), 2);
    let r = h.get(&format!("/jobs/run/{crawl_id}"), &h.user_a).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["status"], "queued");
    assert_eq!(
        h.get(&format!("/jobs/run/{crawl_id}"), &h.user_b)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.get("/jobs/run/zzz", &h.user_a).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.get(&format!("/jobs/{}", h.site_b), &h.user_a)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // audit trail written
    let n: i64 =
        h.st.db
            .read(|c| {
                c.query_row(
                    "SELECT count(*) FROM audit_log WHERE action LIKE 'job.enqueue.%'",
                    [],
                    |r| r.get(0),
                )
                .map_err(sf_store::err)
            })
            .await
            .unwrap();
    assert!(n >= 5);
    h.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_channel() {
    let h = H::new().await;
    let r = h
        .call(
            "POST",
            "/auth/handshake",
            None,
            Some(json!({"api_key": h.site_a_key})),
        )
        .await;
    let site_tok = r.json["token"].as_str().unwrap().to_string();
    let post = |url: &str, html: &str| {
        json!({"wp_post_id": 99, "url": url, "slug": "s", "title": "T", "content_html": html,
               "post_type": "post", "post_status": "publish", "builder": "gutenberg", "meta_description": "d"})
    };
    let batch = json!({"domain": "a.example.com", "posts": [
        post("https://a.example.com/new", "<p>Hello <b>world</b></p>"),
        post("https://a.example.com/p0", "<p>changed</p>"),
        post("javascript:alert(1)", "x"),
    ]});
    let r = h.post("/sync", &site_tok, batch.clone()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json, json!({"received": 3, "upserted": 2, "skipped": 1}));
    let r = h.post("/sync", &site_tok, batch).await;
    assert_eq!(r.json, json!({"received": 3, "upserted": 0, "skipped": 3}));
    // L-04: posts on a foreign host are skipped, subdomains of the site are accepted.
    let foreign = json!({"domain": "a.example.com", "posts": [
        post("https://evil.example.org/x", "<p>planted</p>"),
        post("https://blog.a.example.com/y", "<p>sub</p>"),
    ]});
    let r = h.post("/sync", &site_tok, foreign).await;
    assert_eq!(r.json, json!({"received": 2, "upserted": 1, "skipped": 1}));
    let (text, wc, meta): (String, i64, String) = {
        let id = h.site_a.clone();
        h.st.db
            .read(move |c| {
                c.query_row(
                    "SELECT content_text, word_count, meta FROM posts WHERE site_id=?1 AND url='https://a.example.com/new'",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map_err(sf_store::err)
            })
            .await
            .unwrap()
    };
    assert_eq!((text.as_str(), wc), ("Hello world", 2));
    assert!(meta.contains("\"description\":\"d\""));
    // human sync needs ?site_id
    assert_eq!(
        h.post("/sync", &h.user_a, json!({"domain": "x", "posts": []}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let r = h
        .post(
            &format!("/sync?site_id={}", h.site_a),
            &h.user_a,
            json!({"domain": "x", "posts": []}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        h.post(
            &format!("/sync?site_id={}", h.site_a),
            &h.user_b,
            json!({"domain": "x", "posts": []})
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.post("/sync", &site_tok, json!({"domain": "x"}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let bad_builder = json!({"domain": "x", "posts": [{"wp_post_id": 1, "url": "https://a.example.com/q", "slug": "", "title": "",
        "content_html": "", "post_type": "post", "post_status": "publish", "builder": "wix"}]});
    assert_eq!(
        h.post("/sync", &site_tok, bad_builder).await.status,
        StatusCode::BAD_REQUEST
    );
    // suggestions: approved edge with paragraph index
    let r = h
        .get(&format!("/suggestions/{}", h.site_a), &site_tok)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let s = r.json.as_array().unwrap();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0]["edge_id"], h.edges[1].as_str());
    assert_eq!(s[0]["wp_post_id"], 99); // p0 was re-synced above
    assert_eq!(s[0]["paragraph_index"], 2);
    assert!(s[0].get("paragraph_hash").is_some()); // #5: additive block hash
    assert_eq!(s[0]["replacement_html"], Value::Null);
    // B9: ?status= is honoured; an unknown value keeps the approved default.
    let r = h
        .get(
            &format!("/suggestions/{}?status=applied", h.site_a),
            &site_tok,
        )
        .await;
    assert!(
        r.json
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["edge_id"] != h.edges[1].as_str())
    );
    let r = h
        .get(
            &format!("/suggestions/{}?status=bogus", h.site_a),
            &site_tok,
        )
        .await;
    assert_eq!(r.json.as_array().unwrap().len(), 1);
    assert_eq!(s[0]["insert_module"], "native");
    assert_eq!(
        h.get(&format!("/suggestions/{}", h.site_b), &site_tok)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // site tokens cannot use user routes
    assert_eq!(
        h.get("/sites", &site_tok).await.status,
        StatusCode::UNAUTHORIZED
    );
    // ack: only approved -> applied/failed
    let ack = format!("/suggestions/{}/ack", h.site_a);
    let r = h
        .post(&ack, &site_tok, json!({"edge_id": h.edges[0], "ok": true}))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = h
        .post(&ack, &site_tok, json!({"edge_id": h.edges[1], "ok": true}))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["status"], "applied");
    assert!(r.json["applied_at"].is_string());
    let r = h
        .post(&ack, &site_tok, json!({"edge_id": h.edges[1], "ok": true}))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = h
        .post(
            &ack,
            &site_tok,
            json!({"edge_id": sf_core::new_id(), "ok": true}),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(
        h.post(&ack, &site_tok, json!({"edge_id": "x", "ok": true}))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.post(&ack, &site_tok, json!({"edge_id": h.edges[1]}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    // revert-ack
    let rev = format!("/suggestions/{}/revert-ack", h.site_a);
    let r = h
        .post(&rev, &site_tok, json!({"edge_id": h.edges[1]}))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["status"], "reverted");
    let r = h
        .post(&rev, &site_tok, json!({"edge_id": h.edges[1]}))
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.json["error"], "no applied edge to revert");
    // L-03: a crawled `existing` link (edges[2], applied) cannot be flipped by the plugin
    let r = h
        .post(&rev, &site_tok, json!({"edge_id": h.edges[2]}))
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    h.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn edges_bulk_rules() {
    let h = H::new().await;
    let sa = h.site_a.clone();
    // list with clamped limit (UI asks 2000)
    let r = h
        .get(
            &format!("/edges/{sa}?status=suggested&limit=2000"),
            &h.user_a,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["total"], 2);
    let e = &r.json["edges"][0];
    for k in [
        "id",
        "source_url",
        "source_title",
        "target_url",
        "target_title",
        "score_breakdown",
        "composite_score",
        "anchor_type",
        "status",
    ] {
        assert!(e.get(k).is_some(), "missing {k}");
    }
    assert!(e["score_breakdown"].is_object());
    let r = h
        .get(
            &format!("/edges/{sa}?limit=abc&offset=-5&tool=existing"),
            &h.user_a,
        )
        .await;
    assert_eq!(r.json["total"], 1);
    assert_eq!(
        h.get(&format!("/edges/{sa}?status=bogus"), &h.user_a)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.get(&format!("/edges/{sa}"), &h.user_b).await.status,
        StatusCode::NOT_FOUND
    );
    // decide
    let d = |i: usize| format!("/edges/{}/decide", h.edges[i]);
    assert_eq!(
        h.post(&d(0), &h.user_a, json!({"decision": "maybe"}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.post(&d(0), &h.user_b, json!({"decision": "approved"}))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let r = h
        .post(&d(0), &h.user_a, json!({"decision": "approved"}))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["status"], "approved");
    assert!(r.json["decided_at"].is_string());
    let r = h
        .post(&d(2), &h.user_a, json!({"decision": "approved"}))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.json["error"].as_str().unwrap().contains("applied"));
    assert_eq!(
        h.post(
            "/edges/nope/decide",
            &h.user_a,
            json!({"decision": "approved"})
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    // anchor
    let a = |i: usize| format!("/edges/{}/anchor", h.edges[i]);
    let r = h
        .call(
            "PATCH",
            &a(3),
            Some(&h.user_a),
            Some(json!({"anchor": "  new anchor  ", "anchor_type": "cta"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["anchor"], "new anchor");
    assert_eq!(r.json["anchor_type"], "cta");
    assert_eq!(
        h.call(
            "PATCH",
            &a(3),
            Some(&h.user_a),
            Some(json!({"anchor": "  "}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.call(
            "PATCH",
            &a(3),
            Some(&h.user_a),
            Some(json!({"anchor": "x", "anchor_type": "weird"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.call(
            "PATCH",
            &a(2),
            Some(&h.user_a),
            Some(json!({"anchor": "x"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // edge 4 (posts3 -> posts0, "delta"); add a clash on the same pair
    {
        let (id, s, src, dst) = (
            sf_core::new_id(),
            sa.clone(),
            h.posts[3].clone(),
            h.posts[0].clone(),
        );
        h.st.db
            .write(move |t| {
                t.execute(
                    "INSERT INTO link_edges(id,site_id,source_post_id,target_post_id,anchor) VALUES(?1,?2,?3,?4,'clash')",
                    [&id, &s, &src, &dst],
                )
                .map_err(sf_store::err)?;
                Ok(())
            })
            .await
            .unwrap();
    }
    assert_eq!(
        h.call(
            "PATCH",
            &a(4),
            Some(&h.user_a),
            Some(json!({"anchor": "clash"}))
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    // bulk decide (non-uuids dropped, foreign scope ignored)
    let r = h
        .post(
            "/bulk/decide",
            &h.user_a,
            json!({"edge_ids": [h.edges[3], "junk"], "decision": "approved"}),
        )
        .await;
    assert_eq!(r.json, json!({"updated": 1}));
    let r = h
        .post(
            "/bulk/decide",
            &h.user_b,
            json!({"edge_ids": [h.edges[4]], "decision": "approved"}),
        )
        .await;
    assert_eq!(r.json, json!({"updated": 0}));
    assert_eq!(
        h.post(
            "/bulk/decide",
            &h.user_a,
            json!({"edge_ids": [], "decision": "approved"})
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.post(
            "/bulk/decide",
            &h.user_a,
            json!({"edge_ids": ["junk"], "decision": "approved"})
        )
        .await
        .json,
        json!({"updated": 0})
    );
    // bulk run / undo
    let r = h.post("/bulk/run", &h.user_a, json!({"site_id": sa})).await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert_eq!(r.json["count"], 3);
    let r = h
        .post(
            "/bulk/run",
            &h.user_a,
            json!({"site_id": sa, "edge_ids": ["junk"]}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        h.post("/bulk/run", &h.user_b, json!({"site_id": sa}))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.post("/bulk/run", &h.user_a, json!({})).await.status,
        StatusCode::BAD_REQUEST
    );
    let r = h
        .post(
            "/bulk/undo",
            &h.user_a,
            json!({"site_id": sa, "edge_ids": [h.edges[2]]}),
        )
        .await;
    // edges[2] is an applied `existing` link (crawled, never injected): nothing to undo
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json, json!({"job_run_id": null, "count": 0}));
    let r = h
        .post(
            "/bulk/undo",
            &h.user_a,
            json!({"site_id": sa, "edge_ids": [h.edges[0]]}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json, json!({"job_run_id": null, "count": 0}));
    // keyword rules
    let kw = format!("/rules/keywords/{sa}");
    let r = h.post(&kw, &h.user_a, json!({"keyword": " shoes ", "target_url": "https://a.example.com/p1", "anchor_type": "naked"})).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.json["keyword"], "shoes");
    assert_eq!(r.json["max_links"], 3);
    assert_eq!(r.json["active"], true);
    let rule = r.json["id"].as_str().unwrap().to_string();
    let r = h
        .post(
            &kw,
            &h.user_a,
            json!({"keyword": "shoes", "target_url": "https://a.example.com/p1"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    for bad in [
        json!({"keyword": "x", "target_url": "ftp://a"}),
        json!({"keyword": "x", "target_url": "https://a.com", "max_links": 0}),
        json!({"keyword": "x", "target_url": "https://a.com", "anchor_type": "evil"}),
        json!({"keyword": "", "target_url": "https://a.com"}),
    ] {
        assert_eq!(
            h.post(&kw, &h.user_a, bad).await.status,
            StatusCode::BAD_REQUEST
        );
    }
    let r = h.post(&kw, &h.user_a, json!({"keyword": "cta", "target_url": "https://a.example.com/p2", "anchor_type": "cta", "max_links": 100})).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let r = h.get(&kw, &h.user_a).await;
    assert_eq!(r.json.as_array().unwrap().len(), 2);
    let rp = format!("/rules/keywords/{rule}");
    let r = h
        .call(
            "PATCH",
            &rp,
            Some(&h.user_a),
            Some(json!({"active": false})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["active"], false);
    for bad in [
        json!({"max_links": 0}),
        json!({"max_links": -3}),
        json!({"max_links": "5"}),
        json!({"anchor_type": "x"}),
    ] {
        assert_eq!(
            h.call("PATCH", &rp, Some(&h.user_a), Some(bad))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        h.call("PATCH", &rp, Some(&h.user_b), Some(json!({})))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.call("DELETE", &rp, Some(&h.user_b), None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.call("DELETE", &rp, Some(&h.user_a), None).await.json,
        json!({"deleted": true})
    );
    assert_eq!(
        h.call("DELETE", &rp, Some(&h.user_a), None).await.status,
        StatusCode::NOT_FOUND
    );
    // ignore list
    let ig = format!("/rules/ignore/{sa}");
    let r = h
        .post(&ig, &h.user_a, json!({"phrase": " click here "}))
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let pid = r.json["id"].as_str().unwrap().to_string();
    assert_eq!(
        h.post(&ig, &h.user_a, json!({"phrase": "click here"}))
            .await
            .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.post(&ig, &h.user_a, json!({"phrase": " "})).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(h.get(&ig, &h.user_a).await.json[0]["phrase"], "click here");
    assert_eq!(h.get(&ig, &h.user_b).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        h.call(
            "DELETE",
            &format!("/rules/ignore/{pid}"),
            Some(&h.user_a),
            None
        )
        .await
        .json,
        json!({"deleted": true})
    );
    h.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboard_reports_and_exports() {
    let h = H::new().await;
    let sa = h.site_a.clone();
    let r = h.get(&format!("/dashboard/{sa}"), &h.user_a).await;
    assert_eq!(r.status, StatusCode::OK);
    for k in [
        "site",
        "health_score",
        "posts",
        "orphans",
        "dead_ends",
        "avg_links_per_post",
        "edges_by_status",
        "clusters",
        "cannibalization_warnings",
        "recent_jobs",
        "opportunities",
        "http_links",
        "outbound_domains",
        "link_duplication",
        "orphan_posts",
    ] {
        assert!(r.json.get(k).is_some(), "dashboard missing {k}");
    }
    let hs = r.json["health_score"].as_i64().unwrap();
    assert!((0..=100).contains(&hs));
    assert_eq!(r.json["posts"], 4);
    assert_eq!(r.json["orphans"], 1);
    assert_eq!(r.json["opportunities"], 2);
    assert_eq!(r.json["http_links"], 1);
    assert_eq!(r.json["outbound_domains"], 1);
    assert_eq!(r.json["orphan_posts"][0]["title"], "Post 3");
    // G5: the fixture was link-health-checked -> a timestamp; never checked -> null
    // (http_links/outbound_domains are then unknown, not 0)
    assert!(r.json["link_health_checked_at"].is_string(), "{}", r.json);
    {
        let id = sa.clone();
        h.st.db
            .write(move |t| {
                t.execute(
                    "UPDATE posts SET link_health_checked_at=NULL WHERE site_id=?1",
                    [&id],
                )
                .map_err(sf_store::err)
            })
            .await
            .unwrap();
    }
    let r2 = h.get(&format!("/dashboard/{sa}"), &h.user_a).await;
    assert!(r2.json["link_health_checked_at"].is_null(), "{}", r2.json);
    {
        // restore the fixture's timestamps (the breadcrumb report below counts checked posts)
        let (id, ts) = (
            sa.clone(),
            r.json["link_health_checked_at"]
                .as_str()
                .unwrap()
                .to_string(),
        );
        h.st.db
            .write(move |t| {
                t.execute(
                    "UPDATE posts SET link_health_checked_at=?2 WHERE site_id=?1",
                    [&id, &ts],
                )
                .map_err(sf_store::err)
            })
            .await
            .unwrap();
    }
    let cw = r.json["cannibalization_warnings"].as_array().unwrap();
    assert_eq!(cw.len(), 1);
    assert_eq!(cw[0]["anchor"], "alpha");
    assert!(cw[0]["target_a_url"].is_string() && cw[0]["target_b_title"].is_string());
    assert!(r.json["site"].get("api_key").is_none());
    assert_eq!(
        h.get(&format!("/dashboard/{sa}"), &h.user_b).await.status,
        StatusCode::NOT_FOUND
    );
    // usage
    let r = h.get("/usage", &h.user_a).await;
    assert_eq!(r.json["sites"], 1);
    assert_eq!(r.json["posts"], 4);
    assert_eq!(r.json["paragraphs"], 1);
    assert_eq!(r.json["applied_links"], 1);
    assert_eq!(h.get("/usage", &h.admin).await.json["sites"], 2);
    // posts
    let r = h
        .get(
            &format!("/posts/{sa}?limit=9999&search=Post%201&is_orphan=0"),
            &h.user_a,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["total"], 1);
    assert_eq!(r.json["posts"][0]["cluster_label"], "Topic A");
    assert_eq!(r.json["post_types"], json!(["post"]));
    assert_eq!(r.json["clusters"][0]["count"], 4);
    let r = h.get(&format!("/posts/{sa}?limit=500"), &h.user_a).await;
    assert_eq!(r.json["posts"][0]["suggested_out"], 1);
    assert_eq!(r.json["posts"].as_array().unwrap().len(), 4);
    let r = h.get(&format!("/posts/{sa}?search=%25"), &h.user_a).await;
    assert_eq!(r.json["total"], 0);
    assert_eq!(
        h.get(&format!("/posts/{sa}?cluster_id=bad"), &h.user_a)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.get(&format!("/posts/{sa}?is_orphan=maybe"), &h.user_a)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    // links report
    let r = h.get(&format!("/reports/links/{sa}"), &h.user_a).await;
    let rows = r.json.as_array().unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["post_id"], h.posts[0].as_str());
    assert_eq!(rows[0]["link_flow_score"], 54);
    assert_eq!(rows[2]["inbound"], 2);
    for k in [
        "url",
        "title",
        "inbound",
        "outbound",
        "pagerank",
        "click_depth",
        "duplicate_count",
    ] {
        assert!(rows[0].get(k).is_some());
    }
    // graph
    let r = h
        .get(
            &format!("/reports/graph/{sa}?max_nodes=2&max_edges=99999"),
            &h.user_a,
        )
        .await;
    assert_eq!(r.json["node_limit"], 2);
    assert_eq!(r.json["edge_limit"], 2500);
    assert_eq!(r.json["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(r.json["truncated"], true);
    assert_eq!(r.json["total_nodes"], 4);
    assert!(r.json["nodes"][0]["is_orphan"].is_boolean());
    // anchors
    let r = h.get(&format!("/reports/anchors/{sa}"), &h.user_a).await;
    assert_eq!(r.json["total"], 4);
    assert_eq!(r.json["byType"]["exact"], 3);
    assert_eq!(
        r.json["byAnchor"][0],
        json!({"anchor": "alpha", "count": 2})
    );
    assert!(r.json["warnings"][0].as_str().unwrap().contains("75%"));
    // history JSON + CSV (ISO timestamps)
    let r = h.get(&format!("/history/{sa}?limit=500"), &h.user_a).await;
    assert_eq!(r.json["limit"], 100);
    assert_eq!(r.json["total"], 1);
    assert_eq!(r.json["rows"][0]["tool"], "existing");
    let r = h.get(&format!("/history/{sa}?format=csv"), &h.user_a).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/csv")
    );
    assert!(
        r.text
            .starts_with("Domain,Source,Target,Tool,Anchor,Status,Applied At\r\n")
    );
    assert!(r.text.contains("2026-02-01T10:00:00.000Z"));
    assert!(
        r.headers["content-disposition"]
            .to_str()
            .unwrap()
            .contains("link-history-a.example.com.csv")
    );
    // csv for the site with a hostile domain: header is sanitized
    let r = h.get(&format!("/csv/export/{}", h.site_b), &h.admin).await;
    assert_eq!(r.status, StatusCode::OK);
    let cd = r.headers["content-disposition"]
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        cd,
        "attachment; filename=\"siloforge-edges-ev_il__.com.csv\""
    );
    // edges csv
    let r = h.get(&format!("/csv/export/{sa}"), &h.user_a).await;
    assert!(
        r.text
            .starts_with("source_url,target_url,anchor,anchor_type,status,insert_module")
    );
    assert_eq!(r.text.split("\r\n").count(), 3);
    // link health JSON + CSV (formula guard)
    let r = h
        .get(&format!("/reports/link-health/{sa}?limit=1"), &h.user_a)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["summary"]["broken"], 1);
    assert_eq!(r.json["summary"]["chains_over_two"], 1);
    assert_eq!(r.json["summary"]["external"], 1);
    assert_eq!(
        r.json["broken"][0]["source_url"],
        "https://a.example.com/p0"
    );
    assert_eq!(r.json["broken"][0]["is_broken"], true);
    assert!(r.json["broken"][0]["redirect_chain"].is_array());
    assert_eq!(r.json["breadcrumbs"]["checked"], 4);
    assert_eq!(
        r.json["breadcrumbs"]["missing"].as_array().unwrap().len(),
        1
    );
    assert_eq!(r.json["limit"], 1);
    let r = h
        .get(&format!("/reports/link-health/{sa}?format=csv"), &h.user_a)
        .await;
    assert!(r.text.contains(",'=cmd,"));
    assert!(r.text.contains(",true,"));
    // metrics
    let r = h
        .get(&format!("/reports/metrics/{sa}?trend=1"), &h.user_a)
        .await;
    assert_eq!(r.json["latest"]["metrics"]["orphans"], 3);
    assert_eq!(r.json["trend"].as_array().unwrap().len(), 1);
    let d = r.json["deltas"].as_array().unwrap();
    let orph = d.iter().find(|x| x["key"] == "orphans").unwrap();
    assert_eq!(orph["delta"], -2);
    assert_eq!(orph["improved"], true);
    let r = h.get(&format!("/reports/metrics/{sa}"), &h.user_a).await;
    assert_eq!(r.json["trend"].as_array().unwrap().len(), 2);
    assert_eq!(r.json["trend"][0]["metrics"]["orphans"], 5);
    let r = h
        .get(&format!("/reports/metrics/{}", h.site_b), &h.admin)
        .await;
    assert_eq!(r.json, json!({"latest": null, "deltas": [], "trend": []}));
    h.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn silo_csv_import_schedules() {
    let h = H::new().await;
    let sa = h.site_a.clone();
    let r = h.get(&format!("/silo/{sa}/clusters"), &h.user_a).await;
    assert_eq!(r.json[0]["members"], 4);
    let body = json!({"clusters": [
        {"label": "Custom 1", "pillar_url": "https://a.example.com/p0", "member_urls": ["https://a.example.com/p1", "https://nope"]},
        {"label": "Custom 2", "member_urls": []}
    ]});
    let r = h
        .post(
            &format!("/silo/{sa}/clusters/import"),
            &h.user_a,
            body.clone(),
        )
        .await;
    assert_eq!(r.json, json!({"clusters": 2, "members": 1, "skipped": 1}));
    let r = h
        .post(&format!("/silo/{sa}/clusters/import"), &h.user_a, body)
        .await;
    assert_eq!(r.json["clusters"], 2);
    let r = h.get(&format!("/silo/{sa}/clusters"), &h.user_a).await;
    let arr = r.json.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    let c1 = arr.iter().find(|c| c["label"] == "Custom 1").unwrap();
    assert_eq!(c1["members"], 2);
    assert_eq!(c1["pillar_url"], "https://a.example.com/p0");
    let r = h
        .get(&format!("/silo/{sa}/clusters/export"), &h.user_a)
        .await;
    let ex = r.json["clusters"].as_array().unwrap();
    let c1 = ex.iter().find(|c| c["label"] == "Custom 1").unwrap();
    assert_eq!(
        c1["member_urls"],
        json!(["https://a.example.com/p0", "https://a.example.com/p1"])
    );
    assert_eq!(
        h.post(&format!("/silo/{sa}/clusters/import"), &h.user_a, json!({}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.post(
            &format!("/silo/{sa}/clusters/import"),
            &h.user_b,
            json!({"clusters": []})
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    // CSV import (raw text/csv body)
    let csv = "url,title,content_html\r\nhttps://a.example.com/c1,\"Title, one\",\"<p>Hi there</p>\"\n,,\njavascript:x,t,c\nhttps://a.example.com/c1,T,<p>Hi there</p>\n";
    let r = h
        .raw(
            "POST",
            &format!("/csv/import/{sa}"),
            &h.user_a,
            "text/csv",
            csv,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json, json!({"received": 4, "upserted": 1, "skipped": 3}));
    for bad in ["", "url,title\nhttps://a.com,x", "only-header\n"] {
        let r = h
            .raw(
                "POST",
                &format!("/csv/import/{sa}"),
                &h.user_a,
                "text/csv",
                bad,
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
    }
    let r = h
        .raw(
            "POST",
            &format!("/csv/import/{sa}"),
            &h.user_b,
            "text/csv",
            csv,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // schedules
    let sp = format!("/schedules/{sa}");
    let r = h.post(&sp, &h.user_a, json!({"job_kind": "crawl", "cron": "*/30 * * * *", "tz": "Europe/Berlin", "payload": {}})).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["scheduled"], true);
    assert_eq!(r.json["active"], true);
    assert!(r.json["next_run_at"].is_string());
    let sched_id = r.json["id"].as_str().unwrap().to_string();
    let r = h
        .post(
            &sp,
            &h.user_a,
            json!({"job_kind": "crawl", "cron": "0 3 * * *", "tz": "UTC", "active": false}),
        )
        .await;
    assert_eq!(r.json["id"], sched_id.as_str());
    assert_eq!(r.json["cron"], "0 3 * * *");
    assert_eq!(r.json["next_run_at"], Value::Null);
    let r = h
        .post(
            &sp,
            &h.user_a,
            json!({"cron": "0 4 * * *", "payload": {"topology": "circle"}}),
        )
        .await;
    assert_eq!(r.json["job_kind"], "audit");
    let r = h.post(&sp, &h.user_a, json!({"cron": "@daily"})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        r.json["message"],
        "invalid cron: macro \"@daily\" is not supported — use a 5-field cron expression"
    );
    assert_eq!(
        h.post(
            &sp,
            &h.user_a,
            json!({"cron": "* * * * *", "tz": "Mars/Base"})
        )
        .await
        .json["error"],
        "unknown timezone: Mars/Base"
    );
    assert_eq!(
        h.post(
            &sp,
            &h.user_a,
            json!({"cron": "* * * * *", "job_kind": "inject"})
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.post(&sp, &h.user_a, json!({"cron": "* * * * *", "payload": [1]}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let r = h.get(&sp, &h.user_a).await;
    let list = r.json.as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["job_kind"], "audit");
    assert!(list[0]["payload"].is_object());
    // run-now: default audit (v1), by kind, by schedule id
    let r = h.post(&format!("{sp}/run-now"), &h.user_a, json!({})).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["job_kind"], "audit");
    let r = h
        .post(
            &format!("{sp}/run-now"),
            &h.user_a,
            json!({"schedule_id": sched_id}),
        )
        .await;
    assert_eq!(r.json["job_kind"], "crawl");
    let job = r.json["job_run_id"].as_str().unwrap().to_string();
    let r = h.get(&sp, &h.user_a).await;
    let crawl = r
        .json
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["job_kind"] == "crawl")
        .unwrap()
        .clone();
    assert_eq!(crawl["last_job_run_id"], job.as_str());
    let r = h
        .post(
            &format!("{sp}/run-now"),
            &h.user_a,
            json!({"job_kind": "link_health"}),
        )
        .await;
    assert_eq!(r.json["job_kind"], "link_health");
    assert_eq!(
        h.post(
            &format!("{sp}/run-now"),
            &h.user_a,
            json!({"topology": "x"})
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.post(
            &format!("{sp}/run-now"),
            &h.user_a,
            json!({"schedule_id": sf_core::new_id()})
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.post(&format!("{sp}/run-now"), &h.user_b, json!({}))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.call(
            "DELETE",
            &format!("/schedules/{sched_id}"),
            Some(&h.user_b),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.call(
            "DELETE",
            &format!("/schedules/{sched_id}"),
            Some(&h.user_a),
            None
        )
        .await
        .json,
        json!({"deleted": true})
    );
    assert_eq!(
        h.call("DELETE", "/schedules/zzz", Some(&h.user_a), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    h.done();
}

struct FakeProbe;
impl ProbeService for FakeProbe {
    fn probe_provider<'a>(&'a self, _id: &'a str) -> BoxFut<'a, sf_core::Result<Value>> {
        Box::pin(async {
            Ok(json!({"ok": true, "latency_ms": 12, "status_code": 200, "error": null}))
        })
    }
    fn probe_model<'a>(&'a self, _id: &'a str) -> BoxFut<'a, sf_core::Result<Value>> {
        Box::pin(async {
            Ok(
                json!({"ok": false, "latency_ms": 5, "status_code": 401, "error": "unauthorized", "dim": 384}),
            )
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn providers_models_chains() {
    let h = H::new().await;
    // admin only
    assert_eq!(
        h.get("/providers/health", &h.user_a).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.post("/providers", &h.user_a, json!({})).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.call("GET", "/providers", None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let ad = h.admin.clone();
    let r = h
        .post("/providers", &ad, json!({"name": " openai ", "kind": "openai-compatible", "base_url": "https://api.x.com/v1/",
              "api_key": "sk-live-SECRET-1234", "headers": {"X-A": "b"}}))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.json["name"], "openai");
    assert_eq!(r.json["label"], "openai");
    assert_eq!(r.json["base_url"], "https://api.x.com/v1");
    assert_eq!(r.json["has_api_key"], true);
    assert_eq!(r.json["api_key_masked"], "************1234");
    assert_eq!(r.json["enabled"], true);
    let pid = r.json["id"].as_str().unwrap().to_string();
    assert_eq!(
        h.post(
            "/providers",
            &ad,
            json!({"name": "openai", "kind": "custom", "base_url": "http://x.io"})
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    for bad in [
        json!({"name": "b", "kind": "weird", "base_url": "http://x.io"}),
        json!({"name": "b", "kind": "custom", "base_url": "file:///etc"}),
        json!({"name": "", "kind": "custom", "base_url": "http://x.io"}),
        json!({"name": "b", "kind": "custom", "base_url": "http://x.io", "headers": {"a": 1}}),
    ] {
        assert_eq!(
            h.post("/providers", &ad, bad).await.status,
            StatusCode::BAD_REQUEST
        );
    }
    let r = h.get(&format!("/providers/{pid}"), &ad).await;
    // L-07: header values are masked in responses and sealed at rest.
    assert_eq!(r.json["headers"], json!({"X-A": "****"}));
    let stored: String = {
        let p = pid.clone();
        h.st.db
            .read(move |c| {
                c.query_row(
                    "SELECT headers FROM model_providers WHERE id=?1",
                    [&p],
                    |r| r.get(0),
                )
                .map_err(sf_store::err)
            })
            .await
            .unwrap()
    };
    assert!(
        stored.contains("v1:") && !stored.contains("\"b\""),
        "{stored}"
    );
    // Echoing the masked value back keeps the secret; a new value replaces it.
    h.call(
        "PATCH",
        &format!("/providers/{pid}"),
        Some(&ad),
        Some(json!({"headers": {"X-A": "****", "X-Long": "Bearer abcdefgh"}})),
    )
    .await;
    let r = h.get(&format!("/providers/{pid}"), &ad).await;
    assert_eq!(r.json["headers"]["X-Long"], "***********efgh");
    let stored2: String = {
        let p = pid.clone();
        h.st.db
            .read(move |c| {
                c.query_row(
                    "SELECT headers FROM model_providers WHERE id=?1",
                    [&p],
                    |r| r.get(0),
                )
                .map_err(sf_store::err)
            })
            .await
            .unwrap()
    };
    let v: Value = serde_json::from_str(&stored2).unwrap();
    assert_eq!(
        sf_core::crypto::open_header_value(&h.st.secret_key, v["X-A"].as_str().unwrap()),
        "b"
    );
    assert_eq!(
        h.get(&format!("/providers/{}", sf_core::new_id()), &ad)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // patch: keep key when absent, clear with ""
    let r = h
        .call(
            "PATCH",
            &format!("/providers/{pid}"),
            Some(&ad),
            Some(json!({"label": "OpenAI"})),
        )
        .await;
    assert_eq!(r.json["has_api_key"], true);
    assert_eq!(r.json["label"], "OpenAI");
    // probe with no models: v1 result without calling the backend
    let r = h
        .post(&format!("/providers/{pid}/probe"), &ad, json!({}))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["ok"], false);
    assert!(
        r.json["error"]
            .as_str()
            .unwrap()
            .contains("no enabled models")
    );
    // models
    let r = h
        .post(
            "/models",
            &ad,
            json!({"provider_id": pid, "model_id": "gpt-x", "role": "chat", "price_in": 1.5}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.json["label"], "gpt-x");
    let chat_model = r.json["id"].as_str().unwrap().to_string();
    let r = h
        .post(
            "/models",
            &ad,
            json!({"provider_id": pid, "model_id": "emb", "role": "embed", "dim": 768}),
        )
        .await;
    let embed_model = r.json["id"].as_str().unwrap().to_string();
    assert_eq!(
        h.post(
            "/models",
            &ad,
            json!({"provider_id": pid, "model_id": "gpt-x", "role": "chat"})
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.post(
            "/models",
            &ad,
            json!({"provider_id": sf_core::new_id(), "model_id": "m", "role": "chat"})
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.post(
            "/models",
            &ad,
            json!({"provider_id": "x", "model_id": "m", "role": "chat"})
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.post(
            "/models",
            &ad,
            json!({"provider_id": pid, "model_id": "m", "role": "vision"})
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.post(
            "/models",
            &ad,
            json!({"provider_id": pid, "model_id": "m", "role": "chat", "price_in": -1})
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    let r = h
        .get(&format!("/models?provider_id={pid}&role=embed"), &ad)
        .await;
    assert_eq!(r.json.as_array().unwrap().len(), 1);
    assert_eq!(
        h.get("/models?provider_id=bad", &ad).await.status,
        StatusCode::BAD_REQUEST
    );
    let r = h
        .call(
            "PATCH",
            &format!("/models/{chat_model}"),
            Some(&ad),
            Some(json!({"notes": "n", "enabled": false})),
        )
        .await;
    assert_eq!(r.json["enabled"], false);
    let r = h
        .call(
            "PATCH",
            &format!("/models/{chat_model}"),
            Some(&ad),
            Some(json!({"enabled": true})),
        )
        .await;
    assert_eq!(r.json["notes"], "n");
    // probes: backend missing -> 503, registered -> v1 shape
    assert_eq!(
        h.post(&format!("/models/{chat_model}/probe"), &ad, json!({}))
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    h.st.services.insert(Arc::new(Prober(Arc::new(FakeProbe))));
    let r = h
        .post(&format!("/models/{chat_model}/probe"), &ad, json!({}))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["index_dim"], 384);
    assert_eq!(r.json["status_code"], 401);
    let r = h
        .post(&format!("/providers/{pid}/probe"), &ad, json!({}))
        .await;
    assert_eq!(r.json["ok"], true);
    let r = h.get("/providers/health", &ad).await;
    assert_eq!(r.json["index_dim"], 384);
    let p = &r.json["providers"][0];
    assert_eq!(p["last_probe_ok"], true);
    assert_eq!(p["models"].as_array().unwrap().len(), 2);
    let m = p["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == chat_model.as_str())
        .unwrap()
        .clone();
    assert_eq!(m["last_probe_ok"], true);
    assert_eq!(
        h.get("/providers", &ad)
            .await
            .json
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // chains
    let r = h
        .post(
            "/model-chains",
            &ad,
            json!({"name": "Main chat", "role": "chat", "active": true}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.json["active"], true);
    let c1 = r.json["id"].as_str().unwrap().to_string();
    let r = h
        .post("/model-chains", &ad, json!({"name": "Alt", "role": "chat"}))
        .await;
    let c2 = r.json["id"].as_str().unwrap().to_string();
    assert_eq!(r.json["active"], false);
    assert_eq!(
        h.post(
            "/model-chains",
            &ad,
            json!({"name": "S", "role": "chat", "site_id": sf_core::new_id()})
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let r = h
        .post(
            "/model-chains",
            &ad,
            json!({"name": "Site", "role": "chat", "site_id": h.site_a}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let r = h
        .post(&format!("/model-chains/{c2}/activate"), &ad, json!({}))
        .await;
    assert_eq!(r.json["active"], true);
    let r = h.get("/model-chains?site_id=global&role=chat", &ad).await;
    let chains = r.json.as_array().unwrap();
    assert_eq!(chains.len(), 2);
    assert_eq!(chains.iter().filter(|c| c["active"] == true).count(), 1);
    assert_eq!(
        h.get("/model-chains?site_id=bad", &ad).await.status,
        StatusCode::BAD_REQUEST
    );
    let r = h
        .post(&format!("/model-chains/{c2}/deactivate"), &ad, json!({}))
        .await;
    assert_eq!(r.json["active"], false);
    let r = h
        .call(
            "PATCH",
            &format!("/model-chains/{c1}"),
            Some(&ad),
            Some(json!({"name": "Renamed"})),
        )
        .await;
    assert_eq!(r.json["name"], "Renamed");
    // rungs: role + dim guards
    let rp = format!("/model-chains/{c1}/rungs");
    let r = h.post(&rp, &ad, json!({"model_row_id": embed_model})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(
        r.json["error"]
            .as_str()
            .unwrap()
            .contains("cannot be a rung")
    );
    let r = h
        .post(
            &rp,
            &ad,
            json!({"model_row_id": chat_model, "max_retries": 2}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.json["position"], 0);
    let rung1 = r.json["id"].as_str().unwrap().to_string();
    let r = h
        .post(
            &rp,
            &ad,
            json!({"model_row_id": chat_model, "timeout_ms": 5000}),
        )
        .await;
    assert_eq!(r.json["position"], 1);
    let rung2 = r.json["id"].as_str().unwrap().to_string();
    assert_eq!(
        h.post(
            &rp,
            &ad,
            json!({"model_row_id": chat_model, "max_retries": 11})
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.post(&rp, &ad, json!({"model_row_id": sf_core::new_id()}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let r = h
        .post(
            &format!("{rp}/reorder"),
            &ad,
            json!({"rung_ids": [rung2, rung1]}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json[0]["id"], rung2.as_str());
    assert_eq!(r.json[1]["position"], 1);
    assert_eq!(
        h.post(&format!("{rp}/reorder"), &ad, json!({"rung_ids": [rung2]}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.post(
            &format!("{rp}/reorder"),
            &ad,
            json!({"rung_ids": [rung2, rung2]})
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    let r = h
        .call(
            "PATCH",
            &format!("/model-chain-rungs/{rung1}"),
            Some(&ad),
            Some(json!({"enabled": false})),
        )
        .await;
    assert_eq!(r.json["enabled"], false);
    let r = h.get("/model-chains", &ad).await;
    let main = r
        .json
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == c1.as_str())
        .unwrap()
        .clone();
    assert_eq!(main["rungs"].as_array().unwrap().len(), 2);
    assert_eq!(main["rungs"][0]["wire_model_id"], "gpt-x");
    assert_eq!(main["rungs"][0]["provider_name"], "openai");
    // embed chain with wrong dim then right dim via PUT
    let r = h
        .post(
            "/model-chains",
            &ad,
            json!({"name": "Emb", "role": "embed"}),
        )
        .await;
    let ce = r.json["id"].as_str().unwrap().to_string();
    let r = h
        .call(
            "PUT",
            &format!("/model-chains/{ce}/rungs"),
            Some(&ad),
            Some(json!({"rungs": [{"model_row_id": embed_model}]})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.json["error"].as_str().unwrap().contains("768-d"));
    h.call(
        "PATCH",
        &format!("/models/{embed_model}"),
        Some(&ad),
        Some(json!({"dim": 384})),
    )
    .await;
    let r = h
        .call(
            "PUT",
            &format!("/model-chains/{ce}/rungs"),
            Some(&ad),
            Some(json!({"rungs": [{"model_row_id": embed_model}]})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json.as_array().unwrap().len(), 1);
    let r = h.get(&format!("/model-chains/{ce}/rungs"), &ad).await;
    assert_eq!(r.json[0]["model_id"], embed_model.as_str());
    // B11: a chained embed model cannot change its width.
    let r = h
        .call(
            "PATCH",
            &format!("/models/{embed_model}"),
            Some(&ad),
            Some(json!({"dim": 768})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    // deletes
    assert_eq!(
        h.call(
            "DELETE",
            &format!("/model-chain-rungs/{rung1}"),
            Some(&ad),
            None
        )
        .await
        .json,
        json!({"deleted": true})
    );
    assert_eq!(
        h.call(
            "DELETE",
            &format!("/model-chain-rungs/{rung1}"),
            Some(&ad),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.call("DELETE", &format!("/model-chains/{c2}"), Some(&ad), None)
            .await
            .json,
        json!({"deleted": true})
    );
    assert_eq!(
        h.call("DELETE", &format!("/models/{chat_model}"), Some(&ad), None)
            .await
            .json,
        json!({"deleted": true})
    );
    let r = h
        .call(
            "PATCH",
            &format!("/providers/{pid}"),
            Some(&ad),
            Some(json!({"api_key": ""})),
        )
        .await;
    assert_eq!(r.json["has_api_key"], false);
    assert_eq!(r.json["api_key_masked"], "");
    assert_eq!(
        h.call("DELETE", &format!("/providers/{pid}"), Some(&ad), None)
            .await
            .json,
        json!({"deleted": true})
    );
    assert_eq!(
        h.call("DELETE", &format!("/providers/{pid}"), Some(&ad), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // stored key is encrypted
    h.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn api_token_scopes_and_hostile_input() {
    let h = H::new().await;
    let (read_tok, read_hash, _) = sf_core::crypto::new_api_token();
    let (admin_tok, admin_hash, _) = sf_core::crypto::new_api_token();
    {
        let (a, ad) = (h.a_id.clone(), h.admin_id.clone());
        h.st.db
            .write(move |t| {
                t.execute(
                    "INSERT INTO api_tokens(id,user_id,name,token_hash,prefix,scopes) VALUES(?1,?2,'ro',?3,'sfk_','[\"read\"]')",
                    rusqlite::params![sf_core::new_id(), a, read_hash],
                )
                .map_err(sf_store::err)?;
                t.execute(
                    "INSERT INTO api_tokens(id,user_id,name,token_hash,prefix,scopes) VALUES(?1,?2,'hermes',?3,'sfk_','[\"write\"]')",
                    rusqlite::params![sf_core::new_id(), ad, admin_hash],
                )
                .map_err(sf_store::err)?;
                Ok(())
            })
            .await
            .unwrap();
    }
    assert_eq!(h.get("/sites", &read_tok).await.status, StatusCode::OK);
    assert_eq!(
        h.post(&format!("/sites/{}/crawl", h.site_a), &read_tok, json!({}))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.post("/sites", &read_tok, json!({"domain": "x.com"}))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    // write-scoped admin token: site routes ok, admin routes need the admin scope
    assert_eq!(
        h.post(&format!("/sites/{}/crawl", h.site_a), &admin_tok, json!({}))
            .await
            .status,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        h.get("/providers", &admin_tok).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.post(
            "/auth/change-password",
            &admin_tok,
            json!({"current_password": ADMIN_PW, "new_password": "whatever12"})
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.get("/sites", "sfk_bogus").await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        h.get("/sites", "garbage.jwt.value").await.status,
        StatusCode::UNAUTHORIZED
    );
    // unknown /api/v1 path -> JSON 404, never the SPA
    let r = h.get("/does/not/exist", &h.admin).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(r.json["error"].is_string());
    // hostile ids / params never 500
    let ids = [
        "not-a-uuid",
        "'%20OR%201=1--",
        "00000000-0000-0000-0000-000000000000",
        "%00",
        "..%2F..%2Fetc",
    ];
    for id in ids {
        for path in [
            format!("/sites/{id}"),
            format!("/dashboard/{id}"),
            format!("/edges/{id}?limit=-1&offset=abc"),
            format!("/posts/{id}?search=%25%5F"),
            format!("/reports/links/{id}"),
            format!("/reports/graph/{id}?max_nodes=-5"),
            format!("/reports/anchors/{id}"),
            format!("/reports/link-health/{id}?format=csv"),
            format!("/reports/metrics/{id}?trend=1e999"),
            format!("/history/{id}?format=csv"),
            format!("/jobs/{id}"),
            format!("/jobs/run/{id}"),
            format!("/silo/{id}/clusters"),
            format!("/silo/{id}/clusters/export"),
            format!("/csv/export/{id}"),
            format!("/schedules/{id}"),
            format!("/rules/keywords/{id}"),
            format!("/rules/ignore/{id}"),
            format!("/sites/{id}/gsc"),
            format!("/providers/{id}"),
            format!("/model-chains/{id}/rungs"),
        ] {
            let r = h.get(&path, &h.admin).await;
            assert!(r.status.as_u16() < 500, "{path} -> {}", r.status);
            assert!(
                r.status == StatusCode::NOT_FOUND
                    || r.status == StatusCode::OK
                    || r.status == StatusCode::BAD_REQUEST,
                "{path} -> {}",
                r.status
            );
        }
        for (m, path) in [
            ("POST", format!("/edges/{id}/decide")),
            ("PATCH", format!("/edges/{id}/anchor")),
            ("POST", format!("/sites/{id}/plan")),
            ("POST", format!("/suggestions/{id}/ack")),
            ("DELETE", format!("/schedules/{id}")),
            ("DELETE", format!("/model-chain-rungs/{id}")),
            ("POST", format!("/model-chains/{id}/activate")),
            ("POST", format!("/models/{id}/probe")),
        ] {
            let r = h
                .call(
                    m,
                    &path,
                    Some(&h.admin),
                    Some(json!({"decision": "approved", "anchor": "x", "edge_id": id, "ok": true})),
                )
                .await;
            assert!(r.status.as_u16() < 500, "{m} {path} -> {}", r.status);
        }
    }
    // wrong JSON types everywhere answer 400
    for (m, path, body) in [
        (
            "POST",
            "/bulk/decide".to_string(),
            json!({"edge_ids": "x", "decision": 1}),
        ),
        ("POST", "/bulk/run".to_string(), json!({"site_id": 5})),
        (
            "POST",
            format!("/rules/keywords/{}", h.site_a),
            json!({"keyword": ["a"], "target_url": {}}),
        ),
        (
            "POST",
            format!("/silo/{}/clusters/import", h.site_a),
            json!({"clusters": "x"}),
        ),
        (
            "POST",
            format!("/schedules/{}", h.site_a),
            json!({"cron": 5}),
        ),
        (
            "POST",
            "/sync".to_string(),
            json!({"domain": "x", "posts": [{"wp_post_id": "nan"}]}),
        ),
        (
            "PATCH",
            format!("/sites/{}", h.site_a),
            json!({"settings": {"money_page_urls": "x"}}),
        ),
    ] {
        let r = h.call(m, &path, Some(&h.user_a), Some(body)).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{m} {path}: {}", r.text);
        assert!(r.json["error"].is_string());
    }
    h.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn probe_through_sf_ai() {
    let h = H::new().await;
    let ai = sf_ai::Ai::new(h.st.db.clone(), h.st.secret_key.clone(), Default::default()).unwrap();
    h.st.services.insert(Arc::new(ai));
    let ad = h.admin.clone();
    let r = h
        .post("/providers", &ad, json!({"name": "dead", "kind": "openai-compatible", "base_url": "http://127.0.0.1:9/v1"}))
        .await;
    let pid = r.json["id"].as_str().unwrap().to_string();
    let r = h
        .post(
            "/models",
            &ad,
            json!({"provider_id": pid, "model_id": "m", "role": "chat"}),
        )
        .await;
    let mid = r.json["id"].as_str().unwrap().to_string();
    let r = h
        .post(&format!("/models/{mid}/probe"), &ad, json!({}))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["ok"], false);
    assert_eq!(r.json["index_dim"], 384);
    assert!(r.json["error"].is_string());
    let r = h
        .post(&format!("/providers/{pid}/probe"), &ad, json!({}))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["ok"], false);
    h.done();
}

/// Contract-diff regression (docs/testing/contract-diff.md): v1's Postgres sorted text with the
/// `en_US.UTF-8` collation (punctuation ignored, case-insensitive, lowercase first). SQLite's BINARY
/// order reordered posts, orphan lists, links-report ties, clusters, export members, link-health
/// rows and cannibalization warnings, and shifted `limit`/`offset` page boundaries.
#[tokio::test(flavor = "multi_thread")]
async fn text_order_matches_v1_collation() {
    let h = H::new().await;
    let sa = h.site_a.clone();
    let names = [
        ("Zebra", "https://a.example.com/zebra/"),
        (
            "beer bottle crafts",
            "https://a.example.com/tag/bottle-art/",
        ),
        ("Beer Bottle Crafts", "https://a.example.com/tag/bottle/"),
        ("apple", "https://a.example.com/apple/"),
        ("10 Best Picks", "https://a.example.com/10-best/"),
        ("Éclair", "https://a.example.com/eclair/"),
    ];
    let ids: Vec<String> = names.iter().map(|_| sf_core::new_id()).collect();
    {
        let (sa, ids) = (sa.clone(), ids.clone());
        h.st.db
            .write(move |t| {
                let x = |sql: &str, p: &[&dyn rusqlite::ToSql]| {
                    t.execute(sql, p).map_err(sf_store::err).map(|_| ())
                };
                let (k1, k2) = (sf_core::new_id(), sf_core::new_id());
                x("INSERT INTO clusters(id,site_id,label,source) VALUES(?1,?2,'water bottle many','auto')", &[&k1, &sa])?;
                x("INSERT INTO clusters(id,site_id,label,source) VALUES(?1,?2,'water bottled brands','auto')", &[&k2, &sa])?;
                for (i, ((title, url), id)) in names.iter().zip(&ids).enumerate() {
                    x("INSERT INTO posts(id,site_id,wp_post_id,url,title,pagerank,is_orphan,cluster_id)
                       VALUES(?1,?2,?3,?4,?5,0.05,1,?6)",
                      &[id, &sa, &(100 + i as i64), url, title, &k1])?;
                }
                // nofollow rows from bottle-art/ (inserted first) and bottle/
                for src in [&ids[1], &ids[2]] {
                    x("INSERT INTO link_health(id,site_id,source_post_id,href,requested_url,scope,is_nofollow,anchor)
                       VALUES(?1,?2,?3,'https://a.example.com/x','https://a.example.com/x','internal',1,'x')",
                      &[&sf_core::new_id(), &sa, src])?;
                }
                // two conflicting exact anchors: 'Bottle Cap' (BINARY-first) and 'bottle'
                for (anchor, s, d1, d2) in [("Bottle Cap", 0, 1, 2), ("bottle", 3, 4, 5)] {
                    for d in [d1, d2] {
                        x("INSERT INTO link_edges(id,site_id,source_post_id,target_post_id,anchor,anchor_type,status)
                           VALUES(?1,?2,?3,?4,?5,'exact','rejected')",
                          &[&sf_core::new_id(), &sa, &ids[s], &ids[d], &anchor])?;
                    }
                }
                Ok(())
            })
            .await
            .unwrap();
    }
    let strs = |v: &Value, key: &str| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|x| x[key].as_str().unwrap().to_string())
            .collect()
    };
    // posts: suggested_out DESC, then title in v1 order
    let r = h.get(&format!("/posts/{sa}?limit=500"), &h.user_a).await;
    let titles: Vec<&str> = r.json["posts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["suggested_out"] == 0)
        .map(|p| p["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        [
            "10 Best Picks",
            "apple",
            "beer bottle crafts",
            "Beer Bottle Crafts",
            "Éclair",
            "Post 1",
            "Post 3",
            "Zebra"
        ]
    );
    // page boundaries follow the same order (2 posts with suggested_out=1 come first)
    let r = h
        .get(&format!("/posts/{sa}?limit=2&offset=3"), &h.user_a)
        .await;
    assert_eq!(
        strs(&r.json["posts"], "title"),
        ["apple", "beer bottle crafts"]
    );
    assert_eq!(
        strs(&r.json["clusters"], "label"),
        ["Topic A", "water bottled brands", "water bottle many"]
    );
    // dashboard orphan_posts and cannibalization warnings
    let r = h.get(&format!("/dashboard/{sa}"), &h.user_a).await;
    let orphans = strs(&r.json["orphan_posts"], "title");
    assert_eq!(
        &orphans[..3],
        ["10 Best Picks", "apple", "beer bottle crafts"]
    );
    let anchors = strs(&r.json["cannibalization_warnings"], "anchor");
    assert_eq!(anchors, ["alpha", "bottle", "Bottle Cap"]);
    // links report: equal pagerank -> url in v1 order (tag/bottle/ before tag/bottle-art/)
    let r = h.get(&format!("/reports/links/{sa}"), &h.user_a).await;
    let urls: Vec<String> = strs(&r.json, "url")
        .into_iter()
        .filter(|u| u.contains("/tag/"))
        .collect();
    assert_eq!(
        urls,
        [
            "https://a.example.com/tag/bottle/",
            "https://a.example.com/tag/bottle-art/"
        ]
    );
    // silo clusters list + export (labels and member urls)
    let r = h.get(&format!("/silo/{sa}/clusters"), &h.user_a).await;
    assert_eq!(
        strs(&r.json, "label"),
        ["Topic A", "water bottled brands", "water bottle many"]
    );
    let r = h
        .get(&format!("/silo/{sa}/clusters/export"), &h.user_a)
        .await;
    let cl = r.json["clusters"].as_array().unwrap();
    assert_eq!(cl[1]["label"], "water bottled brands");
    let members: Vec<&str> = cl[2]["member_urls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u.as_str().unwrap())
        .collect();
    assert_eq!(
        members,
        [
            "https://a.example.com/10-best/",
            "https://a.example.com/apple/",
            "https://a.example.com/eclair/",
            "https://a.example.com/tag/bottle/",
            "https://a.example.com/tag/bottle-art/",
            "https://a.example.com/zebra/"
        ]
    );
    // link-health nofollow list sorted by source url in v1 order
    let r = h
        .get(&format!("/reports/link-health/{sa}?limit=250"), &h.user_a)
        .await;
    let srcs: Vec<&str> = r.json["nofollow"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["scope"] == "internal")
        .map(|row| row["source_url"].as_str().unwrap())
        .collect();
    assert_eq!(
        srcs,
        [
            "https://a.example.com/tag/bottle/",
            "https://a.example.com/tag/bottle-art/"
        ]
    );
    h.done();
}

/// E2E pilot regressions: bulk undo counts only edges SiloForge injected, run-now passes the
/// stored schedule payload for every kind, link-health accepts `{internal_only}`.
#[tokio::test(flavor = "multi_thread")]
async fn pilot_regressions_undo_runnow_linkhealth() {
    let h = H::new().await;
    let sa = h.site_a.clone();
    let payload_of = |job: String| {
        let db = h.st.db.clone();
        async move {
            db.read(move |c| {
                c.query_row("SELECT payload FROM job_runs WHERE id=?1", [&job], |r| {
                    r.get::<_, String>(0)
                })
                .map_err(sf_store::err)
            })
            .await
            .map(|s| serde_json::from_str::<Value>(&s).unwrap())
            .unwrap()
        }
    };
    // (1) one injected edge (plan, applied_at) next to the fixture's applied `existing` edge
    let injected = sf_core::new_id();
    {
        let (sa, id, p) = (sa.clone(), injected.clone(), h.posts.clone());
        h.st.db
            .write(move |t| {
                t.execute(
                    "INSERT INTO link_edges(id,site_id,source_post_id,target_post_id,anchor,status,origin,applied_at)
                     VALUES(?1,?2,?3,?4,'injected','applied','plan','2026-03-01T00:00:00.000Z')",
                    rusqlite::params![id, sa, p[3], p[1]],
                )
                .map_err(sf_store::err)?;
                Ok(())
            })
            .await
            .unwrap();
    }
    let r = h
        .post("/bulk/undo", &h.user_a, json!({"site_id": sa}))
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_eq!(r.json["count"], 1);
    let p = payload_of(r.json["job_run_id"].as_str().unwrap().to_string()).await;
    assert_eq!(p["edge_ids"], json!([injected]));
    // (3) link-health: empty body, {} and {internal_only}
    let r = h
        .call(
            "POST",
            &format!("/sites/{sa}/link-health"),
            Some(&h.user_a),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    let p = payload_of(r.json["job_run_id"].as_str().unwrap().to_string()).await;
    assert_eq!(p, json!({}));
    let r = h
        .post(
            &format!("/sites/{sa}/link-health"),
            &h.user_a,
            json!({"internal_only": true}),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    let p = payload_of(r.json["job_run_id"].as_str().unwrap().to_string()).await;
    assert_eq!(p, json!({"internal_only": true}));
    assert_eq!(
        h.post(
            &format!("/sites/{sa}/link-health"),
            &h.user_a,
            json!({"internal_only": "yes"})
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // (2) run-now keeps the stored payload for non audit/plan kinds
    let sp = format!("/schedules/{sa}");
    let r = h
        .post(
            &sp,
            &h.user_a,
            json!({"job_kind": "link_health", "cron": "0 3 * * *", "payload": {"internal_only": false}}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let sched = r.json["id"].as_str().unwrap().to_string();
    let r = h
        .post(
            &format!("{sp}/run-now"),
            &h.user_a,
            json!({"schedule_id": sched}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["job_kind"], "link_health");
    let p = payload_of(r.json["job_run_id"].as_str().unwrap().to_string()).await;
    assert_eq!(p, json!({"internal_only": false}));
    // audit still normalises its keys on top of the stored payload
    let r = h
        .post(
            &sp,
            &h.user_a,
            json!({"job_kind": "audit", "cron": "0 4 * * *", "payload": {"skipCrawl": true, "note": "x"}}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let r = h.post(&format!("{sp}/run-now"), &h.user_a, json!({})).await;
    let p = payload_of(r.json["job_run_id"].as_str().unwrap().to_string()).await;
    assert_eq!(
        p,
        json!({"topology": null, "skip_crawl": true, "note": "x"})
    );
    h.done();
}
