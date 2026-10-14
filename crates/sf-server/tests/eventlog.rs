//! §9 unified event log through the real router: request middleware (ui/api/mcp detection, reads
//! skipped unless `log_reads`, errors and denials recorded, route templates instead of raw ids),
//! auth events, and the `/api/v2/logs` API (filters, paging, tail, CSV, stats, user isolation,
//! hostile input).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use sf_core::Config;
use sf_core::crypto::{sign_jwt, user_claims};
use sf_ops::eventlog::{EventLog, EventLogOptions};
use sf_server::AppState;
use sf_store::Db;
use tower::ServiceExt;

const SECRET: &str = "test-jwt-secret-0123456789abcdef";
const PW: &str = "user-one-password";

struct H {
    app: Router,
    st: AppState,
    log: EventLog,
    dir: std::path::PathBuf,
    admin: String,
    u1: String,
    u2: String,
}

struct Resp {
    status: StatusCode,
    json: Value,
    text: String,
    headers: axum::http::HeaderMap,
}

impl H {
    async fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("sf-server-evlog-{}", sf_core::new_id()));
        let db = Db::open(&dir.join("t.db")).unwrap();
        let mut cfg = Config {
            home: dir.clone(),
            ..Default::default()
        };
        cfg.auth.jwt_secret = SECRET.into();
        let hash = sf_core::crypto::hash_password(PW).unwrap();
        db.write(move |t| {
            t.execute(
                "INSERT INTO users(id,email,password_hash,role) VALUES('u1','u1@x.io',?1,'user')",
                [&hash],
            )
            .map_err(sf_store::err)?;
            t.execute_batch(
                "INSERT INTO users(id,email,password_hash,role) VALUES('ua','a@x.io','x','admin');
                 INSERT INTO users(id,email,password_hash,role) VALUES('u2','u2@x.io','x','user');
                 INSERT INTO sites(id,domain,api_key,user_id) VALUES('s1','one.com','sf_1','u1');
                 INSERT INTO sites(id,domain,api_key,user_id) VALUES('s2','two.com','sf_2','u2');",
            )
            .map_err(sf_store::err)?;
            Ok(())
        })
        .await
        .unwrap();
        let st = AppState::new(cfg, db.clone(), [7u8; 32]);
        let log = EventLog::start(
            db,
            EventLogOptions {
                interval: Duration::from_millis(20),
                ..Default::default()
            },
        );
        st.services.insert(Arc::new(log.clone()));
        let app = sf_server::router(st.clone(), None);
        let tok = |role: &str, id: &str| sign_jwt(SECRET, &user_claims(role, id, 0)).unwrap();
        Self {
            app,
            st,
            log,
            dir,
            admin: tok("admin", "ua"),
            u1: tok("user", "u1"),
            u2: tok("user", "u2"),
        }
    }

    async fn req(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
        extra: &[(&str, &str)],
    ) -> Resp {
        let mut b = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        for (k, v) in extra {
            b = b.header(*k, *v);
        }
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
        let text = String::from_utf8_lossy(&bytes).to_string();
        Resp {
            status,
            json: serde_json::from_str(&text).unwrap_or(Value::Null),
            text,
            headers,
        }
    }

    async fn get(&self, path: &str, token: &str) -> Resp {
        self.req("GET", path, Some(token), None, &[]).await
    }

    /// Every event_log row, oldest first (after a flush).
    async fn rows(&self) -> Vec<Value> {
        self.log.flush().await;
        self.st
            .db
            .read(|c| {
                let mut s = c
                    .prepare("SELECT * FROM event_log ORDER BY id")
                    .map_err(sf_store::err)?;
                let v = s
                    .query_map([], |r| Ok(sf_store::row_to_json(r, &["detail"], &[])))
                    .map_err(sf_store::err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(sf_store::err)?;
                Ok(v)
            })
            .await
            .unwrap()
    }

    async fn clear(&self) {
        self.log.flush().await;
        self.st
            .db
            .write(|t| {
                t.execute("DELETE FROM event_log", [])
                    .map_err(sf_store::err)
            })
            .await
            .unwrap();
    }

    fn done(self) {
        self.st.db.close();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn find<'a>(rows: &'a [Value], action: &str) -> &'a Value {
    rows.iter()
        .find(|r| r["action"] == action)
        .unwrap_or_else(|| panic!("no {action} row in {rows:#?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn middleware_classifies_and_filters_requests() {
    let h = H::new().await;
    // successful reads are skipped (log_reads=false); /health never logged
    assert_eq!(h.get("/api/v1/sites", &h.u1).await.status, StatusCode::OK);
    assert_eq!(h.get("/health", &h.u1).await.status, StatusCode::OK);
    assert!(h.rows().await.is_empty(), "{:?}", h.rows().await);

    // UI session write: route template, target/site from the path, never the raw id in action
    let r = h
        .req(
            "PATCH",
            "/api/v1/sites/s2",
            Some(&h.u1),
            Some(json!({"name": "x"})),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // token create (ui) -> sfk token
    let r = h
        .req(
            "POST",
            "/api/v2/tokens",
            Some(&h.u1),
            Some(json!({"name": "hermes", "scopes": ["*"]})),
            &[],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let sfk = r.json["token"].as_str().unwrap().to_string();
    // API token GET: skipped; API token write: source api
    assert_eq!(h.get("/api/v2/tokens", &sfk).await.status, StatusCode::OK);
    h.req(
        "POST",
        "/api/v2/tokens",
        Some(&sfk),
        Some(json!({"name": "child", "scopes": ["read"]})),
        &[],
    )
    .await;
    // MCP GET: always logged, with the tool name
    let r = h
        .req(
            "GET",
            "/api/v2/jobs/j-none",
            Some(&sfk),
            None,
            &[
                ("X-SiloForge-Via", "mcp"),
                ("X-SiloForge-Tool", "siloforge_job_get"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // denials
    assert_eq!(
        h.req("GET", "/api/v2/system", None, None, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        h.get("/api/v2/system", &h.u1).await.status,
        StatusCode::FORBIDDEN
    );
    // unknown api route is an error row too
    assert_eq!(
        h.get("/api/v9/whatever/123", &h.u1).await.status,
        StatusCode::NOT_FOUND
    );

    let rows = h.rows().await;
    let p = find(&rows, "PATCH /api/v1/sites/{id}");
    assert_eq!(
        (&p["source"], &p["actor"], &p["status"], &p["http_status"]),
        (
            &json!("ui"),
            &json!("user:u1"),
            &json!("error"),
            &json!(404)
        )
    );
    assert_eq!(
        (&p["target"], &p["site_id"], &p["user_id"]),
        (&json!("s2"), &json!("s2"), &json!("u1"))
    );
    assert!(p["duration_ms"].is_i64());
    assert!(
        !rows
            .iter()
            .any(|r| r["action"].as_str().unwrap().contains("/s2"))
    );

    let posts: Vec<&Value> = rows
        .iter()
        .filter(|r| r["action"] == "POST /api/v2/tokens")
        .collect();
    assert_eq!(posts.len(), 2, "{rows:#?}");
    assert_eq!(
        (&posts[0]["source"], &posts[0]["actor"]),
        (&json!("ui"), &json!("user:u1"))
    );
    assert_eq!(
        (&posts[1]["source"], &posts[1]["actor"]),
        (&json!("api"), &json!("token:hermes"))
    );
    // auth rows for token create (no plaintext token anywhere)
    let a = find(&rows, "token create");
    assert_eq!(
        (&a["source"], &a["user_id"]),
        (&json!("auth"), &json!("u1"))
    );
    let all = serde_json::to_string(&rows).unwrap();
    assert!(!all.contains(&sfk), "plaintext token leaked into the log");

    let m = find(&rows, "GET /api/v2/jobs/{id}");
    assert_eq!(
        (
            &m["source"],
            &m["actor"],
            &m["target"],
            &m["detail"]["tool"]
        ),
        (
            &json!("mcp"),
            &json!("mcp:hermes"),
            &json!("j-none"),
            &json!("siloforge_job_get")
        )
    );
    let denied: Vec<&Value> = rows
        .iter()
        .filter(|r| r["action"] == "GET /api/v2/system")
        .collect();
    assert_eq!(denied.len(), 2);
    assert!(denied.iter().all(|r| r["status"] == "denied"));
    assert_eq!(denied[0]["actor"], "anonymous");
    assert_eq!(denied[1]["actor"], "user:u1");
    assert_eq!(find(&rows, "GET /api/{*rest}")["status"], "error");
    // successful GETs never logged without log_reads
    assert!(!rows.iter().any(|r| r["action"] == "GET /api/v1/sites"));
    assert!(!rows.iter().any(|r| r["action"] == "GET /api/v2/tokens"));

    // log_reads is live
    let mut cfg = (*h.st.live.get()).clone();
    cfg.monitor.log_reads = true;
    h.st.live.set(cfg);
    h.get("/api/v1/sites", &h.u1).await;
    let rows = h.rows().await;
    assert_eq!(find(&rows, "GET /api/v1/sites")["status"], "ok");
    h.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn login_events_and_no_event_log_is_harmless() {
    let h = H::new().await;
    let ok = h
        .req(
            "POST",
            "/api/v1/auth/login",
            None,
            Some(json!({"email": "u1@x.io", "password": PW})),
            &[("sec-fetch-mode", "cors")],
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
    let bad = h
        .req(
            "POST",
            "/api/v1/auth/login",
            None,
            Some(json!({"email": "u1@x.io", "password": "wrong-password"})),
            &[],
        )
        .await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);
    let rows = h.rows().await;
    let a = find(&rows, "login ok");
    assert_eq!(
        (&a["source"], &a["actor"], &a["user_id"]),
        (&json!("auth"), &json!("user:u1"), &json!("u1"))
    );
    let f = find(&rows, "login failed");
    assert_eq!(
        (&f["status"], &f["target"]),
        (&json!("denied"), &json!("u1@x.io"))
    );
    let all = serde_json::to_string(&rows).unwrap();
    assert!(!all.contains(PW) && !all.contains("wrong-password"));
    let reqs: Vec<&Value> = rows
        .iter()
        .filter(|r| r["action"] == "POST /api/v1/auth/login")
        .collect();
    assert_eq!(reqs.len(), 2);
    assert_eq!(
        (&reqs[0]["source"], &reqs[0]["status"]),
        (&json!("ui"), &json!("ok"))
    );
    assert_eq!(
        (&reqs[1]["source"], &reqs[1]["status"]),
        (&json!("api"), &json!("denied"))
    );
    h.done();

    // without a registered EventLog the middleware and the logs API still work
    let dir = std::env::temp_dir().join(format!("sf-server-evlog-off-{}", sf_core::new_id()));
    let db = Db::open(&dir.join("t.db")).unwrap();
    let mut cfg = Config {
        home: dir.clone(),
        ..Default::default()
    };
    cfg.auth.jwt_secret = SECRET.into();
    let st = AppState::new(cfg, db.clone(), [7u8; 32]);
    let app = sf_server::router(st, None);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/sites")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    db.close();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn logs_api_filters_pages_exports_and_isolates() {
    let h = H::new().await;
    h.clear().await;
    h.st.db
        .write(|t| {
            t.execute_batch(
                "INSERT INTO event_log(ts,source,actor,action,target,site_id,status,http_status,detail,user_id) VALUES
                  ('2026-01-01T00:00:00.000Z','ui','user:u1','PATCH /api/v1/sites/{id}','s1','s1','ok',200,'{\"a\":1}','u1'),
                  ('2026-01-02T00:00:00.000Z','api','token:t','POST /api/v1/sites/{id}/plan','s1','s1','error',500,NULL,'u1'),
                  ('2026-01-03T00:00:00.000Z','mcp','mcp:t','GET /api/v2/jobs/{id}','j1',NULL,'ok',200,'{\"tool\":\"x\"}','u1'),
                  ('2026-01-04T00:00:00.000Z','job','system','job plan done','j2','s2','ok',NULL,NULL,'u2'),
                  ('2026-01-05T00:00:00.000Z','auth','anonymous','login failed','100%_off','\\x',  'denied',NULL,NULL,NULL),
                  ('2026-01-06T00:00:00.000Z','system','system','=cmd|calc','x',NULL,'ok',NULL,NULL,NULL);",
            )
            .map_err(sf_store::err)
        })
        .await
        .unwrap();
    let ids = |r: &Resp| -> Vec<i64> {
        r.json["logs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["id"].as_i64().unwrap())
            .collect()
    };
    // admin: everything, newest first, detail parsed
    let r = h.get("/api/v2/logs", &h.admin).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(ids(&r), vec![6, 5, 4, 3, 2, 1]);
    assert_eq!(r.json["logs"][5]["detail"]["a"], 1);
    assert!(r.json["next_before_id"].is_null());
    // paging
    let r = h.get("/api/v2/logs?limit=2", &h.admin).await;
    assert_eq!(
        (ids(&r), &r.json["next_before_id"]),
        (vec![6, 5], &json!(5))
    );
    let r = h.get("/api/v2/logs?limit=2&before_id=5", &h.admin).await;
    assert_eq!(
        (ids(&r), &r.json["next_before_id"]),
        (vec![4, 3], &json!(3))
    );
    let r = h.get("/api/v2/logs?limit=2&before_id=3", &h.admin).await;
    assert_eq!(
        (ids(&r), &r.json["next_before_id"]),
        (vec![2, 1], &Value::Null)
    );
    // tail: ascending after an id
    let r = h.get("/api/v2/logs?after_id=3&limit=2", &h.admin).await;
    assert_eq!((ids(&r), &r.json["next_after_id"]), (vec![4, 5], &json!(5)));
    let r = h.get("/api/v2/logs?after_id=6", &h.admin).await;
    assert_eq!((ids(&r), &r.json["next_after_id"]), (vec![], &json!(6)));
    // filters
    let cases: &[(&str, Vec<i64>)] = &[
        ("source=ui,mcp", vec![3, 1]),
        ("status=error", vec![2]),
        ("status=denied&source=auth", vec![5]),
        ("actor=token:t", vec![2]),
        ("site_id=s1", vec![2, 1]),
        ("q=plan", vec![4, 2]),
        ("q=100%25_", vec![5]),
        ("q=%25", vec![5]),
        ("q=_", vec![5]),
        ("q=%5C", vec![5]),
        (
            "since=2026-01-03T00:00:00Z&until=2026-01-04T23:00:00Z",
            vec![4, 3],
        ),
        ("since=2026-01-05T01:00:00%2B01:00", vec![6, 5]),
        ("since=1h", vec![]),
        ("source=nope", vec![]),
    ];
    for (qs, want) in cases {
        let r = h.get(&format!("/api/v2/logs?{qs}"), &h.admin).await;
        assert_eq!(r.status, StatusCode::OK, "{qs}: {}", r.text);
        assert_eq!(&ids(&r), want, "{qs}");
    }
    // users see only their own rows, whatever they ask for
    let r = h.get("/api/v2/logs", &h.u1).await;
    assert_eq!(ids(&r), vec![3, 2, 1]);
    let r = h.get("/api/v2/logs?site_id=s2", &h.u1).await;
    assert!(ids(&r).is_empty());
    let r = h.get("/api/v2/logs", &h.u2).await;
    assert_eq!(ids(&r), vec![4]);
    // stats
    let r = h.get("/api/v2/logs/stats", &h.admin).await;
    assert_eq!(r.json["total"], 6, "{}", r.text);
    assert_eq!(r.json["by_source"]["ui"], 1);
    assert_eq!(r.json["by_status"]["ok"], 4);
    assert_eq!(r.json["by_status"]["denied"], 1);
    let r = h
        .get("/api/v2/logs/stats?since=2026-01-04T00:00:00Z", &h.admin)
        .await;
    assert_eq!(r.json["total"], 3);
    let r = h.get("/api/v2/logs/stats", &h.u1).await;
    assert_eq!(
        (&r.json["total"], &r.json["by_source"]["job"]),
        (&json!(3), &Value::Null)
    );
    // CSV export
    let r = h
        .get("/api/v2/logs/export?source=system,ui", &h.admin)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/csv")
    );
    assert!(
        r.headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .contains("attachment")
    );
    let lines: Vec<&str> = r.text.lines().collect();
    assert_eq!(lines.len(), 3, "{}", r.text);
    assert!(lines[0].starts_with("id,ts,source,actor,action"));
    assert!(
        lines[1].contains("'=cmd|calc"),
        "formula guard: {}",
        lines[1]
    );
    assert!(lines[2].contains("PATCH /api/v1/sites/{id}"));
    let r = h.get("/api/v2/logs/export", &h.u2).await;
    assert_eq!(r.text.lines().count(), 2);
    // anonymous: 401
    assert_eq!(
        h.req("GET", "/api/v2/logs", None, None, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
    h.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn logs_api_hostile_params_never_panic() {
    let h = H::new().await;
    let long = "x".repeat(10_000);
    let qs = [
        "limit=abc".to_string(),
        "limit=-5".into(),
        "limit=99999999999999999999999".into(),
        "before_id=1e9".into(),
        "after_id=%00".into(),
        "since=yesterday".into(),
        "since=99999999999999999999d".into(),
        "until=-5m".into(),
        format!("q={long}"),
        format!("source={}", "a,".repeat(500)),
        format!("actor={long}&site_id={long}"),
        "q=%27%3B%20DROP%20TABLE%20event_log%3B--".into(),
        "status=%E2%82%AC%F0%9F%98%80".into(),
        "source=&status=&q=&since=&until=&limit=&before_id=&after_id=".into(),
    ];
    for q in &qs {
        for path in ["/api/v2/logs", "/api/v2/logs/export", "/api/v2/logs/stats"] {
            let r = h.get(&format!("{path}?{q}"), &h.admin).await;
            assert!(
                r.status == StatusCode::OK || r.status == StatusCode::BAD_REQUEST,
                "{path}?{}: {} {}",
                q.chars().take(80).collect::<String>(),
                r.status,
                r.text.chars().take(200).collect::<String>()
            );
        }
    }
    let r = h.get("/api/v2/logs?limit=abc", &h.admin).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = h.get("/api/v2/logs?limit=-5", &h.admin).await;
    assert_eq!(r.status, StatusCode::OK);
    // the table survived
    let r = h.get("/api/v2/logs/stats", &h.admin).await;
    assert_eq!(r.status, StatusCode::OK);
    h.done();
}
