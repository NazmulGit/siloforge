//! End-to-end tests: a stub SiloForge REST API (axum) + the MCP server driven over an in-memory
//! stdio pipe (newline-delimited JSON-RPC) and over the streamable HTTP transport.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::type_complexity
)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sf_mcp::McpConfig;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

const SITE_A: &str = "11111111-1111-1111-1111-111111111111";
const SITE_KEY: &str = "sf_0123456789abcdef0123456789abcdef";

#[derive(Clone, Debug)]
struct Rec {
    method: String,
    path: String,
    query: String,
    body: Value,
    auth: String,
    via: String,
    tool: String,
    raw_body: String,
}

const POLICY_PATH: &str = "/api/v2/mcp/policy/effective";
const USAGE_PATH: &str = "/api/v2/mcp/usage";

#[derive(Default)]
struct Stub {
    reqs: Mutex<Vec<Rec>>,
    logins: AtomicUsize,
    reject_next: AtomicUsize,
    polls: Mutex<HashMap<String, usize>>,
    /// Answer of the policy endpoint (None = 404, an older server).
    policy: Mutex<Option<(u16, Value)>>,
    /// Per `Authorization` header overrides of `policy`.
    policy_by_auth: Mutex<HashMap<String, (u16, Value)>>,
    /// Bodies POSTed to the usage endpoint.
    usage: Mutex<Vec<Value>>,
}

/// Paths that are MCP plumbing, not tool traffic.
fn is_plumbing(path: &str) -> bool {
    path == POLICY_PATH || path == USAGE_PATH
}

impl Stub {
    fn api_reqs(&self) -> Vec<Rec> {
        self.reqs
            .lock()
            .iter()
            .filter(|r| (r.path != "/api/v1/sites" || r.method != "GET") && !is_plumbing(&r.path))
            .cloned()
            .collect()
    }
    fn policy_fetches(&self) -> usize {
        self.reqs
            .lock()
            .iter()
            .filter(|r| r.path == POLICY_PATH)
            .count()
    }
    fn allow(&self, names: &[&str]) {
        *self.policy.lock() = Some((
            200,
            json!({"http_enabled": true, "scope": "admin", "allowed": names, "tools": {}}),
        ));
    }
    fn allow_all(&self) {
        let names = sf_mcp::tool_names();
        self.allow(&names);
    }
    fn site_lists(&self) -> usize {
        self.reqs
            .lock()
            .iter()
            .filter(|r| r.path == "/api/v1/sites" && r.method == "GET")
            .count()
    }
    fn last(&self) -> Rec {
        self.api_reqs().last().cloned().expect("a request")
    }
}

fn j(status: StatusCode, v: Value) -> Response {
    (status, axum::Json(v)).into_response()
}

async fn handle(
    State(st): State<Arc<Stub>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    let path = uri.path().to_string();
    let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    let hdr = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    if path == "/api/v1/auth/login" {
        let n = st.logins.fetch_add(1, Ordering::SeqCst) + 1;
        if body["password"] == "pw" {
            return j(
                StatusCode::OK,
                json!({"token": format!("jwt-{n}"), "user": {"id": "u1"}}),
            );
        }
        return j(
            StatusCode::UNAUTHORIZED,
            json!({"error": "invalid email or password"}),
        );
    }
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    st.reqs.lock().push(Rec {
        method: method.to_string(),
        path: path.clone(),
        query: uri.query().unwrap_or("").to_string(),
        body: body.clone(),
        auth: auth.clone(),
        via: hdr("x-siloforge-via"),
        tool: hdr("x-siloforge-tool"),
        raw_body: String::from_utf8_lossy(&raw).into_owned(),
    });
    if path == POLICY_PATH {
        let ans = st
            .policy_by_auth
            .lock()
            .get(&auth)
            .cloned()
            .or_else(|| st.policy.lock().clone());
        return match ans {
            Some((code, v)) => j(StatusCode::from_u16(code).unwrap(), v),
            None => j(StatusCode::NOT_FOUND, json!({"error": "route not found"})),
        };
    }
    if path == USAGE_PATH {
        st.usage.lock().push(body);
        return StatusCode::NO_CONTENT.into_response();
    }
    if st.reject_next.load(Ordering::SeqCst) > 0 {
        st.reject_next.fetch_sub(1, Ordering::SeqCst);
        return j(StatusCode::UNAUTHORIZED, json!({"error": "unauthorized"}));
    }
    if auth == "Bearer bad" {
        return j(StatusCode::UNAUTHORIZED, json!({"error": "unauthorized"}));
    }
    let m = method.as_str();
    if m == "GET" && path == "/api/v1/sites" {
        return j(
            StatusCode::OK,
            json!([
                {"id": SITE_A, "domain": "a.com", "api_key": SITE_KEY, "settings": {"wp_user": "bob", "wp_app_password": "secret-pass"}},
                {"id": "site-b", "domain": "www.b.com", "api_key": "sf_ffffffffffffffffffffffffffffffff"}
            ]),
        );
    }
    if m == "POST" && path == "/api/v1/sites" {
        return j(
            StatusCode::CREATED,
            json!({"id": "new-site", "domain": body["domain"], "api_key": SITE_KEY, "settings": {}}),
        );
    }
    if let Some(id) = path.strip_prefix("/api/v1/jobs/run/") {
        let n = {
            let mut p = st.polls.lock();
            let e = p.entry(id.to_string()).or_insert(0);
            *e += 1;
            *e
        };
        let status = if id == "job-fail" {
            "failed"
        } else if n >= 2 {
            "done"
        } else {
            "running"
        };
        return j(
            StatusCode::OK,
            json!({"id": id, "kind": "crawl", "status": status, "stats": {"upserted": 3}, "error": null}),
        );
    }
    if m == "POST" && path.ends_with("/crawl") {
        return j(StatusCode::ACCEPTED, json!({"job_run_id": "job-crawl"}));
    }
    if m == "POST" && path.ends_with("/plan") {
        return j(StatusCode::ACCEPTED, json!({"job_run_id": "job-plan"}));
    }
    if m == "GET" && path.starts_with("/api/v1/edges/") {
        return j(
            StatusCode::OK,
            json!({"edges": [{"id": "e1", "anchor": "x"}, {"id": "e2", "anchor": "y"}], "total": 1500}),
        );
    }
    if path.starts_with("/api/v1/csv/export/") {
        return (
            [(header::CONTENT_TYPE, "text/csv")],
            "source_url,target_url\nhttp://a/1,http://a/2\n",
        )
            .into_response();
    }
    if path == "/health" {
        return j(
            StatusCode::OK,
            json!({"ok": true, "version": "2.0.0", "uptime_secs": 5, "jobs": {"running": 1, "queued": 2}}),
        );
    }
    if path == "/api/v2/docs/intro" {
        return j(
            StatusCode::OK,
            json!({"slug": "intro", "title": "Intro", "section": "Start", "markdown": "Hello **world**"}),
        );
    }
    if m == "GET" && path == format!("/api/v1/dashboard/{SITE_A}") {
        return j(
            StatusCode::OK,
            json!({"health_score": 81, "orphans": 3, "cannibalization_warnings": [{"keyword": "shoes"}]}),
        );
    }
    if path.starts_with("/api/v1/dashboard/")
        && !path.ends_with(SITE_A)
        && !path.ends_with("site-b")
    {
        return j(StatusCode::NOT_FOUND, json!({"error": "site not found"}));
    }
    j(StatusCode::OK, json!({"ok": true}))
}

async fn start_stub() -> (Arc<Stub>, String) {
    let st = Arc::new(Stub::default());
    let app = Router::new().fallback(handle).with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (st, format!("http://{addr}"))
}

fn cfg(url: &str) -> McpConfig {
    let mut c = McpConfig::from_lookup(|_| None);
    c.api_url = url.to_string();
    c.token = Some("sfk_testtoken".into());
    c.min_poll = Duration::from_millis(10);
    c
}

// ---------------------------------------------------------------- stdio client

struct Client {
    w: DuplexStream,
    r: BufReader<DuplexStream>,
    next_id: u64,
}

impl Client {
    async fn start(cfg: McpConfig) -> Self {
        let (client_w, server_r) = tokio::io::duplex(1 << 20);
        let (server_w, client_r) = tokio::io::duplex(1 << 20);
        tokio::spawn(sf_mcp::serve_io(cfg, server_r, server_w));
        let mut c = Self {
            w: client_w,
            r: BufReader::new(client_r),
            next_id: 1,
        };
        let init = c
            .request("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}))
            .await;
        assert_eq!(init["result"]["serverInfo"]["name"], "siloforge", "{init}");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        c.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        c
    }

    async fn send(&mut self, v: Value) {
        let mut line = v.to_string();
        line.push('\n');
        self.w.write_all(line.as_bytes()).await.unwrap();
        self.w.flush().await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(30), self.r.read_line(&mut line))
                .await
                .expect("timeout")
                .unwrap();
            assert!(n > 0, "server closed the pipe");
            let v: Value = serde_json::from_str(line.trim()).expect("valid json line");
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    /// Calls a tool; returns (text, is_error, full result).
    async fn call(&mut self, name: &str, args: Value) -> (String, bool, Value) {
        let v = self
            .request("tools/call", json!({"name": name, "arguments": args}))
            .await;
        let res = v["result"].clone();
        assert!(res.is_object(), "no result for {name}: {v}");
        let text = res["content"][0]["text"].as_str().unwrap_or("").to_string();
        (text, res["isError"] == json!(true), res)
    }
}

/// 28 v1/v2.0 tools + 90 v2.1 tools.
const ALL_TOOLS: usize = 118;

const V1_TOOLS: [&str; 20] = [
    "siloforge_list_sites",
    "siloforge_dashboard",
    "siloforge_list_edges",
    "siloforge_links_report",
    "siloforge_anchor_report",
    "siloforge_link_history",
    "siloforge_list_jobs",
    "siloforge_list_clusters",
    "siloforge_add_site",
    "siloforge_crawl_site",
    "siloforge_plan_site",
    "siloforge_decide_edge",
    "siloforge_bulk_decide",
    "siloforge_bulk_inject",
    "siloforge_bulk_undo",
    "siloforge_edit_anchor",
    "siloforge_add_keyword_rule",
    "siloforge_run_keyword_linker",
    "siloforge_add_ignore_keyword",
    "siloforge_export_edges_csv",
];

#[tokio::test]
async fn tools_list_has_all_v1_and_v2_tools() {
    let (_st, url) = start_stub().await;
    let mut c = Client::start(cfg(&url)).await;
    let v = c.request("tools/list", json!({})).await;
    let names: Vec<String> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for n in V1_TOOLS {
        assert!(names.iter().any(|x| x == n), "missing {n}");
    }
    for n in [
        "siloforge_system_status",
        "siloforge_job_status",
        "siloforge_job_logs",
        "siloforge_cancel_job",
        "siloforge_wait_for_job",
        "siloforge_run_pipeline",
        "siloforge_create_backup",
        "siloforge_list_notifications",
    ] {
        assert!(names.iter().any(|x| x == n), "missing {n}");
    }
    // Old server (policy endpoint 404): everything except the default-disabled escape hatch.
    assert_eq!(names.len(), ALL_TOOLS - 1);
    assert!(!names.iter().any(|x| x == "siloforge_api_request"));
    // v1 parameter names are kept.
    let tools = v["result"]["tools"].as_array().unwrap();
    let edges = tools
        .iter()
        .find(|t| t["name"] == "siloforge_list_edges")
        .unwrap();
    for p in ["site", "status", "tool", "limit", "offset"] {
        assert!(
            edges["inputSchema"]["properties"][p].is_object(),
            "list_edges.{p}"
        );
    }
    let inject = tools
        .iter()
        .find(|t| t["name"] == "siloforge_bulk_inject")
        .unwrap();
    assert_eq!(inject["annotations"]["destructiveHint"], true);
}

#[tokio::test]
async fn each_tool_maps_to_method_path_body() {
    let (st, url) = start_stub().await;
    let mut c = Client::start(cfg(&url)).await;
    let a = SITE_A;
    let cases: Vec<(&str, Value, &str, String, &str, Option<Value>)> = vec![
        (
            "siloforge_dashboard",
            json!({"site": "a.com"}),
            "GET",
            format!("/api/v1/dashboard/{a}"),
            "",
            None,
        ),
        (
            "siloforge_list_edges",
            json!({"site": "a.com", "status": "approved", "tool": "plan", "limit": 5}),
            "GET",
            format!("/api/v1/edges/{a}"),
            "status=approved&tool=plan&limit=5",
            None,
        ),
        (
            "siloforge_links_report",
            json!({"site": "a.com"}),
            "GET",
            format!("/api/v1/reports/links/{a}"),
            "",
            None,
        ),
        (
            "siloforge_anchor_report",
            json!({"site": "a.com"}),
            "GET",
            format!("/api/v1/reports/anchors/{a}"),
            "",
            None,
        ),
        (
            "siloforge_link_history",
            json!({"site": "a.com"}),
            "GET",
            format!("/api/v1/history/{a}"),
            "",
            None,
        ),
        (
            "siloforge_list_jobs",
            json!({"site": "a.com"}),
            "GET",
            format!("/api/v1/jobs/{a}"),
            "",
            None,
        ),
        (
            "siloforge_list_jobs",
            json!({"status": "running"}),
            "GET",
            "/api/v2/jobs".into(),
            "status=running&limit=50",
            None,
        ),
        (
            "siloforge_list_clusters",
            json!({"site": "a.com"}),
            "GET",
            format!("/api/v1/silo/{a}/clusters"),
            "",
            None,
        ),
        (
            "siloforge_add_site",
            json!({"domain": "c.com", "mode": "sitemap"}),
            "POST",
            "/api/v1/sites".into(),
            "",
            Some(json!({"domain": "c.com", "mode": "sitemap"})),
        ),
        (
            "siloforge_crawl_site",
            json!({"site": "a.com"}),
            "POST",
            format!("/api/v1/sites/{a}/crawl"),
            "",
            Some(json!({})),
        ),
        (
            "siloforge_plan_site",
            json!({"site": "a.com", "topology": "serial", "archive": true}),
            "POST",
            format!("/api/v1/sites/{a}/plan"),
            "",
            Some(json!({"topology": "serial", "archive": true})),
        ),
        (
            // pilot 2026-10-13: the server accepts all five topologies, MCP refused this one
            "siloforge_plan_site",
            json!({"site": "a.com", "topology": "bidirectional"}),
            "POST",
            format!("/api/v1/sites/{a}/plan"),
            "",
            Some(json!({"topology": "bidirectional"})),
        ),
        (
            "siloforge_plan_site",
            json!({"site": "a.com"}),
            "POST",
            format!("/api/v1/sites/{a}/plan"),
            "",
            Some(json!({})),
        ),
        (
            "siloforge_decide_edge",
            json!({"edge_id": "e1", "decision": "approved"}),
            "POST",
            "/api/v1/edges/e1/decide".into(),
            "",
            Some(json!({"decision": "approved"})),
        ),
        (
            "siloforge_bulk_decide",
            json!({"edge_ids": ["e1", "e2"], "decision": "rejected"}),
            "POST",
            "/api/v1/bulk/decide".into(),
            "",
            Some(json!({"edge_ids": ["e1", "e2"], "decision": "rejected"})),
        ),
        (
            "siloforge_bulk_inject",
            json!({"site": "a.com"}),
            "POST",
            "/api/v1/bulk/run".into(),
            "",
            Some(json!({"site_id": a})),
        ),
        (
            "siloforge_bulk_inject",
            json!({"site": "a.com", "edge_ids": ["e1"]}),
            "POST",
            "/api/v1/bulk/run".into(),
            "",
            Some(json!({"site_id": a, "edge_ids": ["e1"]})),
        ),
        (
            "siloforge_bulk_undo",
            json!({"site": "a.com"}),
            "POST",
            "/api/v1/bulk/undo".into(),
            "",
            Some(json!({"site_id": a})),
        ),
        (
            "siloforge_edit_anchor",
            json!({"edge_id": "e1", "anchor": "best shoes", "anchor_type": "partial"}),
            "PATCH",
            "/api/v1/edges/e1/anchor".into(),
            "",
            Some(json!({"anchor": "best shoes", "anchor_type": "partial"})),
        ),
        (
            "siloforge_add_keyword_rule",
            json!({"site": "a.com", "keyword": "shoes", "target_url": "https://a.com/s", "max_links": 2}),
            "POST",
            format!("/api/v1/rules/keywords/{a}"),
            "",
            Some(json!({"keyword": "shoes", "target_url": "https://a.com/s", "max_links": 2})),
        ),
        (
            "siloforge_run_keyword_linker",
            json!({"site": "a.com"}),
            "POST",
            format!("/api/v1/rules/keywords/{a}/run"),
            "",
            Some(json!({})),
        ),
        (
            "siloforge_add_ignore_keyword",
            json!({"site": "a.com", "phrase": "click here"}),
            "POST",
            format!("/api/v1/rules/ignore/{a}"),
            "",
            Some(json!({"phrase": "click here"})),
        ),
        (
            "siloforge_export_edges_csv",
            json!({"site": "a.com"}),
            "GET",
            format!("/api/v1/csv/export/{a}"),
            "",
            None,
        ),
        (
            "siloforge_job_status",
            json!({"job_id": "job-x"}),
            "GET",
            "/api/v1/jobs/run/job-x".into(),
            "",
            None,
        ),
        (
            "siloforge_job_logs",
            json!({"job_id": "job-x", "after": 10}),
            "GET",
            "/api/v2/jobs/job-x/logs".into(),
            "after=10&limit=200",
            None,
        ),
        (
            // agents echo the enqueue tools' `job_run_id` name (pilot 2026-10-13)
            "siloforge_job_status",
            json!({"job_run_id": "job-y"}),
            "GET",
            "/api/v1/jobs/run/job-y".into(),
            "",
            None,
        ),
        (
            "siloforge_cancel_job",
            json!({"job_id": "job-x"}),
            "POST",
            "/api/v2/jobs/job-x/cancel".into(),
            "",
            Some(json!({})),
        ),
        (
            "siloforge_create_backup",
            json!({"note": "pre-cutover"}),
            "POST",
            "/api/v2/backups".into(),
            "",
            Some(json!({"note": "pre-cutover"})),
        ),
        (
            "siloforge_list_notifications",
            json!({"unread_only": true}),
            "GET",
            "/api/v2/notifications".into(),
            "limit=50&unread=1",
            None,
        ),
    ];
    for (tool, args, method, path, query, body) in cases {
        let (text, is_err, _) = c.call(tool, args.clone()).await;
        assert!(!is_err, "{tool} {args}: {text}");
        let r = st.last();
        assert_eq!(
            (r.method.as_str(), r.path.as_str(), r.query.as_str()),
            (method, path.as_str(), query),
            "{tool} {args}"
        );
        if let Some(b) = body {
            assert_eq!(r.body, b, "{tool} body");
        }
        assert_eq!(r.auth, "Bearer sfk_testtoken");
    }
    let (text, _, _) = c
        .call("siloforge_export_edges_csv", json!({"site": "a.com"}))
        .await;
    assert!(text.starts_with("source_url,target_url"));
    let (text, _, res) = c.call("siloforge_system_status", json!({})).await;
    assert!(text.contains("\"queued\": 2"), "{text}");
    assert_eq!(res["structuredContent"]["jobs"]["running"], 1);
}

#[tokio::test]
async fn relogin_once_on_401_with_password_auth() {
    let (st, url) = start_stub().await;
    let mut c = cfg(&url);
    c.token = None;
    c.password = Some("pw".into());
    let mut client = Client::start(c).await;
    let (text, is_err, _) = client
        .call("siloforge_crawl_site", json!({"site": SITE_A}))
        .await;
    assert!(!is_err, "{text}");
    assert_eq!(st.logins.load(Ordering::SeqCst), 1);
    assert_eq!(st.last().auth, "Bearer jwt-1");
    st.reject_next.store(1, Ordering::SeqCst);
    let (text, is_err, _) = client
        .call("siloforge_crawl_site", json!({"site": SITE_A}))
        .await;
    assert!(!is_err, "{text}");
    assert_eq!(st.logins.load(Ordering::SeqCst), 2);
    assert_eq!(st.last().auth, "Bearer jwt-2");
    // Persistent 401: exactly one retry, then a v1-style error.
    st.reject_next.store(5, Ordering::SeqCst);
    let (text, is_err, _) = client
        .call("siloforge_crawl_site", json!({"site": SITE_A}))
        .await;
    assert!(is_err);
    assert!(
        text.starts_with("Error: SILOFORGE POST /api/v1/sites/")
            && text.contains("→ 401: unauthorized"),
        "{text}"
    );
    assert_eq!(st.logins.load(Ordering::SeqCst), 3);
    st.reject_next.store(0, Ordering::SeqCst);
}

#[tokio::test]
async fn static_token_never_relogins_and_bad_password_reports() {
    let (st, url) = start_stub().await;
    let mut c = cfg(&url);
    c.token = Some("bad".into());
    c.password = Some("pw".into());
    let mut client = Client::start(c).await;
    let (text, is_err, _) = client.call("siloforge_list_sites", json!({})).await;
    assert!(is_err && text.contains("→ 401"), "{text}");
    assert_eq!(st.logins.load(Ordering::SeqCst), 0);

    let mut c = cfg(&url);
    c.token = None;
    c.password = Some("wrong".into());
    let mut client = Client::start(c).await;
    let (text, is_err, _) = client.call("siloforge_list_sites", json!({})).await;
    assert!(is_err && text.contains("login failed (401)"), "{text}");
    assert!(!text.contains("wrong"));
}

#[tokio::test]
async fn credentials_never_reach_the_model() {
    let (_st, url) = start_stub().await;
    let mut c = Client::start(cfg(&url)).await;
    for (tool, args) in [
        ("siloforge_list_sites", json!({})),
        ("siloforge_add_site", json!({"domain": "c.com"})),
    ] {
        let (text, is_err, _) = c.call(tool, args).await;
        assert!(!is_err, "{text}");
        for secret in [
            "api_key",
            SITE_KEY,
            "secret-pass",
            "wp_app_password",
            "sf_ffff",
        ] {
            assert!(!text.contains(secret), "{tool} leaked {secret}: {text}");
        }
        assert!(text.contains("domain"));
    }
}

#[tokio::test]
async fn site_resolution_is_cached() {
    let (st, url) = start_stub().await;
    let mut c = Client::start(cfg(&url)).await;
    for _ in 0..3 {
        let (text, is_err, _) = c
            .call("siloforge_dashboard", json!({"site": "https://A.com/"}))
            .await;
        assert!(!is_err, "{text}");
    }
    assert_eq!(st.site_lists(), 1);
    // www. alias and ids resolve from the same cached list.
    let (_, is_err, _) = c
        .call("siloforge_dashboard", json!({"site": "b.com"}))
        .await;
    assert!(!is_err);
    assert_eq!(st.last().path, "/api/v1/dashboard/site-b");
    // UUIDs skip the lookup entirely.
    let (_, is_err, _) = c.call("siloforge_dashboard", json!({"site": SITE_A})).await;
    assert!(!is_err);
    assert_eq!(st.site_lists(), 1);
    // Unknown domain: one refresh, then a helpful error.
    let (text, is_err, _) = c
        .call("siloforge_dashboard", json!({"site": "nope.com"}))
        .await;
    assert!(is_err && text.contains("site not found"), "{text}");
    assert_eq!(st.site_lists(), 2);
    // add_site invalidates the cache.
    let _ = c
        .call("siloforge_add_site", json!({"domain": "d.com"}))
        .await;
    let _ = c
        .call("siloforge_dashboard", json!({"site": "a.com"}))
        .await;
    assert_eq!(st.site_lists(), 3);
}

#[tokio::test]
async fn list_edges_reports_paging() {
    let (st, url) = start_stub().await;
    let mut c = Client::start(cfg(&url)).await;
    let (text, is_err, _) = c
        .call(
            "siloforge_list_edges",
            json!({"site": "a.com", "limit": 2, "offset": 4}),
        )
        .await;
    assert!(!is_err, "{text}");
    assert_eq!(st.last().query, "limit=2&offset=4");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["total"], 1500);
    assert_eq!(v["returned"], 2);
    assert_eq!(v["next_offset"], 6);
    assert_eq!(v["has_more"], true);
    let (text, is_err, _) = c
        .call(
            "siloforge_list_edges",
            json!({"site": "a.com", "limit": 5000}),
        )
        .await;
    assert!(is_err && text.contains("between 1 and 1000"), "{text}");
}

#[tokio::test]
async fn wait_for_job_and_pipeline() {
    let (st, url) = start_stub().await;
    let mut c = Client::start(cfg(&url)).await;
    let (text, is_err, res) = c
        .call(
            "siloforge_wait_for_job",
            json!({"job_id": "job-w", "poll_secs": 1}),
        )
        .await;
    assert!(!is_err, "{text}");
    assert_eq!(res["structuredContent"]["status"], "done");
    assert_eq!(res["structuredContent"]["finished"], true);
    assert_eq!(st.polls.lock().get("job-w").copied(), Some(2));

    let (text, is_err, res) = c
        .call(
            "siloforge_run_pipeline",
            json!({"site": "a.com", "topology": "circle"}),
        )
        .await;
    assert!(!is_err, "{text}");
    let s = &res["structuredContent"];
    assert_eq!(s["ok"], true, "{s}");
    assert_eq!(s["steps"].as_array().map(Vec::len), Some(2));
    assert_eq!(s["suggested_edges"], 1500);
    assert_eq!(s["written_to_wordpress"], false);
    let plan = st
        .api_reqs()
        .into_iter()
        .find(|r| r.path.ends_with("/plan"))
        .unwrap();
    assert_eq!(plan.body, json!({"topology": "circle"}));
    assert!(
        st.api_reqs()
            .iter()
            .all(|r| !r.path.starts_with("/api/v1/bulk")),
        "pipeline must not inject"
    );

    let (_, is_err, res) = c
        .call("siloforge_wait_for_job", json!({"job_id": "job-fail"}))
        .await;
    assert!(!is_err);
    assert_eq!(res["structuredContent"]["status"], "failed");
}

#[tokio::test]
async fn stdio_ping_and_unknown_tool() {
    let (_st, url) = start_stub().await;
    let mut c = Client::start(cfg(&url)).await;
    let v = c.request("ping", json!({})).await;
    assert!(v["result"].is_object(), "{v}");
    let v = c
        .request("tools/call", json!({"name": "nope", "arguments": {}}))
        .await;
    assert!(v["error"].is_object(), "{v}");
    let v = c.request("ping", json!({})).await;
    assert!(v["result"].is_object());
}

#[tokio::test]
async fn hostile_arguments_never_panic() {
    let (st, url) = start_stub().await;
    st.allow_all();
    let mut c = Client::start(cfg(&url)).await;
    let big = "x".repeat(100_000);
    let mut deep = json!(1);
    for _ in 0..60 {
        deep = json!({"a": deep});
    }
    let hostile = vec![
        json!({}),
        json!({"site": 123, "edge_id": [], "job_id": {}}),
        json!({"site": "../../etc/passwd", "edge_id": "../x", "job_id": "a/b?c=d"}),
        json!({"site": "", "edge_ids": "notarray", "decision": "maybe"}),
        json!({"site": "a.com", "limit": -5, "offset": -1, "timeout_secs": 99999}),
        json!({"site": "a.com", "limit": 1e308, "max_links": "NaN", "archive": "yes"}),
        json!({"site": big.clone(), "anchor": big, "edge_id": "e1"}),
        json!({"site": deep.clone(), "edge_ids": [1, null, "e1"], "note": deep}),
        json!({"site": "日本語.com", "edge_ids": ["e1/../../x"], "phrase": "\u{0000}"}),
        json!({"edge_ids": (0..20_000).map(|i| format!("e{i}")).collect::<Vec<_>>(), "decision": "approved"}),
        json!({"method": "DELETE", "path": "/api/v1/../../etc", "query": {"a": {"b": 1}}, "body": [1, 2], "confirm": "maybe"}),
        json!({"method": 7, "path": ["x"], "settings": "str", "clusters": [1, 2], "rungs": {}, "ids": ["a"], "scopes": [1]}),
        json!({"site": "a.com", "rule_id": "../x", "token_id": "a b", "slug": "..", "backup_id": "%2e%2e", "fingerprint": "", "csv": 5}),
        json!({"site": "a.com", "expires_in_days": null, "tool_policy": [], "price_in": "NaN", "dim": 1e300, "window": "1y", "confirm": false}),
    ];
    let names = sf_mcp::tool_names();
    for name in &names {
        if *name == "siloforge_wait_for_job" || *name == "siloforge_run_pipeline" {
            continue; // covered elsewhere (they block by design)
        }
        for args in &hostile {
            let (_text, _is_err, res) = c.call(name, args.clone()).await;
            assert!(res["content"].is_array(), "{name}");
        }
    }
    // Non-object arguments are a protocol error, not a crash.
    let v = c
        .request(
            "tools/call",
            json!({"name": "siloforge_dashboard", "arguments": "boom"}),
        )
        .await;
    assert!(v["error"].is_object() || v["result"].is_object());
    for r in st.reqs.lock().iter() {
        assert!(
            !r.path.contains(".."),
            "path traversal reached the API: {}",
            r.path
        );
    }
    let v = c.request("ping", json!({})).await;
    assert!(v["result"].is_object(), "server still alive");
}

// ---------------------------------------------------------------- HTTP transport

async fn start_mcp_http(url: &str) -> String {
    let mut c = cfg(url);
    c.token = Some("privileged-must-not-be-used".into());
    let app: Router = sf_mcp::http_router(c);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/mcp")
}

async fn http_rpc(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    body: Value,
) -> (u16, Value) {
    let mut req = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .json(&body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let res = req.send().await.unwrap();
    let status = res.status().as_u16();
    let text = res.text().await.unwrap();
    let json_text = text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(str::trim)
        .rfind(|l| l.starts_with('{'))
        .map(str::to_string)
        .unwrap_or(text);
    (
        status,
        serde_json::from_str(&json_text).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn http_transport_initialize_list_call_forwarding_token() {
    let (st, url) = start_stub().await;
    let mcp = start_mcp_http(&url).await;
    let http = reqwest::Client::new();
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}});

    let (status, _) = http_rpc(&http, &mcp, None, init.clone()).await;
    assert_eq!(status, 401);

    let (status, v) = http_rpc(&http, &mcp, Some("sfk_caller"), init).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["result"]["serverInfo"]["name"], "siloforge", "{v}");

    let (status, v) = http_rpc(
        &http,
        &mcp,
        Some("sfk_caller"),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        v["result"]["tools"].as_array().map(Vec::len),
        Some(ALL_TOOLS - 1),
        "{v}"
    );

    let (status, v) = http_rpc(
        &http,
        &mcp,
        Some("sfk_caller"),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "siloforge_crawl_site", "arguments": {"site": "a.com"}}}),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_ne!(v["result"]["isError"], true, "{v}");
    let reqs = st.reqs.lock().clone();
    assert!(!reqs.is_empty());
    for r in &reqs {
        assert_eq!(
            r.auth, "Bearer sfk_caller",
            "caller token must be forwarded, never the configured one"
        );
    }
}

// ---------------------------------------------------------------- v2.1 tools

type Case = (
    &'static str,
    Value,
    &'static str,
    String,
    String,
    Option<Value>,
);

fn v21_cases() -> Vec<Case> {
    let a = SITE_A;
    let c =
        |tool: &'static str,
         args: Value,
         m: &'static str,
         path: &str,
         q: &str,
         body: Option<Value>| { (tool, args, m, path.to_string(), q.to_string(), body) };
    let site = json!({"site": "a.com"});
    let none = Some(Value::Null);
    let empty = Some(json!({}));
    vec![
        // sites
        c(
            "siloforge_site_get",
            site.clone(),
            "GET",
            &format!("/api/v1/sites/{a}"),
            "",
            None,
        ),
        c(
            "siloforge_site_update",
            json!({"site": "a.com", "settings": {"preset": "balanced"}, "mode": "sitemap"}),
            "PATCH",
            &format!("/api/v1/sites/{a}"),
            "",
            Some(json!({"settings": {"preset": "balanced"}, "mode": "sitemap"})),
        ),
        c(
            "siloforge_site_set_wp_credentials",
            json!({"site": "a.com", "wp_user": "bob", "wp_app_password": "abcd efgh"}),
            "PATCH",
            &format!("/api/v1/sites/{a}"),
            "",
            Some(json!({"settings": {"wp_user": "bob", "wp_app_password": "abcd efgh"}})),
        ),
        c(
            "siloforge_site_rotate_key",
            json!({"site": "a.com", "confirm": true}),
            "POST",
            &format!("/api/v1/sites/{a}/rotate-key"),
            "",
            empty.clone(),
        ),
        c(
            "siloforge_usage_counts",
            json!({}),
            "GET",
            "/api/v1/usage",
            "",
            None,
        ),
        // runs
        c(
            "siloforge_archive_plan",
            json!({"site": "a.com", "topology": "circle"}),
            "POST",
            &format!("/api/v1/sites/{a}/archive-plan"),
            "",
            Some(json!({"topology": "circle"})),
        ),
        c(
            "siloforge_audit_site",
            json!({"site": "a.com", "skip_crawl": true}),
            "POST",
            &format!("/api/v1/schedules/{a}/run-now"),
            "",
            Some(json!({"skip_crawl": true, "job_kind": "audit"})),
        ),
        c(
            "siloforge_link_health_run",
            json!({"site": "a.com", "internal_only": true}),
            "POST",
            &format!("/api/v1/sites/{a}/link-health"),
            "",
            Some(json!({"internal_only": true})),
        ),
        c(
            "siloforge_gsc_status",
            site.clone(),
            "GET",
            &format!("/api/v1/sites/{a}/gsc"),
            "",
            None,
        ),
        c(
            "siloforge_gsc_sync",
            site.clone(),
            "POST",
            &format!("/api/v2/sites/{a}/gsc/sync"),
            "",
            empty.clone(),
        ),
        c(
            "siloforge_gsc_connect",
            json!({"site": "a.com", "credentials_json": "{}"}),
            "POST",
            &format!("/api/v1/sites/{a}/gsc"),
            "",
            Some(json!({"credentials_json": "{}"})),
        ),
        c(
            "siloforge_gsc_disconnect",
            json!({"site": "a.com", "confirm": true}),
            "DELETE",
            &format!("/api/v1/sites/{a}/gsc"),
            "",
            none.clone(),
        ),
        // rules
        c(
            "siloforge_list_keyword_rules",
            site.clone(),
            "GET",
            &format!("/api/v1/rules/keywords/{a}"),
            "",
            None,
        ),
        c(
            "siloforge_update_keyword_rule",
            json!({"rule_id": "r1", "active": false}),
            "PATCH",
            "/api/v1/rules/keywords/r1",
            "",
            Some(json!({"active": false})),
        ),
        c(
            "siloforge_delete_keyword_rule",
            json!({"rule_id": "r1", "confirm": true}),
            "DELETE",
            "/api/v1/rules/keywords/r1",
            "",
            none.clone(),
        ),
        c(
            "siloforge_list_ignore_keywords",
            site.clone(),
            "GET",
            &format!("/api/v1/rules/ignore/{a}"),
            "",
            None,
        ),
        c(
            "siloforge_delete_ignore_keyword",
            json!({"phrase_id": "p1", "confirm": true}),
            "DELETE",
            "/api/v1/rules/ignore/p1",
            "",
            none.clone(),
        ),
        // schedules
        c(
            "siloforge_list_schedules",
            site.clone(),
            "GET",
            &format!("/api/v1/schedules/{a}"),
            "",
            None,
        ),
        c(
            "siloforge_upsert_schedule",
            json!({"site": "a.com", "job_kind": "crawl", "cron": "0 3 * * *", "active": true}),
            "POST",
            &format!("/api/v1/schedules/{a}"),
            "",
            Some(json!({"job_kind": "crawl", "cron": "0 3 * * *", "active": true})),
        ),
        c(
            "siloforge_delete_schedule",
            json!({"schedule_id": "s1", "confirm": true}),
            "DELETE",
            "/api/v1/schedules/s1",
            "",
            none.clone(),
        ),
        c(
            "siloforge_run_schedule_now",
            json!({"site": "a.com", "job_kind": "plan"}),
            "POST",
            &format!("/api/v1/schedules/{a}/run-now"),
            "",
            Some(json!({"job_kind": "plan"})),
        ),
        // reports
        c(
            "siloforge_orphans_report",
            json!({"site": "a.com", "limit": 10}),
            "GET",
            &format!("/api/v1/posts/{a}"),
            "limit=10&is_orphan=true",
            None,
        ),
        c(
            "siloforge_cannibalization_report",
            site.clone(),
            "GET",
            &format!("/api/v1/dashboard/{a}"),
            "",
            None,
        ),
        c(
            "siloforge_metrics_trend",
            json!({"site": "a.com", "trend": 5}),
            "GET",
            &format!("/api/v1/reports/metrics/{a}"),
            "trend=5",
            None,
        ),
        c(
            "siloforge_link_health_report",
            json!({"site": "a.com", "offset": 100}),
            "GET",
            &format!("/api/v1/reports/link-health/{a}"),
            "offset=100",
            None,
        ),
        c(
            "siloforge_link_graph",
            json!({"site": "a.com", "max_nodes": 10}),
            "GET",
            &format!("/api/v1/reports/graph/{a}"),
            "max_nodes=10",
            None,
        ),
        c(
            "siloforge_export_history_csv",
            site.clone(),
            "GET",
            &format!("/api/v1/history/{a}"),
            "format=csv",
            None,
        ),
        c(
            "siloforge_export_link_health_csv",
            site.clone(),
            "GET",
            &format!("/api/v1/reports/link-health/{a}"),
            "format=csv",
            None,
        ),
        // silo
        c(
            "siloforge_export_clusters",
            site.clone(),
            "GET",
            &format!("/api/v1/silo/{a}/clusters/export"),
            "",
            None,
        ),
        c(
            "siloforge_import_clusters",
            json!({"site": "a.com", "clusters": [{"label": "x", "pillar_url": "https://a.com/p", "member_urls": []}], "confirm": true}),
            "POST",
            &format!("/api/v1/silo/{a}/clusters/import"),
            "",
            Some(
                json!({"clusters": [{"label": "x", "pillar_url": "https://a.com/p", "member_urls": []}]}),
            ),
        ),
        // posts
        c(
            "siloforge_list_posts",
            json!({"site": "a.com", "search": "shoe", "is_orphan": false, "limit": 20}),
            "GET",
            &format!("/api/v1/posts/{a}"),
            "search=shoe&is_orphan=false&limit=20",
            None,
        ),
        c(
            "siloforge_import_posts_csv",
            json!({"site": "a.com", "csv": "url,content_html\nhttps://a.com/x,<p>x</p>\n"}),
            "POST",
            &format!("/api/v1/csv/import/{a}"),
            "",
            None,
        ),
        // ai
        c(
            "siloforge_ai_status",
            json!({}),
            "GET",
            "/api/v1/providers/health",
            "",
            None,
        ),
        c(
            "siloforge_list_ai_providers",
            json!({}),
            "GET",
            "/api/v1/providers",
            "",
            None,
        ),
        c(
            "siloforge_get_ai_provider",
            json!({"provider_id": "p1"}),
            "GET",
            "/api/v1/providers/p1",
            "",
            None,
        ),
        c(
            "siloforge_create_ai_provider",
            json!({"name": "or", "kind": "openrouter", "base_url": "https://x", "api_key": "k"}),
            "POST",
            "/api/v1/providers",
            "",
            Some(
                json!({"name": "or", "kind": "openrouter", "base_url": "https://x", "api_key": "k"}),
            ),
        ),
        c(
            "siloforge_update_ai_provider",
            json!({"provider_id": "p1", "enabled": false}),
            "PATCH",
            "/api/v1/providers/p1",
            "",
            Some(json!({"enabled": false})),
        ),
        c(
            "siloforge_delete_ai_provider",
            json!({"provider_id": "p1", "confirm": true}),
            "DELETE",
            "/api/v1/providers/p1",
            "",
            none.clone(),
        ),
        c(
            "siloforge_probe_ai_provider",
            json!({"provider_id": "p1"}),
            "POST",
            "/api/v1/providers/p1/probe",
            "",
            empty.clone(),
        ),
        c(
            "siloforge_list_ai_models",
            json!({"provider_id": "p1"}),
            "GET",
            "/api/v1/models",
            "provider_id=p1",
            None,
        ),
        c(
            "siloforge_create_ai_model",
            json!({"provider_id": "p1", "model_id": "gpt", "role": "llm", "price_in": 0.5}),
            "POST",
            "/api/v1/models",
            "",
            Some(json!({"provider_id": "p1", "model_id": "gpt", "role": "llm", "price_in": 0.5})),
        ),
        c(
            "siloforge_update_ai_model",
            json!({"model_row_id": "m1", "label": "L"}),
            "PATCH",
            "/api/v1/models/m1",
            "",
            Some(json!({"label": "L"})),
        ),
        c(
            "siloforge_delete_ai_model",
            json!({"model_row_id": "m1", "confirm": true}),
            "DELETE",
            "/api/v1/models/m1",
            "",
            none.clone(),
        ),
        c(
            "siloforge_probe_ai_model",
            json!({"model_row_id": "m1"}),
            "POST",
            "/api/v1/models/m1/probe",
            "",
            empty.clone(),
        ),
        c(
            "siloforge_list_ai_chains",
            json!({"site_id": "global"}),
            "GET",
            "/api/v1/model-chains",
            "site_id=global",
            None,
        ),
        c(
            "siloforge_create_ai_chain",
            json!({"name": "c", "role": "llm", "site": "a.com"}),
            "POST",
            "/api/v1/model-chains",
            "",
            Some(json!({"name": "c", "role": "llm", "site_id": a})),
        ),
        c(
            "siloforge_update_ai_chain",
            json!({"chain_id": "c1", "notes": "n"}),
            "PATCH",
            "/api/v1/model-chains/c1",
            "",
            Some(json!({"notes": "n"})),
        ),
        c(
            "siloforge_delete_ai_chain",
            json!({"chain_id": "c1", "confirm": true}),
            "DELETE",
            "/api/v1/model-chains/c1",
            "",
            none.clone(),
        ),
        c(
            "siloforge_activate_ai_chain",
            json!({"chain_id": "c1"}),
            "POST",
            "/api/v1/model-chains/c1/activate",
            "",
            empty.clone(),
        ),
        c(
            "siloforge_deactivate_ai_chain",
            json!({"chain_id": "c1"}),
            "POST",
            "/api/v1/model-chains/c1/deactivate",
            "",
            empty.clone(),
        ),
        c(
            "siloforge_list_ai_rungs",
            json!({"chain_id": "c1"}),
            "GET",
            "/api/v1/model-chains/c1/rungs",
            "",
            None,
        ),
        c(
            "siloforge_add_ai_rung",
            json!({"chain_id": "c1", "model_row_id": "m1", "max_retries": 2}),
            "POST",
            "/api/v1/model-chains/c1/rungs",
            "",
            Some(json!({"model_row_id": "m1", "max_retries": 2})),
        ),
        c(
            "siloforge_replace_ai_rungs",
            json!({"chain_id": "c1", "rungs": [{"model_row_id": "m1"}], "confirm": true}),
            "PUT",
            "/api/v1/model-chains/c1/rungs",
            "",
            Some(json!({"rungs": [{"model_row_id": "m1"}]})),
        ),
        c(
            "siloforge_reorder_ai_rungs",
            json!({"chain_id": "c1", "rung_ids": ["r2", "r1"]}),
            "POST",
            "/api/v1/model-chains/c1/rungs/reorder",
            "",
            Some(json!({"rung_ids": ["r2", "r1"]})),
        ),
        c(
            "siloforge_update_ai_rung",
            json!({"rung_id": "r1", "enabled": true}),
            "PATCH",
            "/api/v1/model-chain-rungs/r1",
            "",
            Some(json!({"enabled": true})),
        ),
        c(
            "siloforge_delete_ai_rung",
            json!({"rung_id": "r1", "confirm": true}),
            "DELETE",
            "/api/v1/model-chain-rungs/r1",
            "",
            none.clone(),
        ),
        // settings
        c(
            "siloforge_settings_get",
            json!({}),
            "GET",
            "/api/v2/settings",
            "",
            None,
        ),
        c(
            "siloforge_settings_update",
            json!({"resources": {"max_concurrent_jobs": 4}}),
            "PUT",
            "/api/v2/settings",
            "",
            Some(json!({"resources": {"max_concurrent_jobs": 4}})),
        ),
        // system
        c(
            "siloforge_system_stats",
            json!({}),
            "GET",
            "/api/v2/system/stats",
            "",
            None,
        ),
        c(
            "siloforge_system_metrics",
            json!({"window": "24h"}),
            "GET",
            "/api/v2/system/metrics",
            "window=24h",
            None,
        ),
        c(
            "siloforge_system_restart",
            json!({"confirm": true}),
            "POST",
            "/api/v2/system/restart",
            "",
            empty.clone(),
        ),
        c(
            "siloforge_list_errors",
            json!({"resolved": "false", "limit": 5}),
            "GET",
            "/api/v2/errors",
            "resolved=false&limit=5",
            None,
        ),
        c(
            "siloforge_resolve_error",
            json!({"fingerprint": "abc123"}),
            "POST",
            "/api/v2/errors/abc123/resolve",
            "",
            empty.clone(),
        ),
        c(
            "siloforge_audit_log",
            json!({"limit": 10, "before_id": 99}),
            "GET",
            "/api/v2/audit",
            "limit=10&before_id=99",
            None,
        ),
        // backups
        c(
            "siloforge_list_backups",
            json!({"limit": 3}),
            "GET",
            "/api/v2/backups",
            "limit=3",
            None,
        ),
        c(
            "siloforge_get_backup",
            json!({"backup_id": "b1"}),
            "GET",
            "/api/v2/backups/b1",
            "",
            None,
        ),
        c(
            "siloforge_verify_backup",
            json!({"backup_id": "b1"}),
            "POST",
            "/api/v2/backups/b1/verify",
            "",
            empty.clone(),
        ),
        c(
            "siloforge_delete_backup",
            json!({"backup_id": "b1", "confirm": true}),
            "DELETE",
            "/api/v2/backups/b1",
            "",
            none.clone(),
        ),
        // notifications
        c(
            "siloforge_mark_notifications_read",
            json!({"ids": [1, 2]}),
            "POST",
            "/api/v2/notifications/read",
            "",
            Some(json!({"ids": [1, 2]})),
        ),
        c(
            "siloforge_list_notification_channels",
            json!({}),
            "GET",
            "/api/v2/notifications/channels",
            "",
            None,
        ),
        c(
            "siloforge_create_notification_channel",
            json!({"name": "d", "kind": "discord", "config": {"url": "https://d"}, "events": ["job.failed"]}),
            "POST",
            "/api/v2/notifications/channels",
            "",
            Some(
                json!({"name": "d", "kind": "discord", "config": {"url": "https://d"}, "events": ["job.failed"]}),
            ),
        ),
        c(
            "siloforge_update_notification_channel",
            json!({"channel_id": "ch1", "enabled": false}),
            "PATCH",
            "/api/v2/notifications/channels/ch1",
            "",
            Some(json!({"enabled": false})),
        ),
        c(
            "siloforge_delete_notification_channel",
            json!({"channel_id": "ch1", "confirm": true}),
            "DELETE",
            "/api/v2/notifications/channels/ch1",
            "",
            none.clone(),
        ),
        c(
            "siloforge_test_notification_channel",
            json!({"channel_id": "ch1"}),
            "POST",
            "/api/v2/notifications/channels/ch1/test",
            "",
            empty.clone(),
        ),
        // tokens
        c(
            "siloforge_list_tokens",
            json!({"all": true}),
            "GET",
            "/api/v2/tokens",
            "all=true",
            None,
        ),
        c(
            "siloforge_create_token",
            json!({"name": "hermes", "scopes": ["read"], "expires_in_days": 30}),
            "POST",
            "/api/v2/tokens",
            "",
            Some(json!({"name": "hermes", "scopes": ["read"], "expires_in_days": 30})),
        ),
        c(
            "siloforge_update_token",
            json!({"token_id": "t1", "scopes": ["read", "write"], "expires_in_days": null}),
            "PATCH",
            "/api/v2/tokens/t1",
            "",
            Some(json!({"scopes": ["read", "write"], "expires_in_days": null})),
        ),
        c(
            "siloforge_revoke_token",
            json!({"token_id": "t1", "confirm": true}),
            "DELETE",
            "/api/v2/tokens/t1",
            "",
            none.clone(),
        ),
        // mcp
        c(
            "siloforge_mcp_info",
            json!({}),
            "GET",
            "/api/v2/mcp",
            "",
            None,
        ),
        c(
            "siloforge_mcp_policy_get",
            json!({}),
            "GET",
            "/api/v2/mcp/policy",
            "",
            None,
        ),
        c(
            "siloforge_mcp_policy_update",
            json!({"http_enabled": false, "confirm": true}),
            "PUT",
            "/api/v2/mcp/policy",
            "",
            Some(json!({"http_enabled": false})),
        ),
        c(
            "siloforge_mcp_stats",
            json!({}),
            "GET",
            "/api/v2/mcp/stats",
            "",
            None,
        ),
        // logs
        c(
            "siloforge_logs_query",
            json!({"source": "mcp", "site": "a.com", "status": "error", "limit": 50}),
            "GET",
            "/api/v2/logs",
            &format!("source=mcp&status=error&limit=50&site_id={a}"),
            None,
        ),
        c(
            "siloforge_logs_stats",
            json!({"since": "24h"}),
            "GET",
            "/api/v2/logs/stats",
            "since=24h",
            None,
        ),
        // docs / downloads
        c(
            "siloforge_docs_list",
            json!({}),
            "GET",
            "/api/v2/docs",
            "",
            None,
        ),
        c(
            "siloforge_docs_get",
            json!({"slug": "intro"}),
            "GET",
            "/api/v2/docs/intro",
            "",
            None,
        ),
        c(
            "siloforge_downloads_list",
            json!({}),
            "GET",
            "/api/v2/downloads",
            "",
            None,
        ),
    ]
}

#[tokio::test]
async fn each_v21_tool_maps_to_method_path_body_with_headers() {
    let (st, url) = start_stub().await;
    let mut c = Client::start(cfg(&url)).await;
    let cases = v21_cases();
    // Every v2.1 route tool is covered (custom ones have their own tests).
    let covered: Vec<&str> = cases.iter().map(|c| c.0).collect();
    let legacy: Vec<&str> = V1_TOOLS.to_vec();
    for name in sf_mcp::tool_names() {
        let custom = matches!(
            name,
            "siloforge_capabilities" | "siloforge_doctor" | "siloforge_api_request"
        );
        let v2_0 = sf_mcp::tools::catalogue()
            .iter()
            .position(|t| t.name == name)
            .is_some_and(|i| i < 28);
        if custom || v2_0 || legacy.contains(&name) {
            continue;
        }
        assert!(covered.contains(&name), "no mapping test for {name}");
    }
    for (tool, args, method, path, query, body) in cases {
        let (text, is_err, _) = c.call(tool, args.clone()).await;
        assert!(!is_err, "{tool} {args}: {text}");
        let r = st.last();
        assert_eq!(
            (r.method.as_str(), r.path.as_str(), r.query.as_str()),
            (method, path.as_str(), query.as_str()),
            "{tool} {args}"
        );
        if let Some(b) = body {
            assert_eq!(r.body, b, "{tool} body");
        }
        assert_eq!(r.auth, "Bearer sfk_testtoken");
        assert_eq!(r.via, "mcp", "{tool}");
        assert_eq!(r.tool, tool, "{tool}");
    }
    // CSV import sends the CSV text as-is.
    let csv = st
        .api_reqs()
        .into_iter()
        .find(|r| r.path.starts_with("/api/v1/csv/import/"))
        .unwrap();
    assert!(
        csv.raw_body.starts_with("url,content_html\n"),
        "{}",
        csv.raw_body
    );
    // Post-processing: cannibalization keeps only its keys; docs render as Markdown.
    let (text, _, _) = c
        .call("siloforge_cannibalization_report", json!({"site": "a.com"}))
        .await;
    assert!(
        text.contains("cannibalization_warnings") && !text.contains("orphans"),
        "{text}"
    );
    let (text, _, _) = c.call("siloforge_docs_get", json!({"slug": "intro"})).await;
    assert_eq!(text, "# Intro\n\nHello **world**");
    // Every REST request made for a tool carried the telemetry headers; plumbing did not name a tool.
    for r in st.reqs.lock().iter() {
        if r.path == POLICY_PATH {
            assert_eq!(r.tool, "", "policy fetch is not a tool call");
        } else {
            assert_eq!(r.via, "mcp", "{} {}", r.method, r.path);
            assert!(r.tool.starts_with("siloforge_"), "{} {}", r.method, r.path);
        }
    }
}

#[tokio::test]
async fn destructive_tools_need_confirm_and_send_nothing() {
    let (st, url) = start_stub().await;
    let mut c = Client::start(cfg(&url)).await;
    let gated: Vec<String> = sf_mcp::tools::catalogue()
        .into_iter()
        .filter(|t| sf_mcp::tools::needs_confirm(t.name))
        .map(|t| t.name.to_string())
        .collect();
    for n in [
        "siloforge_site_rotate_key",
        "siloforge_revoke_token",
        "siloforge_delete_backup",
        "siloforge_system_restart",
        "siloforge_mcp_policy_update",
        "siloforge_delete_notification_channel",
        "siloforge_import_clusters",
    ] {
        assert!(gated.iter().any(|g| g == n), "{n} must need confirm");
    }
    // Legacy destructive tools keep working without confirm (contract deviation, Hermes cards).
    assert!(!sf_mcp::tools::needs_confirm("siloforge_bulk_inject"));
    assert!(!sf_mcp::tools::needs_confirm("siloforge_bulk_undo"));
    let before = st.reqs.lock().len();
    for name in &gated {
        for confirm in [json!(null), json!(false), json!("yes"), json!(1)] {
            let args = json!({"site": SITE_A, "token_id": "t1", "backup_id": "b1", "rule_id": "r1",
                "phrase_id": "p1", "schedule_id": "s1", "provider_id": "p1", "model_row_id": "m1",
                "chain_id": "c1", "rung_id": "r1", "channel_id": "ch1", "http_enabled": true,
                "clusters": [], "rungs": [], "confirm": confirm});
            let (text, is_err, _) = c.call(name, args).await;
            assert!(is_err && text.contains("confirm: true"), "{name}: {text}");
        }
    }
    let api_calls: Vec<Rec> = st.reqs.lock()[before..]
        .iter()
        .filter(|r| !is_plumbing(&r.path))
        .cloned()
        .collect();
    assert!(
        api_calls.is_empty(),
        "refused tools reached the API: {api_calls:?}"
    );
    // bulk inject without confirm still works.
    let (text, is_err, _) = c
        .call("siloforge_bulk_inject", json!({"site": "a.com"}))
        .await;
    assert!(!is_err, "{text}");
}

fn names_of(v: &Value) -> Vec<String> {
    v["result"]["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn stdio_policy_filters_list_and_refuses_calls() {
    let (st, url) = start_stub().await;
    st.allow(&[
        "siloforge_list_sites",
        "siloforge_capabilities",
        "siloforge_dashboard",
    ]);
    let mut c = Client::start(cfg(&url)).await;
    let v = c.request("tools/list", json!({})).await;
    let mut names = names_of(&v);
    names.sort();
    assert_eq!(
        names,
        vec![
            "siloforge_capabilities",
            "siloforge_dashboard",
            "siloforge_list_sites"
        ]
    );
    // A disallowed tool is an MCP error (not a tool result) and never reaches the API.
    let v = c
        .request(
            "tools/call",
            json!({"name": "siloforge_crawl_site", "arguments": {"site": SITE_A}}),
        )
        .await;
    assert!(v["error"].is_object(), "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("policy"),
        "{v}"
    );
    assert!(st.api_reqs().iter().all(|r| !r.path.ends_with("/crawl")));
    // Allowed tools work, and the policy is cached (fetched once for the session so far).
    let (text, is_err, _) = c.call("siloforge_dashboard", json!({"site": SITE_A})).await;
    assert!(!is_err, "{text}");
    assert_eq!(st.policy_fetches(), 1);
    // capabilities reflects the caller's allowed tools.
    let (_, is_err, res) = c.call("siloforge_capabilities", json!({})).await;
    assert!(!is_err);
    let s = &res["structuredContent"];
    assert_eq!(s["policy"]["source"], "server");
    assert_eq!(s["policy"]["tools_available"], 3);
    assert_eq!(s["policy"]["tools_hidden"], ALL_TOOLS - 3);
    let listed: Vec<String> = s["categories"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["tools"].as_array().unwrap().iter())
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(listed.len(), 3, "{listed:?}");
    assert!(s["workflow"].as_array().unwrap().len() >= 7);
}

#[tokio::test]
async fn policy_refreshes_and_keeps_last_known_on_errors() {
    let (st, url) = start_stub().await;
    st.allow(&["siloforge_list_sites"]);
    let mut conf = cfg(&url);
    conf.policy_ttl = Duration::from_millis(1);
    let mut c = Client::start(conf).await;
    assert_eq!(
        names_of(&c.request("tools/list", json!({})).await),
        vec!["siloforge_list_sites"]
    );
    // A transient error keeps the last known policy (no fail-open widening).
    *st.policy.lock() = Some((500, json!({"error": "boom"})));
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(
        names_of(&c.request("tools/list", json!({})).await),
        vec!["siloforge_list_sites"]
    );
    // A changed policy is picked up after the TTL.
    st.allow(&["siloforge_list_sites", "siloforge_list_jobs"]);
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(names_of(&c.request("tools/list", json!({})).await).len(), 2);
    assert!(st.policy_fetches() >= 3);
}

#[tokio::test]
async fn unreachable_api_fails_open_without_the_escape_hatch() {
    // Nothing listens on this port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let mut c = Client::start(cfg(&url)).await;
    let names = names_of(&c.request("tools/list", json!({})).await);
    assert_eq!(names.len(), ALL_TOOLS - 1);
    assert!(!names.iter().any(|n| n == "siloforge_api_request"));
    let (text, is_err, _) = c.call("siloforge_list_sites", json!({})).await;
    assert!(is_err && text.contains("unreachable"), "{text}");
}

#[tokio::test]
async fn http_policy_is_per_bearer() {
    let (st, url) = start_stub().await;
    st.policy_by_auth.lock().insert(
        "Bearer sfk_reader".into(),
        (
            200,
            json!({"http_enabled": true, "scope": "read", "allowed": ["siloforge_list_sites"]}),
        ),
    );
    st.policy_by_auth.lock().insert(
        "Bearer sfk_nohttp".into(),
        (
            200,
            json!({"http_enabled": false, "scope": "admin", "allowed": ["siloforge_list_sites"]}),
        ),
    );
    let mcp = start_mcp_http(&url).await;
    let http = reqwest::Client::new();
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}});
    let (_, v) = http_rpc(&http, &mcp, Some("sfk_reader"), list.clone()).await;
    assert_eq!(names_of(&v), vec!["siloforge_list_sites"]);
    let (_, v) = http_rpc(&http, &mcp, Some("sfk_other"), list.clone()).await;
    assert_eq!(names_of(&v).len(), ALL_TOOLS - 1, "404 -> fail open");
    let (_, v) = http_rpc(&http, &mcp, Some("sfk_nohttp"), list).await;
    assert!(
        names_of(&v).is_empty(),
        "http_enabled=false hides every tool over HTTP"
    );
    let call = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "siloforge_crawl_site", "arguments": {"site": SITE_A}}});
    let (status, v) = http_rpc(&http, &mcp, Some("sfk_reader"), call.clone()).await;
    assert_eq!(status, 200);
    assert!(v["error"].is_object(), "{v}");
    assert!(st.api_reqs().iter().all(|r| !r.path.ends_with("/crawl")));
    let (_, v) = http_rpc(&http, &mcp, Some("sfk_other"), call).await;
    assert_ne!(v["result"]["isError"], true, "{v}");
    let crawl = st
        .api_reqs()
        .into_iter()
        .find(|r| r.path.ends_with("/crawl"))
        .unwrap();
    assert_eq!(crawl.auth, "Bearer sfk_other");
    assert_eq!(
        (crawl.via.as_str(), crawl.tool.as_str()),
        ("mcp", "siloforge_crawl_site")
    );
}

async fn wait_usage(st: &Stub, n: usize) -> Vec<Value> {
    for _ in 0..200 {
        let u = st.usage.lock().clone();
        if u.len() >= n {
            return u;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    st.usage.lock().clone()
}

#[tokio::test]
async fn usage_is_reported_after_each_call() {
    let (st, url) = start_stub().await;
    st.allow_all();
    let mut c = Client::start(cfg(&url)).await;
    let (_, is_err, _) = c.call("siloforge_dashboard", json!({"site": SITE_A})).await;
    assert!(!is_err);
    let (_, is_err, _) = c
        .call("siloforge_dashboard", json!({"site": "nope.com"}))
        .await;
    assert!(is_err);
    let usage = wait_usage(&st, 2).await;
    assert_eq!(usage.len(), 2, "{usage:?}");
    assert_eq!(usage[0]["tool"], "siloforge_dashboard");
    assert_eq!(usage[0]["ok"], true);
    assert!(usage[0]["duration_ms"].is_u64());
    assert_eq!(usage[0]["error"], Value::Null);
    assert_eq!(usage[1]["ok"], false);
    assert!(
        usage[1]["error"]
            .as_str()
            .unwrap()
            .contains("site not found")
    );
    let rec = st
        .reqs
        .lock()
        .iter()
        .find(|r| r.path == USAGE_PATH)
        .cloned()
        .unwrap();
    assert_eq!(rec.auth, "Bearer sfk_testtoken");
    assert_eq!(rec.tool, "siloforge_dashboard");
}

#[tokio::test]
async fn old_server_gets_no_usage_reports() {
    let (st, url) = start_stub().await;
    let mut c = Client::start(cfg(&url)).await;
    let (_, is_err, _) = c.call("siloforge_list_sites", json!({})).await;
    assert!(!is_err);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(st.usage.lock().is_empty());
}

#[tokio::test]
async fn api_request_is_validated_and_default_disabled() {
    let (st, url) = start_stub().await;
    // Default policy (fail-open): the escape hatch is hidden and refused.
    let mut c = Client::start(cfg(&url)).await;
    let v = c
        .request("tools/call", json!({"name": "siloforge_api_request", "arguments": {"method": "GET", "path": "/api/v2/system"}}))
        .await;
    assert!(v["error"].is_object(), "{v}");
    assert!(st.api_reqs().is_empty());

    st.allow_all();
    let mut c = Client::start(cfg(&url)).await;
    for path in [
        "/api/v1/../v1/auth/login",
        "http://127.0.0.1:8600/api/v1/sites",
        "//evil/api/v1/sites",
        "/api/v1/sites?x=1",
        "/api/v1/sites/%2e%2e/x",
        "/api/v3/x",
        "/health",
        "/api/v1/auth/login",
        "/api/v1/sync",
        "/api/v2/backups/b1/download",
    ] {
        let (text, is_err, _) = c
            .call(
                "siloforge_api_request",
                json!({"method": "POST", "path": path, "confirm": true}),
            )
            .await;
        assert!(is_err, "{path} accepted: {text}");
    }
    let (text, is_err, _) = c
        .call("siloforge_api_request", json!({"method": "POST", "path": "/api/v2/tokens", "body": {"name": "x"}, "confirm": true}))
        .await;
    assert!(is_err && text.contains("minting"), "{text}");
    // Non-GET needs confirm.
    let (text, is_err, _) = c
        .call(
            "siloforge_api_request",
            json!({"method": "DELETE", "path": "/api/v2/backups/b1"}),
        )
        .await;
    assert!(is_err && text.contains("confirm: true"), "{text}");
    assert!(st.api_reqs().is_empty(), "{:?}", st.api_reqs());
    // Valid requests go through with query and body.
    let (text, is_err, _) = c
        .call("siloforge_api_request", json!({"method": "get", "path": "/api/v2/jobs", "query": {"status": "running", "limit": 5}}))
        .await;
    assert!(!is_err, "{text}");
    let r = st.last();
    assert_eq!(
        (r.method.as_str(), r.path.as_str()),
        ("GET", "/api/v2/jobs")
    );
    let mut qp: Vec<&str> = r.query.split('&').collect();
    qp.sort_unstable();
    assert_eq!(qp, vec!["limit=5", "status=running"]);
    assert_eq!(r.tool, "siloforge_api_request");
    let (text, is_err, _) = c
        .call("siloforge_api_request", json!({"method": "PATCH", "path": "/api/v1/sites/s1", "body": {"mode": "sitemap"}, "confirm": true}))
        .await;
    assert!(!is_err, "{text}");
    let r = st.last();
    assert_eq!(
        (r.method.as_str(), r.path.as_str()),
        ("PATCH", "/api/v1/sites/s1")
    );
    assert_eq!(r.body, json!({"mode": "sitemap"}));
}

#[tokio::test]
async fn capabilities_and_doctor() {
    let (_st, url) = start_stub().await;
    let mut c = Client::start(cfg(&url)).await;
    let (text, is_err, res) = c.call("siloforge_capabilities", json!({})).await;
    assert!(!is_err, "{text}");
    let s = &res["structuredContent"];
    assert!(
        s["policy"]["source"]
            .as_str()
            .unwrap()
            .starts_with("fail-open")
    );
    assert_eq!(s["policy"]["tools_available"], ALL_TOOLS - 1);
    let cats: Vec<&str> = s["categories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["category"].as_str().unwrap())
        .collect();
    for want in [
        "sites", "runs", "edges", "inject", "reports", "ai", "settings", "tokens", "mcp", "logs",
        "docs",
    ] {
        assert!(cats.contains(&want), "missing category {want}: {cats:?}");
    }
    assert!(
        !cats.contains(&"api"),
        "escape hatch hidden by the default policy"
    );
    assert!(text.contains("crawl") && text.contains("undo"));
    let (text, is_err, res) = c.call("siloforge_doctor", json!({})).await;
    assert!(!is_err, "{text}");
    assert_eq!(res["structuredContent"]["ok"], true, "{text}");
    assert!(res["structuredContent"]["checks"]["health"]["ok"] == true);
}

#[test]
fn catalogue_metadata_for_the_server() {
    let all = sf_mcp::tools::catalogue();
    assert_eq!(all.len(), ALL_TOOLS);
    for n in V1_TOOLS {
        assert!(all.iter().any(|t| t.name == n), "{n}");
    }
    let api = all
        .iter()
        .find(|t| t.name == "siloforge_api_request")
        .unwrap();
    assert!(!api.default_enabled && api.min_scope == "admin" && api.category == "api");
    assert_eq!(all.iter().filter(|t| !t.default_enabled).count(), 1);
    for t in &all {
        assert!(!t.category.is_empty(), "{}", t.name);
        if t.read_only {
            assert!(!t.destructive, "{}", t.name);
        }
    }
    let admin = |n: &str| all.iter().find(|t| t.name == n).unwrap().min_scope == "admin";
    for n in [
        "siloforge_settings_update",
        "siloforge_system_restart",
        "siloforge_list_backups",
        "siloforge_revoke_token",
        "siloforge_mcp_policy_update",
        "siloforge_logs_query",
        "siloforge_create_ai_model",
    ] {
        assert!(admin(n), "{n} must be admin");
    }
    assert!(!admin("siloforge_list_sites") && !admin("siloforge_docs_get"));
}
