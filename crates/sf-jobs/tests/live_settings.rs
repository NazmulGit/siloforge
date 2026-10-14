//! v2.1 live settings: runner concurrency follows `resources.max_concurrent_jobs`, WordPress
//! defaults fill unset site settings, the vector cache budget is adjustable, crawl client config
//! follows the live `[crawl]` section.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use futures::FutureExt;
use serde_json::json;
use sf_core::types::JobKind;
use sf_jobs::pipelines::PipelineFn;
use sf_jobs::{NewJob, Runner, RunnerOptions};

/// Pipeline that records the peak number of jobs running at once.
fn counter(cur: Arc<AtomicUsize>, peak: Arc<AtomicUsize>) -> PipelineFn {
    Arc::new(move |_ctx, _job| {
        let (cur, peak) = (cur.clone(), peak.clone());
        async move {
            let n = cur.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(200)).await;
            cur.fetch_sub(1, Ordering::SeqCst);
            Ok(json!({"ok": true}))
        }
        .boxed()
    })
}

async fn run_batch(e: &Env, sites: &[String]) {
    let mut ids = Vec::new();
    for s in sites {
        ids.push(
            e.ctx
                .queue
                .enqueue(NewJob::new(JobKind::Crawl, Some(s), json!({})))
                .await
                .unwrap()
                .job_run_id,
        );
    }
    for _ in 0..400 {
        let mut all = true;
        for id in &ids {
            all &= e.ctx.queue.get(id).await.unwrap()["status"] == "done";
        }
        if all {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("jobs did not finish");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runner_concurrency_follows_live_setting() {
    let e = env("live-conc");
    let mut sites = Vec::new();
    for i in 1..=3 {
        sites.push(add_site(&e, &format!("http://127.0.0.{i}:9"), false, json!({})).await);
    }
    let mut cfg = (*e.ctx.live.get()).clone();
    cfg.resources.max_concurrent_jobs = 1;
    e.ctx.live.set(cfg.clone());
    let (cur, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let mut opts = RunnerOptions::from_config(&e.ctx.cfg);
    opts.poll = Duration::from_millis(20);
    opts.overrides
        .insert("crawl".into(), counter(cur.clone(), peak.clone()));
    let runner = Runner::start_with(e.ctx.clone(), opts);
    run_batch(&e, &sites).await;
    assert_eq!(peak.load(Ordering::SeqCst), 1, "live limit 1 respected");
    // raise it live: the same runner now runs the three sites together
    peak.store(0, Ordering::SeqCst);
    cfg.resources.max_concurrent_jobs = 3;
    e.ctx.live.set(cfg);
    run_batch(&e, &sites).await;
    assert!(peak.load(Ordering::SeqCst) >= 2, "raised limit applied");
    runner.shutdown(Duration::from_secs(2)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fixed_limit_when_not_following_live() {
    let e = env("fixed-conc");
    let mut sites = Vec::new();
    for i in 1..=3 {
        sites.push(add_site(&e, &format!("http://127.0.1.{i}:9"), false, json!({})).await);
    }
    let (cur, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let mut opts = RunnerOptions::from_config(&e.ctx.cfg);
    opts.follow_live = false;
    opts.max_concurrent = 1;
    opts.poll = Duration::from_millis(20);
    opts.overrides
        .insert("crawl".into(), counter(cur.clone(), peak.clone()));
    let runner = Runner::start_with(e.ctx.clone(), opts);
    run_batch(&e, &sites).await;
    assert_eq!(peak.load(Ordering::SeqCst), 1);
    runner.shutdown(Duration::from_secs(2)).await;
}

#[test]
fn wordpress_defaults_fill_only_unset_keys() {
    let mut wp = sf_core::config::WordpressCfg::default();
    let mut s = json!({"max_links_per_post": 4, "min_links_per_post": null});
    sf_jobs::pipelines::common::apply_wordpress_defaults(&mut s, &wp);
    assert_eq!(s["max_links_per_post"], 4, "site value wins");
    assert_eq!(s["min_links_per_post"], 3, "null is unset");
    assert_eq!(s["max_new_links_per_pass"], 5);
    assert_eq!(s["insert_modules"], json!(["native"]));
    assert!(s.get("skip_inject_verify").is_none());
    wp.verify_after_inject = false;
    wp.default_insert_modules = vec!["native".into(), "rephrase".into()];
    let mut s = json!({"skip_inject_verify": false});
    sf_jobs::pipelines::common::apply_wordpress_defaults(&mut s, &wp);
    assert_eq!(s["skip_inject_verify"], false, "explicit site value wins");
    assert_eq!(s["insert_modules"], json!(["native", "rephrase"]));
    let mut s = json!({});
    sf_jobs::pipelines::common::apply_wordpress_defaults(&mut s, &wp);
    assert_eq!(s["skip_inject_verify"], true);
    // hostile: non-object settings are left alone
    let mut s = json!([1, 2]);
    sf_jobs::pipelines::common::apply_wordpress_defaults(&mut s, &wp);
    assert_eq!(s, json!([1, 2]));
}

#[tokio::test(flavor = "multi_thread")]
async fn crawl_client_and_cache_budget_follow_live_config() {
    let e = env("live-http");
    let before = e.ctx.http_config();
    assert!(before.allow_private, "test tuning kept");
    let mut cfg = (*e.ctx.live.get()).clone();
    cfg.crawl.timeout_ms = 1234;
    cfg.crawl.user_agent = "LiveUA/1".into();
    cfg.wordpress.request_timeout_ms = 4321;
    cfg.resources.vector_cache_mb = 64;
    e.ctx.live.set(cfg);
    let after = e.ctx.http_config();
    assert_eq!(after.timeout, Duration::from_millis(1234));
    assert_eq!(after.user_agent, "LiveUA/1");
    assert!(after.allow_private, "test tuning still kept");
    assert_eq!(e.ctx.wp_http_config().timeout, Duration::from_millis(4321));
    e.ctx.apply_resources();
    assert_eq!(e.ctx.vectors.budget(), 64 * 1024 * 1024);
    e.ctx.vectors.set_budget(0);
    assert_eq!(e.ctx.vectors.cached_bytes(), 0);
}
