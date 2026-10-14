//! v2.1 routes: settings, restart, token PATCH, MCP policy/effective/usage/stats + HTTP gate,
//! docs, downloads.
#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    dir: std::path::PathBuf,
}

const TOML: &str = "# operator comment\n[resources]\nmax_concurrent_jobs = 3 # keep me\nunknown_key = \"stays\"\n\n[custom_table]\nx = 1\n";

async fn setup() -> Ctx {
    let dir = std::env::temp_dir().join(format!("sf-v21-test-{}", sf_core::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("siloforge.toml"), TOML).unwrap();
    std::fs::write(
        dir.join("secrets.env"),
        "# secrets\nJWT_SECRET=keepthisline\nEMBED_REMOTE_MODEL=some/model\n",
    )
    .unwrap();
    let mut cfg = Config::load(&dir).unwrap();
    cfg.auth.jwt_secret = SECRET.into();
    cfg.downloads.dir = dir.join("releases").display().to_string();
    let db = Db::open(&cfg.db_path()).unwrap();
    db.write(|t| {
        t.execute_batch(
            "INSERT INTO users(id,email,password_hash,role) VALUES('ua','a@x.io','x','admin');
             INSERT INTO users(id,email,password_hash,role) VALUES('u1','u1@x.io','x','user');",
        )
        .map_err(err)
    })
    .await
    .unwrap();
    let st = AppState::new(cfg, db, [3u8; 32]);
    st.services.insert(Arc::new(crate::state::McpTools(vec![
        json!({"name": "siloforge_list_sites", "read_only": true, "destructive": false, "min_scope": "read", "default_enabled": true, "category": "sites"}),
        json!({"name": "siloforge_bulk_inject", "read_only": false, "destructive": true, "min_scope": "write", "default_enabled": true, "category": "inject"}),
        json!({"name": "siloforge_settings_update", "read_only": false, "destructive": false, "min_scope": "admin", "default_enabled": true, "category": "settings"}),
        json!({"name": "siloforge_api_request", "read_only": false, "destructive": true, "min_scope": "admin", "default_enabled": false, "category": "api"}),
    ])));
    let mcp = Router::new().route("/mcp", axum::routing::any(|| async { "mcp ok" }));
    let app = crate::router(st.clone(), Some(mcp));
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

    async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let (s, b, _) = self.raw(method, path, token, body).await;
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    /// Creates an API token for `owner_jwt`, returns (id, plaintext).
    async fn token(&self, owner_jwt: &str, scopes: Value) -> (String, String) {
        let (s, v) = self
            .call(
                "POST",
                "/api/v2/tokens",
                owner_jwt,
                Some(json!({"name": "t", "scopes": scopes})),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        (
            v["id"].as_str().unwrap().into(),
            v["token"].as_str().unwrap().into(),
        )
    }

    fn done(self) {
        self.st.db.close();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_get_shape_and_admin_only() {
    let c = setup().await;
    let (s, v) = c.call("GET", "/api/v2/settings", &c.admin, None).await;
    assert_eq!(s, StatusCode::OK);
    for sec in [
        "resources",
        "compute",
        "wordpress",
        "crawl",
        "llm",
        "server",
        "backup",
        "monitor",
    ] {
        assert!(v[sec].is_object(), "{sec}");
    }
    assert_eq!(v["resources"]["max_concurrent_jobs"], 3);
    assert!(v["resources"]["logical_cpus"].as_u64().unwrap() >= 1);
    assert_eq!(v["compute"]["embed_mode"], "auto");
    assert_eq!(v["compute"]["remote_key_set"], false);
    assert_eq!(v["restart_required"], json!([]));
    assert_eq!(v["restart_supported"], false);
    assert_eq!(v["server"]["mcp_http_enabled"], true);
    let (s, _) = c.call("GET", "/api/v2/settings", &c.u1, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    c.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_put_validates_persists_and_applies_live() {
    let c = setup().await;
    // one bad key -> nothing written
    let (s, v) = c
        .call(
            "PUT",
            "/api/v2/settings",
            &c.admin,
            Some(json!({"resources": {"max_concurrent_jobs": 5, "vector_cache_mb": 1}})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("resources.vector_cache_mb")
    );
    assert_eq!(c.st.live.get().resources.max_concurrent_jobs, 3);
    for bad in [
        json!({"resources": {"max_concurrent_jobs": 17}}),
        json!({"resources": {"max_memory_mb": 100}}),
        json!({"resources": {"defer_when_cpu_above": 101}}),
        json!({"compute": {"hybrid_gpu_share": 0.95}}),
        json!({"compute": {"embed_batch_size": 4}}),
        json!({"compute": {"embed_mode": "quantum"}}),
        json!({"compute": {"embed_chain": ["onnx", "bogus"]}}),
        json!({"compute": {"active_device": "cpu"}}),
        json!({"crawl": {"concurrency": 0}}),
        json!({"backup": {"keep": 0}}),
        json!({"backup": {"schedule": "not a cron"}}),
        json!({"server": {"binds": ["nope"]}}),
        json!({"llm": {"fallback_url": "ftp://x"}}),
        json!({"nosuch": {"x": 1}}),
        json!({"resources": "flat"}),
        json!([1, 2]),
    ] {
        let (s, v) = c
            .call("PUT", "/api/v2/settings", &c.admin, Some(bad.clone()))
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad} -> {v}");
    }
    let (s, v) = c
        .call(
            "PUT",
            "/api/v2/settings",
            &c.admin,
            Some(json!({
                "resources": {"max_concurrent_jobs": 5, "max_memory_mb": 0, "defer_when_cpu_above": 85.5, "cpu_threads": 1},
                "compute": {"embed_mode": "hybrid", "hybrid_gpu_share": 0.6, "embed_batch_size": 32,
                            "remote_key": "sk-or-SECRET-1", "remote_model": "other/model"},
                "wordpress": {"verify_after_inject": false, "user_agent": "UA/2"},
                "crawl": {"concurrency": 8},
                "backup": {"schedule": "", "keep": 5, "passphrase": "long-enough-pass"},
                "server": {"mcp_http_enabled": false, "binds": ["127.0.0.1:8670"]},
                "monitor": {"log_reads": true},
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // full GET shape back, secrets masked
    assert_eq!(v["resources"]["max_concurrent_jobs"], 5);
    assert_eq!(v["compute"]["embed_mode"], "hybrid");
    assert_eq!(v["compute"]["remote_key_set"], true);
    assert_eq!(v["compute"]["remote_key"], "***");
    assert_eq!(v["backup"]["passphrase_set"], true);
    assert_eq!(v["server"]["mcp_http_enabled"], false);
    let text = v.to_string();
    assert!(!text.contains("sk-or-SECRET-1") && !text.contains("long-enough-pass"));
    let mut rr: Vec<String> = serde_json::from_value(v["restart_required"].clone()).unwrap();
    rr.sort();
    assert_eq!(rr, ["resources.cpu_threads", "server.binds"]);
    // live
    let live = c.st.live.get();
    assert_eq!(live.resources.max_concurrent_jobs, 5);
    assert_eq!(live.embed.mode, "hybrid");
    assert_eq!(live.embed.remote_key, "sk-or-SECRET-1");
    assert_eq!(live.crawl.user_agent, "UA/2");
    assert!(!live.wordpress.verify_after_inject);
    // toml: comments + unknown keys kept
    let toml = std::fs::read_to_string(c.dir.join("siloforge.toml")).unwrap();
    assert!(toml.contains("# operator comment"));
    assert!(toml.contains("max_concurrent_jobs = 5 # keep me"), "{toml}");
    assert!(toml.contains("unknown_key = \"stays\"") && toml.contains("[custom_table]"));
    assert!(!toml.contains("sk-or-SECRET-1") && !toml.contains("long-enough-pass"));
    assert!(toml.contains("mode = \"hybrid\""));
    // remote_model has a secrets.env override line -> written there, not to the toml
    assert!(!toml.contains("other/model"));
    let env = std::fs::read_to_string(c.dir.join("secrets.env")).unwrap();
    assert!(
        env.starts_with("# secrets\nJWT_SECRET=keepthisline\n"),
        "{env}"
    );
    assert!(env.contains("EMBED_REMOTE_MODEL=other/model"));
    assert!(env.contains("OPENROUTER_API_KEY=sk-or-SECRET-1"));
    assert!(env.contains("SILOFORGE_BACKUP_PASSPHRASE=long-enough-pass"));
    // the files reload to the same values
    let reloaded = Config::load(&c.dir).unwrap();
    assert_eq!(reloaded.resources.max_concurrent_jobs, 5);
    assert_eq!(reloaded.embed.remote_model, "other/model");
    assert_eq!(reloaded.embed.remote_key, "sk-or-SECRET-1");
    assert_eq!(reloaded.embed.hybrid_gpu_share, 0.6);
    // "***" keeps, "" clears a secret
    let (s, _) = c
        .call(
            "PUT",
            "/api/v2/settings",
            &c.admin,
            Some(json!({"compute": {"remote_key": "***"}})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(c.st.live.get().embed.remote_key, "sk-or-SECRET-1");
    let (s, v) = c
        .call(
            "PUT",
            "/api/v2/settings",
            &c.admin,
            Some(json!({"compute": {"remote_key": ""}})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["compute"]["remote_key_set"], false);
    let env = std::fs::read_to_string(c.dir.join("secrets.env")).unwrap();
    assert!(!env.contains("OPENROUTER_API_KEY="));
    // MCP over HTTP is now off -> 404 at /mcp
    let (s, _, _) = c.raw("POST", "/mcp", &c.admin, Some(json!({}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    c.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_endpoint_calls_the_hook() {
    let c = setup().await;
    let (s, _) = c
        .call("POST", "/api/v2/system/restart", &c.admin, None)
        .await;
    assert_eq!(s, StatusCode::NOT_IMPLEMENTED, "no hook registered");
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    c.st.services
        .insert(Arc::new(crate::v2::settings::SettingsHooks {
            active_device: Arc::new(|| "directml".into()),
            restart: Some(Arc::new(move || {
                n2.fetch_add(1, Ordering::SeqCst);
            })),
        }));
    let (s, _) = c.call("POST", "/api/v2/system/restart", &c.u1, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, v) = c
        .call("POST", "/api/v2/system/restart", &c.admin, None)
        .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    assert_eq!(v["restarting"], true);
    for _ in 0..50 {
        if n.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(n.load(Ordering::SeqCst), 1);
    let (_, v) = c.call("GET", "/api/v2/settings", &c.admin, None).await;
    assert_eq!(v["restart_supported"], true);
    assert_eq!(v["compute"]["active_device"], "directml");
    c.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn token_patch_scope_rules_and_cache_clear() {
    let c = setup().await;
    let (uid, utok) = c.token(&c.u1, json!(["read"])).await;
    // the read token can read but not write
    let (s, _) = c.call("POST", "/api/v2/auth/logout-all", &utok, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // a user cannot grant admin or * to their token
    for sc in [json!(["admin"]), json!(["*"])] {
        let (s, _) = c
            .call(
                "PATCH",
                &format!("/api/v2/tokens/{uid}"),
                &c.u1,
                Some(json!({"scopes": sc})),
            )
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }
    let (s, _) = c
        .call(
            "PATCH",
            &format!("/api/v2/tokens/{uid}"),
            &c.u1,
            Some(json!({"scopes": ["bogus"]})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // raise to write: applies immediately (auth cache cleared)
    let (_, before) = c
        .call("GET", "/api/v2/mcp/policy/effective", &utok, None)
        .await;
    assert_eq!(before["scope"], "read");
    let (s, v) = c
        .call(
            "PATCH",
            &format!("/api/v2/tokens/{uid}"),
            &c.u1,
            Some(json!({"scopes": ["write"], "name": "renamed", "expires_in_days": 30})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["scopes"], json!(["write"]));
    assert_eq!(v["name"], "renamed");
    assert!(v["expires_at"].is_string());
    assert!(v.get("token").is_none() && v.get("token_hash").is_none());
    let (_, after) = c
        .call("GET", "/api/v2/mcp/policy/effective", &utok, None)
        .await;
    assert_eq!(after["scope"], "write");
    // expiry back to none
    let (_, v) = c
        .call(
            "PATCH",
            &format!("/api/v2/tokens/{uid}"),
            &c.u1,
            Some(json!({"expires_in_days": null})),
        )
        .await;
    assert!(v["expires_at"].is_null());
    // other users' tokens are invisible
    let (aid, atok) = c.token(&c.admin, json!(["*"])).await;
    let (s, _) = c
        .call(
            "PATCH",
            &format!("/api/v2/tokens/{aid}"),
            &c.u1,
            Some(json!({"name": "x"})),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // child of an admin-scoped token can never be raised above its parent; lowering the parent
    // caps the child
    let (s, child) = c
        .call(
            "POST",
            "/api/v2/tokens",
            &atok,
            Some(json!({"name": "child", "scopes": ["write"]})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{child}");
    let cid = child["id"].as_str().unwrap().to_string();
    let (s, v) = c
        .call(
            "PATCH",
            &format!("/api/v2/tokens/{aid}"),
            &c.admin,
            Some(json!({"scopes": ["read"]})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["descendants_capped"], 1);
    let (_, list) = c.call("GET", "/api/v2/tokens", &c.admin, None).await;
    let row = list["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == cid.as_str());
    assert!(row.is_none(), "child with nothing left was revoked");
    let (s, _) = c
        .call(
            "PATCH",
            &format!("/api/v2/tokens/{aid}"),
            &c.admin,
            Some(json!({"scopes": ["*"]})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    // tool_policy round trip + listing
    let (s, v) = c
        .call(
            "PATCH",
            &format!("/api/v2/tokens/{uid}"),
            &c.u1,
            Some(json!({"tool_policy": {"allow": ["siloforge_list_sites"]}})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["tool_policy"]["allow"], json!(["siloforge_list_sites"]));
    let (_, eff) = c
        .call("GET", "/api/v2/mcp/policy/effective", &utok, None)
        .await;
    assert_eq!(eff["allowed"], json!(["siloforge_list_sites"]));
    let (s, _) = c
        .call(
            "PATCH",
            &format!("/api/v2/tokens/{uid}"),
            &c.u1,
            Some(json!({"tool_policy": {"allow": ["siloforge_nope"]}})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, list) = c.call("GET", "/api/v2/tokens", &c.u1, None).await;
    assert_eq!(
        list["tokens"][0]["tool_policy"]["allow"],
        json!(["siloforge_list_sites"])
    );
    let (_, v) = c
        .call(
            "PATCH",
            &format!("/api/v2/tokens/{uid}"),
            &c.u1,
            Some(json!({"tool_policy": null})),
        )
        .await;
    assert!(v["tool_policy"].is_null());
    c.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_policy_effective_usage_and_stats() {
    let c = setup().await;
    let (_, rtok) = c.token(&c.u1, json!(["read"])).await;
    let (_, eff) = c
        .call("GET", "/api/v2/mcp/policy/effective", &rtok, None)
        .await;
    assert_eq!(eff["allowed"], json!(["siloforge_list_sites"]));
    let (_, eff) = c
        .call("GET", "/api/v2/mcp/policy/effective", &c.admin, None)
        .await;
    assert_eq!(
        eff["allowed"],
        json!([
            "siloforge_list_sites",
            "siloforge_bulk_inject",
            "siloforge_settings_update"
        ]),
        "api_request is disabled by default"
    );
    let (s, _) = c.call("GET", "/api/v2/mcp/policy", &c.u1, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, v) = c
        .call(
            "PUT",
            "/api/v2/mcp/policy",
            &c.admin,
            Some(
                json!({"tools": {"siloforge_bulk_inject": {"enabled": false},
                                  "siloforge_api_request": {"enabled": true},
                                  "siloforge_list_sites": {"min_scope": "write"}}}),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["tools"]["siloforge_bulk_inject"]["enabled"], false);
    assert_eq!(v["tools"]["siloforge_api_request"]["enabled"], true);
    let (_, eff) = c
        .call("GET", "/api/v2/mcp/policy/effective", &rtok, None)
        .await;
    assert_eq!(eff["allowed"], json!([]));
    for bad in [
        json!({"tools": {"siloforge_nope": {"enabled": true}}}),
        json!({"tools": {"siloforge_list_sites": {"min_scope": "root"}}}),
        json!({"per_token": {"not-a-token": {"allow": []}}}),
    ] {
        let (s, _) = c
            .call("PUT", "/api/v2/mcp/policy", &c.admin, Some(bad))
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }
    // usage: accepted from a read token (POST), counted in stats
    for ok in [true, false, true] {
        let (s, _, _) = c
            .raw(
                "POST",
                "/api/v2/mcp/usage",
                &rtok,
                Some(json!({"tool": "siloforge_list_sites", "ok": ok, "duration_ms": 12, "error": if ok { Value::Null } else { json!("boom") }})),
            )
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
    }
    let (s, _, _) = c
        .raw(
            "POST",
            "/api/v2/mcp/usage",
            &rtok,
            Some(json!({"tool": "../etc"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _, _) = c
        .raw(
            "POST",
            "/api/v2/mcp/usage",
            "garbage",
            Some(json!({"tool": "siloforge_x"})),
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (_, st) = c.call("GET", "/api/v2/mcp/stats", &c.admin, None).await;
    let t = st["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "siloforge_list_sites")
        .unwrap()
        .clone();
    assert_eq!(t["calls_24h"], 3);
    assert_eq!(t["errors_24h"], 1);
    assert_eq!(t["category"], "sites");
    assert!(t["last_called_at"].is_string());
    // http gate on by default
    let (s, _, _) = c.raw("POST", "/mcp", &c.admin, None).await;
    assert_eq!(s, StatusCode::OK);
    c.done();
}

#[tokio::test(flavor = "multi_thread")]
async fn docs_and_downloads() {
    let c = setup().await;
    let (s, v) = c.call("GET", "/api/v2/docs", &c.u1, None).await;
    assert_eq!(s, StatusCode::OK);
    let docs = v["docs"].as_array().unwrap();
    if let Some(first) = docs.first() {
        let slug = first["slug"].as_str().unwrap();
        let (s, d) = c
            .call("GET", &format!("/api/v2/docs/{slug}"), &c.u1, None)
            .await;
        assert_eq!(s, StatusCode::OK);
        assert!(d["markdown"].is_string() && d["title"].is_string());
    }
    for bad in ["nope", "..%2F..%2FCargo", "UPPER"] {
        let (s, _) = c
            .call("GET", &format!("/api/v2/docs/{bad}"), &c.u1, None)
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{bad}");
    }
    let (s, _) = c.call("GET", "/api/v2/docs", "bad", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    let rel = c.dir.join("releases");
    std::fs::create_dir_all(&rel).unwrap();
    std::fs::write(rel.join("siloforge-2.1.0-windows-x64.zip"), b"zipbytes").unwrap();
    std::fs::write(rel.join("siloforge-connect-0.2.0.zip"), b"plug").unwrap();
    std::fs::write(rel.join("secret.txt"), b"no").unwrap();
    let (s, v) = c.call("GET", "/api/v2/downloads", &c.u1, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["current_version"], sf_core::VERSION);
    assert_eq!(v["downloads"].as_array().unwrap().len(), 2);
    let (s, body, h) = c
        .raw(
            "GET",
            "/api/v2/downloads/siloforge-2.1.0-windows-x64.zip",
            &c.u1,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, b"zipbytes");
    assert!(
        h[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .contains("attachment; filename=\"siloforge-2.1.0-windows-x64.zip\"")
    );
    for bad in [
        "secret.txt",
        "..%2Fsecrets.env",
        "siloforge-9.9.9-windows-x64.zip",
        "%2E%2E",
    ] {
        let (s, _, _) = c
            .raw("GET", &format!("/api/v2/downloads/{bad}"), &c.u1, None)
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{bad}");
    }
    c.done();
}
