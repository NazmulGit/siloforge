#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use sf_core::Config;
use sf_core::crypto::{sign_jwt, user_claims};
use sf_store::{Db, err};
use tower::ServiceExt;

use crate::AppState;

const SECRET: &str = "test-secret-0123456789abcdef";

struct Ctx {
    app: Router,
    st: AppState,
    admin: String,
    u1: String,
    u2: String,
    dir: std::path::PathBuf,
}

async fn setup() -> Ctx {
    let dir = std::env::temp_dir().join(format!("sf-v2-test-{}", sf_core::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = Config::default();
    cfg.home = dir.clone();
    cfg.auth.jwt_secret = SECRET.into();
    cfg.backup.passphrase_env = String::new();
    let db = Db::open(&cfg.db_path()).unwrap();
    db.write(|t| {
        t.execute_batch(
            "INSERT INTO users(id,email,password_hash,role) VALUES('ua','a@x.io','x','admin');
             INSERT INTO users(id,email,password_hash,role) VALUES('u1','u1@x.io','x','user');
             INSERT INTO users(id,email,password_hash,role) VALUES('u2','u2@x.io','x','user');
             INSERT INTO sites(id,domain,api_key,user_id) VALUES('s1','one.com','sf_1','u1');
             INSERT INTO sites(id,domain,api_key,user_id) VALUES('s2','two.com','sf_2','u2');
             INSERT INTO job_runs(id,site_id,kind,status) VALUES('j1','s1','crawl','running');
             INSERT INTO job_runs(id,site_id,kind,status) VALUES('j2','s2','plan','done');
             INSERT INTO job_runs(id,site_id,kind,status) VALUES('j3',NULL,'backup','done');
             INSERT INTO job_logs(job_run_id,level,message) VALUES('j1','info','first');
             INSERT INTO job_logs(job_run_id,level,message) VALUES('j1','warn','second');",
        )
        .map_err(err)
    })
    .await
    .unwrap();
    let st = AppState::new(cfg, db.clone(), [3u8; 32]);
    let notifier =
        sf_ops::notify::Notifier::start(db.clone(), st.secret_key.clone(), Default::default());
    let monitor = sf_ops::monitor::Monitor::new(st.cfg.clone(), db, Some(notifier.clone()));
    st.services.insert(Arc::new(notifier));
    st.services.insert(Arc::new(monitor));
    let app = Router::new()
        .nest("/api/v2", super::router())
        .with_state(st.clone());
    let tok = |role: &str, id: &str| sign_jwt(SECRET, &user_claims(role, id, 0)).unwrap();
    Ctx {
        app,
        admin: tok("admin", "ua"),
        u1: tok("user", "u1"),
        u2: tok("user", "u2"),
        st,
        dir,
    }
}

impl Ctx {
    async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let (s, bytes, _) = self.raw(method, path, token, body).await;
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (s, v)
    }

    async fn raw(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
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
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), 64 << 20)
            .await
            .unwrap();
        (status, bytes.to_vec(), headers)
    }

    fn done(self) {
        self.st.db.close();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_plaintext_once_and_scopes() {
    let c = setup().await;
    let (s, v) = c
        .call(
            "POST",
            "/api/v2/tokens",
            &c.u1,
            Some(json!({"name":"hermes","scopes":["read"],"expires_in_days":30})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let token = v["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("sfk_") && v["expires_at"].is_string());
    let (_, list) = c.call("GET", "/api/v2/tokens", &c.u1, None).await;
    let text = list.to_string();
    assert_eq!(list["tokens"].as_array().unwrap().len(), 1);
    assert!(!text.contains(&token) && !text.contains("token_hash"));
    // other user does not see it
    let (_, l2) = c.call("GET", "/api/v2/tokens", &c.u2, None).await;
    assert!(l2["tokens"].as_array().unwrap().is_empty());

    // read-scope token: GET ok, POST blocked
    let (s, _) = c.call("GET", "/api/v2/tokens", &token, None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = c
        .call(
            "POST",
            "/api/v2/tokens",
            &token,
            Some(json!({"name":"x","scopes":["read"]})),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = c.call("POST", "/api/v2/jobs/j1/cancel", &token, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // M-04: a write token cannot mint tokens at all (only * / admin tokens can)
    let (_, w) = c
        .call(
            "POST",
            "/api/v2/tokens",
            &c.u1,
            Some(json!({"name":"w","scopes":["write"]})),
        )
        .await;
    let wt = w["token"].as_str().unwrap().to_string();
    let (s, _) = c
        .call(
            "POST",
            "/api/v2/tokens",
            &wt,
            Some(json!({"name":"x","scopes":["*"]})),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = c
        .call(
            "POST",
            "/api/v2/tokens",
            &wt,
            Some(json!({"name":"x","scopes":["read"]})),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // M-04: a * token with an expiry mints children that cannot outlive it, records the parent,
    // and revoking it revokes the whole subtree.
    let (_, full) = c
        .call(
            "POST",
            "/api/v2/tokens",
            &c.u1,
            Some(json!({"name":"full","scopes":["*"],"expires_in_days":7})),
        )
        .await;
    let ft = full["token"].as_str().unwrap().to_string();
    let fid = full["id"].as_str().unwrap().to_string();
    let (s, over) = c
        .call(
            "POST",
            "/api/v2/tokens",
            &ft,
            Some(json!({"name":"child","scopes":["*"],"expires_in_days":30})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{over}");
    let (s, child) = c
        .call(
            "POST",
            "/api/v2/tokens",
            &ft,
            Some(json!({"name":"child","scopes":["*"]})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{child}");
    assert_eq!(child["parent_id"], fid.as_str());
    assert_eq!(child["expires_at"], full["expires_at"]);
    let ct = child["token"].as_str().unwrap().to_string();
    let (s, grand) = c
        .call(
            "POST",
            "/api/v2/tokens",
            &ct,
            Some(json!({"name":"grandchild","scopes":["read"],"expires_in_days":1})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{grand}");
    let gt = grand["token"].as_str().unwrap().to_string();
    let (s, _) = c.call("GET", "/api/v2/tokens", &gt, None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = c
        .call("DELETE", &format!("/api/v2/tokens/{fid}"), &c.u1, None)
        .await;
    assert_eq!(s, StatusCode::OK);
    for t in [&ft, &ct, &gt] {
        let (s, _) = c.call("GET", "/api/v2/tokens", t, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }

    // validation
    for bad in [
        json!({"name":"x","scopes":["admin"]}), // user cannot get admin scope
        json!({"name":"x","scopes":["root"]}),
        json!({"name":"","scopes":["read"]}),
        json!({"name":"x","scopes":[]}),
        json!({"name":"x","expires_in_days":0}),
    ] {
        let (s, _) = c
            .call("POST", "/api/v2/tokens", &c.u1, Some(bad.clone()))
            .await;
        assert!(
            s == StatusCode::BAD_REQUEST || s == StatusCode::FORBIDDEN,
            "{bad} -> {s}"
        );
    }
    let (s, _, _) = c.raw("POST", "/api/v2/tokens", &c.u1, None).await;
    assert!(s.is_client_error());

    // revoke -> token stops working; others cannot revoke it
    let id = v["id"].as_str().unwrap();
    let (s, _) = c
        .call("DELETE", &format!("/api/v2/tokens/{id}"), &c.u2, None)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = c
        .call("DELETE", &format!("/api/v2/tokens/{id}"), &c.u1, None)
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = c.call("GET", "/api/v2/tokens", &token, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // admin may mint admin tokens, which then pass admin routes
    let (_, a) = c
        .call(
            "POST",
            "/api/v2/tokens",
            &c.admin,
            Some(json!({"name":"ops","scopes":["admin"]})),
        )
        .await;
    let at = a["token"].as_str().unwrap().to_string();
    let (s, _) = c.call("GET", "/api/v2/audit?limit=5", &at, None).await;
    assert_eq!(s, StatusCode::OK);
    let (_, audit) = c.call("GET", "/api/v2/audit", &c.admin, None).await;
    assert!(
        audit["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "token.create")
    );
    c.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn jobs_are_scoped_per_user() {
    let c = setup().await;
    let (s, v) = c.call("GET", "/api/v2/jobs", &c.u1, None).await;
    assert_eq!(s, StatusCode::OK);
    let jobs = v["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["id"], "j1");
    assert_eq!(jobs[0]["site_domain"], "one.com");
    assert!(jobs[0]["resources"].is_object());
    let (s, _) = c.call("GET", "/api/v2/jobs/j2", &c.u1, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = c.call("GET", "/api/v2/jobs/j3", &c.u1, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = c.call("POST", "/api/v2/jobs/j2/cancel", &c.u1, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, all) = c
        .call("GET", "/api/v2/jobs?limit=99999", &c.admin, None)
        .await;
    assert_eq!(all["jobs"].as_array().unwrap().len(), 3);
    let (_, f) = c
        .call("GET", "/api/v2/jobs?status=done&kind=plan", &c.admin, None)
        .await;
    assert_eq!(f["jobs"].as_array().unwrap().len(), 1);

    let (_, logs) = c.call("GET", "/api/v2/jobs/j1/logs", &c.u1, None).await;
    assert_eq!(logs["logs"].as_array().unwrap().len(), 2);
    assert_eq!(logs["done"], false);
    let first = logs["logs"][0]["id"].as_i64().unwrap();
    let (_, more) = c
        .call(
            "GET",
            &format!("/api/v2/jobs/j1/logs?after_id={first}"),
            &c.u1,
            None,
        )
        .await;
    assert_eq!(more["logs"].as_array().unwrap().len(), 1);
    let (s, v) = c.call("POST", "/api/v2/jobs/j1/cancel", &c.u1, None).await;
    assert_eq!(
        (s, v["status"].as_str()),
        (StatusCode::OK, Some("cancelled"))
    );
    // non-admins are kept out of admin routes
    for p in [
        "/api/v2/system",
        "/api/v2/errors",
        "/api/v2/backups",
        "/api/v2/audit",
        "/api/v2/notifications",
    ] {
        let (s, _) = c.call("GET", p, &c.u1, None).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{p}");
    }
    let (s, _) = c.call("GET", "/api/v2/jobs", "garbage", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, m) = c.call("GET", "/api/v2/mcp", &c.u1, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(m["http_url"].as_str().unwrap().ends_with("/mcp"));
    assert!(!m.to_string().contains(SECRET));
    c.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn system_notifications_errors_backups() {
    let c = setup().await;
    let (s, v) = c.call("GET", "/api/v2/system", &c.admin, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["version"], sf_core::VERSION);
    assert!(v["capacity"]["logical_cpus"].as_u64().unwrap() >= 1);
    assert!(v["current"]["mem_total_mb"].as_u64().unwrap() > 0);
    assert_eq!(v["jobs"]["running"], 1);
    assert!(!v.to_string().contains(SECRET));
    let (_, m) = c
        .call("GET", "/api/v2/system/metrics?window=1h", &c.admin, None)
        .await;
    assert!(!m["samples"].as_array().unwrap().is_empty());
    let (_, m) = c
        .call("GET", "/api/v2/system/metrics?window=7d", &c.admin, None)
        .await;
    assert!(m["samples"].is_array());
    let (s, _) = c
        .call("GET", "/api/v2/system/metrics?window=1y", &c.admin, None)
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, st) = c.call("GET", "/api/v2/system/stats", &c.admin, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(st["mem_total_bytes"].as_u64().unwrap() > 0);

    // channels: secrets masked, PATCH without secret keeps it
    let (s, ch) = c
        .call(
            "POST",
            "/api/v2/notifications/channels",
            &c.admin,
            Some(json!({"name":"tg","kind":"telegram","config":{"bot_token":"BOT-SECRET","chat_id":"1"}})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{ch}");
    assert_eq!(ch["config"]["bot_token"], "***");
    let id = ch["id"].as_str().unwrap();
    let (s, ch2) = c
        .call(
            "PATCH",
            &format!("/api/v2/notifications/channels/{id}"),
            &c.admin,
            Some(json!({"config":{"chat_id":"2"},"enabled":false})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(ch2["config"]["chat_id"], "2");
    assert_eq!(ch2["config"]["bot_token"], "***");
    let (_, list) = c
        .call("GET", "/api/v2/notifications/channels", &c.admin, None)
        .await;
    assert!(!list.to_string().contains("BOT-SECRET"));
    let (s, _) = c
        .call(
            "POST",
            "/api/v2/notifications/channels",
            &c.admin,
            Some(json!({"name":"x","kind":"smtp"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = c
        .call(
            "POST",
            "/api/v2/notifications/channels",
            &c.admin,
            Some(json!("not an object")),
        )
        .await;
    assert!(s.is_client_error());
    // test endpoint on a local-only "log" channel (never call real Telegram from tests)
    let (_, lg) = c
        .call(
            "POST",
            "/api/v2/notifications/channels",
            &c.admin,
            Some(json!({"name":"log","kind":"log"})),
        )
        .await;
    let lid = lg["id"].as_str().unwrap();
    let (s, t) = c
        .call(
            "POST",
            &format!("/api/v2/notifications/channels/{lid}/test"),
            &c.admin,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(t["ok"], true, "{t}");
    let (_, n) = c
        .call("GET", "/api/v2/notifications?unread=1", &c.admin, None)
        .await;
    assert!(n["unread"].as_i64().unwrap() >= 1);
    let (_, r) = c
        .call(
            "POST",
            "/api/v2/notifications/read",
            &c.admin,
            Some(json!({"all":true})),
        )
        .await;
    assert!(r["updated"].as_u64().unwrap() >= 1);
    let (s, _) = c
        .call(
            "DELETE",
            &format!("/api/v2/notifications/channels/{id}"),
            &c.admin,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);

    // errors
    let fp = sf_ops::errors::ErrorTracker::new(c.st.db.clone())
        .record("test", "boom 42", json!({}))
        .await
        .unwrap();
    let (_, e) = c.call("GET", "/api/v2/errors", &c.admin, None).await;
    assert_eq!(e["errors"][0]["fingerprint"], fp);
    let (s, _) = c
        .call(
            "POST",
            &format!("/api/v2/errors/{fp}/resolve"),
            &c.admin,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = c
        .call("POST", "/api/v2/errors/nope/resolve", &c.admin, None)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // backups: create (background) -> poll -> download -> verify -> delete
    let (s, b) = c
        .call(
            "POST",
            "/api/v2/backups",
            &c.admin,
            Some(json!({"kind":"db","note":"api"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let bid = b["id"].as_str().unwrap().to_string();
    let mut status = String::new();
    for _ in 0..200 {
        let (_, row) = c
            .call("GET", &format!("/api/v2/backups/{bid}"), &c.admin, None)
            .await;
        status = row["status"].as_str().unwrap_or("").to_string();
        if status != "running" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(status, "ok");
    let (s, bytes, h) = c
        .raw(
            "GET",
            &format!("/api/v2/backups/{bid}/download"),
            &c.admin,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(bytes.starts_with(b"PK"));
    assert!(
        h.get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .contains(".zip")
    );
    let (_, ver) = c
        .call(
            "POST",
            &format!("/api/v2/backups/{bid}/verify"),
            &c.admin,
            None,
        )
        .await;
    assert_eq!(ver["ok"], true, "{ver}");
    let (_, lst) = c.call("GET", "/api/v2/backups", &c.admin, None).await;
    assert_eq!(lst["backups"].as_array().unwrap().len(), 1);
    let (s, _) = c
        .call(
            "POST",
            "/api/v2/backups",
            &c.admin,
            Some(json!({"kind":"weird"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = c
        .call("DELETE", &format!("/api/v2/backups/{bid}"), &c.admin, None)
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = c
        .call(
            "GET",
            &format!("/api/v2/backups/{bid}/download"),
            &c.admin,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    c.done();
}
