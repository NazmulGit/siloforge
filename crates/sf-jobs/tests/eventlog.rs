//! §9 unified event log: job lifecycle rows (queued / started / done / failed / cancelled).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use futures::FutureExt;
use serde_json::json;
use sf_core::types::JobKind;
use sf_jobs::{JobError, NewJob, Runner, RunnerOptions};
use sf_ops::eventlog::{EventLog, EventLogOptions};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn job_lifecycle_is_recorded() {
    let mut e = env("evlog");
    let site = add_site(&e, "http://127.0.0.1:9", false, json!({})).await;
    exec(
        &e.db,
        "INSERT INTO users(id,email,password_hash,role) VALUES('u1','u1@x.io','x','user')",
        vec![],
    )
    .await;
    exec(
        &e.db,
        "UPDATE sites SET user_id='u1' WHERE id=?1",
        vec![site.clone()],
    )
    .await;
    let log = EventLog::start(
        e.db.clone(),
        EventLogOptions {
            interval: Duration::from_millis(20),
            ..Default::default()
        },
    );
    Arc::get_mut(&mut e.ctx).expect("sole ctx owner").events = Some(log.clone());
    e.ctx.queue.set_events(log.clone());

    let ok: sf_jobs::pipelines::PipelineFn = Arc::new(|_c, _j| {
        async move { Ok(json!({"upserted": 3, "skipped": 1, "pages": [1, 2, 3], "note": "fine"})) }
            .boxed()
    });
    let bad: sf_jobs::pipelines::PipelineFn =
        Arc::new(|_c, _j| async move { Err(JobError::msg("boom")) }.boxed());
    let mut opts = RunnerOptions::from_config(&e.ctx.cfg);
    opts.poll = Duration::from_millis(10);
    opts.heartbeat = Duration::from_millis(20);
    opts.overrides.insert("crawl".into(), ok);
    opts.overrides.insert("plan".into(), bad);
    let runner = Runner::start_with(e.ctx.clone(), opts);
    let crawl = e
        .ctx
        .queue
        .enqueue(NewJob::new(JobKind::Crawl, Some(&site), json!({})).by("user:u1"))
        .await
        .unwrap();
    let plan = e
        .ctx
        .queue
        .enqueue(NewJob::new(JobKind::Plan, Some(&site), json!({})))
        .await
        .unwrap();
    for _ in 0..300 {
        let a = e.ctx.queue.get(&crawl.job_run_id).await.unwrap();
        let b = e.ctx.queue.get(&plan.job_run_id).await.unwrap();
        if a["status"] == "done" && b["status"] == "failed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    runner.shutdown(Duration::from_secs(1)).await;
    log.flush().await;
    let rows = q_rows(
        &e.db,
        "SELECT action, actor, status, target, site_id, user_id, ifnull(detail,''), duration_ms
           FROM event_log WHERE source='job' ORDER BY id",
        vec![],
    )
    .await;
    let actions: Vec<String> = rows.iter().map(|r| s(&r[0])).collect();
    for want in [
        "job crawl queued",
        "job crawl started",
        "job crawl done",
        "job plan queued",
        "job plan started",
        "job plan failed",
    ] {
        assert!(
            actions.iter().any(|a| a == want),
            "{want} missing: {rows:?}"
        );
    }
    let find = |a: &str| rows.iter().find(|r| s(&r[0]) == a).unwrap().clone();
    let q = find("job crawl queued");
    assert_eq!(s(&q[1]), "user:u1");
    assert_eq!(s(&q[3]), crawl.job_run_id);
    assert_eq!((s(&q[4]), s(&q[5])), (site.clone(), "u1".to_string()));
    let done = find("job crawl done");
    assert_eq!(s(&done[2]), "ok");
    let detail: serde_json::Value = serde_json::from_str(&s(&done[6])).unwrap();
    assert_eq!(
        detail,
        json!({"upserted": 3, "skipped": 1, "note": "fine", "job_id": crawl.job_run_id})
    );
    let qd: serde_json::Value = serde_json::from_str(&s(&q[6])).unwrap();
    assert_eq!(qd["job_id"], crawl.job_run_id);
    assert!(done[7].is_i64());
    let failed = find("job plan failed");
    assert_eq!(
        (s(&failed[2]), s(&failed[5])),
        ("error".into(), "u1".into())
    );
    assert!(s(&failed[6]).contains("boom"));
    // a deduplicated enqueue records nothing new
    let n_before = rows.len();
    let mut nj = NewJob::new(JobKind::Crawl, Some(&site), json!({"x": 1}));
    nj.dedupe = Some(true);
    let a = e.ctx.queue.enqueue(nj.clone()).await.unwrap();
    let b = e.ctx.queue.enqueue(nj).await.unwrap();
    assert!(!a.deduplicated && b.deduplicated);
    log.flush().await;
    let n = q_i64(
        &e.db,
        "SELECT count(*) FROM event_log WHERE source='job'",
        vec![],
    )
    .await;
    assert_eq!(usize::try_from(n).unwrap(), n_before + 1);
}
