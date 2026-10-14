//! v2.4 SaaS readiness (docs/specs/v2.4-saas.md): workspace authz matrix, cross-workspace
//! isolation, plan quotas (402), registration modes, email verification, reset links, TOTP 2FA +
//! recovery codes, sessions, the migration back-fill (identical /api/v1/sites) and SMTP through a
//! local stub server. No real mail servers or providers are contacted.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sf_core::Config;
use sf_core::crypto::{sign_jwt, user_claims};
use sf_server::AppState;
use sf_store::Db;
use tower::ServiceExt;

const SECRET: &str = "test-jwt-secret-0123456789abcdef";
const PW: &str = "correct-horse-1";

struct T {
    app: Router,
    st: AppState,
    dir: PathBuf,
}

struct R {
    status: StatusCode,
    json: Value,
    text: String,
}

fn tok(role: &str, id: &str) -> String {
    sign_jwt(SECRET, &user_claims(role, id, 0)).unwrap()
}

impl T {
    async fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("sf-saas-{}", sf_core::new_id()));
        let db = Db::open(&dir.join("t.db")).unwrap();
        Self::with_db(dir, db)
    }

    fn with_db(dir: PathBuf, db: Db) -> Self {
        let mut cfg = Config {
            home: dir.clone(),
            ..Default::default()
        };
        cfg.auth.jwt_secret = SECRET.into();
        let st = AppState::new(cfg, db, [9u8; 32]);
        let app = sf_server::router(st.clone(), None);
        Self { app, st, dir }
    }

    async fn sql(&self, sql: &str) {
        let s = sql.to_string();
        self.st
            .db
            .write(move |t| t.execute_batch(&s).map_err(sf_store::err))
            .await
            .unwrap();
    }

    async fn one(&self, sql: &str) -> Value {
        let s = sql.to_string();
        self.st
            .db
            .read(move |c| {
                c.query_row(&s, [], |r| {
                    let v: rusqlite_value::V = r.get(0)?;
                    Ok(v.0)
                })
                .map_err(sf_store::err)
            })
            .await
            .unwrap()
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> R {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let body = body.map_or_else(Body::empty, |v| Body::from(v.to_string()));
        let res = self
            .app
            .clone()
            .oneshot(b.body(body).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        R {
            status,
            json: serde_json::from_str(&text).unwrap_or(Value::Null),
            text,
        }
    }

    async fn get(&self, path: &str, t: &str) -> R {
        self.call("GET", path, Some(t), &[], None).await
    }

    async fn post(&self, path: &str, t: Option<&str>, body: Value) -> R {
        self.call("POST", path, t, &[], Some(body)).await
    }

    async fn latest_link(&self, kind: &str) -> String {
        self.one(&format!(
            "SELECT link FROM mail_outbox WHERE kind='{kind}' ORDER BY id DESC LIMIT 1"
        ))
        .await
        .as_str()
        .unwrap_or_default()
        .to_string()
    }

    fn done(self) {
        self.st.db.close();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Tiny `FromSql` adapter so `one()` can return any column as JSON.
mod rusqlite_value {
    use sf_store::rusqlite::types::{FromSql, FromSqlResult, ValueRef};
    pub struct V(pub serde_json::Value);
    impl FromSql for V {
        fn column_result(v: ValueRef<'_>) -> FromSqlResult<Self> {
            Ok(V(match v {
                ValueRef::Null => serde_json::Value::Null,
                ValueRef::Integer(i) => serde_json::json!(i),
                ValueRef::Real(f) => serde_json::json!(f),
                ValueRef::Text(t) => serde_json::json!(String::from_utf8_lossy(t)),
                ValueRef::Blob(_) => serde_json::Value::Null,
            }))
        }
    }
}

fn token_of(link: &str) -> String {
    link.rsplit(['=', '/'])
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Workspace `w1` (Pro) with owner/admin/member/viewer, outsider `x` with its own workspace `w2`,
/// a platform admin `pa`. Sites: `s1` in w1, `s2` in w2.
async fn fixture() -> T {
    let t = T::new().await;
    t.sql(
        "INSERT INTO users(id,email,password_hash,role,email_verified_at) VALUES
           ('pa','pa@example.com','x','admin','2026-01-01T00:00:00Z'),
           ('own','own@example.com','x','user','2026-01-01T00:00:00Z'),
           ('adm','adm@example.com','x','user','2026-01-01T00:00:00Z'),
           ('mem','mem@example.com','x','user','2026-01-01T00:00:00Z'),
           ('vie','vie@example.com','x','user','2026-01-01T00:00:00Z'),
           ('x','x@example.com','x','user','2026-01-01T00:00:00Z');
         INSERT INTO workspaces(id,name,slug,plan_id,owner_user_id) VALUES
           ('w1','Acme','acme','pro','own'),('w2','Other','other','pro','x');
         INSERT INTO workspace_members(workspace_id,user_id,role) VALUES
           ('w1','own','owner'),('w1','adm','admin'),('w1','mem','member'),('w1','vie','viewer'),
           ('w2','x','owner');
         INSERT INTO sites(id,domain,api_key,user_id,workspace_id) VALUES
           ('11111111-1111-4111-8111-111111111111','acme.example','k1','own','w1'),
           ('22222222-2222-4222-8222-222222222222','other.example','k2','x','w2');",
    )
    .await;
    t
}

const S1: &str = "11111111-1111-4111-8111-111111111111";
const S2: &str = "22222222-2222-4222-8222-222222222222";

#[tokio::test(flavor = "multi_thread")]
async fn authz_matrix_roles_by_route() {
    let t = fixture().await;
    let (own, adm, mem, vie, x, pa) = (
        tok("user", "own"),
        tok("user", "adm"),
        tok("user", "mem"),
        tok("user", "vie"),
        tok("user", "x"),
        tok("admin", "pa"),
    );
    // read routes: every member; outsider 404
    for (who, want) in [
        (&own, 200),
        (&adm, 200),
        (&mem, 200),
        (&vie, 200),
        (&pa, 200),
        (&x, 404),
    ] {
        for path in [
            format!("/api/v1/sites/{S1}"),
            format!("/api/v1/dashboard/{S1}"),
            format!("/api/v1/edges/{S1}"),
            format!("/api/v2/sites/{S1}/link-runs"),
        ] {
            let r = t.get(&path, who).await;
            assert_eq!(r.status.as_u16(), want, "{path}: {}", r.text);
        }
        let r = t.get("/api/v2/workspaces/w1/members", who).await;
        assert_eq!(r.status.as_u16(), want, "members: {}", r.text);
    }
    // member-level writes: viewer 403, member+ ok, outsider 404
    for (who, want) in [
        (&vie, 403),
        (&x, 404),
        (&mem, 201),
        (&adm, 201),
        (&own, 201),
        (&pa, 201),
    ] {
        let phrase = format!("phrase-{}", sf_core::new_id());
        let r = t
            .post(
                &format!("/api/v1/rules/ignore/{S1}"),
                Some(who),
                json!({"phrase": phrase}),
            )
            .await;
        assert_eq!(r.status.as_u16(), want, "ignore: {}", r.text);
    }
    let r = t
        .post(&format!("/api/v1/sites/{S1}/crawl"), Some(&vie), json!({}))
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = t
        .post(&format!("/api/v1/sites/{S1}/crawl"), Some(&mem), json!({}))
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    // admin-level site settings: member 403, admin ok
    for (who, want) in [
        (&mem, 403),
        (&vie, 403),
        (&adm, 200),
        (&own, 200),
        (&pa, 200),
    ] {
        let r = t
            .call(
                "PATCH",
                &format!("/api/v1/sites/{S1}"),
                Some(who),
                &[],
                Some(json!({"settings": {"max_links_per_post": 7}})),
            )
            .await;
        assert_eq!(r.status.as_u16(), want, "patch site: {}", r.text);
    }
    // workspace admin routes
    let r = t
        .post(
            "/api/v2/workspaces/w1/invites",
            Some(&mem),
            json!({"email": "new@example.com"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = t
        .post(
            "/api/v2/workspaces/w1/invites",
            Some(&adm),
            json!({"email": "new@example.com"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.json["delivery"], "manual");
    assert!(r.json["link"].as_str().unwrap().contains("/invite/"));
    // only the owner can promote to admin
    let r = t
        .call(
            "PATCH",
            "/api/v2/workspaces/w1/members/mem",
            Some(&adm),
            &[],
            Some(json!({"role": "admin"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = t
        .call(
            "PATCH",
            "/api/v2/workspaces/w1/members/vie",
            Some(&adm),
            &[],
            Some(json!({"role": "member"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    // owner-only: delete
    let r = t
        .call(
            "DELETE",
            "/api/v2/workspaces/w1",
            Some(&adm),
            &[],
            Some(json!({"confirm": "acme"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // admin console: platform admins only
    for who in [&own, &adm, &x] {
        assert_eq!(
            t.get("/api/v2/admin/overview", who).await.status,
            StatusCode::FORBIDDEN
        );
    }
    let r = t.get("/api/v2/admin/overview", &pa).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert!(r.json["users"]["total"].as_i64().unwrap() >= 6);
    // suspended workspace: reads ok, writes 403
    t.sql("UPDATE workspaces SET status='suspended' WHERE id='w1'")
        .await;
    assert_eq!(
        t.get(&format!("/api/v1/sites/{S1}"), &mem).await.status,
        StatusCode::OK
    );
    let r = t
        .post(&format!("/api/v1/sites/{S1}/crawl"), Some(&mem), json!({}))
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.text.contains("suspended"));
    t.sql("UPDATE workspaces SET status='active' WHERE id='w1'")
        .await;
    // owner deletes (soft) -> sites vanish for members, still visible to platform admins
    let r = t
        .call(
            "DELETE",
            "/api/v2/workspaces/w1",
            Some(&own),
            &[],
            Some(json!({"confirm": "acme"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(
        t.get(&format!("/api/v1/sites/{S1}"), &mem).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        t.get(&format!("/api/v1/sites/{S1}"), &pa).await.status,
        StatusCode::OK
    );
    t.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn cross_workspace_isolation() {
    let t = fixture().await;
    let (x, mem, pa) = (tok("user", "x"), tok("user", "mem"), tok("admin", "pa"));
    let r = t.get("/api/v1/sites", &x).await;
    assert_eq!(r.json.as_array().unwrap().len(), 1);
    assert!(r.text.contains(S2) && !r.text.contains(S1) && !r.text.contains("acme"));
    let r = t.get("/api/v1/usage", &x).await;
    assert_eq!(r.json["sites"], 1);
    // foreign ids are 404, never 403
    for p in [
        format!("/api/v1/sites/{S1}"),
        format!("/api/v1/posts/{S1}"),
        format!("/api/v1/jobs/{S1}"),
        "/api/v2/workspaces/w1".to_string(),
        "/api/v2/workspaces/w1/usage".to_string(),
    ] {
        let r = t.get(&p, &x).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{p}");
        assert!(!r.text.contains("acme"));
    }
    // naming a foreign workspace in the header is 404
    let r = t
        .call(
            "GET",
            "/api/v1/sites",
            Some(&x),
            &[("x-siloforge-workspace", "w1")],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // workspace list shows only own memberships
    let r = t.get("/api/v2/workspaces", &x).await;
    let ids: Vec<&str> = r.json["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["w2"]);
    // bulk decide cannot reach foreign edges
    t.sql(&format!(
        "INSERT INTO posts(id,site_id,url,title) VALUES('p1','{S1}','https://acme.example/a','A'),('p2','{S1}','https://acme.example/b','B');
         INSERT INTO link_edges(id,site_id,source_post_id,target_post_id,anchor,status) VALUES
           ('33333333-3333-4333-8333-333333333333','{S1}','p1','p2','a b','suggested');"
    ))
    .await;
    let r = t
        .post(
            "/api/v1/bulk/decide",
            Some(&x),
            json!({"edge_ids": ["33333333-3333-4333-8333-333333333333"], "decision": "approved"}),
        )
        .await;
    assert_eq!(r.json["updated"], 0);
    let r = t
        .post(
            "/api/v1/bulk/decide",
            Some(&tok("user", "vie")),
            json!({"edge_ids": ["33333333-3333-4333-8333-333333333333"], "decision": "approved"}),
        )
        .await;
    assert_eq!(r.json["updated"], 0, "viewers cannot decide");
    let r = t
        .post(
            "/api/v1/bulk/decide",
            Some(&mem),
            json!({"edge_ids": ["33333333-3333-4333-8333-333333333333"], "decision": "approved"}),
        )
        .await;
    assert_eq!(r.json["updated"], 1);
    // platform admin: all sites without a header, one workspace with it, view-as is read-only
    assert_eq!(
        t.get("/api/v1/sites", &pa)
            .await
            .json
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let r = t
        .call(
            "GET",
            "/api/v1/sites",
            Some(&pa),
            &[("x-siloforge-workspace", "w2")],
            None,
        )
        .await;
    assert_eq!(r.json.as_array().unwrap().len(), 1);
    let r = t
        .call(
            "GET",
            "/api/v1/sites",
            Some(&pa),
            &[("x-siloforge-view-as", "w1")],
            None,
        )
        .await;
    assert_eq!(r.json.as_array().unwrap().len(), 1);
    assert!(r.text.contains(S1));
    let r = t
        .call(
            "POST",
            &format!("/api/v1/sites/{S1}/crawl"),
            Some(&pa),
            &[("x-siloforge-view-as", "w1")],
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.text.contains("read-only"));
    let r = t
        .call(
            "GET",
            "/api/v1/sites",
            Some(&x),
            &[("x-siloforge-view-as", "w1")],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // a member's API token bound to w2 cannot reach w1 even if the user joins w1
    t.sql("INSERT INTO workspace_members(workspace_id,user_id,role) VALUES('w1','x','member')")
        .await;
    let r = t
        .post(
            "/api/v2/tokens",
            Some(&x),
            json!({"name": "w2 token", "scopes": ["read"], "workspace_id": "w2"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let api = r.json["token"].as_str().unwrap().to_string();
    assert_eq!(r.json["workspace_id"], "w2");
    assert_eq!(
        t.get(&format!("/api/v1/sites/{S1}"), &api).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        t.get(&format!("/api/v1/sites/{S2}"), &api).await.status,
        StatusCode::OK
    );
    let r = t.get("/api/v1/sites", &api).await;
    assert!(!r.text.contains(S1));
    t.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_402_shape_and_points() {
    let t = fixture().await;
    t.sql("UPDATE workspaces SET plan_id='free' WHERE id='w2'")
        .await;
    let x = tok("user", "x");
    let ws = [("x-siloforge-workspace", "w2")];
    // sites: Free allows 1 and w2 already has one
    let r = t
        .call(
            "POST",
            "/api/v1/sites",
            Some(&x),
            &ws,
            Some(json!({"domain": "second.example"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::PAYMENT_REQUIRED, "{}", r.text);
    assert_eq!(r.json["error"], "plan limit");
    assert_eq!(r.json["key"], "sites");
    assert_eq!(r.json["limit"], 1);
    assert_eq!(r.json["used"], 1);
    assert_eq!(r.json["plan"], "Free");
    assert_eq!(r.json["statusCode"], 402);
    // api tokens: 1
    let r = t
        .post(
            "/api/v2/tokens",
            Some(&x),
            json!({"name": "a", "workspace_id": "w2"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let r = t
        .post(
            "/api/v2/tokens",
            Some(&x),
            json!({"name": "b", "workspace_id": "w2"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(r.json["key"], "api_tokens");
    // members: 1 (the owner)
    let r = t
        .post(
            "/api/v2/workspaces/w2/invites",
            Some(&x),
            json!({"email": "friend@example.com"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(r.json["key"], "members");
    // schedules: 1
    let r = t
        .post(
            &format!("/api/v1/schedules/{S2}"),
            Some(&x),
            json!({"job_kind": "audit", "cron": "0 3 * * *"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let r = t
        .post(
            &format!("/api/v1/schedules/{S2}"),
            Some(&x),
            json!({"job_kind": "audit", "cron": "0 4 * * *"}),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "updating the same schedule is free: {}",
        r.text
    );
    let r = t
        .post(
            &format!("/api/v1/schedules/{S2}"),
            Some(&x),
            json!({"job_kind": "crawl", "cron": "0 5 * * *"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::PAYMENT_REQUIRED);
    // link runs: 5 per month
    for i in 0..5 {
        t.sql(&format!(
            "INSERT INTO link_runs(id,site_id,name) VALUES('lr{i}','{S2}','r')"
        ))
        .await;
    }
    let r = t
        .post(
            &format!("/api/v2/sites/{S2}/link-runs"),
            Some(&x),
            json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::PAYMENT_REQUIRED, "{}", r.text);
    assert_eq!(r.json["key"], "link_runs_per_month");
    // auto-linker is not in Free
    let r = t
        .call(
            "PUT",
            &format!("/api/v2/sites/{S2}/auto-linker"),
            Some(&x),
            &[],
            Some(json!({"enabled": true})),
        )
        .await;
    assert_eq!(r.status, StatusCode::PAYMENT_REQUIRED, "{}", r.text);
    assert_eq!(r.json["key"], "auto_linker");
    assert_eq!(r.json["limit"], false);
    // posts per site: 500 → crawl refused once the site holds the cap
    t.sql(&format!(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 500)
         INSERT INTO posts(id,site_id,url,title) SELECT 'p' || i, '{S2}', 'https://other.example/' || i, 't' FROM n;"
    ))
    .await;
    let r = t
        .post(&format!("/api/v1/sites/{S2}/crawl"), Some(&x), json!({}))
        .await;
    assert_eq!(r.status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(r.json["key"], "posts_per_site");
    assert_eq!(r.json["used"], 500);
    // AI tokens: the sf-ai quota sees the workspace's month usage
    t.sql(&format!(
        "INSERT INTO model_call_log(provider,role,ok,tokens_in,tokens_out,site_id) VALUES('p','chat',1,90000,20000,'{S2}')"
    ))
    .await;
    let q =
        t.st.db
            .read(|c| Ok(sf_ai::accounting::workspace_quota(c, S2)))
            .await
            .unwrap();
    assert_eq!(q, Some(("w2".to_string(), 110_000, 100_000)));
    let r = t.get("/api/v2/workspaces/w2/usage", &x).await;
    assert_eq!(r.json["usage"]["ai_tokens_month"], 110000);
    assert_eq!(r.json["usage"]["posts_max_site"], 500);
    assert_eq!(r.json["limits"]["sites"], 1);
    // platform admin moves w2 to Pro: limits lift
    let pa = tok("admin", "pa");
    let r = t
        .call(
            "PATCH",
            "/api/v2/admin/workspaces/w2",
            Some(&pa),
            &[],
            Some(json!({"plan_id": "pro"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let r = t
        .post(&format!("/api/v1/sites/{S2}/crawl"), Some(&x), json!({}))
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    // unlimited Default workspace never answers 402
    t.done();
}

async fn set_platform(t: &T, body: Value) {
    let pa = tok("admin", "pa");
    let r = t
        .call("PUT", "/api/v2/admin/platform", Some(&pa), &[], Some(body))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
}

#[tokio::test(flavor = "multi_thread")]
async fn registration_modes_and_verification() {
    let t = fixture().await;
    let reg = |email: &str| json!({"email": email, "password": PW, "name": "N"});
    // closed by default (users exist)
    let r = t
        .post("/api/v2/auth/register", None, reg("a@example.com"))
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.text.contains("closed"));
    let r = t
        .post("/api/v1/auth/register", None, reg("a@example.com"))
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let cfg = t.call("GET", "/api/v2/auth/config", None, &[], None).await;
    assert_eq!(cfg.json["registration_mode"], "closed");
    assert_eq!(cfg.json["first_run"], false);
    // invite_only: needs a valid invite for the same address
    set_platform(&t, json!({"registration": {"mode": "invite_only"}})).await;
    let r = t
        .post("/api/v2/auth/register", None, reg("a@example.com"))
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.text.contains("invitation"));
    let inv = t
        .post(
            "/api/v2/workspaces/w1/invites",
            Some(&tok("user", "own")),
            json!({"email": "inv@example.com", "role": "viewer"}),
        )
        .await;
    let itok = token_of(inv.json["link"].as_str().unwrap());
    let info = t
        .call("GET", &format!("/api/v2/invites/{itok}"), None, &[], None)
        .await;
    assert_eq!(info.json["valid"], true);
    assert_eq!(info.json["workspace_name"], "Acme");
    let mut b = reg("other@example.com");
    b["invite_token"] = json!(itok);
    assert_eq!(
        t.post("/api/v2/auth/register", None, b).await.status,
        StatusCode::FORBIDDEN
    );
    let mut b = reg("inv@example.com");
    b["invite_token"] = json!(itok);
    let r = t.post("/api/v2/auth/register", None, b).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.json["workspace"]["id"], "w1");
    let uid = r.json["user"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        t.one(&format!(
            "SELECT role FROM workspace_members WHERE workspace_id='w1' AND user_id='{uid}'"
        ))
        .await,
        "viewer"
    );
    let info = t
        .call("GET", &format!("/api/v2/invites/{itok}"), None, &[], None)
        .await;
    assert_eq!(info.json["valid"], false, "invites are single use");
    // open: own workspace on the Free plan; honeypot and denied domains refused
    set_platform(
        &t,
        json!({"registration": {"mode": "open", "denied_domains": ["spam.test"], "require_email_verification": true}}),
    )
    .await;
    let mut hp = reg("bot@example.com");
    hp["website"] = json!("http://spam");
    assert_eq!(
        t.post("/api/v2/auth/register", None, hp).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        t.post("/api/v2/auth/register", None, reg("z@spam.test"))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let r = t
        .post("/api/v2/auth/register", None, reg("open@example.com"))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.json["email_verification_required"], true);
    assert_eq!(r.json["verification_delivery"], "manual");
    let wid = r.json["workspace"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        t.one(&format!("SELECT plan_id FROM workspaces WHERE id='{wid}'"))
            .await,
        "free"
    );
    let token = r.json["token"].as_str().unwrap().to_string();
    // verification gate: site create refused until the link is used
    let r = t
        .post(
            "/api/v1/sites",
            Some(&token),
            json!({"domain": "mine.example"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.json["error"], "email not verified");
    let me = t.get("/api/v2/auth/me", &token).await;
    assert_eq!(me.json["email_verified"], false);
    assert_eq!(me.json["workspaces"][0]["plan"]["id"], "free");
    let vt = token_of(&t.latest_link("verify_email").await);
    let r = t
        .post("/api/v2/auth/verify-email", None, json!({"token": vt}))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let r = t
        .post("/api/v2/auth/verify-email", None, json!({"token": vt}))
        .await;
    assert_eq!(
        r.status,
        StatusCode::BAD_REQUEST,
        "verify links are single use"
    );
    let r = t
        .post(
            "/api/v1/sites",
            Some(&token),
            json!({"domain": "mine.example"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    // the new site lives in the new workspace and the Free plan caps it at one
    let r = t
        .post(
            "/api/v1/sites",
            Some(&token),
            json!({"domain": "mine2.example"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::PAYMENT_REQUIRED);
    // per-IP sign-up limit (default 5/h; 1 open sign-up done above in this window)
    let mut last = StatusCode::OK;
    for i in 0..6 {
        last = t
            .post(
                "/api/v2/auth/register",
                None,
                reg(&format!("n{i}@example.com")),
            )
            .await
            .status;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    t.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn first_run_bootstrap_makes_default_owner() {
    let t = T::new().await;
    let cfg = t.call("GET", "/api/v2/auth/config", None, &[], None).await;
    assert_eq!(cfg.json["first_run"], true);
    let r = t
        .post(
            "/api/v2/auth/register",
            None,
            json!({"email": "boss@example.com", "password": PW}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.json["user"]["role"], "admin");
    assert_eq!(r.json["workspace"]["id"], "default");
    let uid = r.json["user"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        t.one("SELECT owner_user_id FROM workspaces WHERE id='default'")
            .await,
        uid.as_str()
    );
    let token = r.json["token"].as_str().unwrap();
    let r = t
        .post(
            "/api/v1/sites",
            Some(token),
            json!({"domain": "first.example"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(t.one("SELECT workspace_id FROM sites").await, "default");
    t.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn reset_link_single_use_and_expiry() {
    let t = fixture().await;
    let hash = sf_core::crypto::hash_password(PW).unwrap();
    t.sql(&format!(
        "UPDATE users SET password_hash='{hash}' WHERE id='mem'"
    ))
    .await;
    let login = t
        .post(
            "/api/v2/auth/login",
            None,
            json!({"email": "mem@example.com", "password": PW}),
        )
        .await;
    assert_eq!(login.status, StatusCode::OK, "{}", login.text);
    let old = login.json["token"].as_str().unwrap().to_string();
    // unknown email: same answer, nothing queued
    let r = t
        .post(
            "/api/v2/auth/forgot",
            None,
            json!({"email": "nobody@example.com"}),
        )
        .await;
    assert_eq!(r.json, json!({"ok": true}));
    let r = t
        .post(
            "/api/v2/auth/forgot",
            None,
            json!({"email": "MEM@example.com"}),
        )
        .await;
    assert_eq!(r.json, json!({"ok": true}));
    let link = t.latest_link("reset_password").await;
    assert!(link.contains("/reset-password?token="), "{link}");
    let rt = token_of(&link);
    let r = t
        .post(
            "/api/v2/auth/reset",
            None,
            json!({"token": rt, "password": "short"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = t
        .post(
            "/api/v2/auth/reset",
            None,
            json!({"token": rt, "password": "brand-new-pass-1"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let r = t
        .post(
            "/api/v2/auth/reset",
            None,
            json!({"token": rt, "password": "brand-new-pass-2"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "single use");
    // old session is dead, new password works
    assert_eq!(
        t.get("/api/v2/auth/me", &old).await.status,
        StatusCode::UNAUTHORIZED
    );
    let r = t
        .post(
            "/api/v2/auth/login",
            None,
            json!({"email": "mem@example.com", "password": "brand-new-pass-1"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    // expired link
    t.post(
        "/api/v2/auth/forgot",
        None,
        json!({"email": "mem@example.com"}),
    )
    .await;
    let rt = token_of(&t.latest_link("reset_password").await);
    t.sql("UPDATE auth_tokens SET expires_at='2000-01-01T00:00:00.000Z' WHERE used_at IS NULL")
        .await;
    let r = t
        .post(
            "/api/v2/auth/reset",
            None,
            json!({"token": rt, "password": "brand-new-pass-3"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = t
        .post(
            "/api/v2/auth/reset",
            None,
            json!({"token": "nope", "password": "brand-new-pass-3"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    t.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn totp_two_factor_and_recovery_codes() {
    let t = fixture().await;
    let hash = sf_core::crypto::hash_password(PW).unwrap();
    t.sql(&format!(
        "UPDATE users SET password_hash='{hash}' WHERE id='own'"
    ))
    .await;
    let login =
        |remember: bool| json!({"email": "own@example.com", "password": PW, "remember": remember});
    let r = t.post("/api/v2/auth/login", None, login(true)).await;
    let sess = r.json["token"].as_str().unwrap().to_string();
    let r = t
        .post(
            "/api/v2/auth/2fa/setup",
            Some(&sess),
            json!({"password": "wrong-password"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = t
        .post(
            "/api/v2/auth/2fa/setup",
            Some(&sess),
            json!({"password": PW}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let secret = r.json["secret"].as_str().unwrap().to_string();
    assert!(
        r.json["otpauth_uri"]
            .as_str()
            .unwrap()
            .starts_with("otpauth://totp/SiloForge:own%40example.com")
            || r.json["otpauth_uri"]
                .as_str()
                .unwrap()
                .starts_with("otpauth://totp/SiloForge:own@example.com")
    );
    let r = t
        .post(
            "/api/v2/auth/2fa/enable",
            Some(&sess),
            json!({"code": "000000"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    // use the previous step so the login below can use the current one (replay guard)
    let now = sf_core::now_unix();
    let prev = sf_core::totp::code_at(&secret, now - 30).unwrap();
    let r = t
        .post(
            "/api/v2/auth/2fa/enable",
            Some(&sess),
            json!({"code": prev}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let codes: Vec<String> = r.json["recovery_codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    assert_eq!(codes.len(), 10);
    // password alone no longer signs in
    let r = t.post("/api/v2/auth/login", None, login(false)).await;
    assert_eq!(r.json["two_factor_required"], true);
    assert!(r.json.get("token").is_none());
    let challenge = r.json["challenge"].as_str().unwrap().to_string();
    // the challenge is not a credential
    assert_eq!(
        t.get("/api/v2/auth/me", &challenge).await.status,
        StatusCode::UNAUTHORIZED
    );
    let v1 = t
        .post(
            "/api/v1/auth/login",
            None,
            json!({"email": "own@example.com", "password": PW}),
        )
        .await;
    assert_eq!(v1.status, StatusCode::UNAUTHORIZED);
    assert!(v1.text.contains("two-factor"));
    let r = t
        .post(
            "/api/v2/auth/2fa/login",
            None,
            json!({"challenge": challenge, "code": "123456"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let cur = sf_core::totp::code_at(&secret, sf_core::now_unix()).unwrap();
    let r = t
        .post(
            "/api/v2/auth/2fa/login",
            None,
            json!({"challenge": challenge, "code": cur}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert!(r.json["token"].is_string());
    // the same code cannot be replayed
    let r2 = t.post("/api/v2/auth/login", None, login(true)).await;
    let ch2 = r2.json["challenge"].as_str().unwrap().to_string();
    let r = t
        .post(
            "/api/v2/auth/2fa/login",
            None,
            json!({"challenge": ch2, "code": cur}),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "replay refused");
    // recovery code: works once (any case / spacing)
    let rc = codes[0].to_uppercase().replace('-', " ");
    let r = t
        .post(
            "/api/v2/auth/2fa/login",
            None,
            json!({"challenge": ch2, "code": rc}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let me = t
        .get("/api/v2/auth/me", r.json["token"].as_str().unwrap())
        .await;
    assert_eq!(me.json["two_factor_enabled"], true);
    assert_eq!(me.json["recovery_codes_left"], 9);
    let r3 = t.post("/api/v2/auth/login", None, login(true)).await;
    let ch3 = r3.json["challenge"].as_str().unwrap().to_string();
    let r = t
        .post(
            "/api/v2/auth/2fa/login",
            None,
            json!({"challenge": ch3, "code": codes[0]}),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::UNAUTHORIZED,
        "recovery codes are single use"
    );
    // admin reset removes 2FA
    let r = t
        .post(
            "/api/v2/admin/users/own/reset-2fa",
            Some(&tok("admin", "pa")),
            json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let r = t.post("/api/v2/auth/login", None, login(true)).await;
    assert!(r.json["token"].is_string());
    t.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_list_revoke_logout_and_remember() {
    let t = fixture().await;
    let hash = sf_core::crypto::hash_password(PW).unwrap();
    t.sql(&format!(
        "UPDATE users SET password_hash='{hash}' WHERE id='adm'"
    ))
    .await;
    let a = t
        .post(
            "/api/v2/auth/login",
            None,
            json!({"email": "adm@example.com", "password": PW, "remember": false}),
        )
        .await;
    let b = t
        .post(
            "/api/v1/auth/login",
            None,
            json!({"email": "adm@example.com", "password": PW}),
        )
        .await;
    assert_eq!(b.status, StatusCode::OK);
    assert_eq!(
        b.json.as_object().unwrap().len(),
        2,
        "v1 login keeps {{token,user}}"
    );
    let (ta, tb) = (
        a.json["token"].as_str().unwrap().to_string(),
        b.json["token"].as_str().unwrap().to_string(),
    );
    // remember=false = 1 day
    let exp = a.json["expires_at"].as_str().unwrap();
    let dt = chrono::DateTime::parse_from_rfc3339(exp)
        .unwrap()
        .timestamp()
        - sf_core::now_unix();
    assert!((86_000..=86_500).contains(&dt), "{dt}");
    let l = t.get("/api/v2/auth/sessions", &ta).await;
    let rows = l.json["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.iter().filter(|r| r["current"] == true).count(), 1);
    let sid_b = rows.iter().find(|r| r["current"] == false).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = t
        .call(
            "DELETE",
            &format!("/api/v2/auth/sessions/{sid_b}"),
            Some(&ta),
            &[],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        t.get("/api/v2/auth/me", &tb).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(t.get("/api/v2/auth/me", &ta).await.status, StatusCode::OK);
    // someone else's session id is 404
    let r = t
        .call(
            "DELETE",
            &format!("/api/v2/auth/sessions/{sid_b}"),
            Some(&tok("user", "mem")),
            &[],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = t.post("/api/v2/auth/logout", Some(&ta), json!({})).await;
    assert_eq!(r.json["session_revoked"], true);
    assert_eq!(
        t.get("/api/v2/auth/me", &ta).await.status,
        StatusCode::UNAUTHORIZED
    );
    // pre-2.4 tokens without sid keep working
    assert_eq!(
        t.get("/api/v2/auth/me", &tok("user", "adm")).await.status,
        StatusCode::OK
    );
    // disabled accounts: login 403, tokens 401
    let r = t
        .post(
            "/api/v2/admin/users/adm/disable",
            Some(&tok("admin", "pa")),
            json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let r = t
        .post(
            "/api/v2/auth/login",
            None,
            json!({"email": "adm@example.com", "password": PW}),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(
        t.get("/api/v2/auth/me", &tok("user", "adm")).await.status,
        StatusCode::UNAUTHORIZED
    );
    t.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_console_users_workspaces_plans() {
    let t = fixture().await;
    let pa = tok("admin", "pa");
    let r = t
        .get("/api/v2/admin/users?q=example.com&limit=3", &pa)
        .await;
    assert_eq!(r.json["total"], 6);
    assert_eq!(r.json["users"].as_array().unwrap().len(), 3);
    let r = t
        .post(
            "/api/v2/admin/users",
            Some(&pa),
            json!({"email": "Made@Example.com", "name": "Made"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.json["delivery"], "manual");
    assert!(r.json["link"].as_str().unwrap().contains("reset-password"));
    let made = r.json["user"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        r.json["user"]["workspaces"].as_array().unwrap().len(),
        1,
        "personal workspace"
    );
    // delete with transfer: owned workspace + sites move
    let r = t
        .call(
            "DELETE",
            "/api/v2/admin/users/x?transfer_to=own",
            Some(&pa),
            &[],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(
        t.one("SELECT owner_user_id FROM workspaces WHERE id='w2'")
            .await,
        "own"
    );
    assert_eq!(
        t.one(&format!("SELECT user_id FROM sites WHERE id='{S2}'"))
            .await,
        "own"
    );
    let r = t
        .call("DELETE", "/api/v2/admin/users/pa", Some(&pa), &[], None)
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    // plans CRUD
    let r = t
        .post(
            "/api/v2/admin/plans",
            Some(&pa),
            json!({"id": "team", "name": "Team", "limits": {"sites": 3, "mcp": true}}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let r = t
        .post(
            "/api/v2/admin/plans",
            Some(&pa),
            json!({"name": "Bad", "limits": {"sites": -1}}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = t
        .call(
            "PATCH",
            "/api/v2/admin/workspaces/w2",
            Some(&pa),
            &[],
            Some(json!({"plan_id": "team"})),
        )
        .await;
    assert_eq!(r.json["plan"]["id"], "team");
    let r = t
        .call("DELETE", "/api/v2/admin/plans/team", Some(&pa), &[], None)
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = t.get("/api/v2/plans", &tok("user", "own")).await;
    assert!(!r.text.contains("Unlimited"), "internal plan hidden");
    // workspaces: list, suspend, delete + restore
    let r = t.get("/api/v2/admin/workspaces?q=acme", &pa).await;
    assert_eq!(r.json["total"], 1);
    assert_eq!(r.json["workspaces"][0]["owner"]["id"], "own");
    let r = t
        .post("/api/v2/admin/workspaces/w1/suspend", Some(&pa), json!({}))
        .await;
    assert_eq!(r.json["status"], "suspended");
    let r = t
        .call(
            "DELETE",
            "/api/v2/admin/workspaces/default",
            Some(&pa),
            &[],
            None,
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::BAD_REQUEST,
        "Default cannot be deleted"
    );
    let r = t
        .call(
            "DELETE",
            "/api/v2/admin/workspaces/w1",
            Some(&pa),
            &[],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = t.get("/api/v2/admin/workspaces?status=deleted", &pa).await;
    assert_eq!(r.json["total"], 1);
    let r = t
        .post("/api/v2/admin/workspaces/w1/restore", Some(&pa), json!({}))
        .await;
    assert_eq!(r.json["deleted_at"], Value::Null);
    // platform settings: secrets masked, validation
    let r = t
        .call("PUT", "/api/v2/admin/platform", Some(&pa), &[], Some(json!({"smtp": {"host": "mail.example.com", "password": "s3cret-pw", "from_email": "no-reply@example.com"}})))
        .await;
    assert_eq!(r.json["smtp"]["password"], "***");
    assert!(!r.text.contains("s3cret-pw"));
    let r = t.get("/api/v2/admin/platform", &pa).await;
    assert!(!r.text.contains("s3cret-pw"));
    let r = t
        .call(
            "PUT",
            "/api/v2/admin/platform",
            Some(&pa),
            &[],
            Some(json!({"branding": {"primary_color": "red"}})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = t
        .call(
            "PUT",
            "/api/v2/admin/platform",
            Some(&pa),
            &[],
            Some(json!({"registration": {"mode": "anyone"}})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = t.call("GET", "/api/v2/auth/config", None, &[], None).await;
    assert_eq!(r.json["email_enabled"], true);
    assert!(!r.text.contains("mail.example.com"));
    let _ = made;
    // billing stub
    let r = t.post("/api/v2/billing/webhook", None, json!({})).await;
    assert_eq!(r.status, StatusCode::NOT_IMPLEMENTED);
    let r = t
        .get("/api/v2/billing?workspace_id=w1", &tok("user", "own"))
        .await;
    assert_eq!(r.json["provider"], "manual");
    // hostile admin input never panics
    for body in [
        json!(null),
        json!([]),
        json!({"registration": 5}),
        json!({"smtp": {"port": 99999}}),
    ] {
        let r = t
            .call("PUT", "/api/v2/admin/platform", Some(&pa), &[], Some(body))
            .await;
        assert!(r.status.is_client_error(), "{}", r.text);
    }
    t.done();
}

/// Builds the same rows in (A) a current DB without workspaces (legacy rows) and (B) a v8 DB
/// upgraded by migration 0009: a single-admin install sees identical `/api/v1/sites` output.
#[tokio::test(flavor = "multi_thread")]
async fn migrated_single_admin_sees_identical_sites() {
    const ROWS: &str = "INSERT INTO users(id,email,password_hash,name,role,created_at) VALUES
           ('adm1','admin@example.com','x','Admin','admin','2026-01-01T00:00:00.000Z');
         INSERT INTO sites(id,domain,api_key,user_id,settings,created_at) VALUES
           ('11111111-1111-4111-8111-111111111111','one.example','k1','adm1','{\"max_links_per_post\":5}','2026-01-02T00:00:00.000Z'),
           ('22222222-2222-4222-8222-222222222222','two.example','k2',NULL,'{}','2026-01-03T00:00:00.000Z');
         INSERT INTO posts(id,site_id,url,title) VALUES('p1','11111111-1111-4111-8111-111111111111','https://one.example/a','A'),
           ('p2','11111111-1111-4111-8111-111111111111','https://one.example/b','B');
         INSERT INTO link_edges(id,site_id,source_post_id,target_post_id,anchor,status) VALUES
           ('e1','11111111-1111-4111-8111-111111111111','p1','p2','anchor','suggested');";
    // A: current schema, rows without a workspace (pre-2.4 semantics)
    let a = T::new().await;
    a.sql(ROWS).await;
    // B: schema v8, same rows, then the 2.4 migration back-fills
    let dir = std::env::temp_dir().join(format!("sf-saas-v8-{}", sf_core::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    {
        let mut c = sf_store::rusqlite::Connection::open(dir.join("t.db")).unwrap();
        sf_store::migrate::run_until(&mut c, 8).unwrap();
        c.execute_batch(ROWS).unwrap();
    }
    let b = T::with_db(dir.clone(), Db::open(&dir.join("t.db")).unwrap());
    assert_eq!(
        b.one("SELECT count(*) FROM sites WHERE workspace_id='default'")
            .await,
        2
    );
    assert_eq!(
        b.one("SELECT owner_user_id FROM workspaces WHERE id='default'")
            .await,
        "adm1"
    );
    let token = tok("admin", "adm1");
    let ra = a.get("/api/v1/sites", &token).await;
    let rb = b.get("/api/v1/sites", &token).await;
    assert_eq!(ra.status, StatusCode::OK);
    assert_eq!(
        ra.json, rb.json,
        "identical v1 site list after the migration"
    );
    assert_eq!(ra.json.as_array().unwrap().len(), 2);
    // the UI's workspace header (Default) shows the same list
    let rc = b
        .call(
            "GET",
            "/api/v1/sites",
            Some(&token),
            &[("x-siloforge-workspace", "default")],
            None,
        )
        .await;
    assert_eq!(rc.json, rb.json);
    for t in [&a, &b] {
        assert_eq!(t.get("/api/v1/usage", &token).await.json["sites"], 2);
    }
    // and the boot pass gives legacy rows of A the same workspace B got
    sf_server::tenancy::ensure_default_workspace(&a.st.db)
        .await
        .unwrap();
    assert_eq!(
        a.one("SELECT count(*) FROM sites WHERE workspace_id='default'")
            .await,
        2
    );
    assert_eq!(a.get("/api/v1/sites", &token).await.json, rb.json);
    a.done();
    b.done();
}

/// Minimal SMTP server (no TLS) that records DATA bodies.
async fn smtp_stub() -> (u16, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let got = Arc::new(Mutex::new(Vec::new()));
    let g2 = got.clone();
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = l.accept().await else { return };
            let g3 = g2.clone();
            tokio::spawn(async move {
                let (r, mut w) = s.into_split();
                let mut r = BufReader::new(r);
                let _ = w.write_all(b"220 stub ESMTP\r\n").await;
                let mut line = String::new();
                let mut data = false;
                let mut body = String::new();
                loop {
                    line.clear();
                    if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                        break;
                    }
                    if data {
                        if line == ".\r\n" {
                            data = false;
                            g3.lock().unwrap().push(std::mem::take(&mut body));
                            let _ = w.write_all(b"250 queued\r\n").await;
                        } else {
                            body.push_str(&line);
                        }
                        continue;
                    }
                    let up = line.to_ascii_uppercase();
                    let reply: &[u8] = if up.starts_with("EHLO") {
                        b"250-stub\r\n250 AUTH PLAIN LOGIN\r\n"
                    } else if up.starts_with("AUTH") {
                        b"235 ok\r\n"
                    } else if up.starts_with("DATA") {
                        data = true;
                        b"354 go\r\n"
                    } else if up.starts_with("QUIT") {
                        let _ = w.write_all(b"221 bye\r\n").await;
                        break;
                    } else {
                        b"250 ok\r\n"
                    };
                    let _ = w.write_all(reply).await;
                }
            });
        }
    });
    (port, got)
}

#[tokio::test(flavor = "multi_thread")]
async fn smtp_delivery_through_stub_and_test_email() {
    let t = fixture().await;
    let (port, got) = smtp_stub().await;
    set_platform(
        &t,
        json!({"smtp": {"host": "127.0.0.1", "port": port, "tls": "none", "username": "u", "password": "p",
                        "from_email": "no-reply@example.com"},
               "deployment": {"public_url": "https://links.example.com/"}}),
    )
    .await;
    let pa = tok("admin", "pa");
    let r = t
        .post(
            "/api/v2/admin/email/test",
            Some(&pa),
            json!({"to": "ops@example.com"}),
        )
        .await;
    assert_eq!(r.json["ok"], true, "{}", r.text);
    let r = t
        .post(
            "/api/v2/auth/forgot",
            None,
            json!({"email": "mem@example.com"}),
        )
        .await;
    assert_eq!(r.json["ok"], true);
    let bodies = got.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert!(bodies[0].contains("test email"));
    assert!(
        bodies[1].contains("https://links.example.com/reset-password?token="),
        "{}",
        bodies[1]
    );
    // sent mail keeps no link in the outbox
    let r = t.get("/api/v2/admin/email", &pa).await;
    let rows = r.json["outbox"].as_array().unwrap();
    assert!(
        rows.iter()
            .all(|m| m["link"].is_null() && !m["sent_at"].is_null()),
        "{rows:?}"
    );
    // an unreachable SMTP server falls back to the manual outbox
    set_platform(&t, json!({"smtp": {"port": 1}})).await;
    t.post(
        "/api/v2/auth/forgot",
        None,
        json!({"email": "vie@example.com"}),
    )
    .await;
    let r = t.get("/api/v2/admin/email", &pa).await;
    let m = &r.json["outbox"][0];
    assert!(m["sent_at"].is_null() && m["link"].as_str().unwrap().contains("reset-password"));
    assert!(m["error"].as_str().unwrap().contains("smtp"));
    t.done();
}
