//! Integration tests against local axum stubs. Never touches real sites.
#![allow(clippy::unwrap_used)] // test helpers outside #[test] fns

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use sf_crawl::html::site_host_set;
use sf_crawl::{
    CrawlError, FailureKind, FetchRequest, HttpClient, LinkCheckInput, LinkCheckOptions,
    SitemapCrawler, SitemapOptions, TraceOptions, VerifyOptions, WpCrawler, WpCredentials, WpSite,
    WpWriter, check_links, extract_links, verify_live,
};
use tokio::sync::mpsc;
use url::Url;

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// Client allowed to reach the loopback stub (and nothing else private).
fn local_client() -> HttpClient {
    HttpClient::builder()
        .allow_host("127.0.0.1")
        .politeness_delay(Duration::ZERO)
        .base_backoff(Duration::from_millis(20))
        .build()
        .unwrap()
}

fn u(s: &str) -> Url {
    Url::parse(s).unwrap()
}

// ---------------------------------------------------------------- WordPress crawl

#[derive(Default)]
struct WpState {
    auth_headers: Mutex<Vec<String>>,
    queries: Mutex<Vec<HashMap<String, String>>>,
}

fn wp_item(id: u64) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "link": format!("https://ex.com/p{id}/"),
        "slug": format!("p{id}"),
        "title": {"rendered": format!("Post &#8211; {id}"), "raw": format!("Post - {id}")},
        "content": {"rendered": format!("<p>Body {id}</p>"), "raw": format!("<!-- wp:paragraph --><p>Body {id}</p><!-- /wp:paragraph -->")},
        "excerpt": {"rendered": "<p>Ex</p>"},
        "type": "post", "status": "publish",
        "date_gmt": "2026-01-01T00:00:00", "modified_gmt": format!("2026-02-0{id}T00:00:00"),
        "author": 1, "categories": [3], "tags": [],
        "yoast_head_json": {"title": "Y title", "description": "Y desc"}
    })
}

async fn wp_posts(
    State(st): State<Arc<WpState>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    st.auth_headers.lock().unwrap().push(auth.clone());
    st.queries.lock().unwrap().push(q.clone());
    if q.get("context").map(String::as_str) == Some("edit") && auth.is_empty() {
        return (
            StatusCode::UNAUTHORIZED,
            "{\"code\":\"rest_forbidden_context\"}",
        )
            .into_response();
    }
    let page: u64 = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
    let items: Vec<serde_json::Value> = match page {
        1 => vec![wp_item(1), wp_item(2)],
        2 => vec![wp_item(3), serde_json::json!({"id": "broken"})],
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "{\"code\":\"rest_post_invalid_page_number\"}",
            )
                .into_response();
        }
    };
    let mut h = HeaderMap::new();
    h.insert("x-wp-totalpages", "2".parse().unwrap());
    h.insert("x-wp-total", "4".parse().unwrap());
    (h, axum::Json(items)).into_response()
}

#[tokio::test]
async fn wp_pagination_with_context_edit_and_basic_auth() {
    let st = Arc::new(WpState::default());
    let app = Router::new()
        .route("/wp-json/wp/v2/posts", get(wp_posts))
        .route(
            "/wp-json/wp/v2/pages",
            get(|| async { (StatusCode::NOT_FOUND, "{\"code\":\"rest_no_route\"}") }),
        )
        .with_state(st.clone());
    let base = serve(app).await;
    let mut site = WpSite::new(&base)
        .unwrap()
        .with_credentials(WpCredentials::new("admin", "abcd efgh ijkl"));
    site.per_page = 2;
    let crawler = WpCrawler::new(local_client(), site);
    let (tx, mut rx) = mpsc::channel(4);
    let handle = tokio::spawn(async move { crawler.crawl(tx).await });
    let mut pages = Vec::new();
    while let Some(p) = rx.recv().await {
        pages.push(p);
    }
    let stats = handle.await.unwrap().unwrap();
    assert_eq!(stats.posts, 3);
    assert_eq!(stats.skipped_items, 1);
    assert_eq!(stats.pages_fetched, 2);
    assert_eq!(stats.types_absent, vec!["pages".to_string()]);
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].total_pages, Some(2));
    let p1 = &pages[0].posts[0];
    assert_eq!(p1.wp_post_id, Some(1));
    assert_eq!(p1.title, "Post \u{2013} 1");
    assert!(
        p1.content_raw
            .as_deref()
            .unwrap()
            .starts_with("<!-- wp:paragraph -->")
    );
    assert_eq!(p1.content_rendered, "<p>Body 1</p>");
    assert_eq!(p1.builder_hint, "gutenberg");
    assert_eq!(p1.seo.description.as_deref(), Some("Y desc"));
    assert_eq!(p1.modified_gmt.as_deref(), Some("2026-02-01T00:00:00"));

    // Basic base64("admin:abcd efgh ijkl")
    let auths = st.auth_headers.lock().unwrap().clone();
    assert!(
        auths
            .iter()
            .all(|a| a == "Basic YWRtaW46YWJjZCBlZmdoIGlqa2w="),
        "{auths:?}"
    );
    let qs = st.queries.lock().unwrap().clone();
    assert!(
        qs.iter()
            .all(|q| q.get("context").map(String::as_str) == Some("edit"))
    );
    assert!(
        qs.iter()
            .all(|q| q.get("per_page").map(String::as_str) == Some("2"))
    );
    assert!(qs[0].get("_fields").unwrap().contains("content"));
}

#[tokio::test]
async fn wp_without_credentials_uses_view_context() {
    let st = Arc::new(WpState::default());
    let app = Router::new()
        .route("/wp-json/wp/v2/posts", get(wp_posts))
        .with_state(st.clone());
    let base = serve(app).await;
    let site = WpSite::new(&base)
        .unwrap()
        .with_post_types(vec![sf_crawl::WpPostType::new("posts", "post")]);
    let (tx, mut rx) = mpsc::channel(4);
    let crawler = WpCrawler::new(local_client(), site);
    let h = tokio::spawn(async move { crawler.crawl(tx).await });
    let mut n = 0;
    while let Some(p) = rx.recv().await {
        n += p.posts.len();
        // rendered only would be absent in real WP view context; stub always sends raw
    }
    assert_eq!(n, 3);
    h.await.unwrap().unwrap();
    let qs = st.queries.lock().unwrap().clone();
    assert!(qs.iter().all(|q| !q.contains_key("context")));
    assert!(st.auth_headers.lock().unwrap().iter().all(String::is_empty));
}

#[tokio::test]
async fn wp_malformed_payloads_are_decode_errors() {
    let app = Router::new()
        .route(
            "/wp-json/wp/v2/posts",
            get(|| async { ([(header::CONTENT_TYPE, "application/json")], "[{\"id\":1,") }),
        )
        .route(
            "/wp-json/wp/v2/pages",
            get(|| async { axum::Json(serde_json::json!({"not": "an array"})) }),
        );
    let base = serve(app).await;
    let site = WpSite::new(&base).unwrap();
    let crawler = WpCrawler::new(local_client(), site);
    let posts = sf_crawl::WpPostType::new("posts", "post");
    let pages = sf_crawl::WpPostType::new("pages", "page");
    assert!(matches!(
        crawler.fetch_page(&posts, 1).await,
        Err(CrawlError::Decode(_))
    ));
    assert!(matches!(
        crawler.fetch_page(&pages, 1).await,
        Err(CrawlError::Decode(_))
    ));
}

#[tokio::test]
async fn wp_receiver_drop_cancels() {
    let st = Arc::new(WpState::default());
    let app = Router::new()
        .route("/wp-json/wp/v2/posts", get(wp_posts))
        .with_state(st);
    let base = serve(app).await;
    let mut site = WpSite::new(&base).unwrap();
    site.per_page = 2;
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let r = WpCrawler::new(local_client(), site).crawl(tx).await;
    assert!(matches!(r, Err(CrawlError::Cancelled)));
}

// ---------------------------------------------------------------- WordPress writer

struct PostRec {
    raw: String,
    modified: String,
    rev: u32,
}

#[derive(Default)]
struct WriterState {
    posts: Mutex<HashMap<(String, u64), PostRec>>,
    writes: AtomicUsize,
}

async fn wp_one(
    State(st): State<Arc<WriterState>>,
    Path((kind, id)): Path<(String, u64)>,
    method: Method,
    headers: HeaderMap,
    body: String,
) -> Response {
    if headers.get(header::AUTHORIZATION).is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut posts = st.posts.lock().unwrap();
    let Some(p) = posts.get_mut(&(kind.clone(), id)) else {
        return (StatusCode::NOT_FOUND, "{\"code\":\"rest_post_invalid_id\"}").into_response();
    };
    if method == Method::POST {
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        p.raw = v["content"].as_str().unwrap().to_string();
        p.rev += 1;
        p.modified = format!("2026-03-01T00:00:{:02}", p.rev);
        st.writes.fetch_add(1, Ordering::SeqCst);
    }
    axum::Json(serde_json::json!({
        "id": id, "link": format!("https://ex.com/{kind}/{id}/"), "status": "publish",
        "modified_gmt": p.modified,
        "content": {"raw": p.raw, "rendered": format!("<div>{}</div>", p.raw)}
    }))
    .into_response()
}

#[tokio::test]
async fn writer_optimistic_concurrency_and_endpoints() {
    let st = Arc::new(WriterState::default());
    {
        let mut m = st.posts.lock().unwrap();
        for (k, id) in [("posts", 5u64), ("pages", 9u64)] {
            m.insert(
                (k.to_string(), id),
                PostRec {
                    raw: "<!-- wp:paragraph --><p>old</p><!-- /wp:paragraph -->".into(),
                    modified: "2026-01-01T00:00:00".into(),
                    rev: 0,
                },
            );
        }
    }
    let app = Router::new()
        .route("/wp-json/wp/v2/{kind}/{id}", any(wp_one))
        .with_state(st.clone());
    let base = serve(app).await;
    let site = WpSite::new(&base)
        .unwrap()
        .with_credentials(WpCredentials::new("u", "p"));
    let w = WpWriter::new(local_client(), site.clone()).unwrap();

    let raw = w.get_post_raw(5, "post").await.unwrap();
    assert_eq!(raw.rest_base, "posts");
    assert_eq!(raw.modified_gmt, "2026-01-01T00:00:00");
    let new_raw = raw.raw.replace("old", "new <a href=\"/t/\">t</a>");
    let out = w
        .update_post_content(5, "post", &new_raw, &raw.modified_gmt)
        .await
        .unwrap();
    assert_eq!(out.modified_gmt, "2026-03-01T00:00:01");
    assert!(out.rendered.contains("new <a href"));

    // Stale read: refused, nothing written.
    let before = st.writes.load(Ordering::SeqCst);
    let err = w
        .update_post_content(5, "post", "clobber", &raw.modified_gmt)
        .await
        .unwrap_err();
    assert!(
        matches!(err, CrawlError::Conflict { post_id: 5, .. }),
        "{err:?}"
    );
    assert!(!err.is_retryable());
    assert_eq!(st.writes.load(Ordering::SeqCst), before);

    // Page id requested as "post" falls back to the pages endpoint.
    let pg = w.get_post_raw(9, "post").await.unwrap();
    assert_eq!(pg.rest_base, "pages");
    let out = w
        .update_post_content(9, "page", "x", &pg.modified_gmt)
        .await
        .unwrap();
    assert_eq!(out.link, "https://ex.com/pages/9/");

    // No credentials -> config error.
    assert!(matches!(
        WpWriter::new(local_client(), WpSite::new(&base).unwrap()),
        Err(CrawlError::Config(_))
    ));
}

// ---------------------------------------------------------------- SSRF / aliases / backoff

#[tokio::test]
async fn redirect_to_private_ip_is_blocked() {
    let app = Router::new()
        .route(
            "/to-private",
            get(|| async {
                (
                    StatusCode::FOUND,
                    [(header::LOCATION, "http://10.255.255.1/admin")],
                )
            }),
        )
        .route(
            "/to-metadata",
            get(|| async {
                (
                    StatusCode::MOVED_PERMANENTLY,
                    [(header::LOCATION, "http://169.254.169.254/latest/meta-data/")],
                )
            }),
        )
        .route("/ok", get(|| async { "fine" }));
    let base = serve(app).await;
    let port = Url::parse(&base).unwrap().port().unwrap();
    let app2 = Router::new().route(
        "/to-localhost",
        get(move || async move {
            (
                StatusCode::FOUND,
                [(header::LOCATION, format!("http://localhost:{port}/ok"))],
            )
        }),
    );
    let base2 = serve(app2).await;
    let c = local_client();
    for path in ["/to-private", "/to-metadata"] {
        let r = c
            .fetch(FetchRequest::get(u(&format!("{base}{path}"))))
            .await;
        assert!(
            matches!(r, Err(CrawlError::Blocked { .. })),
            "{path}: {r:?}"
        );
    }
    // Hostname that resolves to loopback is blocked by the connect-time resolver.
    let r = c
        .fetch(FetchRequest::get(u(&format!("{base2}/to-localhost"))))
        .await;
    assert!(matches!(r, Err(CrawlError::Blocked { .. })), "{r:?}");
    // Direct request to a non-allowlisted private literal.
    let r = c.fetch(FetchRequest::get(u("http://192.168.0.1/"))).await;
    assert!(matches!(r, Err(CrawlError::Blocked { .. })));
    // Default client (no allowlist) refuses loopback entirely.
    let strict = HttpClient::builder().build().unwrap();
    assert!(matches!(
        strict.get(&u(&format!("{base}/ok"))).await,
        Err(CrawlError::Blocked { .. })
    ));
    // Allowed target works.
    assert_eq!(
        c.get(&u(&format!("{base}/ok"))).await.unwrap().text(),
        "fine"
    );
}

#[tokio::test]
async fn host_alias_rewrites_connect_but_keeps_host_header() {
    let app = Router::new().route(
        "/host",
        get(|headers: HeaderMap| async move {
            headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        }),
    );
    let base = serve(app).await;
    let port = Url::parse(&base).unwrap().port().unwrap();
    // The alias applies only because wp.docker.test is a configured site host that is allowed
    // to be private.
    let c = HttpClient::builder()
        .alias("wp.docker.test", "127.0.0.1")
        .allow_host("wp.docker.test")
        .politeness_delay(Duration::ZERO)
        .build()
        .unwrap();
    let r = c
        .get(&u(&format!("http://wp.docker.test:{port}/host")))
        .await
        .unwrap();
    assert_eq!(r.text(), format!("wp.docker.test:{port}"));
}

/// H-02 regression: an alias (default `site01.example -> 127.0.0.1`) must not turn an
/// arbitrary link or redirect target into a trusted loopback destination.
#[tokio::test]
async fn host_alias_is_not_an_ssrf_bypass() {
    let app = Router::new()
        .route("/ok", get(|| async { "secret" }))
        .route(
            "/hop",
            get(|Query(q): Query<HashMap<String, String>>| async move {
                let to = q.get("to").cloned().unwrap_or_default();
                (StatusCode::FOUND, [(header::LOCATION, to)]).into_response()
            }),
        );
    let base = serve(app).await;
    let port = Url::parse(&base).unwrap().port().unwrap();
    let target = format!("http://site01.example:{port}/ok");

    // Not a site host: the alias is ignored, and the name does not resolve to anything allowed.
    let c = HttpClient::builder()
        .alias("site01.example", "127.0.0.1")
        .site_host("site.example")
        .politeness_delay(Duration::ZERO)
        .build()
        .unwrap();
    assert!(c.get(&u(&target)).await.is_err());

    // Site host, but private hosts not allowed for the site: the aliased loopback is blocked.
    let c = HttpClient::builder()
        .alias("site01.example", "127.0.0.1")
        .site_host("site01.example")
        .politeness_delay(Duration::ZERO)
        .build()
        .unwrap();
    assert!(matches!(
        c.get(&u(&target)).await,
        Err(CrawlError::Blocked { .. })
    ));

    // Site host AND private allowed for that site: works.
    let c = HttpClient::builder()
        .alias("site01.example", "127.0.0.1")
        .site_host("site01.example")
        .allow_host("site01.example")
        .allow_host("127.0.0.1")
        .politeness_delay(Duration::ZERO)
        .build()
        .unwrap();
    assert_eq!(c.get(&u(&target)).await.unwrap().text(), "secret");

    // A redirect from a site page to an aliased host that is not a site host is refused.
    let hop = format!("{base}/hop?to=http://site01.example:{port}/ok");
    let c = HttpClient::builder()
        .alias("site01.example", "127.0.0.1")
        .site_host("127.0.0.1")
        .allow_host("127.0.0.1")
        .politeness_delay(Duration::ZERO)
        .build()
        .unwrap();
    assert!(c.get(&u(&hop)).await.is_err());
}

#[tokio::test]
async fn backoff_on_429_honours_retry_after() {
    let hits = Arc::new(AtomicUsize::new(0));
    let h2 = hits.clone();
    let app = Router::new()
        .route(
            "/limited",
            get(move || {
                let h = h2.clone();
                async move {
                    if h.fetch_add(1, Ordering::SeqCst) == 0 {
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            [(header::RETRY_AFTER, "1")],
                            "slow down",
                        )
                            .into_response()
                    } else {
                        "ok".into_response()
                    }
                }
            }),
        )
        .route(
            "/always",
            get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "busy") }),
        );
    let base = serve(app).await;
    let c = local_client();
    let t0 = Instant::now();
    let r = c.get(&u(&format!("{base}/limited"))).await.unwrap();
    assert_eq!(r.text(), "ok");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert!(
        t0.elapsed() >= Duration::from_millis(900),
        "{:?}",
        t0.elapsed()
    );

    // Retries exhausted: the 503 response comes back; as an error it is retryable.
    let r = c
        .fetch(FetchRequest::get(u(&format!("{base}/always"))))
        .await
        .unwrap();
    assert_eq!(r.status, 503);
    let e = r.error_for_status().unwrap_err();
    assert!(e.is_retryable());
}

// ---------------------------------------------------------------- hostile inputs

#[tokio::test]
async fn huge_bodies_are_capped() {
    let app = Router::new().route("/huge", get(|| async { "x".repeat(3 * 1024 * 1024) }));
    let base = serve(app).await;
    let c = local_client();
    let r = c
        .fetch(FetchRequest::get(u(&format!("{base}/huge"))).max_body(1024 * 1024))
        .await;
    assert!(matches!(r, Err(CrawlError::BodyTooLarge { .. })));
    let r = c
        .fetch(
            FetchRequest::get(u(&format!("{base}/huge")))
                .max_body(1000)
                .truncate(),
        )
        .await
        .unwrap();
    assert!(r.truncated);
    assert_eq!(r.body.len(), 1000);
}

// ---------------------------------------------------------------- link health

fn health_app(hits: Arc<AtomicUsize>) -> Router {
    Router::new()
        .route("/ok", get(|| async { "ok" }))
        .route("/nf", get(|| async { StatusCode::NOT_FOUND }))
        .route("/gone", get(|| async { StatusCode::GONE }))
        .route("/forbidden", get(|| async { StatusCode::FORBIDDEN }))
        .route("/err", get(|| async { StatusCode::BAD_GATEWAY }))
        .route(
            "/loop-a",
            get(|| async {
                (
                    StatusCode::MOVED_PERMANENTLY,
                    [(header::LOCATION, "/loop-b")],
                )
            }),
        )
        .route(
            "/loop-b",
            get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/loop-a#x")]) }),
        )
        .route(
            "/hop/{n}",
            get(|Path(n): Path<u32>| async move {
                (
                    StatusCode::MOVED_PERMANENTLY,
                    [(header::LOCATION, format!("/hop/{}", n + 1))],
                )
            }),
        )
        .route(
            "/temp",
            get(|| async { (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, "/ok")]) }),
        )
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                "late"
            }),
        )
        .route(
            "/nohead",
            any(move |m: Method| {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    if m == Method::HEAD {
                        StatusCode::METHOD_NOT_ALLOWED.into_response()
                    } else {
                        "ok".into_response()
                    }
                }
            }),
        )
}

#[tokio::test]
async fn link_health_taxonomy() {
    let hits = Arc::new(AtomicUsize::new(0));
    let base = serve(health_app(hits.clone())).await;
    let client = HttpClient::builder()
        .allow_host("127.0.0.1")
        .politeness_delay(Duration::ZERO)
        .timeout(Duration::from_millis(400))
        .max_retries(0)
        .build()
        .unwrap();
    let hosts = site_host_set([base.as_str()]);
    let html = format!(
        r#"<p><a href="/ok">ok</a><a href="/ok">ok</a><a href="{base}/nf">nf</a><a href="/gone">g</a>
        <a href="/forbidden">f</a><a href="/err">e</a><a href="/loop-a">l</a><a href="/hop/0">h</a>
        <a href="/temp" rel="nofollow">t</a><a href="/slow">s</a><a href="/nohead">nh</a>
        <a href="/nohead#frag">nh2</a><a href="http://does-not-exist.invalid/">dns</a></p>"#
    );
    let links = extract_links(&html, &format!("{base}/post/"), &hosts);
    assert_eq!(links.len(), 13);
    let mut inputs: Vec<LinkCheckInput> = links
        .into_iter()
        .map(|l| LinkCheckInput {
            source_post_id: Some("p1".into()),
            link: l,
        })
        .collect();
    // Same target from a second post: one request, two rows.
    inputs.push(LinkCheckInput {
        source_post_id: Some("p2".into()),
        link: inputs[10].link.clone(),
    });
    let opts = LinkCheckOptions {
        concurrency: 4,
        interval_cap: 100,
        interval: Duration::ZERO,
        trace: TraceOptions {
            max_hops: 3,
            ..Default::default()
        },
        ..Default::default()
    };
    let report = check_links(&client, &inputs, &opts, None).await;
    let by = |href: &str| {
        report
            .rows
            .iter()
            .find(|r| r.href == href && r.source_post_id.as_deref() == Some("p1"))
            .unwrap()
            .clone()
    };
    // duplicate (post, href, anchor) collapsed
    assert_eq!(report.rows.len(), 13);
    assert_eq!(by("/ok").failure_kind, FailureKind::None);
    assert!(by("/ok").ok);
    let nf = by(&format!("{base}/nf"));
    assert_eq!(
        (nf.failure_kind, nf.is_broken, nf.status_code),
        (FailureKind::NotFound, true, Some(404))
    );
    assert_eq!(by("/gone").failure_kind, FailureKind::Gone);
    let f = by("/forbidden");
    assert_eq!(
        (f.failure_kind, f.is_broken, f.is_unverified),
        (FailureKind::Blocked, false, true)
    );
    assert_eq!(by("/err").failure_kind, FailureKind::ServerError);
    let l = by("/loop-a");
    assert_eq!(
        (l.failure_kind, l.has_redirect_loop, l.is_broken),
        (FailureKind::Loop, true, true)
    );
    assert!(l.has_temp_redirect);
    assert_eq!(l.redirect_chain.len(), 2);
    let h = by("/hop/0");
    assert_eq!(
        (h.failure_kind, h.is_broken, h.is_unverified),
        (FailureKind::HopCap, false, true)
    );
    assert_eq!(h.redirect_chain.len(), 4);
    let t = by("/temp");
    assert!(t.ok && t.has_temp_redirect && t.is_nofollow);
    assert_eq!(t.redirect_hops, 1);
    assert_eq!(
        t.resolved_url.as_deref(),
        Some(format!("{base}/ok").as_str())
    );
    assert_eq!(t.redirect_chain[0].status, 307);
    assert_eq!(t.redirect_chain[1].order, 1);
    let s = by("/slow");
    assert_eq!(
        (s.failure_kind, s.is_unverified, s.is_broken),
        (FailureKind::Timeout, true, false)
    );
    let nh = by("/nohead");
    assert!(nh.ok, "HEAD 405 falls back to GET: {nh:?}");
    let d = by("http://does-not-exist.invalid/");
    assert_eq!((d.failure_kind, d.is_broken), (FailureKind::Dns, true));
    // /nohead and /nohead#frag dedupe to one target: one HEAD + one GET
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(report.stats.timeouts, 1);
    assert!(report.stats.broken >= 4);
    assert_eq!(report.stats.nofollow, 1);
    assert!(report.rows.iter().all(|r| !r.skipped));
    assert!(by("/loop-a").redirect_chain_json().starts_with("[{\"url\""));
}

#[tokio::test]
async fn link_health_internal_only_and_cap() {
    let base = serve(health_app(Arc::new(AtomicUsize::new(0)))).await;
    let client = local_client();
    let hosts = site_host_set([base.as_str()]);
    let links = extract_links(
        r#"<a href="/ok">a</a><a href="/gone">b</a><a href="https://external.example/">x</a>"#,
        &format!("{base}/"),
        &hosts,
    );
    let inputs: Vec<LinkCheckInput> = links
        .into_iter()
        .map(|l| LinkCheckInput {
            source_post_id: None,
            link: l,
        })
        .collect();
    let opts = LinkCheckOptions {
        internal_only: true,
        max_urls: 1,
        interval: Duration::ZERO,
        ..Default::default()
    };
    let rep = check_links(&client, &inputs, &opts, None).await;
    assert_eq!(rep.rows.len(), 3);
    assert_eq!(rep.stats.distinct_urls, 1);
    assert_eq!(rep.stats.skipped, 2);
    assert!(rep.rows[2].skipped && rep.rows[2].scope == "external");
}

// ---------------------------------------------------------------- sitemap

fn gz(data: &str) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(data.as_bytes()).unwrap();
    e.finish().unwrap()
}

#[tokio::test]
async fn sitemap_index_recursion_robots_and_extraction() {
    let base_cell: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let b = base_cell.clone();
    let app = Router::new().route(
        "/{*path}",
        get(move |Path(path): Path<String>| {
            let base = b.lock().unwrap().clone();
            async move {
                let xml = |s: String| ([(header::CONTENT_TYPE, "application/xml")], s).into_response();
                match path.as_str() {
                    "robots.txt" => format!(
                        "User-agent: *\nDisallow: /private/\nSitemap: {base}/idx.xml\n"
                    )
                    .into_response(),
                    "idx.xml" => xml(format!(
                        "<sitemapindex><sitemap><loc>{base}/a.xml</loc></sitemap><sitemap><loc>{base}/nested.xml</loc></sitemap><sitemap><loc>{base}/broken.xml</loc></sitemap><sitemap><loc>http://site01.example:1/evil.xml</loc></sitemap></sitemapindex>"
                    )),
                    "nested.xml" => xml(format!(
                        "<sitemapindex><sitemap><loc>{base}/b.xml.gz</loc></sitemap><sitemap><loc>{base}/idx.xml</loc></sitemap></sitemapindex>"
                    )),
                    "a.xml" => xml(format!(
                        "<urlset><url><loc>{base}/p1/</loc></url><url><loc>{base}/private/x</loc></url><url><loc>https://offsite.example/z</loc></url></urlset>"
                    )),
                    "b.xml.gz" => (
                        [(header::CONTENT_TYPE, "application/x-gzip")],
                        Body::from(gz(&format!(
                            "<urlset><url><loc>{base}/p2/</loc></url><url><loc>{base}/p1/</loc></url><url><loc>{base}/file.pdf</loc></url></urlset>"
                        ))),
                    )
                        .into_response(),
                    "broken.xml" => xml("<urlset><url></loc></urlset>".into()),
                    "p1/" => axum::response::Html(
                        "<html><head><title>Page One</title></head><body><nav>menu</nav><article><p>Hello one</p></article></body></html>",
                    )
                    .into_response(),
                    "p2/" => axum::response::Html(
                        "<html><body><h1>Two</h1><div class=\"elementor-widget\"><p>Hi</p></div><footer>f</footer></body></html>",
                    )
                    .into_response(),
                    "file.pdf" => ([(header::CONTENT_TYPE, "application/pdf")], "%PDF").into_response(),
                    _ => StatusCode::NOT_FOUND.into_response(),
                }
            }
        }),
    );
    let base = serve(app).await;
    *base_cell.lock().unwrap() = base.clone();
    let crawler = SitemapCrawler::new(local_client(), &base, SitemapOptions::default()).unwrap();
    let (tx, mut rx) = mpsc::channel(4);
    let h = tokio::spawn(async move { crawler.crawl(tx).await });
    let mut posts = Vec::new();
    while let Some(batch) = rx.recv().await {
        posts.extend(batch);
    }
    let stats = h.await.unwrap().unwrap();
    assert_eq!(stats.discovered, 3, "{stats:?}");
    assert_eq!(stats.crawled, 2);
    assert_eq!(stats.skipped_non_html, 1);
    assert_eq!(stats.disallowed, 1);
    assert!(stats.errors.iter().any(|e| e.contains("broken.xml")));
    // H-02: an off-site sitemap-index child is never fetched.
    assert!(
        !stats
            .errors
            .iter()
            .any(|e| e.contains("site01.example")),
        "{stats:?}"
    );
    assert_eq!(posts[0].title, "Page One");
    assert_eq!(posts[0].content_rendered, "<p>Hello one</p>");
    assert_eq!(posts[0].slug.as_deref(), Some("p1"));
    assert_eq!(posts[1].title, "Two");
    assert_eq!(posts[1].builder_hint, "elementor");
    assert!(!posts[1].content_rendered.contains("<footer"));
    assert!(
        posts
            .iter()
            .all(|p| p.wp_post_id.is_none() && p.content_raw.is_none())
    );
}

// ---------------------------------------------------------------- verify

#[tokio::test]
async fn verify_live_finds_injected_link() {
    let app = Router::new()
        .route(
            "/post/",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-cache");
                axum::response::Html(
                    "<html><body><div class=\"entry-content\"><p>Read <a href=\"/target/\">best bike locks</a>.</p></div></body></html>",
                )
            }),
        )
        .route("/missing/", get(|| async { StatusCode::NOT_FOUND }));
    let base = serve(app).await;
    let c = local_client();
    let opts = VerifyOptions {
        backoff: Duration::from_millis(10),
        ..Default::default()
    };
    let r = verify_live(
        &c,
        &format!("{base}/post/"),
        &format!("{base}/target"),
        "Best Bike Locks",
        &opts,
    )
    .await
    .unwrap();
    assert!(r.found(), "{r:?}");
    assert_eq!(r.presence.region, ".entry-content");
    let r = verify_live(&c, &format!("{base}/post/"), "/other/", "x", &opts)
        .await
        .unwrap();
    assert!(!r.found());
    assert_eq!(r.attempts, 1);
    let r = verify_live(&c, &format!("{base}/missing/"), "/t/", "x", &opts)
        .await
        .unwrap();
    assert_eq!(r.status, Some(404));
    assert!(!r.found());
}
