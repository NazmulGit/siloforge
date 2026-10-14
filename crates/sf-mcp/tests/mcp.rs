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
}

#[derive(Default)]
struct Stub {
    reqs: Mutex<Vec<Rec>>,
    logins: AtomicUsize,
    reject_next: AtomicUsize,
    polls: Mutex<HashMap<String, usize>>,
}

impl Stub {
    fn api_reqs(&self) -> Vec<Rec> {
        self.reqs
            .lock()
            .iter()
            .filter(|r| r.path != "/api/v1/sites" || r.method != "GET")
            .cloned()
            .collect()
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
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
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
    });
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
    assert_eq!(names.len(), 28);
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
        Some(28),
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
