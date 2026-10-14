//! v2.3 routes: `ai` settings (validation, merge, persistence), `/ai/settings`, `/ai/usage`,
//! `/ai/tiers`, chain tiers on the v1 routes, the Link Run estimate and the logs `ai` totals.

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
const SITE: &str = "5a1e0000-0000-4000-8000-000000000001";

struct Ctx {
    app: Router,
    st: AppState,
    admin: String,
    u1: String,
    dir: std::path::PathBuf,
}

async fn setup() -> Ctx {
    let dir = std::env::temp_dir().join(format!("sf-v23-test-{}", sf_core::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("siloforge.toml"),
        "# keep\n[ai]\nbatch_size = 10 # mine\n",
    )
    .unwrap();
    let mut cfg = Config::load(&dir).unwrap();
    cfg.auth.jwt_secret = SECRET.into();
    let db = Db::open(&cfg.db_path()).unwrap();
    db.write(|t| {
        t.execute_batch(&format!(
            "INSERT INTO users(id,email,password_hash,role) VALUES('ua','a@x.io','x','admin');
             INSERT INTO users(id,email,password_hash,role) VALUES('u1','u1@x.io','x','user');
             INSERT INTO sites(id,domain,api_key,user_id) VALUES('{SITE}','a.example','k','ua');
             INSERT INTO posts(id,site_id,url,title) VALUES('p1','{SITE}','https://a.example/1','One');
             INSERT INTO posts(id,site_id,url,title) VALUES('p2','{SITE}','https://a.example/2','Two');"
        ))
        .map_err(err)
    })
    .await
    .unwrap();
    let st = AppState::new(cfg, db, [3u8; 32]);
    let app = crate::router(st.clone(), None);
    let tok = |role: &str, id: &str| sign_jwt(SECRET, &user_claims(role, id, 0)).unwrap();
    Ctx {
        app,
        admin: tok("admin", "ua"),
        u1: tok("user", "u1"),
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
        let bytes = axum::body::to_bytes(res.into_body(), 4 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn exec(&self, sql: &str) {
        let sql = sql.to_string();
        self.st
            .db
            .write(move |t| t.execute_batch(&sql).map_err(err))
            .await
            .unwrap();
    }

    fn done(self) {
        self.st.db.close();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_settings_shape_validation_merge_and_persistence() {
    let c = setup().await;
    let (s, v) = c.call("GET", "/api/v2/settings", &c.admin, None).await;
    assert_eq!(s, StatusCode::OK);
    let ai = &v["ai"];
    assert_eq!(ai["always_on"], false);
    assert_eq!(ai["batch_size"], 10, "toml value read");
    assert_eq!(ai["task_tiers"]["anchor_pick"], "low");
    assert_eq!(ai["task_tiers"]["money_page"], "high");
    assert_eq!(ai["task_tiers"].as_object().unwrap().len(), 8);
    assert_eq!(ai["budget"]["daily_tokens"]["low"], 2_000_000);
    assert_eq!(ai["reasoning_effort"]["high"], "medium");
    assert_eq!(ai["cache_ttl_days"], 30);

    // invalid values: nothing written
    for bad in [
        json!({"ai": {"task_tiers": {"anchor_pick": "ultra"}}}),
        json!({"ai": {"task_tiers": {"nope": "low"}}}),
        json!({"ai": {"batch_size": 0}}),
        json!({"ai": {"batch_size": 51}}),
        json!({"ai": {"confidence_skip": {"anchor": 1.5}}}),
        json!({"ai": {"confidence_skip": {"other": 0.5}}}),
        json!({"ai": {"budget": {"daily_tokens": {"low": -1}}}}),
        json!({"ai": {"budget": {"daily_tokens": {"turbo": 1}}}}),
        json!({"ai": {"budget": {"per_site_daily_tokens": 1e12}}}),
        json!({"ai": {"cache_ttl_days": 366}}),
        json!({"ai": {"reasoning_effort": {"low": "max"}}}),
        json!({"ai": {"always_on": "yes"}}),
        json!({"ai": {"unknown": 1}}),
        json!({"ai": {"task_tiers": "low"}}),
        json!({"ai": {"always_on": true, "batch_size": 0}}),
    ] {
        let (s, e) = c
            .call("PUT", "/api/v2/settings", &c.admin, Some(bad.clone()))
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad} -> {e}");
    }
    let (_, v) = c.call("GET", "/api/v2/settings", &c.admin, None).await;
    assert_eq!(
        v["ai"]["always_on"], false,
        "a rejected PUT changes nothing"
    );

    // valid partial update merges nested objects
    let (s, v) = c
        .call(
            "PUT",
            "/api/v2/settings",
            &c.admin,
            Some(json!({"ai": {
                "always_on": true,
                "task_tiers": {"rephrase": "high"},
                "budget": {"daily_tokens": {"low": 5000}},
                "reasoning_effort": {"main": "none"},
            }})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["ai"]["always_on"], true);
    assert_eq!(v["ai"]["task_tiers"]["rephrase"], "high");
    assert_eq!(v["ai"]["task_tiers"]["anchor_pick"], "low", "merged");
    assert_eq!(v["ai"]["budget"]["daily_tokens"]["low"], 5000);
    assert_eq!(v["ai"]["budget"]["daily_tokens"]["main"], 1_000_000);
    assert_eq!(v["ai"]["reasoning_effort"]["main"], "none");
    assert!(c.st.live.get().ai.always_on, "applied live");
    let toml = std::fs::read_to_string(c.dir.join("siloforge.toml")).unwrap();
    assert!(toml.contains("# keep") && toml.contains("# mine"), "{toml}");
    let reloaded = Config::load(&c.dir).unwrap();
    assert!(reloaded.ai.always_on);
    assert_eq!(reloaded.ai.tier_for("rephrase"), "high");
    assert_eq!(reloaded.ai.budget.daily_tokens.low, 5000);
    assert_eq!(reloaded.ai.batch_size, 10);

    // the dedicated /ai/settings view
    let (s, v) = c.call("GET", "/api/v2/ai/settings", &c.admin, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["always_on"], true);
    let (s, v) = c
        .call(
            "PUT",
            "/api/v2/ai/settings",
            &c.admin,
            Some(json!({"always_on": false, "cache_ttl_days": 0})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["always_on"], false);
    assert_eq!(v["cache_ttl_days"], 0);
    assert!(v["changed"].as_array().unwrap().len() >= 2);
    let (s, _) = c
        .call(
            "PUT",
            "/api/v2/ai/settings",
            &c.admin,
            Some(json!({"batch_size": 99})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // admin only
    let (s, _) = c.call("GET", "/api/v2/ai/settings", &c.u1, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = c.call("GET", "/api/v2/ai/usage", &c.u1, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    c.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_groups_totals_and_budget() {
    let c = setup().await;
    c.exec(&format!(
        "INSERT INTO model_call_log(provider,role,ok,tokens_in,tokens_out,task,tier,site_id,run_id,cost_usd,estimated,items,model_id)
           VALUES('codex-proxy','chat',1,100,20,'anchor_pick','low','{SITE}','r1',0.001,0,5,'gpt-5.6-luna');
         INSERT INTO model_call_log(provider,role,ok,tokens_in,tokens_out,task,tier,site_id,run_id,cost_usd,estimated,items,model_id)
           VALUES('dario','chat',0,0,0,'rephrase','main','{SITE}','r1',0,0,1,'claude-haiku-4-5');
         INSERT INTO model_call_log(provider,role,ok,tokens_in,tokens_out,task,tier,site_id,run_id,cache_hit,items,model_id)
           VALUES('cache','chat',1,0,0,'anchor_pick','low','{SITE}','r1',1,5,'gpt-5.6-luna');
         INSERT INTO model_call_log(provider,role,ok,tokens_in,tokens_out,task,tier,called_at)
           VALUES('old','chat',1,999,999,'anchor_pick','low','2000-01-01T00:00:00.000Z');"
    ))
    .await;
    let (s, v) = c
        .call("GET", "/api/v2/ai/usage?group=tier", &c.admin, None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let rows = v["rows"].as_array().unwrap();
    let low = rows.iter().find(|r| r["key"] == "low").unwrap();
    assert_eq!(low["calls"], 1);
    assert_eq!(low["items"], 5);
    assert_eq!(low["tokens_in"], 100);
    assert_eq!(low["cache_hits"], 5);
    let main = rows.iter().find(|r| r["key"] == "main").unwrap();
    assert_eq!(main["errors"], 1);
    assert_eq!(v["totals"]["tokens_in"], 100, "old rows outside the window");
    assert_eq!(
        v["totals"]["cache_hit_rate"], 0.455,
        "5 hits / (5 hits + 6 real items)"
    );
    assert_eq!(v["budget"]["tiers"]["low"]["used"], 120);
    assert_eq!(v["budget"]["tiers"]["low"]["limit"], 2_000_000);
    assert_eq!(v["budget"]["tiers"]["low"]["exhausted"], false);
    assert_eq!(v["budget"]["sites"][0]["site_id"], SITE);
    assert_eq!(v["budget"]["sites"][0]["name"], "a.example");
    let (_, v) = c
        .call("GET", "/api/v2/ai/usage?group=site", &c.admin, None)
        .await;
    assert_eq!(v["rows"][0]["label"], "a.example");
    let (_, v) = c
        .call(
            "GET",
            "/api/v2/ai/usage?group=day&since=30d",
            &c.admin,
            None,
        )
        .await;
    assert_eq!(
        v["rows"][0]["key"].as_str().unwrap().len(),
        10,
        "YYYY-MM-DD"
    );
    let (_, v) = c
        .call(
            "GET",
            "/api/v2/ai/usage?group=model&run_id=r1",
            &c.admin,
            None,
        )
        .await;
    assert_eq!(v["rows"].as_array().unwrap().len(), 2);
    for bad in [
        "group=nope",
        "since=yesterday",
        "site_id=x".repeat(40).as_str(),
    ] {
        let (s, _) = c
            .call("GET", &format!("/api/v2/ai/usage?{bad}"), &c.admin, None)
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
    }
    // the run object carries the AI summary
    c.exec(&format!(
        "INSERT INTO link_runs(id,site_id,status) VALUES('r1','{SITE}','ready');
         INSERT INTO kv(key,value) VALUES('ai_run:r1','{{\"asked\":5,\"skipped_confident\":2,\"fallback\":null}}');"
    ))
    .await;
    let run =
        c.st.db
            .read(|c| sf_jobs::pipelines::link_run::store::ai_summary(c, "r1"))
            .await
            .unwrap();
    assert_eq!(run["tokens_in"], 100);
    assert_eq!(run["cache_hits"], 5);
    assert_eq!(run["errors"], 1);
    assert_eq!(run["asked"], 5);
    assert_eq!(run["skipped_confident"], 2);
    assert_eq!(run["by_task"]["anchor_pick"]["calls"], 1);
    c.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn chain_tiers_on_v1_routes_and_tier_view() {
    let c = setup().await;
    let (s, p) = c
        .call(
            "POST",
            "/api/v1/providers",
            &c.admin,
            Some(json!({"name": "codex-proxy", "kind": "openai-compatible", "base_url": "http://127.0.0.1:9/v1"})),
        )
        .await;
    assert!(s.is_success(), "{p}");
    let pid = p["id"].as_str().unwrap().to_string();
    let (_, m) = c
        .call(
            "POST",
            "/api/v1/models",
            &c.admin,
            Some(json!({"provider_id": pid, "model_id": "gpt-5.6-luna", "role": "chat"})),
        )
        .await;
    let mid = m["id"].as_str().unwrap().to_string();
    let mk = |name: &str, tier: Option<&str>| {
        let mut b = json!({"name": name, "role": "chat", "active": true});
        if let Some(t) = tier {
            b["tier"] = json!(t);
        }
        b
    };
    let (s, main) = c
        .call(
            "POST",
            "/api/v1/model-chains",
            &c.admin,
            Some(mk("main", None)),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{main}");
    assert_eq!(main["tier"], "main");
    let (s, low) = c
        .call(
            "POST",
            "/api/v1/model-chains",
            &c.admin,
            Some(mk("low", Some("low"))),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(low["active"], true, "main and low are separate scopes");
    let low_id = low["id"].as_str().unwrap().to_string();
    let (s, _) = c
        .call(
            "POST",
            "/api/v1/model-chains",
            &c.admin,
            Some(mk("bad", Some("ultra"))),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // a second active low chain deactivates the first
    let (_, low2) = c
        .call(
            "POST",
            "/api/v1/model-chains",
            &c.admin,
            Some(mk("low2", Some("low"))),
        )
        .await;
    let (_, list) = c
        .call("GET", "/api/v1/model-chains?tier=low", &c.admin, None)
        .await;
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(
        list.iter().filter(|x| x["active"] == true).count(),
        1,
        "{list:?}"
    );
    let (s, _) = c
        .call("GET", "/api/v1/model-chains?tier=x", &c.admin, None)
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // moving the active low2 chain to high keeps it active there
    let l2 = low2["id"].as_str().unwrap().to_string();
    let (s, moved) = c
        .call(
            "PATCH",
            &format!("/api/v1/model-chains/{l2}"),
            &c.admin,
            Some(json!({"tier": "high"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{moved}");
    assert_eq!(moved["tier"], "high");
    assert_eq!(moved["active"], true);
    // rung + tier view
    let (s, _) = c
        .call(
            "POST",
            &format!("/api/v1/model-chains/{low_id}/rungs"),
            &c.admin,
            Some(json!({"model_row_id": mid})),
        )
        .await;
    assert!(s.is_success());
    c.call(
        "POST",
        &format!("/api/v1/model-chains/{low_id}/activate"),
        &c.admin,
        None,
    )
    .await;
    let (s, t) = c.call("GET", "/api/v2/ai/tiers", &c.admin, None).await;
    assert_eq!(s, StatusCode::OK, "{t}");
    let tiers = t["tiers"].as_array().unwrap();
    assert_eq!(tiers.len(), 3);
    assert_eq!(tiers[0]["tier"], "low");
    assert_eq!(tiers[0]["chain_id"], json!(low_id));
    assert_eq!(tiers[0]["resolved_from"], "tier");
    assert_eq!(tiers[0]["rungs"][0]["model_id"], "gpt-5.6-luna");
    let (s, _) = c
        .call("GET", "/api/v2/ai/tiers?site_id=nope", &c.admin, None)
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    c.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn link_run_estimate_and_logs_ai_totals() {
    let c = setup().await;
    let path = format!("/api/v2/sites/{SITE}/link-runs/estimate");
    // AI off: zero tokens
    let (s, e) = c
        .call("POST", &path, &c.admin, Some(json!({"options": {}})))
        .await;
    assert_eq!(s, StatusCode::OK, "{e}");
    assert_eq!(e["ai_enabled"], false);
    assert_eq!(e["tokens_total"], 0);
    assert_eq!(e["within_budget"], true);
    assert_eq!(e["items_estimated"], 6, "2 posts x 3 new per post");
    // ai_refine + contextual
    let (_, e) = c
        .call(
            "POST",
            &path,
            &c.admin,
            Some(json!({"options": {"ai_refine": true, "strategies": {"contextual": true, "in_text": true, "semantic": true}}})),
        )
        .await;
    assert_eq!(e["ai_enabled"], true);
    assert!(e["tokens_total"].as_i64().unwrap() > 0, "{e}");
    assert!(e["by_tier"]["low"]["tokens"].as_i64().unwrap() > 0);
    assert!(e["by_tier"]["main"]["tokens"].as_i64().unwrap() > 0);
    assert_eq!(e["basis"], "defaults");
    // bare options object and high priority
    let (_, e) = c
        .call(
            "POST",
            &path,
            &c.admin,
            Some(json!({"ai_refine": true, "high_priority": true})),
        )
        .await;
    assert_eq!(e["high_priority"], true);
    assert!(e["by_tier"]["high"]["tokens"].as_i64().unwrap() > 0, "{e}");
    // over budget
    let mut cfg = (*c.st.live.get()).clone();
    cfg.ai.budget.daily_tokens.low = 1;
    c.st.live.set(cfg);
    let (_, e) = c
        .call(
            "POST",
            &path,
            &c.admin,
            Some(json!({"options": {"ai_refine": true}})),
        )
        .await;
    assert_eq!(e["within_budget"], false, "{e}");
    assert_eq!(e["budget"]["low"], 1);
    // validation + ownership
    let (s, _) = c
        .call(
            "POST",
            &path,
            &c.admin,
            Some(json!({"options": {"bogus": 1}})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = c.call("POST", &path, &c.u1, Some(json!({}))).await;
    assert!(
        s == StatusCode::NOT_FOUND || s == StatusCode::FORBIDDEN,
        "{s}"
    );

    // logs: `ai` source and totals in the stats
    c.exec(
        "INSERT INTO event_log(source,actor,action,status,detail) VALUES('ai','system','ai anchor_pick gpt-5.6-luna','ok','{\"tokens_in\":10,\"tokens_out\":3,\"cost_usd\":0.5,\"cache_hit\":false,\"items\":4}');
         INSERT INTO event_log(source,actor,action,status,detail) VALUES('ai','system','ai anchor_pick cache','ok','{\"tokens_in\":0,\"tokens_out\":0,\"cost_usd\":0,\"cache_hit\":true,\"items\":4}');
         INSERT INTO event_log(source,actor,action,status) VALUES('api','user:ua','GET /x','ok');",
    )
    .await;
    let (s, st) = c.call("GET", "/api/v2/logs/stats", &c.admin, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(st["by_source"]["ai"], 2);
    assert_eq!(st["ai"]["calls"], 2);
    assert_eq!(st["ai"]["tokens_in"], 10);
    assert_eq!(st["ai"]["tokens_out"], 3);
    assert_eq!(st["ai"]["cache_hits"], 4);
    let (_, l) = c
        .call("GET", "/api/v2/logs?source=ai", &c.admin, None)
        .await;
    assert_eq!(l["logs"].as_array().unwrap().len(), 2);
    c.done();
}
