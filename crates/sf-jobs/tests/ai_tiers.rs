//! v2.3 in the plan pipeline: "Always use AI", batched low-tier anchor picks, the result cache,
//! skip rules, budget exhaustion fallback and token accounting rows.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;

use axum::extract::State;
use axum::{Json, Router};
use common::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sf_core::types::JobKind;

/// Records the item count of every request; answers each item with its first candidate.
async fn stub(State(seen): State<Arc<Mutex<Vec<usize>>>>, Json(body): Json<Value>) -> Json<Value> {
    let user = body["messages"]
        .as_array()
        .and_then(|m| m.iter().rev().find(|x| x["role"] == "user"))
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_string();
    let items: Vec<Value> = serde_json::from_str(&user).unwrap_or_default();
    seen.lock().push(items.len());
    let replies: Vec<Value> = items
        .iter()
        .map(|it| {
            let first = it["c"]
                .as_array()
                .and_then(|c| c.first())
                .cloned()
                .unwrap_or(json!(""));
            json!({"id": it["id"], "anchor": first, "rel": 0.7})
        })
        .collect();
    // no usage block: tokens are estimated (chars/4)
    Json(json!({
        "choices": [{"message": {"role": "assistant", "content": json!({"items": replies}).to_string()}}]
    }))
}

async fn start_stub() -> (String, Arc<Mutex<Vec<usize>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/v1/chat/completions", axum::routing::post(stub))
        .with_state(seen.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{port}/v1"), seen)
}

async fn setup(e: &Env) -> String {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let site = add_site(e, &base, true, json!({})).await;
    run_job(e, JobKind::Crawl, &site, json!({})).await;
    site
}

async fn ai_meta(e: &Env, site: &str) -> Vec<Value> {
    q_rows(
        &e.db,
        "SELECT score_breakdown FROM link_edges WHERE site_id=?1 AND origin='plan' ORDER BY id",
        vec![site.into()],
    )
    .await
    .into_iter()
    .map(|r| {
        let b: Value = serde_json::from_str(r[0].as_str().unwrap_or("{}")).unwrap_or(json!({}));
        b["ai"].clone()
    })
    .collect()
}

async fn summary(e: &Env) -> Value {
    let r = q_rows(
        &e.db,
        "SELECT value FROM kv WHERE key LIKE 'ai_run:%' ORDER BY updated_at DESC LIMIT 1",
        vec![],
    )
    .await;
    r.first()
        .and_then(|x| x[0].as_str())
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or(json!(null))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn always_on_batches_caches_and_accounts() {
    let (url, seen) = start_stub().await;
    let e = env_ai_cfg("ai-always", &url, |c| {
        c.ai.always_on = true;
        c.ai.batch_size = 3;
        // never "confident": every new edge goes to anchor_pick
        c.ai.confidence_skip.anchor = 1.01;
        c.ai.confidence_skip.relevance = 1.01;
    });
    let site = setup(&e).await;
    // no ai_refine anywhere: always_on alone turns the AI on
    run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    let batches = seen.lock().clone();
    assert!(!batches.is_empty(), "always_on sent nothing");
    assert!(batches.iter().all(|n| (1..=3).contains(n)), "{batches:?}");
    let sent: usize = batches.iter().sum();
    if sent > 3 {
        assert!(batches.len() < sent, "not batched: {batches:?}");
    }
    let metas = ai_meta(&e, &site).await;
    assert!(
        metas.iter().any(|m| m["model"] == "stub-model"),
        "{metas:?}"
    );

    // accounting: one row per request, task/tier/site/run, estimated tokens, items
    let rows = q_rows(
        &e.db,
        "SELECT task, tier, site_id, run_id, estimated, items, tokens_in, tokens_out, cache_hit FROM model_call_log ORDER BY id",
        vec![],
    )
    .await;
    assert_eq!(rows.len(), batches.len());
    for r in &rows {
        assert_eq!(r[0], "anchor_pick");
        assert_eq!(r[1], "low");
        assert_eq!(r[2], json!(site));
        assert!(r[3].as_str().is_some_and(|x| !x.is_empty()));
        assert_eq!(r[4], 1, "no usage block -> estimated");
        assert!(r[6].as_i64().unwrap() > 0 && r[7].as_i64().unwrap() > 0);
        assert_eq!(r[8], 0);
    }
    let items: i64 = rows.iter().map(|r| r[5].as_i64().unwrap()).sum();
    assert_eq!(items as usize, sent);
    let s1 = summary(&e).await;
    assert_eq!(s1["asked"], json!(sent), "{s1}");
    assert_eq!(s1["cache_hits"], 0);

    // same edges again: answered from the cache, no new request
    e.db.write(|t| {
        t.execute("DELETE FROM link_edges", [])
            .map_err(sf_store::err)?;
        Ok(())
    })
    .await
    .unwrap();
    run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    let s2 = summary(&e).await;
    let hits = s2["cache_hits"].as_u64().unwrap() as usize;
    let asked2 = s2["asked"].as_u64().unwrap() as usize;
    // (an edge whose candidates changed with the re-plan's anchor pools is asked again)
    assert!(hits > 0 && hits >= asked2 * 2, "{s2}");
    let new_requests = seen.lock().len() - batches.len();
    let new_items: usize = seen.lock()[batches.len()..].iter().sum();
    assert_eq!(new_items, asked2, "only cache misses call the model");
    assert!(new_requests < batches.len());
    let hit = q_rows(
        &e.db,
        "SELECT items, tokens_in, provider FROM model_call_log WHERE cache_hit=1",
        vec![],
    )
    .await;
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0][0], json!(hits));
    assert_eq!(hit[0][1], 0);
    assert_eq!(hit[0][2], "cache");
    let metas = ai_meta(&e, &site).await;
    assert!(
        metas
            .iter()
            .any(|m| m["model"].as_str().is_some_and(|x| x.starts_with("cache:")))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confident_heuristics_skip_the_ai() {
    let (url, seen) = start_stub().await;
    let e = env_ai_cfg("ai-skip", &url, |c| {
        c.ai.always_on = true;
        c.ai.confidence_skip.anchor = 0.0;
        c.ai.confidence_skip.relevance = 0.0;
    });
    let site = setup(&e).await;
    run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    let metas = ai_meta(&e, &site).await;
    let skipped = metas.iter().filter(|m| m["skipped"] == "confident").count();
    // in-paragraph anchors are confident and skipped; the rest still go to the model
    assert!(skipped > 0, "{metas:?}");
    let s = summary(&e).await;
    assert_eq!(s["skipped_confident"], json!(skipped), "{s}");
    let sent: usize = seen.lock().iter().sum();
    assert_eq!(s["asked"], json!(sent));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exhausted_budget_falls_back_to_heuristics() {
    let (url, seen) = start_stub().await;
    let e = env_ai_cfg("ai-budget", &url, |c| {
        c.ai.always_on = true;
        c.ai.confidence_skip.anchor = 1.01;
        c.ai.budget.daily_tokens.low = 100;
    });
    let site = setup(&e).await;
    // 150 low-tier tokens already used today
    e.db.write(|t| {
        t.execute(
            "INSERT INTO model_call_log(provider, role, rung, ok, tokens_in, tokens_out, task, tier) VALUES('x','chat',0,1,100,50,'anchor_pick','low')",
            [],
        )
        .map_err(sf_store::err)?;
        Ok(())
    })
    .await
    .unwrap();
    let st = run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    assert!(st["candidates"].as_u64().unwrap() > 0);
    assert!(seen.lock().is_empty(), "no provider call over budget");
    let metas = ai_meta(&e, &site).await;
    assert!(
        metas.iter().any(|m| m["error"] == "budget exhausted"),
        "{metas:?}"
    );
    let s = summary(&e).await;
    assert_eq!(s["fallback"], "budget exhausted", "{s}");
    let warns = q_rows(
        &e.db,
        "SELECT message FROM job_logs WHERE level='warn' AND message LIKE 'ai anchors:%'",
        vec![],
    )
    .await;
    assert!(
        warns
            .iter()
            .any(|w| w[0].as_str().unwrap_or("").contains("budget exhausted")),
        "{warns:?}"
    );
}
