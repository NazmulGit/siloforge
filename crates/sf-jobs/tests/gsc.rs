//! GSC pull (G2-2) against a local stub: token endpoint (RS256 assertion checked) + paged
//! searchAnalytics. Never calls Google.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use common::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sf_core::secrets::SiteSecrets;
use sf_core::types::JobKind;
use sf_jobs::pipelines::common::load_site;
use sf_jobs::pipelines::gsc::{GscClient, GscLimits, SCOPE};
use sf_jobs::{Job, NewJob};

// THROWAWAY TEST-ONLY RSA KEY (generated for this test file; grants access to nothing).
const TEST_PRIVATE_KEY_PEM: &str = r"-----BEGIN PRIVATE KEY-----
MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQC3UhGu1k6zJrle
MvcthPDsps5mKbak3I1YQFQm+zvBZ72q3qCsXP4HldR2hWtHPQl1l5ed3NFNxi6a
NwyTSoIITnq25t3EKSWUrD5MbmzsSxLOTk6D6nzK9twZUCgLZjCq+cYoQzu383hl
yVUPYz3tPgtFFlDJMQK8li7fDwkIMefNIOyRnuy3la3I83kdKqXcriNhDN07UE7r
o03wP+3+TuwyXQ9z8mDT7VHFgd+LVa8adTlzfgyRZ+OGmrM2lo0xYGJ0zh2N8mz+
NMlpPQAgpwFglb9OK+zbi87mc8KawDKwAbg918S0UolGnzHeyhI45Z3FlZM+QiNR
gTZ36yMVAgMBAAECggEAHX5muepg3xHr4OvyOLCa0nhD3Mv5Rqz1UK2I3HyvMH6w
VT1cPF75v96rQCZXDC8VxoWNLyjHw+uOs+EHdOsX4J4/wA2EwHqeQT01CLb7b0jk
zaxrlW20hjMRVkE/MBZQ3jExYGjwwre5NycdOEyPOgwxY/wjIztvZUgEYn7YxSh7
5XkSEjvfNmztuY0V0Lq1FMGmln7CmOyzJ45dxEXFJILkBTH93+bxVWms5QKFh/zd
y7hhW0AmO2B1yXLWSEYfehm1m8e2JfhX9y70iIKjFBuhO5Xrh4w8jMDbLXUiJwBe
Lt1hbEu857ii8HNPDx1CqRK8OxvzqhF4HFu7p02aQQKBgQD6Es9oAdt+Xh2YSbL5
Q5Vc0IxdRYVD4rNhT/Ud/o3TqnaqQe9TscjmSPeUh6yEJl/7qzMqR3HLl0G2xh/z
1HZGpZKL2Dg6BwQffSrzbd2NuTpe0YagTbaxTh6rDVzC9CNIAge4RtDLeTp9JnPW
q8nx/q5O8QzhiWSb/Eap8t7fcwKBgQC7qkVKyz+0mza4fv1PWDX4QlqaZsMzxQMw
tau6LKgpndjTznxrsCzx5ApFoCE9nZ4nxC609aA5UQDai3BD+ClpHUGb0d7fst1C
o3GpIH3avOslTBGq6VmNTQD0G4KdEu8lW1c5nJVRVDWzVaBL/Xs6Gdedvc3fAtKZ
QVLT7ItBVwKBgQDBQb4le6H785Hdeb8lYTRRqR2Ai1Rk1RKZdqhQSMOkO00/YHsq
6+79AJDMqVkKO3nbzT9aQiJQGxF+xXg2ABBzoksxZaR2O6vVBe8mdtZMS0QvuK7Y
vZYdSejregUg6HlHkcITiC6dwI8R3qCQ2uvS52YfvaudtIcbAQg/GGFivwKBgQCh
rcpQ69aQtQBp5w45o5ersQ754vrdySg8eOfgV8TxRcqegFD1Pi0aoQqwfYrl2Lt/
KyfKQ6usdbs0jdlE1bJnmwsBQciwFRtG5CVyPknvf79evLCj/LJoaCUgKetXhA6A
srbtLZf/H3u8a1LU5SebINSAVBvfvEDQRFUlu68ltQKBgQDNks8g8dypyIA9gD3v
AF0RRoD8PMayrF31j2ZXIJ+LbVwJX6v4g7oAbi8yB4uUPpkmvSN/OV83f7Xn2Pfc
+996FBk9cLLV0TQLkw3er/x9WV6tozt/2ROO2JN9sW5gRFGBgjmSwWMzaezWKOFG
GjSUwzNwHw9IYH0JcnViw/MuXA==
-----END PRIVATE KEY-----
";

const TEST_PUBLIC_KEY_PEM: &str = r"-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAt1IRrtZOsya5XjL3LYTw
7KbOZim2pNyNWEBUJvs7wWe9qt6grFz+B5XUdoVrRz0JdZeXndzRTcYumjcMk0qC
CE56tubdxCkllKw+TG5s7EsSzk5Og+p8yvbcGVAoC2YwqvnGKEM7t/N4ZclVD2M9
7T4LRRZQyTECvJYu3w8JCDHnzSDskZ7st5WtyPN5HSql3K4jYQzdO1BO66NN8D/t
/k7sMl0Pc/Jg0+1RxYHfi1WvGnU5c34MkWfjhpqzNpaNMWBidM4djfJs/jTJaT0A
IKcBYJW/Tivs24vO5nPCmsAysAG4PdfEtFKJRp8x3soSOOWdxZWTPkIjUYE2d+sj
FQIDAQAB
-----END PUBLIC KEY-----
";

const TOKEN: &str = "stub-access-token-123";
const EMAIL: &str = "svc@test-project.iam.gserviceaccount.com";

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ok,
    BadJson,
    Error500,
    Huge,
    TokenFail,
}

struct Stub {
    base: String,
    mode: Mode,
    rows: Vec<Value>,
    token_calls: usize,
    query_sites: Vec<String>,
    query_bodies: Vec<Value>,
    assertion_errors: Vec<String>,
}

type St = Arc<Mutex<Stub>>;

async fn token(State(st): State<St>, Form(f): Form<HashMap<String, String>>) -> Response {
    let mut s = st.lock();
    s.token_calls += 1;
    if s.mode == Mode::TokenFail {
        return (StatusCode::BAD_REQUEST, "{\"error\":\"invalid_grant\"}").into_response();
    }
    let aud = format!("{}/token", s.base);
    let mut problems = Vec::new();
    if f.get("grant_type").map(String::as_str)
        != Some("urn:ietf:params:oauth:grant-type:jwt-bearer")
    {
        problems.push("grant_type".to_string());
    }
    let a = f.get("assertion").cloned().unwrap_or_default();
    if a.split('.').count() != 3 {
        problems.push("not a 3-part JWT".into());
    }
    match jsonwebtoken::decode_header(&a) {
        Ok(h) if h.alg == jsonwebtoken::Algorithm::RS256 => {}
        _ => problems.push("alg".into()),
    }
    let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    v.set_audience(&[aud.as_str()]);
    v.set_issuer(&[EMAIL]);
    v.set_required_spec_claims(&["exp", "iat", "aud", "iss"]);
    let key = jsonwebtoken::DecodingKey::from_rsa_pem(TEST_PUBLIC_KEY_PEM.as_bytes()).unwrap();
    match jsonwebtoken::decode::<Value>(&a, &key, &v) {
        Ok(d) => {
            let c = d.claims;
            if c["scope"] != json!(SCOPE) {
                problems.push("scope".into());
            }
            let (iat, exp) = (
                c["iat"].as_i64().unwrap_or(0),
                c["exp"].as_i64().unwrap_or(0),
            );
            if exp <= iat || exp - iat > 3600 {
                problems.push("lifetime".into());
            }
        }
        Err(e) => problems.push(format!("verify: {e}")),
    }
    if !problems.is_empty() {
        s.assertion_errors.extend(problems);
        return (StatusCode::UNAUTHORIZED, "{\"error\":\"bad assertion\"}").into_response();
    }
    Json(json!({"access_token": TOKEN, "expires_in": 3600, "token_type": "Bearer"})).into_response()
}

async fn query(
    State(st): State<St>,
    Path(site): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let mut s = st.lock();
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth != format!("Bearer {TOKEN}") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    s.query_sites.push(site.clone());
    s.query_bodies.push(body.clone());
    match s.mode {
        Mode::BadJson => return (StatusCode::OK, "{\"rows\": [ oops").into_response(),
        Mode::Error500 => return (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response(),
        Mode::Huge => return (StatusCode::OK, "x".repeat(300_000)).into_response(),
        _ => {}
    }
    // The domain property is "not verified" for this service account; the URL-prefix one works.
    if site.starts_with("sc-domain:") {
        return (
            StatusCode::FORBIDDEN,
            "{\"error\":{\"message\":\"User does not have sufficient permission\"}}",
        )
            .into_response();
    }
    let start = body["startRow"].as_u64().unwrap_or(0) as usize;
    let limit = body["rowLimit"].as_u64().unwrap_or(1000) as usize;
    let page: Vec<Value> = s.rows.iter().skip(start).take(limit).cloned().collect();
    Json(json!({"rows": page, "responseAggregationType": "byPage"})).into_response()
}

async fn start_stub(rows: Vec<Value>, mode: Mode) -> (String, St) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let st: St = Arc::new(Mutex::new(Stub {
        base: base.clone(),
        mode,
        rows,
        token_calls: 0,
        query_sites: Vec::new(),
        query_bodies: Vec::new(),
        assertion_errors: Vec::new(),
    }));
    let app = Router::new()
        .route("/token", post(token))
        .route(
            "/webmasters/v3/sites/{site}/searchAnalytics/query",
            post(query),
        )
        .with_state(st.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base, st)
}

fn row(page: &str, q: &str, impressions: f64, clicks: f64, pos: f64) -> Value {
    json!({"keys": [page, q], "clicks": clicks, "impressions": impressions, "ctr": 0.0, "position": pos})
}

/// A site `example.test` with posts /alpha/ and /beta/ and sealed GSC credentials.
async fn seed(e: &Env, token_uri: &str) -> String {
    seed_with(e, Some(sa_json(token_uri))).await
}

fn sa_json(token_uri: &str) -> String {
    json!({
        "type": "service_account", "client_email": EMAIL,
        "private_key": TEST_PRIVATE_KEY_PEM, "token_uri": token_uri,
    })
    .to_string()
}

/// Like `seed`, but the site's own credentials are optional (None = not connected).
async fn seed_with(e: &Env, creds: Option<String>) -> String {
    let id = sf_core::new_id();
    let connected = creds.is_some();
    let secrets = SiteSecrets {
        gsc_credentials: creds,
        ..SiteSecrets::default()
    }
    .seal(&e.ctx.secret_key)
    .unwrap();
    let settings = if connected {
        json!({"gsc": {"connected": true, "client_email": EMAIL}}).to_string()
    } else {
        json!({}).to_string()
    };
    let i = id.clone();
    e.db.write(move |t| {
        t.execute(
            "INSERT INTO sites(id, domain, wp_url, mode, api_key, settings, secrets_enc)
             VALUES(?1,'example.test',NULL,'wp_rest',?2,?3,?4)",
            rusqlite::params![i, format!("sf_{i}"), settings, secrets],
        )
        .map_err(sf_store::err)?;
        for slug in ["alpha", "beta"] {
            t.execute(
                "INSERT INTO posts(id, site_id, url, title) VALUES(?1, ?2, ?3, ?4)",
                rusqlite::params![
                    format!("{i}-{slug}"),
                    i,
                    format!("https://example.test/{slug}/"),
                    slug
                ],
            )
            .map_err(sf_store::err)?;
        }
        Ok(())
    })
    .await
    .unwrap();
    id
}

fn client(base: &str, page_size: usize, max_bytes: usize) -> GscClient {
    GscClient::with_endpoints(
        base,
        true,
        GscLimits {
            page_size,
            max_pages: 10,
            max_rows: 1000,
            max_response_bytes: max_bytes,
            timeout: Duration::from_secs(10),
        },
    )
    .unwrap()
}

async fn gsc_rows(e: &Env, site: &str) -> Vec<Vec<Value>> {
    q_rows(
        &e.db,
        "SELECT k.post_id, k.keyword, k.volume, k.position, k.striking FROM keywords k
         JOIN posts p ON p.id=k.post_id WHERE p.site_id=?1 AND k.source='gsc' ORDER BY k.post_id, k.keyword",
        vec![site.to_string()],
    )
    .await
}

#[tokio::test]
async fn gsc_pull_pages_maps_and_replaces_keywords() {
    let rows = vec![
        row("https://example.test/alpha/", "bike lock", 100.0, 5.0, 6.5),
        row(
            "https://example.test/alpha/?utm=x",
            "bike lock",
            100.0,
            5.0,
            8.5,
        ),
        row(
            "https://www.example.test/beta",
            "best helmet",
            40.0,
            1.0,
            2.0,
        ),
        row(
            "https://example.test/beta/",
            "helmet sizes",
            10.0,
            0.0,
            15.0,
        ),
        row(
            "https://example.test/unknown/",
            "orphan query",
            5.0,
            0.0,
            5.0,
        ),
        row("https://other.test/alpha/", "foreign", 5.0, 0.0, 5.0),
        row("https://example.test/alpha/", "   ", 5.0, 0.0, 5.0),
    ];
    let (base, st) = start_stub(rows, Mode::Ok).await;
    let e = env("gsc-pull");
    let sid = seed(&e, &format!("{base}/token")).await;
    let site = load_site(&e.ctx, &sid).await.unwrap();
    let stats = sf_jobs::pipelines::gsc::sync_site(&e.ctx, &site, &client(&base, 3, 1 << 20))
        .await
        .unwrap();
    {
        let s = st.lock();
        assert!(s.assertion_errors.is_empty(), "{:?}", s.assertion_errors);
        assert_eq!(s.token_calls, 1);
        // sc-domain refused once, then the URL-prefix property paged 3+3+1.
        assert_eq!(s.query_sites[0], "sc-domain:example.test");
        assert!(
            s.query_sites[1..]
                .iter()
                .all(|p| p == "https://example.test/")
        );
        let starts: Vec<u64> = s.query_bodies[1..]
            .iter()
            .map(|b| b["startRow"].as_u64().unwrap())
            .collect();
        assert_eq!(starts, vec![0, 3, 6]);
        assert_eq!(s.query_bodies[1]["dimensions"], json!(["page", "query"]));
    }
    assert_eq!(stats["rows"], 7);
    assert_eq!(stats["matched"], 4);
    assert_eq!(stats["upserted"], 3);
    assert_eq!(stats["removed"], 0);
    assert_eq!(stats["property"], "https://example.test/");
    let kw = gsc_rows(&e, &sid).await;
    assert_eq!(kw.len(), 3);
    // alpha/bike lock: impressions summed, position impression-weighted (7.5), striking.
    assert_eq!(kw[0][1], "bike lock");
    assert_eq!(kw[0][2], 200);
    assert!((kw[0][3].as_f64().unwrap() - 7.5).abs() < 1e-9);
    assert_eq!(kw[0][4], 1);
    assert_eq!(kw[1][1], "best helmet");
    assert_eq!(kw[1][4], 0); // position 2: not striking
    assert_eq!(kw[2][1], "helmet sizes");
    assert_eq!(kw[2][4], 0); // position 15: outside v1's 4..=10
    let settings = q_rows(
        &e.db,
        "SELECT json_extract(settings,'$.gsc.last_sync_at'), json_extract(settings,'$.gsc.last_property'),
                json_extract(settings,'$.gsc.connected') FROM sites WHERE id=?1",
        vec![sid.clone()],
    )
    .await;
    assert!(settings[0][0].is_string());
    assert_eq!(settings[0][1], "https://example.test/");
    assert_eq!(settings[0][2], 1);

    // Second pull with fewer rows: stale gsc rows go, non-gsc keywords stay.
    exec(
        &e.db,
        "INSERT INTO keywords(id, post_id, keyword, source, score) VALUES('k-body', ?1, 'bike', 'body', 0.5)",
        vec![format!("{sid}-alpha")],
    )
    .await;
    st.lock().rows = vec![row(
        "https://example.test/beta/",
        "best helmet",
        50.0,
        2.0,
        4.0,
    )];
    let stats = sf_jobs::pipelines::gsc::sync_site(&e.ctx, &site, &client(&base, 3, 1 << 20))
        .await
        .unwrap();
    assert_eq!(stats["upserted"], 1);
    assert_eq!(stats["removed"], 2);
    let kw = gsc_rows(&e, &sid).await;
    assert_eq!(kw.len(), 1);
    assert_eq!(kw[0][2], 50);
    assert_eq!(kw[0][4], 1);
    assert_eq!(
        q_i64(
            &e.db,
            "SELECT count(*) FROM keywords WHERE source='body'",
            vec![]
        )
        .await,
        1
    );
}

/// G2-5: a site without its own credentials uses the install-wide `GSC_CREDENTIALS_JSON`;
/// per-site credentials win when both exist; neither = not connected. Never leaks the key.
#[tokio::test]
async fn gsc_falls_back_to_install_wide_credentials() {
    let rows = vec![row(
        "https://example.test/alpha/",
        "bike lock",
        100.0,
        5.0,
        6.5,
    )];
    let (base, st) = start_stub(rows, Mode::Ok).await;
    let global = sa_json(&format!("{base}/token"));
    let e = env_cfg("gsc-global", |c| c.gsc.credentials_json = global.clone());
    let sid = seed_with(&e, None).await;
    let site = load_site(&e.ctx, &sid).await.unwrap();
    let (_, source) = sf_jobs::pipelines::gsc::credentials_for(&e.ctx, &site).unwrap();
    assert_eq!(source, "global");
    let stats = sf_jobs::pipelines::gsc::sync_site(&e.ctx, &site, &client(&base, 3, 1 << 20))
        .await
        .unwrap();
    assert_eq!(stats["credentials"], "global");
    assert_eq!(stats["upserted"], 1);
    assert!(!stats.to_string().contains("PRIVATE KEY"));
    {
        let s = st.lock();
        assert!(s.assertion_errors.is_empty(), "{:?}", s.assertion_errors);
        assert_eq!(s.token_calls, 1);
    }
    assert_eq!(gsc_rows(&e, &sid).await.len(), 1);
    // The settings stamp only touches sites that have a gsc object: none is invented.
    let gsc_obj = q_rows(
        &e.db,
        "SELECT json_type(settings,'$.gsc') FROM sites WHERE id=?1",
        vec![sid.clone()],
    )
    .await;
    assert!(gsc_obj[0][0].is_null());

    // Per-site credentials take precedence over a (broken) global value.
    let e2 = env_cfg("gsc-precedence", |c| {
        c.gsc.credentials_json = r#"{"client_email":"x"}"#.into();
    });
    let sid2 = seed_with(&e2, Some(sa_json(&format!("{base}/token")))).await;
    let site2 = load_site(&e2.ctx, &sid2).await.unwrap();
    let stats = sf_jobs::pipelines::gsc::sync_site(&e2.ctx, &site2, &client(&base, 3, 1 << 20))
        .await
        .unwrap();
    assert_eq!(stats["credentials"], "site");

    // Neither: a clear fatal error, and the audit hook skips GSC entirely.
    let e3 = env("gsc-none");
    let sid3 = seed_with(&e3, None).await;
    let site3 = load_site(&e3.ctx, &sid3).await.unwrap();
    assert!(sf_jobs::pipelines::gsc::credentials_for(&e3.ctx, &site3).is_none());
    let err = sf_jobs::pipelines::gsc::sync_site(&e3.ctx, &site3, &client(&base, 3, 1 << 20))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("GSC_CREDENTIALS_JSON"), "{err}");
}

#[tokio::test]
async fn gsc_hostile_responses_never_panic_and_keep_rows() {
    for mode in [Mode::BadJson, Mode::Error500, Mode::Huge, Mode::TokenFail] {
        let (base, _st) = start_stub(vec![], mode).await;
        let e = env("gsc-hostile");
        let sid = seed(&e, &format!("{base}/token")).await;
        exec(
            &e.db,
            "INSERT INTO keywords(id, post_id, keyword, source, volume, position, striking)
             VALUES(?1, ?2, 'kept', 'gsc', 10, 5.0, 1)",
            vec![sf_core::new_id(), format!("{sid}-alpha")],
        )
        .await;
        let site = load_site(&e.ctx, &sid).await.unwrap();
        let r =
            sf_jobs::pipelines::gsc::sync_site(&e.ctx, &site, &client(&base, 100, 100_000)).await;
        let msg = r.expect_err("hostile response must fail").to_string();
        assert!(
            !msg.contains("PRIVATE KEY") && !msg.contains(TOKEN),
            "{msg}"
        );
        assert_eq!(gsc_rows(&e, &sid).await.len(), 1, "rows kept on failure");
        let last_err = q_rows(
            &e.db,
            "SELECT json_extract(settings,'$.gsc.last_error') FROM sites WHERE id=?1",
            vec![sid.clone()],
        )
        .await;
        assert!(last_err[0][0].is_string());
    }
}

#[tokio::test]
async fn gsc_job_uses_production_guard_and_never_leaves_localhost() {
    // The production client only accepts Google token hosts: the job fails before any request.
    let (base, st) = start_stub(vec![], Mode::Ok).await;
    let e = env("gsc-job");
    let sid = seed(&e, &format!("{base}/token")).await;
    let mut nj = NewJob::new(JobKind::Gsc, Some(&sid), json!({}));
    nj.dedupe = Some(false);
    let enq = e.ctx.queue.enqueue(nj).await.unwrap();
    let claimed = e.ctx.queue.claim_next(true).await.unwrap().unwrap();
    assert_eq!(claimed.id, enq.job_run_id);
    let job = Arc::new(Job::new(claimed, e.ctx.queue.clone()));
    let r = sf_jobs::pipelines::dispatch(e.ctx.clone(), job).await;
    let msg = r.expect_err("must fail").to_string();
    assert!(msg.contains("token_uri"), "{msg}");
    assert_eq!(st.lock().token_calls, 0);

    // A site without credentials: clear error.
    let other = add_site(&e, "http://example.invalid", false, json!({})).await;
    let site = load_site(&e.ctx, &other).await.unwrap();
    let r = sf_jobs::pipelines::gsc::sync_site(&e.ctx, &site, &client(&base, 10, 1 << 20)).await;
    assert!(r.err().unwrap().to_string().contains("not connected"));
}
