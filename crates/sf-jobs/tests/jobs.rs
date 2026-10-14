//! Integration tests for the runner, scheduler and pipelines (stub WordPress, fake embedder).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use futures::FutureExt;
use serde_json::{Value, json};
use sf_core::types::JobKind;
use sf_jobs::pipelines::PipelineFn;
use sf_jobs::{NewJob, Runner, RunnerOptions, Scheduler};

const SITE_SETTINGS: &str = r#"{"max_new_links_per_pass": 3, "max_links_per_post": 10}"#;

fn settings() -> Value {
    serde_json::from_str(SITE_SETTINGS).unwrap()
}

async fn crawl_and_plan(e: &Env, site: &str) -> Value {
    run_job(e, JobKind::Crawl, site, json!({})).await;
    run_job(e, JobKind::Plan, site, json!({"topology": "reverse"})).await
}

/// (edge id, source, target, anchor, paragraph text, target url)
async fn suggested(e: &Env, site: &str) -> Vec<(String, String, String, String, String, String)> {
    q_rows(
        &e.db,
        "SELECT e.id, e.source_post_id, e.target_post_id, e.anchor, ifnull(pa.text,''), t.url FROM link_edges e
         JOIN posts t ON t.id=e.target_post_id LEFT JOIN paragraphs pa ON pa.id=e.paragraph_id
         WHERE e.site_id=?1 AND e.status='suggested'
         ORDER BY (SELECT url FROM posts WHERE id=e.source_post_id), t.url",
        vec![site.into()],
    )
    .await
    .into_iter()
    .map(|r| (s(&r[0]), s(&r[1]), s(&r[2]), s(&r[3]), s(&r[4]), s(&r[5])))
    .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crawl_stores_raw_and_rechunks_only_changed_posts() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let e = env("crawl");
    let site = add_site(&e, &base, true, settings()).await;

    let st = run_job(&e, JobKind::Crawl, &site, json!({})).await;
    assert_eq!(st["upserted"], 6, "{st}");
    assert_eq!(st["chunked_posts"], 6);
    assert_eq!(st["raw_content"], true);
    let raw_posts = q_i64(&e.db, "SELECT count(*) FROM posts WHERE site_id=?1 AND content_raw LIKE '%<!-- wp:paragraph -->%'", vec![site.clone()]).await;
    assert_eq!(raw_posts, 6);
    let paras = q_i64(&e.db, "SELECT count(*) FROM paragraphs", vec![]).await;
    assert_eq!(paras, 18, "3 prose paragraphs per post");
    // existing in-content link recorded as origin=existing
    let existing = q_i64(
        &e.db,
        "SELECT count(*) FROM link_edges WHERE origin='existing' AND status='applied'",
        vec![],
    )
    .await;
    assert_eq!(existing, 1);

    // embed, then change one paragraph of post 3 and re-crawl
    run_job(&e, JobKind::Embed, &site, json!({})).await;
    let before: HashMap<String, String> = q_rows(
        &e.db,
        "SELECT pa.hash, pa.id FROM paragraphs pa JOIN posts p ON p.id=pa.post_id WHERE p.url LIKE '%mountain-bike-tires%'",
        vec![],
    )
    .await
    .into_iter()
    .map(|r| (s(&r[0]), s(&r[1])))
    .collect();
    {
        let mut w = wp.lock();
        let p = w.posts.get_mut(&3).unwrap();
        p.raw = p.raw.replace(
            "Riders who stay out after sunset",
            "Anyone riding after sunset",
        );
        p.modified += 1;
    }
    let st = run_job(&e, JobKind::Crawl, &site, json!({})).await;
    assert_eq!(st["upserted"], 1, "{st}");
    assert_eq!(st["skipped"], 5);
    assert_eq!(st["chunked_posts"], 1);
    let after: Vec<Vec<Value>> = q_rows(
        &e.db,
        "SELECT pa.hash, pa.id, pa.embedding IS NOT NULL FROM paragraphs pa JOIN posts p ON p.id=pa.post_id WHERE p.url LIKE '%mountain-bike-tires%' ORDER BY pa.idx",
        vec![],
    )
    .await;
    assert_eq!(after.len(), 3);
    let kept: Vec<&Vec<Value>> = after
        .iter()
        .filter(|r| before.get(&s(&r[0])) == Some(&s(&r[1])))
        .collect();
    assert_eq!(kept.len(), 2, "unchanged paragraphs keep their row");
    assert!(kept.iter().all(|r| r[2] == 1), "and their embedding");
    // nothing else was re-chunked: a third crawl is a no-op
    let st = run_job(&e, JobKind::Crawl, &site, json!({})).await;
    assert_eq!(st["chunked_posts"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_suggests_valid_unique_edges_and_respects_rejections() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let e = env("plan");
    let site = add_site(&e, &base, true, settings()).await;
    let st = crawl_and_plan(&e, &site).await;
    assert_eq!(st["posts"], 6, "{st}");
    assert!(st["embedded"].as_i64().unwrap() >= 24);
    assert_eq!(st["progress"]["phase"], "done");
    let edges = suggested(&e, &site).await;
    assert!(
        edges.len() >= 4,
        "plan produced {} edges: {st}",
        edges.len()
    );
    // At most one native suggestion per paragraph, also after the anchor guards (pilot finding).
    let shared = q_i64(
        &e.db,
        "SELECT count(*) FROM (SELECT paragraph_id FROM link_edges WHERE status='suggested'
           AND insert_module='native' AND paragraph_id IS NOT NULL GROUP BY paragraph_id HAVING count(*) > 1)",
        vec![],
    )
    .await;
    assert_eq!(shared, 0, "two native suggestions share a paragraph");

    let (p1, p2): (String, String) = {
        let r = q_rows(&e.db, "SELECT (SELECT id FROM posts WHERE url LIKE '%/bike-locks/'), (SELECT id FROM posts WHERE url LIKE '%/bike-chain-cleaning/')", vec![]).await;
        (s(&r[0][0]), s(&r[0][1]))
    };
    let mut pairs = HashSet::new();
    let mut per_source: HashMap<&str, usize> = HashMap::new();
    let mut in_para = 0;
    for (_, src, tgt, anchor, ptext, _) in &edges {
        assert_ne!(src, tgt, "no self links");
        assert!(
            pairs.insert((src.clone(), tgt.clone())),
            "no duplicate pair"
        );
        assert!(
            !(src == &p2 && tgt == &p1),
            "already linked pair re-suggested"
        );
        assert!(!anchor.trim().is_empty());
        assert!(
            anchor.split_whitespace().count() >= 2,
            "no one-word anchors: {anchor}"
        );
        if ptext.to_lowercase().contains(&anchor.to_lowercase()) {
            in_para += 1;
        }
        *per_source.entry(src).or_default() += 1;
    }
    assert!(
        in_para * 2 >= edges.len(),
        "most anchors are in their paragraph ({in_para}/{})",
        edges.len()
    );
    assert!(
        per_source.values().all(|n| *n <= 3),
        "density respected: {per_source:?}"
    );
    let modules = q_i64(
        &e.db,
        "SELECT count(*) FROM link_edges WHERE status='suggested' AND insert_module IS NULL",
        vec![],
    )
    .await;
    assert_eq!(modules, 0);

    // re-plan of an unchanged site: idempotent
    let st2 = run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    assert_eq!(
        st2["candidates"], 0,
        "nothing new on an unchanged site: {st2}"
    );
    assert_eq!(suggested(&e, &site).await.len(), edges.len());
    // reject one pair, re-plan: never re-suggested (its budget slot may go to another pair)
    let (rej_id, rej_src, rej_tgt, ..) = edges[0].clone();
    exec(
        &e.db,
        "UPDATE link_edges SET status='rejected' WHERE id=?1",
        vec![rej_id.clone()],
    )
    .await;
    let st3 = run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    assert!(st3["candidates"].as_i64().unwrap() <= 1, "{st3}");
    let edges2 = suggested(&e, &site).await;
    assert!(!edges2.iter().any(|x| x.1 == rej_src && x.2 == rej_tgt));
    let rows = q_i64(
        &e.db,
        "SELECT count(*) FROM (SELECT source_post_id, target_post_id, count(*) c FROM link_edges WHERE origin IN ('plan','archive') GROUP BY 1,2 HAVING c > 1)",
        vec![],
    )
    .await;
    assert_eq!(rows, 0, "one row per pair after re-plan");
    // re-plans (other topology too) keep one native suggestion per paragraph (pilot: 31 shared
    // paragraphs after a bidirectional re-plan, from re-located existing suggestions)
    run_job(
        &e,
        JobKind::Plan,
        &site,
        json!({"topology": "bidirectional"}),
    )
    .await;
    let shared = q_i64(
        &e.db,
        "SELECT count(*) FROM (SELECT paragraph_id FROM link_edges WHERE status='suggested'
           AND insert_module='native' AND paragraph_id IS NOT NULL GROUP BY paragraph_id HAVING count(*) > 1)",
        vec![],
    )
    .await;
    assert_eq!(
        shared, 0,
        "re-plan: two native suggestions share a paragraph"
    );
    // #6: an editor links a pending pair by hand -> the next plan drops that suggestion.
    let (pend_id, pend_src, pend_tgt, ..) = suggested(&e, &site).await[0].clone();
    exec(
        &e.db,
        "INSERT INTO link_edges(id, site_id, source_post_id, target_post_id, anchor, status, origin)
         VALUES('hand-made', ?1, ?2, ?3, 'editor typed this', 'applied', 'existing')",
        vec![site.clone(), pend_src.clone(), pend_tgt.clone()],
    )
    .await;
    let st4 = run_job(
        &e,
        JobKind::Plan,
        &site,
        json!({"topology": "bidirectional"}),
    )
    .await;
    let after = suggested(&e, &site).await;
    assert!(
        !after
            .iter()
            .any(|x| x.0 == pend_id || (x.1 == pend_src && x.2 == pend_tgt)),
        "pending suggestion of a now-linked pair survived: {st4}"
    );
}

/// Approve up to `n` in-paragraph edges from distinct-or-same sources; returns them.
async fn approve(
    e: &Env,
    site: &str,
    n: usize,
) -> Vec<(String, String, String, String, String, String)> {
    let mut picked = Vec::new();
    for x in suggested(e, site).await {
        if x.4.to_lowercase().contains(&x.3.to_lowercase()) && picked.len() < n {
            exec(
                &e.db,
                "UPDATE link_edges SET status='approved' WHERE id=?1",
                vec![x.0.clone()],
            )
            .await;
            picked.push(x);
        }
    }
    assert!(!picked.is_empty(), "need in-paragraph edges to approve");
    picked
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inject_then_undo_round_trips_raw_content() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let original: HashMap<u64, String> = wp
        .lock()
        .posts
        .values()
        .map(|p| (p.id, p.raw.clone()))
        .collect();
    let e = env("inject");
    let site = add_site(&e, &base, true, settings()).await;
    crawl_and_plan(&e, &site).await;
    let approved = approve(&e, &site, 50).await;
    let ids: Vec<String> = approved.iter().map(|a| a.0.clone()).collect();
    // a suggested (not approved) edge in the payload is ignored server-side
    let other = suggested(&e, &site)
        .await
        .into_iter()
        .find(|x| !ids.contains(&x.0))
        .map(|x| x.0);
    let mut payload_ids = ids.clone();
    payload_ids.extend(other.clone());

    let st = run_job(&e, JobKind::Inject, &site, json!({"edge_ids": payload_ids})).await;
    assert_eq!(st["applied"].as_u64().unwrap() as usize, ids.len(), "{st}");
    assert_eq!(st["failed"], 0, "{st}");
    assert_eq!(st["verified"].as_u64().unwrap() as usize, ids.len(), "{st}");
    let touched: HashSet<&String> = approved.iter().map(|a| &a.1).collect();
    assert_eq!(
        st["posts_updated"].as_u64().unwrap() as usize,
        touched.len()
    );
    if let Some(o) = other {
        let status = q_rows(&e.db, "SELECT status FROM link_edges WHERE id=?1", vec![o]).await;
        assert_eq!(status[0][0], "suggested");
    }

    // stub content: exactly the approved links, block comments intact
    let slugs = slugs(&e).await;
    let w = wp.lock().posts.clone();
    for p in w.values() {
        let orig = &original[&p.id];
        assert_eq!(
            p.raw.matches("<!-- wp:").count(),
            orig.matches("<!-- wp:").count()
        );
        assert_eq!(
            p.raw.matches("<!-- /wp:").count(),
            orig.matches("<!-- /wp:").count()
        );
        let added = p.raw.matches("<a href=").count() - orig.matches("<a href=").count();
        let expected = approved.iter().filter(|a| slugs[&a.1] == p.slug).count();
        assert_eq!(added, expected, "post {} got {added} links", p.slug);
    }
    for a in &approved {
        let post = w.values().find(|p| slugs[&a.1] == p.slug).unwrap();
        assert!(
            post.raw.contains(&format!("<a href=\"{}\">", a.5)),
            "link to {} in {}",
            a.5,
            post.slug
        );
    }
    let snaps = q_i64(
        &e.db,
        "SELECT count(*) FROM snapshots WHERE reason='pre-inject'",
        vec![],
    )
    .await;
    assert_eq!(snaps as usize, touched.len());
    let applied = q_i64(&e.db, "SELECT count(*) FROM link_edges WHERE status='applied' AND origin='plan' AND link_start IS NOT NULL AND snapshot_id IS NOT NULL", vec![]).await;
    assert_eq!(applied as usize, ids.len());

    // undo removes exactly those links: raw is byte-identical to the original again
    let st = run_job(&e, JobKind::Undo, &site, json!({"edge_ids": ids})).await;
    assert_eq!(
        st["reverted"].as_u64().unwrap() as usize,
        approved.len(),
        "{st}"
    );
    assert_eq!(st["not_present"], 0);
    let w = wp.lock().posts.clone();
    for p in w.values() {
        assert_eq!(
            p.raw, original[&p.id],
            "post {} restored byte-for-byte",
            p.slug
        );
    }
    let reverted = q_i64(
        &e.db,
        "SELECT count(*) FROM link_edges WHERE status='reverted'",
        vec![],
    )
    .await;
    assert_eq!(reverted as usize, approved.len());
}

/// post id -> slug (last path segment of the URL).
async fn slugs(e: &Env) -> HashMap<String, String> {
    q_rows(&e.db, "SELECT id, url FROM posts", vec![])
        .await
        .into_iter()
        .map(|r| {
            (
                s(&r[0]),
                s(&r[1])
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_string(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inject_conflict_rereads_once_then_fails_cleanly() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let e = env("conflict");
    let site = add_site(&e, &base, true, settings()).await;
    crawl_and_plan(&e, &site).await;
    let approved = approve(&e, &site, 50).await;
    // pick two edges from two different source posts
    let mut by_src: Vec<(String, String, String, String, String, String)> = Vec::new();
    for a in approved {
        if !by_src.iter().any(|b| b.1 == a.1) {
            by_src.push(a);
        }
    }
    assert!(by_src.len() >= 2, "need edges from two posts");
    let (a, b) = (by_src[0].clone(), by_src[1].clone());
    let slugs = slugs(&e).await;
    let wp_id = |post_id: &str| -> u64 {
        let slug = &slugs[post_id];
        wp.lock()
            .posts
            .values()
            .find(|p| &p.slug == slug)
            .unwrap()
            .id
    };
    // A: the post changes once between our read and the write → one re-read, then success
    let (ida, idb) = (wp_id(&a.1), wp_id(&b.1));
    wp.lock().bump_on_get.insert(ida, 1);
    let st = run_job(
        &e,
        JobKind::Inject,
        &site,
        json!({"edge_ids": [a.0.clone()]}),
    )
    .await;
    assert_eq!(st["applied"], 1, "{st}");
    assert_eq!(st["conflicts"], 1);
    // B: the post keeps changing → conflict, nothing written, edge failed with failure_kind
    let updates_before = wp.lock().updates;
    wp.lock().bump_on_get.insert(idb, 100);
    let raw_before = wp.lock().posts[&idb].raw.clone();
    let st = run_job(
        &e,
        JobKind::Inject,
        &site,
        json!({"edge_ids": [b.0.clone()]}),
    )
    .await;
    assert_eq!(st["applied"], 0, "{st}");
    assert_eq!(st["failed"], 1);
    assert_eq!(wp.lock().updates, updates_before, "no write on conflict");
    assert_eq!(wp.lock().posts[&idb].raw, raw_before);
    let r = q_rows(
        &e.db,
        "SELECT status, failure_kind FROM link_edges WHERE id=?1",
        vec![b.0.clone()],
    )
    .await;
    assert_eq!(r[0][0], "failed");
    assert_eq!(r[0][1], "conflict");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inject_without_credentials_marks_for_plugin_pull() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let e = env("nocreds");
    let site = add_site(&e, &base, false, settings()).await;
    let st = run_job(&e, JobKind::Crawl, &site, json!({})).await;
    assert_eq!(st["raw_content"], false);
    run_job(&e, JobKind::Plan, &site, json!({})).await;
    let approved = approve(&e, &site, 2).await;
    let ids: Vec<String> = approved.iter().map(|a| a.0.clone()).collect();
    let st = run_job(&e, JobKind::Inject, &site, json!({"edgeIds": ids})).await;
    assert_eq!(
        st["marked"].as_u64().unwrap() as usize,
        approved.len(),
        "{st}"
    );
    assert_eq!(wp.lock().updates, 0);
    let still = q_i64(
        &e.db,
        "SELECT count(*) FROM link_edges WHERE status='approved' AND failure_kind='pending_plugin'",
        vec![],
    )
    .await;
    assert_eq!(still as usize, approved.len());
}

/// Pilot 2026-10-13: a native edge whose anchor was edited away in WordPress after planning was
/// silently "marked" (no error, stays approved forever). It now carries the reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inject_anchor_edited_away_is_marked_with_a_reason() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let e = env("anchorgone");
    let site = add_site(&e, &base, true, settings()).await;
    crawl_and_plan(&e, &site).await;
    let approved = approve(&e, &site, 1).await;
    let (id, source, anchor) = (
        approved[0].0.clone(),
        approved[0].1.clone(),
        approved[0].3.clone(),
    );
    exec(
        &e.db,
        "UPDATE link_edges SET insert_module='native' WHERE id=?1",
        vec![id.clone()],
    )
    .await;
    let slug = slugs(&e).await[&source].clone();
    {
        let mut w = wp.lock();
        let p = w.posts.values_mut().find(|p| p.slug == slug).unwrap();
        // Replace every case variant of the anchor with unrelated words.
        let lower = p.raw.to_lowercase();
        let needle = anchor.to_lowercase();
        let mut out = String::new();
        let mut i = 0;
        while let Some(k) = lower[i..].find(&needle) {
            out.push_str(&p.raw[i..i + k]);
            out.push_str("zzq qqz");
            i += k + needle.len();
        }
        out.push_str(&p.raw[i..]);
        p.raw = out;
        p.modified += 1;
    }
    let before = wp.lock().updates;
    let st = run_job(
        &e,
        JobKind::Inject,
        &site,
        json!({"edge_ids": [id.clone()]}),
    )
    .await;
    assert_eq!(st["marked"], 1, "{st}");
    assert_eq!(st["applied"], 0, "{st}");
    assert_eq!(wp.lock().updates, before, "nothing written");
    let r = q_rows(
        &e.db,
        "SELECT status, failure_kind, ifnull(error,'') FROM link_edges WHERE id=?1",
        vec![id],
    )
    .await;
    assert_eq!(r[0][0], "approved");
    assert_eq!(r[0][1], "content_gap");
    assert!(
        s(&r[0][2]).contains("no longer in the post"),
        "{:?}",
        r[0][2]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_mid_plan_stops_the_job_and_the_runner_continues() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    // slow embedder: 2 texts per batch, 150 ms per batch → the embed phase takes seconds
    let e = env_with("cancel", Duration::from_millis(150), 2);
    let site = add_site(&e, &base, true, settings()).await;
    run_job(&e, JobKind::Crawl, &site, json!({})).await;
    let mut opts = RunnerOptions::from_config(&e.ctx.cfg);
    opts.heartbeat = Duration::from_millis(100);
    opts.poll = Duration::from_millis(50);
    let runner = Runner::start_with(e.ctx.clone(), opts);
    let plan = e
        .ctx
        .queue
        .enqueue(NewJob::new(JobKind::Plan, Some(&site), json!({})))
        .await
        .unwrap();
    // wait until the plan is embedding
    let mut phase = String::new();
    for _ in 0..200 {
        let j = e.ctx.queue.get(&plan.job_run_id).await.unwrap();
        phase = j["stats"]["progress"]["phase"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if phase == "embed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(phase, "embed");
    e.ctx.queue.cancel(&plan.job_run_id).await.unwrap();
    // the runner frees the slot quickly
    for _ in 0..200 {
        if runner.running().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(runner.running().is_empty(), "cancelled plan still running");
    let mut j = e.ctx.queue.get(&plan.job_run_id).await.unwrap();
    for _ in 0..100 {
        if j["resources"]["wall_ms"].is_number() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        j = e.ctx.queue.get(&plan.job_run_id).await.unwrap();
    }
    assert_eq!(j["status"], "cancelled");
    // pilot 2026-10-13: cancelled runs kept `resources: {}`
    assert!(j["resources"]["wall_ms"].is_number(), "{}", j["resources"]);
    let edges = q_i64(
        &e.db,
        "SELECT count(*) FROM link_edges WHERE origin='plan'",
        vec![],
    )
    .await;
    assert_eq!(edges, 0, "cancelled before any suggestion was saved");
    // same site can run again right away
    let c = e
        .ctx
        .queue
        .enqueue(NewJob::new(JobKind::KeywordLink, Some(&site), json!({})))
        .await
        .unwrap();
    let mut status = Value::Null;
    for _ in 0..200 {
        status = e.ctx.queue.get(&c.job_run_id).await.unwrap()["status"].clone();
        if status == "done" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(status, "done");
    runner.shutdown(Duration::from_secs(2)).await;
}

fn sleeper(
    active: Arc<HashMap<String, AtomicUsize>>,
    max: Arc<HashMap<String, AtomicUsize>>,
    overlap: Arc<AtomicUsize>,
    global: Arc<AtomicUsize>,
) -> PipelineFn {
    Arc::new(move |_ctx, job| {
        let (active, max, overlap, global) =
            (active.clone(), max.clone(), overlap.clone(), global.clone());
        async move {
            let site = job.site_id.clone().unwrap_or_default();
            let a = active.get(&site).unwrap();
            let now = a.fetch_add(1, Ordering::SeqCst) + 1;
            max.get(&site).unwrap().fetch_max(now, Ordering::SeqCst);
            let g = global.fetch_add(1, Ordering::SeqCst) + 1;
            overlap.fetch_max(g, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(250)).await;
            a.fetch_sub(1, Ordering::SeqCst);
            global.fetch_sub(1, Ordering::SeqCst);
            Ok(json!({"ok": true}))
        }
        .boxed()
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_site_exclusivity_with_parallel_sites() {
    let e = env("excl");
    let a = add_site(&e, "http://127.0.0.1:9", false, json!({})).await;
    let b = add_site(&e, "http://127.0.0.2:9", false, json!({})).await;
    let mk = || {
        Arc::new(HashMap::from([
            (a.clone(), AtomicUsize::new(0)),
            (b.clone(), AtomicUsize::new(0)),
        ]))
    };
    let (active, max) = (mk(), mk());
    let overlap = Arc::new(AtomicUsize::new(0));
    let global = Arc::new(AtomicUsize::new(0));
    let f = sleeper(active, max.clone(), overlap.clone(), global);
    let mut opts = RunnerOptions::from_config(&e.ctx.cfg);
    opts.max_concurrent = 3;
    opts.poll = Duration::from_millis(20);
    opts.overrides.insert("crawl".into(), f.clone());
    opts.overrides.insert("plan".into(), f.clone());
    opts.overrides.insert("inject".into(), f);
    let mut ids = Vec::new();
    for (kind, site) in [
        (JobKind::Crawl, &a),
        (JobKind::Plan, &a),
        (JobKind::Inject, &a),
        (JobKind::Crawl, &b),
    ] {
        ids.push(
            e.ctx
                .queue
                .enqueue(NewJob::new(kind, Some(site), json!({})))
                .await
                .unwrap()
                .job_run_id,
        );
    }
    let runner = Runner::start_with(e.ctx.clone(), opts);
    for _ in 0..200 {
        let mut all = true;
        for id in &ids {
            all &= e.ctx.queue.get(id).await.unwrap()["status"] == "done";
        }
        if all {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    for id in &ids {
        assert_eq!(e.ctx.queue.get(id).await.unwrap()["status"], "done");
    }
    assert_eq!(
        max[&a].load(Ordering::SeqCst),
        1,
        "site A jobs never overlapped"
    );
    assert_eq!(
        overlap.load(Ordering::SeqCst),
        2,
        "site B ran alongside site A"
    );
    runner.shutdown(Duration::from_secs(2)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runner_survives_panic_timeout_and_reaps_stale_rows() {
    let e = env("panic");
    let site = add_site(&e, "http://127.0.0.1:9", false, json!({})).await;
    // a stale running row from a "previous process" (plan: not idempotent, so it is failed;
    // crawl/link_health/embed rows are re-queued instead, see queue::tests R15)
    let stale = sf_core::new_id();
    exec(
        &e.db,
        "INSERT INTO job_runs(id, site_id, kind, status, heartbeat_at, started_at) VALUES(?1, ?2, 'plan', 'running', '2020-01-01T00:00:00.000Z', '2020-01-01T00:00:00.000Z')",
        vec![stale.clone(), site.clone()],
    )
    .await;
    let mut opts = RunnerOptions::from_config(&e.ctx.cfg);
    opts.poll = Duration::from_millis(20);
    opts.heartbeat = Duration::from_millis(50);
    opts.cancel_grace = Duration::from_millis(100);
    opts.timeout_override = Some(Duration::from_millis(300));
    let boom: PipelineFn = Arc::new(|_c, _j| async move { panic!("kaboom") }.boxed());
    let hang: PipelineFn = Arc::new(|_c, _j| {
        async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(json!({}))
        }
        .boxed()
    });
    let fine: PipelineFn = Arc::new(|_c, _j| async move { Ok(json!({"fine": true})) }.boxed());
    opts.overrides.insert("maintenance".into(), boom);
    opts.overrides.insert("link_health".into(), hang);
    opts.overrides.insert("backup".into(), fine);
    let runner = Runner::start_with(e.ctx.clone(), opts);
    let p = e
        .ctx
        .queue
        .enqueue(NewJob::new(JobKind::Maintenance, None, json!({})))
        .await
        .unwrap();
    let mut lh = NewJob::new(JobKind::LinkHealth, Some(&site), json!({}));
    lh.max_attempts = Some(1);
    let h = e.ctx.queue.enqueue(lh).await.unwrap();
    let b = e
        .ctx
        .queue
        .enqueue(NewJob::new(JobKind::Backup, None, json!({})))
        .await
        .unwrap();
    let wait = |id: String, want: &'static str| {
        let q = e.ctx.queue.clone();
        async move {
            for _ in 0..200 {
                if q.get(&id).await.unwrap()["status"] == want {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            false
        }
    };
    assert!(wait(p.job_run_id.clone(), "failed").await);
    assert!(
        wait(b.job_run_id.clone(), "done").await,
        "runner kept going after a panic"
    );
    assert!(wait(h.job_run_id.clone(), "failed").await, "hard timeout");
    let pj = e.ctx.queue.get(&p.job_run_id).await.unwrap();
    assert!(
        pj["error"].as_str().unwrap().contains("panicked: kaboom"),
        "{pj}"
    );
    let hj = e.ctx.queue.get(&h.job_run_id).await.unwrap();
    assert!(hj["error"].as_str().unwrap().contains("timed out"), "{hj}");
    let sj = e.ctx.queue.get(&stale).await.unwrap();
    assert_eq!(sj["status"], "failed");
    assert!(sj["error"].as_str().unwrap().contains("reaped"));
    let errs = q_i64(
        &e.db,
        "SELECT count(*) FROM errors WHERE source LIKE 'job:%'",
        vec![],
    )
    .await;
    assert!(errs >= 1);
    runner.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scheduler_enqueues_due_rows_and_stamps_every_kind() {
    let e = env("sched");
    let site = add_site(&e, "http://127.0.0.1:9", false, json!({})).await;
    let past = "2020-01-01T00:00:00.000Z".to_string();
    for (kind, next, tz) in [
        ("crawl", Some(past.clone()), "UTC"),
        ("plan", Some(past.clone()), "Europe/Berlin"),
        ("link_health", None, "Not/AZone"),
    ] {
        exec(
            &e.db,
            "INSERT INTO site_schedules(id, site_id, job_kind, cron, tz, payload, next_run_at) VALUES(?1,?2,?3,'*/5 * * * *',?4,?5,?6)",
            vec![
                sf_core::new_id(),
                site.clone(),
                kind.into(),
                tz.into(),
                if kind == "plan" { r#"{"topology":"circle"}"#.into() } else { "{}".into() },
                next.unwrap_or_default(),
            ],
        )
        .await;
    }
    exec(
        &e.db,
        "UPDATE site_schedules SET next_run_at=NULL WHERE next_run_at=''",
        vec![],
    )
    .await;
    let now = chrono::Utc::now();
    let rep = Scheduler::tick(&e.ctx, now).await.unwrap();
    assert_eq!(rep.enqueued.len(), 2, "{rep:?}");
    // the schedule row with no stamp + the daily maintenance slot (kv maintenance_next_at)
    assert_eq!(rep.initialized, 2);
    let rows = q_rows(&e.db, "SELECT job_kind, last_run_at, last_job_run_id, next_run_at FROM site_schedules ORDER BY job_kind", vec![]).await;
    for r in &rows {
        assert!(!r[3].is_null(), "next_run_at stamped for {}", r[0]);
        assert!(s(&r[3]) > now.to_rfc3339(), "next run is in the future");
        if r[0] != "link_health" {
            assert!(
                !r[1].is_null() && !r[2].is_null(),
                "last_run stamped for {}",
                r[0]
            );
        }
    }
    let plan_payload = q_rows(
        &e.db,
        "SELECT payload, requested_by FROM job_runs WHERE kind='plan'",
        vec![],
    )
    .await;
    assert_eq!(plan_payload.len(), 1);
    assert!(s(&plan_payload[0][0]).contains("circle"));
    assert_eq!(plan_payload[0][1], "schedule");
    // a second tick at the same time enqueues nothing
    let rep = Scheduler::tick(&e.ctx, now).await.unwrap();
    assert!(rep.enqueued.is_empty(), "{rep:?}");
    assert_eq!(
        q_i64(&e.db, "SELECT count(*) FROM job_runs", vec![]).await,
        2
    );
    // a due row whose job is still queued is deduplicated (no second row)
    exec(
        &e.db,
        "UPDATE site_schedules SET next_run_at=?1 WHERE job_kind='crawl'",
        vec![past],
    )
    .await;
    let rep = Scheduler::tick(&e.ctx, now).await.unwrap();
    assert_eq!(rep.enqueued.len(), 1);
    assert_eq!(
        q_i64(&e.db, "SELECT count(*) FROM job_runs", vec![]).await,
        2
    );
}

/// Pilot 2026-10-13: link_health checked every external (affiliate) URL with no opt-out.
/// `internal_only` lists external links but never requests them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn link_health_internal_only_skips_external_checks() {
    let (base, wp) = start_wp(vec![]).await;
    let mut posts = bike_posts(&base);
    posts[0].raw.push_str(
        "<!-- wp:paragraph -->\n<p>Buy it <a href=\"http://shop.invalid/item\">at the shop</a> today.</p>\n<!-- /wp:paragraph -->\n",
    );
    wp.lock().posts = posts.into_iter().map(|p| (p.id, p)).collect();
    let e = env("lhinternal");
    let site = add_site(&e, &base, true, settings()).await;
    run_job(&e, JobKind::Crawl, &site, json!({})).await;
    let st = run_job(
        &e,
        JobKind::LinkHealth,
        &site,
        json!({"internal_only": true}),
    )
    .await;
    assert!(st["skipped"].as_i64().unwrap() >= 1, "{st}");
    let r = q_rows(
        &e.db,
        "SELECT scope, ifnull(status_code,-1), ifnull(checked_at,'') FROM link_health WHERE href LIKE 'http://shop.invalid%'",
        vec![],
    )
    .await;
    assert_eq!(r.len(), 1, "external row still listed");
    assert_eq!(r[0][0], "external");
    assert_eq!(r[0][1], -1, "never requested: {r:?}");
    let internal_checked = q_i64(
        &e.db,
        "SELECT count(*) FROM link_health WHERE scope='internal' AND status_code IS NOT NULL",
        vec![],
    )
    .await;
    assert!(internal_checked >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keyword_link_link_health_and_audit() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let e = env("audit");
    let site = add_site(&e, &base, true, settings()).await;
    let st = run_job(&e, JobKind::Audit, &site, json!({"topology": "reverse"})).await;
    let stages = st["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 3, "{st}");
    assert!(stages.iter().all(|s| s["ok"] == true), "{st}");
    assert_eq!(st["metrics_captured"], true);
    assert_eq!(
        q_i64(
            &e.db,
            "SELECT count(*) FROM site_metrics_history WHERE source='audit'",
            vec![]
        )
        .await,
        1
    );
    let lh = &stages[1]["stats"];
    assert!(lh["checked"].as_i64().unwrap() >= 1, "{lh}");
    assert_eq!(lh["broken"], 0);
    let rollup = q_i64(
        &e.db,
        "SELECT count(*) FROM posts WHERE link_health_checked_at IS NOT NULL",
        vec![],
    )
    .await;
    assert_eq!(rollup, 6);

    // keyword rule → the pumps post (plan suggestions cleared so their pairs/paragraphs are free)
    exec(
        &e.db,
        "DELETE FROM link_edges WHERE origin IN ('plan','archive')",
        vec![],
    )
    .await;
    let target_url = format!("{base}/bike-pumps/");
    exec(
        &e.db,
        "INSERT INTO keyword_rules(id, site_id, keyword, target_url, anchor_type, max_links) VALUES(?1,?2,'road bike saddles',?3,'exact',2)",
        vec![sf_core::new_id(), site.clone(), target_url],
    )
    .await;
    let st = run_job(&e, JobKind::KeywordLink, &site, json!({})).await;
    let created = st["created"].as_i64().unwrap();
    assert!((1..=2).contains(&created), "{st}");
    // #20: the site's max_links_per_post (10 in SITE_SETTINGS) caps keyword edges per source.
    assert_eq!(st["max_per_source"], 10, "{st}");
    let kl = q_rows(&e.db, "SELECT anchor, score_breakdown, paragraph_id FROM link_edges WHERE origin='keyword_linker'", vec![]).await;
    assert!(kl.iter().all(|r| r[0] == "road bike saddles"
        && s(&r[1]).contains("keyword_rule")
        && !r[2].is_null()));
    // re-running creates nothing new
    let st = run_job(&e, JobKind::KeywordLink, &site, json!({})).await;
    assert_eq!(st["created"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inject_rephrase_module_uses_validated_llm_output() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let ai = start_ai().await;
    let e = env_with_ai("llm", &ai);
    let site = add_site(
        &e,
        &base,
        true,
        json!({"insert_modules": ["native", "rephrase"]}),
    )
    .await;
    crawl_and_plan(&e, &site).await;
    let edge = suggested(&e, &site)
        .await
        .into_iter()
        .find(|x| !x.4.is_empty())
        .expect("an edge with a paragraph");
    // an anchor that is NOT in the paragraph forces the LLM path
    exec(
        &e.db,
        "UPDATE link_edges SET status='approved', anchor='gravel trail comfort', insert_module='rephrase' WHERE id=?1",
        vec![edge.0.clone()],
    )
    .await;
    let st = run_job(
        &e,
        JobKind::Inject,
        &site,
        json!({"edge_ids": [edge.0.clone()]}),
    )
    .await;
    assert_eq!(st["applied"], 1, "{st}");
    let r = q_rows(
        &e.db,
        "SELECT status, insert_module FROM link_edges WHERE id=?1",
        vec![edge.0.clone()],
    )
    .await;
    assert_eq!(r[0][0], "applied");
    assert_eq!(r[0][1], "rephrase");
    let slugs = slugs(&e).await;
    let w = wp.lock().posts.clone();
    let post = w.values().find(|p| p.slug == slugs[&edge.1]).unwrap();
    assert!(
        post.raw.contains(&format!(
            "<a href=\"{}\">gravel trail comfort guide</a>",
            edge.5
        )),
        "{}",
        post.raw
    );
    let anchor = q_rows(
        &e.db,
        "SELECT anchor FROM link_edges WHERE id=?1",
        vec![edge.0.clone()],
    )
    .await;
    assert_eq!(
        anchor[0][0], "gravel trail comfort guide",
        "edge records the linked text"
    );
    assert!(!post.raw.contains("```"), "code fences stripped");
    let orig = bike_posts(&base)
        .into_iter()
        .find(|p| p.slug == post.slug)
        .unwrap();
    assert_eq!(
        post.raw.matches("<!-- wp:").count(),
        orig.raw.matches("<!-- wp:").count()
    );
    // precise undo finds the LLM link by its recorded text
    let st = run_job(
        &e,
        JobKind::Undo,
        &site,
        json!({"edge_ids": [edge.0.clone()]}),
    )
    .await;
    assert_eq!(st["reverted"], 1, "{st}");
    assert_eq!(st["not_present"], 0, "{st}");
    let raw = wp
        .lock()
        .posts
        .values()
        .find(|p| p.slug == slugs[&edge.1])
        .unwrap()
        .raw
        .clone();
    assert!(!raw.contains(&format!("<a href=\"{}\">", edge.5)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hostile_payloads_and_empty_sites_never_fail() {
    let (base, _wp) = start_wp(vec![]).await;
    let e = env("hostile");
    let site = add_site(
        &e,
        &base,
        true,
        json!({"insert_modules": "nope", "max_links_per_post": -5, "money_page_urls": [1, null]}),
    )
    .await;
    let st = run_job(&e, JobKind::Crawl, &site, json!({"x": [1, 2, 3]})).await;
    assert_eq!(st["upserted"], 0);
    let st = run_job(
        &e,
        JobKind::Plan,
        &site,
        json!({"topology": "zigzag", "k": 999_999, "archive": "yes"}),
    )
    .await;
    assert_eq!(st["posts"], 0);
    let st = run_job(
        &e,
        JobKind::Inject,
        &site,
        json!({"edge_ids": "not-a-list"}),
    )
    .await;
    assert_eq!(st["applied"], 0);
    let st = run_job(
        &e,
        JobKind::Undo,
        &site,
        json!({"edgeIds": [1, null, {"a": 1}, "\u{0}"], "mode": 7}),
    )
    .await;
    assert_eq!(st["reverted"], 0);
    let st = run_job(&e, JobKind::KeywordLink, &site, json!(null)).await;
    assert_eq!(st["created"], 0);
    let st = run_job(&e, JobKind::LinkHealth, &site, json!([])).await;
    assert_eq!(st["checked"], 0);
}
