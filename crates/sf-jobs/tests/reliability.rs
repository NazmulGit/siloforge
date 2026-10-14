//! Reliability regressions (docs/testing/reliability-audit.md): R4 inject/undo journal +
//! reconcile, R9 snapshot-undo refusal, G2-3 daily maintenance scheduling, R3 cancel at runner level.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use common::*;
use serde_json::{Value, json};
use sf_core::types::JobKind;
use sf_jobs::{NewJob, Runner, RunnerOptions, Scheduler};

fn settings() -> Value {
    json!({"max_new_links_per_pass": 3, "max_links_per_post": 10})
}

/// Crawl + plan the bike stub and approve every in-paragraph suggestion.
/// Returns (env, site, wp, approved edge ids, target urls by edge id).
async fn planned(tag: &str) -> (Env, String, Wp, Vec<String>) {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let e = env(tag);
    let site = add_site(&e, &base, true, settings()).await;
    run_job(&e, JobKind::Crawl, &site, json!({})).await;
    run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    let rows = q_rows(
        &e.db,
        "SELECT e.id, e.anchor, ifnull(pa.text,'') FROM link_edges e
         LEFT JOIN paragraphs pa ON pa.id=e.paragraph_id
         WHERE e.site_id=?1 AND e.status='suggested' ORDER BY e.id",
        vec![site.clone()],
    )
    .await;
    let mut ids = Vec::new();
    for r in rows {
        if s(&r[2]).to_lowercase().contains(&s(&r[1]).to_lowercase()) {
            exec(
                &e.db,
                "UPDATE link_edges SET status='approved' WHERE id=?1",
                vec![s(&r[0])],
            )
            .await;
            ids.push(s(&r[0]));
        }
    }
    assert!(!ids.is_empty());
    (e, site, wp, ids)
}

/// One applied edge's source post: (post id, wp id, post type).
async fn source_of(e: &Env, edge: &str) -> (String, u64, String) {
    let r = q_rows(
        &e.db,
        "SELECT p.id, p.wp_post_id, p.post_type FROM link_edges e JOIN posts p ON p.id=e.source_post_id WHERE e.id=?1",
        vec![edge.into()],
    )
    .await;
    (s(&r[0][0]), r[0][1].as_u64().unwrap(), s(&r[0][2]))
}

/// Journal edges JSON for `edges` (as inject would have written them).
async fn journal_edges(e: &Env, edges: &[String]) -> String {
    let mut v = Vec::new();
    for id in edges {
        let r = q_rows(
            &e.db,
            "SELECT t.url, e.anchor, e.link_start, e.insert_module FROM link_edges e JOIN posts t ON t.id=e.target_post_id WHERE e.id=?1",
            vec![id.clone()],
        )
        .await;
        v.push(json!({"id": id, "url": r[0][0], "text": r[0][1], "link_start": r[0][2], "module": r[0][3]}));
    }
    Value::Array(v).to_string()
}

async fn status(e: &Env, edge: &str) -> String {
    s(&q_rows(
        &e.db,
        "SELECT status FROM link_edges WHERE id=?1",
        vec![edge.into()],
    )
    .await[0][0])
}

/// R4: an inject aborted after WordPress accepted the write (edges still `approved`, journal row
/// left) is finished by the next inject's reconcile; one whose write never landed is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inject_journal_is_reconciled_against_live_content() {
    let (e, site, wp, ids) = planned("rel-journal").await;
    let edge = ids[0].clone();
    let (post, wp_id, ptype) = source_of(&e, &edge).await;
    let before = wp.lock().posts[&wp_id].raw.clone();
    // the real write happens ...
    let st = run_job(
        &e,
        JobKind::Inject,
        &site,
        json!({"edge_ids": [edge.clone()]}),
    )
    .await;
    assert_eq!(st["applied"], 1, "{st}");
    let after = wp.lock().posts[&wp_id].raw.clone();
    let snap = s(&q_rows(
        &e.db,
        "SELECT snapshot_id FROM link_edges WHERE id=?1",
        vec![edge.clone()],
    )
    .await[0][0]);
    let jedges = journal_edges(&e, std::slice::from_ref(&edge)).await;
    // ... but the job "died" before its bookkeeping: edge back to approved, journal row left
    exec(
        &e.db,
        "UPDATE link_edges SET status='approved', applied_at=NULL, snapshot_id=NULL WHERE id=?1",
        vec![edge.clone()],
    )
    .await;
    exec(
        &e.db,
        "INSERT INTO inject_journal(id, site_id, post_id, op, snapshot_id, wp_post_id, post_type, edges, expected, previous)
         VALUES('j-landed', ?1, ?2, 'inject', ?3, ?4, ?5, ?6, ?7, ?8)",
        vec![site.clone(), post.clone(), snap.clone(), wp_id.to_string(), ptype.clone(), jedges.clone(), after.clone(), before.clone()],
    )
    .await;
    // a second unfinished write that never reached WordPress (live == previous)
    exec(
        &e.db,
        "INSERT INTO snapshots(id, post_id, content_html) VALUES('snap-orphan', ?1, 'x')",
        vec![post.clone()],
    )
    .await;
    exec(
        &e.db,
        "INSERT INTO inject_journal(id, site_id, post_id, op, snapshot_id, wp_post_id, post_type, edges, expected, previous)
         VALUES('j-lost', ?1, ?2, 'inject', 'snap-orphan', ?3, ?4, '[]', 'never written', ?5)",
        vec![site.clone(), post.clone(), wp_id.to_string(), ptype.clone(), after.clone()],
    )
    .await;
    let updates = wp.lock().updates;
    let st = run_job(&e, JobKind::Inject, &site, json!({"reconcile_only": true})).await;
    assert_eq!(st["reconcile"]["landed"], 1, "{st}");
    assert_eq!(st["reconcile"]["not_landed"], 1, "{st}");
    assert_eq!(
        wp.lock().updates,
        updates,
        "reconcile never writes to WordPress"
    );
    assert_eq!(status(&e, &edge).await, "applied");
    let r = q_rows(
        &e.db,
        "SELECT snapshot_id, link_start FROM link_edges WHERE id=?1",
        vec![edge.clone()],
    )
    .await;
    assert_eq!(s(&r[0][0]), snap);
    assert!(!r[0][1].is_null());
    assert_eq!(
        q_i64(&e.db, "SELECT count(*) FROM inject_journal", vec![]).await,
        0
    );
    assert_eq!(
        q_i64(
            &e.db,
            "SELECT count(*) FROM snapshots WHERE id='snap-orphan'",
            vec![]
        )
        .await,
        0,
        "unused snapshot of a write that never landed is dropped (R13)"
    );
    // and the link is still undoable precisely
    let st = run_job(
        &e,
        JobKind::Undo,
        &site,
        json!({"edge_ids": [edge.clone()]}),
    )
    .await;
    assert_eq!(st["reverted"], 1, "{st}");
    assert_eq!(wp.lock().posts[&wp_id].raw, before);
}

/// R4: WordPress committed the update but the response was lost (500): the post is re-read and
/// the edge counts as applied instead of "WordPress update failed".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_update_response_is_confirmed_by_reread() {
    let (e, site, wp, ids) = planned("rel-lost").await;
    let edge = ids[0].clone();
    wp.lock().fail_after_update = 1;
    let st = run_job(
        &e,
        JobKind::Inject,
        &site,
        json!({"edge_ids": [edge.clone()]}),
    )
    .await;
    assert_eq!(st["applied"], 1, "{st}");
    assert_eq!(st["failed"], 0, "{st}");
    assert_eq!(status(&e, &edge).await, "applied");
    assert_eq!(
        q_i64(&e.db, "SELECT count(*) FROM inject_journal", vec![]).await,
        0
    );
}

/// R9: snapshot-mode undo refuses when the post was edited in WordPress after SiloForge wrote it,
/// unless forced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_undo_refuses_after_editor_change_unless_forced() {
    let (e, site, wp, ids) = planned("rel-snap").await;
    let edge = ids[0].clone();
    let (_, wp_id, _) = source_of(&e, &edge).await;
    let st = run_job(
        &e,
        JobKind::Inject,
        &site,
        json!({"edge_ids": [edge.clone()]}),
    )
    .await;
    assert_eq!(st["applied"], 1, "{st}");
    // an editor saves the post afterwards
    {
        let mut w = wp.lock();
        let p = w.posts.get_mut(&wp_id).unwrap();
        p.raw
            .push_str("\n<!-- wp:paragraph --><p>Editor addition.</p><!-- /wp:paragraph -->");
        p.modified += 1;
    }
    let st = run_job(
        &e,
        JobKind::Undo,
        &site,
        json!({"edge_ids": [edge.clone()], "mode": "snapshot"}),
    )
    .await;
    assert_eq!(st["refused"], 1, "{st}");
    assert_eq!(st["reverted"], 0, "{st}");
    assert_eq!(status(&e, &edge).await, "applied");
    assert!(wp.lock().posts[&wp_id].raw.contains("Editor addition"));
    let st = run_job(
        &e,
        JobKind::Undo,
        &site,
        json!({"edge_ids": [edge.clone()], "mode": "snapshot", "force": true}),
    )
    .await;
    assert_eq!(st["reverted"], 1, "{st}");
    assert_eq!(status(&e, &edge).await, "reverted");
    // without an editor change a snapshot undo is allowed
    let edge2 = ids.iter().find(|i| **i != edge).cloned();
    if let Some(edge2) = edge2 {
        let st = run_job(
            &e,
            JobKind::Inject,
            &site,
            json!({"edge_ids": [edge2.clone()]}),
        )
        .await;
        if st["applied"] == 1 {
            let st = run_job(
                &e,
                JobKind::Undo,
                &site,
                json!({"edge_ids": [edge2.clone()], "mode": "snapshot"}),
            )
            .await;
            assert_eq!(st["refused"], 0, "{st}");
            assert!(st["reverted"].as_u64().unwrap() >= 1, "{st}");
        }
    }
}

/// R4 boot: the runner queues a reconcile-only inject for a site with unfinished writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runner_boot_queues_reconcile_for_unfinished_writes() {
    let e = env("rel-boot");
    let site = add_site(&e, "http://127.0.0.1:9", false, json!({})).await;
    exec(
        &e.db,
        "INSERT INTO posts(id, site_id, url, title) VALUES('p-boot', ?1, 'http://127.0.0.1:9/a/', 'A')",
        vec![site.clone()],
    )
    .await;
    exec(
        &e.db,
        "INSERT INTO inject_journal(id, site_id, post_id, op, wp_post_id, post_type, expected, previous)
         VALUES('j1', ?1, 'p-boot', 'inject', 1, 'post', 'x', 'y')",
        vec![site.clone()],
    )
    .await;
    let mut opts = RunnerOptions::from_config(&e.ctx.cfg);
    opts.poll = Duration::from_millis(20);
    let runner = Runner::start_with(e.ctx.clone(), opts);
    let mut seen = false;
    for _ in 0..200 {
        let n = q_i64(
            &e.db,
            "SELECT count(*) FROM job_runs WHERE kind='inject' AND requested_by='boot-reconcile' AND status='done'",
            vec![],
        )
        .await;
        if n == 1 {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(seen, "boot reconcile job ran");
    // no credentials: the row stays for a later run (nothing could be checked)
    assert_eq!(
        q_i64(&e.db, "SELECT count(*) FROM inject_journal", vec![]).await,
        1
    );
    runner.shutdown(Duration::from_secs(1)).await;
}

/// G2-3: the scheduler queues the global maintenance job daily (monitor.maintenance_cron).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scheduler_queues_daily_maintenance() {
    let e = env("rel-maint");
    let now = chrono::Utc::now();
    let rep = Scheduler::tick(&e.ctx, now).await.unwrap();
    assert!(rep.enqueued.is_empty() && rep.initialized == 1, "{rep:?}");
    exec(
        &e.db,
        "UPDATE kv SET value='2020-01-01T00:00:00.000Z' WHERE key='maintenance_next_at'",
        vec![],
    )
    .await;
    let rep = Scheduler::tick(&e.ctx, now).await.unwrap();
    assert_eq!(rep.enqueued.len(), 1, "{rep:?}");
    let r = q_rows(
        &e.db,
        "SELECT kind, requested_by, site_id FROM job_runs",
        vec![],
    )
    .await;
    assert_eq!(
        (s(&r[0][0]), s(&r[0][1])),
        ("maintenance".into(), "schedule".into())
    );
    assert!(r[0][2].is_null());
    let next = q_rows(
        &e.db,
        "SELECT value FROM kv WHERE key='maintenance_next_at'",
        vec![],
    )
    .await;
    assert!(s(&next[0][0]) > now.to_rfc3339());
    // the job itself runs the retention prune
    let claimed = e.ctx.queue.claim_next(true).await.unwrap().unwrap();
    let job = std::sync::Arc::new(sf_jobs::Job::new(claimed, e.ctx.queue.clone()));
    let st = sf_jobs::pipelines::dispatch(e.ctx.clone(), job)
        .await
        .unwrap();
    assert_eq!(st["analyzed"], true, "{st}");
}

/// R3 at runner level: a cancelled job keeps the site until its pipeline actually returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runner_holds_site_lock_until_cancelled_pipeline_stops() {
    use futures::FutureExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let e = env("rel-cancel");
    let site = add_site(&e, "http://127.0.0.1:9", false, json!({})).await;
    let stopped = Arc::new(AtomicBool::new(false));
    let st2 = stopped.clone();
    // a "slow" inject: ignores the cancel for ~400 ms (one long post), then hits a checkpoint
    let slow: sf_jobs::pipelines::PipelineFn = Arc::new(move |_c, j| {
        let st = st2.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let r = j.checkpoint().await;
            st.store(true, Ordering::SeqCst);
            r?;
            Ok(json!({}))
        }
        .boxed()
    });
    let fast: sf_jobs::pipelines::PipelineFn =
        Arc::new(|_c, _j| async move { Ok(json!({"crawled": true})) }.boxed());
    let mut opts = RunnerOptions::from_config(&e.ctx.cfg);
    opts.poll = Duration::from_millis(10);
    opts.heartbeat = Duration::from_millis(20);
    opts.cancel_grace = Duration::from_secs(5);
    opts.overrides.insert("inject".into(), slow);
    opts.overrides.insert("crawl".into(), fast);
    let runner = Runner::start_with(e.ctx.clone(), opts);
    let inj = e
        .ctx
        .queue
        .enqueue(NewJob::new(JobKind::Inject, Some(&site), json!({})))
        .await
        .unwrap();
    for _ in 0..100 {
        if e.ctx.queue.get(&inj.job_run_id).await.unwrap()["started_at"].is_string() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    e.ctx.queue.cancel(&inj.job_run_id).await.unwrap();
    assert_eq!(
        e.ctx.queue.get(&inj.job_run_id).await.unwrap()["status"],
        "cancelled"
    );
    let crawl = e
        .ctx
        .queue
        .enqueue(NewJob::new(JobKind::Crawl, Some(&site), json!({})))
        .await
        .unwrap();
    let mut done = false;
    for _ in 0..300 {
        let c = e.ctx.queue.get(&crawl.job_run_id).await.unwrap();
        if c["status"] == "running" || c["status"] == "done" {
            assert!(
                stopped.load(Ordering::SeqCst),
                "crawl started while the cancelled inject was still running"
            );
            if c["status"] == "done" {
                done = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(done, "crawl ran after the inject stopped");
    let i = e.ctx.queue.get(&inj.job_run_id).await.unwrap();
    assert_eq!(
        (i["status"].as_str(), i["cancelling"].as_i64()),
        (Some("cancelled"), Some(0))
    );
    runner.shutdown(Duration::from_secs(1)).await;
}

/// R16: a re-chunk that only deletes/re-indexes paragraphs (nothing to embed) must still drop the
/// cached site vectors, or the next plan ranks paragraphs by stale ids/idx.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rechunk_without_new_embeddings_invalidates_vector_cache() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let e = env("r16");
    let site = add_site(&e, &base, true, settings()).await;
    run_job(&e, JobKind::Crawl, &site, json!({})).await;
    run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    let first = e.ctx.vectors.get(&site).await.unwrap();

    // control: a plan with no content change keeps the cached entry
    run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    let same = e.ctx.vectors.get(&site).await.unwrap();
    assert!(std::sync::Arc::ptr_eq(&first, &same));

    // drop the first paragraph of one multi-paragraph post: kept paragraphs move up one idx
    let rows = q_rows(
        &e.db,
        "SELECT p.id, p.content_html FROM posts p WHERE p.site_id=?1
         AND (SELECT count(*) FROM paragraphs pa WHERE pa.post_id=p.id) >= 2 ORDER BY p.id LIMIT 1",
        vec![site.clone()],
    )
    .await;
    let (pid, html) = (s(&rows[0][0]), s(&rows[0][1]));
    let cut = html.find("</p>").map(|i| i + 4).unwrap();
    let paras_before = q_i64(
        &e.db,
        "SELECT count(*) FROM paragraphs WHERE post_id=?1",
        vec![pid.clone()],
    )
    .await;
    exec(
        &e.db,
        "UPDATE posts SET content_html=?2, content_raw=NULL, content_hash='r16-changed' WHERE id=?1",
        vec![pid.clone(), html[cut..].to_string()],
    )
    .await;
    run_job(&e, JobKind::Plan, &site, json!({"topology": "reverse"})).await;
    let paras_after = q_i64(
        &e.db,
        "SELECT count(*) FROM paragraphs WHERE post_id=?1",
        vec![pid.clone()],
    )
    .await;
    assert!(
        paras_after < paras_before,
        "{paras_before} -> {paras_after}"
    );
    let fresh = e.ctx.vectors.get(&site).await.unwrap();
    assert!(
        !std::sync::Arc::ptr_eq(&first, &fresh),
        "vector cache must be reloaded after a re-chunk"
    );
}
