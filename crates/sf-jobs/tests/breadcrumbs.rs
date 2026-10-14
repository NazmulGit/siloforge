//! G2-1: link_health detects breadcrumbs from a live GET of each post's own page and writes
//! `posts.has_breadcrumbs` / `has_breadcrumb_schema` / `breadcrumb_report`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use common::*;
use serde_json::{Value, json};
use sf_core::types::JobKind;

const SCHEMA: &str = r#"<script type="application/ld+json">{"@context":"https://schema.org","@graph":[
  {"@type":"WebPage","name":"x"},
  {"@type":"BreadcrumbList","itemListElement":[
    {"@type":"ListItem","position":1,"name":"Home","item":"/"},
    {"@type":"ListItem","position":2,"name":"Here"}]}]}</script>"#;
const TRAIL: &str = r#"<nav class="yoast-breadcrumbs"><a href="/">Home</a> &raquo; Here</nav>"#;

fn page(head: &str, body: &str) -> Response {
    Html(format!(
        "<html><head><title>t</title>{head}</head><body><nav aria-label=\"Main menu\"><a href=\"/\">Home</a></nav>{body}<p>text</p></body></html>"
    ))
    .into_response()
}

async fn start_site() -> (String, Arc<AtomicBool>) {
    let down = Arc::new(AtomicBool::new(false));
    let app = Router::new()
        .route("/both/", get(|| async { page(SCHEMA, TRAIL) }))
        .route("/visible/", get(|| async { page("", TRAIL) }))
        .route("/schema/", get(|| async { page(SCHEMA, "") }))
        .route("/none/", get(|| async { page("", "") }))
        .route(
            "/flaky/",
            get(|State(d): State<Arc<AtomicBool>>| async move {
                if d.load(Ordering::SeqCst) {
                    StatusCode::NOT_FOUND.into_response()
                } else {
                    page(SCHEMA, TRAIL)
                }
            }),
        )
        .with_state(down.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base, down)
}

async fn crumbs(e: &Env) -> Vec<(String, i64, i64, Value)> {
    q_rows(
        &e.db,
        "SELECT url, has_breadcrumbs, has_breadcrumb_schema, breadcrumb_report FROM posts ORDER BY url",
        vec![],
    )
    .await
    .into_iter()
    .map(|r| {
        let path = s(&r[0]);
        let path = path.rsplit('/').nth(1).unwrap_or_default().to_string();
        (
            path,
            r[1].as_i64().unwrap(),
            r[2].as_i64().unwrap(),
            serde_json::from_str(&s(&r[3])).unwrap(),
        )
    })
    .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn link_health_writes_breadcrumb_columns() {
    let (base, down) = start_site().await;
    let e = env("breadcrumbs");
    let site = add_site(&e, &base, false, json!({})).await;
    for slug in ["both", "visible", "schema", "none", "flaky"] {
        exec(
            &e.db,
            "INSERT INTO posts(id, site_id, url, title, content_html) VALUES(?1, ?2, ?3, ?1, ?4)",
            vec![
                format!("p-{slug}"),
                site.clone(),
                format!("{base}/{slug}/"),
                format!("<p>See <a href=\"{base}/both/\">both</a>.</p>"),
            ],
        )
        .await;
    }

    let st = run_job(
        &e,
        JobKind::LinkHealth,
        &site,
        json!({"internal_only": true}),
    )
    .await;
    assert_eq!(st["breadcrumbs"]["checked"], 5, "{st}");
    assert_eq!(st["breadcrumbs"]["failed"], 0, "{st}");
    assert_eq!(st["breadcrumbs"]["with_visible"], 3, "{st}");
    assert_eq!(st["breadcrumbs"]["with_schema"], 3, "{st}");

    let rows = crumbs(&e).await;
    let get = |p: &str| rows.iter().find(|r| r.0 == p).cloned().unwrap();
    let (_, vis, sch, rep) = get("both");
    assert_eq!((vis, sch), (1, 1));
    assert_eq!(rep["hasVisible"], true);
    assert_eq!(rep["hasSchema"], true);
    assert_eq!(rep["mismatch"], false);
    assert_eq!(rep["schemaSource"], "json-ld");
    assert_eq!(rep["count"], 2);
    assert_eq!(rep["crumbs"][0]["name"], "Home");
    assert_eq!(rep["crumbs"][1]["url"], Value::Null);
    let (_, vis, sch, rep) = get("visible");
    assert_eq!((vis, sch), (1, 0));
    assert_eq!(rep["mismatch"], true);
    let (_, vis, sch, _) = get("schema");
    assert_eq!((vis, sch), (0, 1));
    let (_, vis, sch, rep) = get("none");
    assert_eq!((vis, sch), (0, 0), "main-menu nav is not a trail");
    assert_eq!(rep["hasVisible"], false);
    assert_eq!(get("flaky").1, 1);

    // second run: the page now 404s -> previous values are kept, others refreshed
    down.store(true, Ordering::SeqCst);
    let st = run_job(&e, JobKind::LinkHealth, &site, json!({})).await;
    assert_eq!(st["breadcrumbs"]["checked"], 4, "{st}");
    assert_eq!(st["breadcrumbs"]["failed"], 1, "{st}");
    let (_, vis, sch, rep) = crumbs(&e)
        .await
        .into_iter()
        .find(|r| r.0 == "flaky")
        .unwrap();
    assert_eq!((vis, sch), (1, 1));
    assert_eq!(rep["hasSchema"], true);
}
