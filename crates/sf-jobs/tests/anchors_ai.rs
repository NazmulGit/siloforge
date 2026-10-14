//! Anchor quality features in the plan pipeline: semantic scoring metadata, AI refinement
//! (validated, with automatic fallback) and the idle eviction of the per-site caches.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::{Json, Router};
use common::*;
use serde_json::{Value, json};
use sf_core::types::JobKind;

/// Stub chat endpoint answering anchor-refinement prompts. Mode 0: picks the LAST heuristic
/// candidate with relevance 0.1. Mode 1: proposes an invented (non-verbatim) phrase.
async fn stub_refine(State(mode): State<Arc<AtomicUsize>>, Json(body): Json<Value>) -> Json<Value> {
    let user = body["messages"]
        .as_array()
        .and_then(|m| m.iter().rev().find(|x| x["role"] == "user"))
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string();
    let cands: Vec<String> = user
        .split("imperfect):\n")
        .nth(1)
        .unwrap_or("")
        .split("\n\n")
        .next()
        .unwrap_or("")
        .lines()
        .filter_map(|l| l.strip_prefix("- ").map(str::to_string))
        .collect();
    let reply = match mode.load(Ordering::SeqCst) {
        0 => {
            json!({"anchor": cands.last().cloned().unwrap_or_default(), "relevance": 0.1, "reason": "stub pick"})
        }
        _ => {
            json!({"anchor": "completely invented anchor words", "relevance": 0.9, "reason": "stub"})
        }
    };
    Json(json!({
        "id": "x", "object": "chat.completion", "model": "stub-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": reply.to_string()}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 10}
    }))
}

async fn start_refine_ai(mode: usize) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(AtomicUsize::new(mode));
    let app = Router::new()
        .route("/v1/chat/completions", axum::routing::post(stub_refine))
        .with_state(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://127.0.0.1:{port}/v1")
}

/// (anchor, paragraph text, composite, breakdown) of suggested plan edges.
async fn edges(e: &Env, site: &str) -> Vec<(String, String, f64, Value)> {
    q_rows(
        &e.db,
        "SELECT e.anchor, ifnull(pa.text,''), e.composite_score, e.score_breakdown FROM link_edges e
         LEFT JOIN paragraphs pa ON pa.id=e.paragraph_id
         WHERE e.site_id=?1 AND e.status='suggested' AND e.origin='plan' ORDER BY e.id",
        vec![site.into()],
    )
    .await
    .into_iter()
    .map(|r| {
        let b: Value = serde_json::from_str(r[3].as_str().unwrap_or("{}")).unwrap_or(json!({}));
        (s(&r[0]), s(&r[1]), r[2].as_f64().unwrap_or(0.0), b)
    })
    .collect()
}

async fn setup(e: &Env, settings: Value) -> String {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let site = add_site(e, &base, true, settings).await;
    run_job(e, JobKind::Crawl, &site, json!({})).await;
    site
}

async fn warnings(e: &Env) -> Vec<String> {
    q_rows(
        &e.db,
        "SELECT message FROM job_logs WHERE level='warn' ORDER BY id",
        vec![],
    )
    .await
    .into_iter()
    .map(|r| s(&r[0]))
    .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ai_refinement_validates_replies_and_scales_low_relevance() {
    let ai = start_refine_ai(0).await;
    let e = env_with_ai("ai-good", &ai);
    let site = setup(&e, json!({"ai_anchor_refine": true})).await;
    let st = run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    assert!(st["candidates"].as_u64().unwrap() > 0, "{st}");
    let es = edges(&e, &site).await;
    // edges whose candidate paragraphs are all taken are never sent ("no free paragraph")
    let with_ai: Vec<_> = es
        .iter()
        .filter(|x| x.3["ai"]["model"].is_string())
        .collect();
    assert!(!with_ai.is_empty(), "no AI metadata: {es:?}");
    assert!(
        es.iter()
            .filter_map(|x| x.3.get("ai"))
            .all(|a| a["model"].is_string() || a["error"] == "no free paragraph")
    );
    let mut used_ai = 0;
    for (anchor, para, composite, b) in &with_ai {
        let ai = &b["ai"];
        assert_eq!(ai["model"], "stub-model", "{ai}");
        assert_eq!(ai["relevance"], 0.1);
        assert_eq!(b["ai_factor"], 0.6, "{b}");
        assert!(*composite > 0.0);
        if ai["fallback_used"] == false {
            used_ai += 1;
            assert!(
                ai["anchor"].as_str().unwrap().eq_ignore_ascii_case(anchor),
                "{anchor} vs {ai}"
            );
            assert!(
                para.to_lowercase().contains(&anchor.to_lowercase()),
                "{anchor} not in {para}"
            );
        }
    }
    assert!(used_ai > 0, "{with_ai:?}");
    // anchors carry their selection metadata (semantic cosine from the fake embedder)
    assert!(
        es.iter()
            .any(|x| x.3["anchor"]["semantic"].is_number() && x.3["anchor"]["source"] == "phrase"),
        "{es:?}"
    );
    // a re-plan keeps the AI judgement and its score factor on rescored edges
    run_job(
        &e,
        JobKind::Plan,
        &site,
        json!({"topology": "reverse", "ai_refine": false}),
    )
    .await;
    let again = edges(&e, &site).await;
    let kept: Vec<_> = again
        .iter()
        .filter(|x| x.3["ai"]["model"].is_string())
        .collect();
    assert!(kept.len() >= with_ai.len());
    assert!(kept.iter().all(|x| x.3["ai_factor"] == 0.6));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ai_refinement_rejects_invented_anchors() {
    let ai = start_refine_ai(1).await;
    let e = env_with_ai("ai-bad", &ai);
    let site = setup(&e, json!({})).await;
    run_job(
        &e,
        JobKind::Plan,
        &site,
        json!({"topology": "reverse", "ai_refine": true}),
    )
    .await;
    let es = edges(&e, &site).await;
    let with_ai: Vec<_> = es
        .iter()
        .filter(|x| x.3["ai"]["model"].is_string())
        .collect();
    assert!(!with_ai.is_empty());
    for (anchor, _, _, b) in with_ai {
        assert_eq!(b["ai"]["fallback_used"], true, "{b}");
        assert_eq!(b["ai"]["reject"], "not_verbatim", "{b}");
        assert_ne!(anchor, "completely invented anchor words");
        assert!(b.get("ai_factor").is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ai_unavailable_falls_back_and_the_plan_completes() {
    // nothing listens on port 9: every call fails fast, the circuit stops asking
    let e = env_with_ai("ai-down", "http://127.0.0.1:9/v1");
    let site = setup(&e, json!({"ai_anchor_refine": true})).await;
    let st = run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    assert!(st["candidates"].as_u64().unwrap() > 0);
    let es = edges(&e, &site).await;
    assert!(es.iter().all(|x| !x.0.is_empty()));
    assert!(
        es.iter()
            .filter_map(|x| x.3.get("ai"))
            .all(|a| a["fallback_used"] == true)
    );
    assert!(
        warnings(&e)
            .await
            .iter()
            .any(|w| w.starts_with("ai anchors:")),
        "{:?}",
        warnings(&e).await
    );

    // no AI configured at all: one warning, heuristic anchors
    let e2 = env("ai-none");
    let site2 = setup(&e2, json!({})).await;
    run_job(
        &e2,
        JobKind::Plan,
        &site2,
        json!({"topology": "reverse", "ai_refine": true}),
    )
    .await;
    let w = warnings(&e2).await;
    assert_eq!(
        w.iter()
            .filter(|m| m.contains("no AI is configured"))
            .count(),
        1,
        "{w:?}"
    );
    let es2 = edges(&e2, &site2).await;
    assert!(
        es2.iter()
            .any(|x| x.3["ai"]["error"] == "ai not configured")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_caches_are_dropped() {
    let e = env("idle");
    let site = setup(&e, json!({})).await;
    run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    assert!(e.ctx.vectors.cached_bytes() > 0);
    assert!(!e.ctx.terms.is_empty());
    assert!(!e.ctx.phrases.is_empty());
    // fresh entries survive a long TTL ...
    assert_eq!(e.ctx.vectors.evict_idle(Duration::from_secs(3600)), 0);
    // ... and go once idle
    assert_eq!(e.ctx.vectors.evict_idle(Duration::ZERO), 1);
    assert_eq!(e.ctx.vectors.cached_bytes(), 0);
    assert_eq!(e.ctx.terms.evict_idle(Duration::ZERO), 1);
    assert!(e.ctx.phrases.evict_idle(Duration::ZERO) > 0);
    assert!(e.ctx.phrases.is_empty() && e.ctx.terms.is_empty());
    // the next plan reloads them transparently
    run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    assert!(e.ctx.vectors.cached_bytes() > 0);
}

/// Embedder whose vectors depend on the word-piece cap (like a real model on long inputs).
struct CapEmbedder;

impl sf_embed::Embedder for CapEmbedder {
    fn id(&self) -> &str {
        "cap"
    }
    fn kind(&self) -> sf_embed::EmbedderKind {
        sf_embed::EmbedderKind::Onnx
    }
    fn embed<'a>(&'a self, texts: &'a [String]) -> sf_embed::EmbedFuture<'a> {
        self.embed_with_cap(texts, sf_embed::MAX_TOKENS)
    }
    fn embed_with_cap<'a>(&'a self, texts: &'a [String], cap: usize) -> sf_embed::EmbedFuture<'a> {
        Box::pin(async move {
            Ok(texts
                .iter()
                .map(|t| {
                    if t.len() > 1000 {
                        fake_vector(&format!("{t} cap{cap}"))
                    } else {
                        fake_vector(t)
                    }
                })
                .collect())
        })
    }
}

/// v1 embedded some sites' posts with its local rung (512 word-pieces): re-embedded posts must
/// use the cap that reproduces the stored vectors.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_vectors_reuse_the_sites_token_cap() {
    let e = env_with_embedder("cap", Arc::new(CapEmbedder));
    let site = setup(&e, json!({})).await;
    run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    let long: String = "Long text about bike locks and chains. ".repeat(80);
    let posts: Vec<String> = q_rows(
        &e.db,
        "SELECT id, title FROM posts WHERE site_id=?1 ORDER BY id",
        vec![site.clone()],
    )
    .await
    .into_iter()
    .map(|r| s(&r[0]))
    .collect();
    let v512 = |title: &str| {
        let body: String = long.chars().take(2000).collect();
        fake_vector(&format!("{title}\n{body} cap512"))
    };
    let titles: Vec<String> = q_rows(
        &e.db,
        "SELECT title FROM posts WHERE site_id=?1 ORDER BY id",
        vec![site.clone()],
    )
    .await
    .into_iter()
    .map(|r| s(&r[0]))
    .collect();
    // all posts get long text; stored vectors look like v1-local (512) except one we re-embed
    for (i, (id, title)) in posts.iter().zip(&titles).enumerate() {
        let blob = sf_core::types::vec_to_blob(&v512(title));
        let (id, long, null) = (id.clone(), long.clone(), i == 0);
        e.db.write(move |t| {
            t.execute(
                "UPDATE posts SET content_text=?2, embedding=CASE WHEN ?4 THEN NULL ELSE ?3 END WHERE id=?1",
                rusqlite::params![id, long, blob, null],
            )
            .map_err(sf_store::err)?;
            Ok(())
        })
        .await
        .unwrap();
    }
    run_job(&e, JobKind::Embed, &site, json!({})).await;
    let got = q_rows(
        &e.db,
        "SELECT embedding FROM posts WHERE id=?1",
        vec![posts[0].clone()],
    )
    .await;
    let blob: Vec<u8> =
        e.db.read({
            let id = posts[0].clone();
            move |c| {
                c.query_row("SELECT embedding FROM posts WHERE id=?1", [id], |r| {
                    r.get(0)
                })
                .map_err(sf_store::err)
            }
        })
        .await
        .unwrap();
    assert!(!got.is_empty());
    let v = sf_core::types::blob_to_vec(&blob).unwrap();
    let want = v512(&titles[0]);
    let cos: f32 = v.iter().zip(&want).map(|(a, b)| a * b).sum();
    assert!(cos > 0.9999, "cos {cos}");
}

/// v2.0.1: pending suggestions whose anchor is a spec fragment (v1 split "20V" into "20" + "V")
/// get a fresh anchor on the next plan; natural anchors are kept as they are.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fragment_anchors_of_pending_suggestions_are_repicked() {
    let e = env("frag");
    let site = setup(&e, json!({})).await;
    run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    let rows = q_rows(
        &e.db,
        "SELECT id, anchor FROM link_edges WHERE site_id=?1 AND status='suggested' AND origin='plan'
         ORDER BY id LIMIT 2",
        vec![site.clone()],
    )
    .await;
    assert_eq!(rows.len(), 2, "need two suggestions");
    let (bad_id, keep_id, keep_anchor) = (s(&rows[0][0]), s(&rows[1][0]), s(&rows[1][1]));
    exec(
        &e.db,
        "UPDATE link_edges SET anchor='V kits perfect' WHERE id=?1",
        vec![bad_id.clone()],
    )
    .await;
    run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    let after = q_rows(
        &e.db,
        "SELECT anchor FROM link_edges WHERE id=?1",
        vec![bad_id.clone()],
    )
    .await;
    // re-anchored, or dropped when the paragraph offers nothing natural
    if let Some(r) = after.first() {
        let a = s(&r[0]);
        assert_ne!(a, "V kits perfect");
        assert!(!sf_engine::anchor::is_fragment_anchor(&a), "{a}");
    }
    let kept = q_rows(
        &e.db,
        "SELECT anchor FROM link_edges WHERE id=?1",
        vec![keep_id],
    )
    .await;
    assert_eq!(
        s(&kept[0][0]),
        keep_anchor,
        "natural anchors are not re-picked"
    );
}
