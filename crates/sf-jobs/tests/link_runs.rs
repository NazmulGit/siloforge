//! v2.2 link runs end to end at the pipeline level (stub WordPress, fake embedder): plan with
//! strategies + filters, fix-broken edges, deploy + verify, repair after an edit, undo
//! byte-identical, cancel, Auto-Linker dry run / live / cap / auto-pause, schedule sync.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::*;
use serde_json::{Value, json};
use sf_jobs::Job;
use sf_jobs::pipelines::link_run::{self, store};

fn opts(extra: Value) -> link_run::RunOptions {
    let mut base = json!({
        "crawl": "always",
        "strategies": {"in_text": true, "contextual": false, "semantic": true, "keyword_rules": false,
                       "silo": false, "orphan_rescue": true, "money_pages": false, "fix_broken": true},
        "mode": "opportunity",
        "filters": {"min_score": 0.0, "max_total": 50, "max_new_per_post": 3, "max_links_per_post": 10,
                    "anchor_types": ["exact","partial","lsi","branded","generic","naked","cta"], "max_exact_share": 1.0},
    });
    if let (Some(b), Some(x)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in x {
            b.insert(k.clone(), v.clone());
        }
    }
    link_run::parse_options(Some(&base)).unwrap()
}

/// Claim the next queued job and run it like the runner (finish/fail the row).
async fn run_next(e: &Env) -> Result<Value, String> {
    let claimed = e
        .ctx
        .queue
        .claim_next(true)
        .await
        .unwrap()
        .expect("a queued job");
    let job = Arc::new(Job::new(claimed, e.ctx.queue.clone()));
    match sf_jobs::pipelines::dispatch(e.ctx.clone(), job.clone()).await {
        Ok(v) => {
            e.ctx
                .queue
                .finish(&job.id, v.clone(), json!({}))
                .await
                .unwrap();
            Ok(v)
        }
        Err(err) => {
            let _ = e
                .ctx
                .queue
                .fail(&job.id, &err.to_string(), false, json!({}))
                .await;
            Err(err.to_string())
        }
    }
}

async fn step(e: &Env, run: &str, name: &str) -> Value {
    link_run::enqueue_step(&e.db, &e.ctx.queue, None, run, name, "test")
        .await
        .unwrap();
    run_next(e).await.unwrap_or_else(|m| panic!("{name}: {m}"))
}

async fn run_json(e: &Env, id: &str) -> Value {
    let id = id.to_string();
    e.db.read(move |c| store::run_json(c, &id))
        .await
        .unwrap()
        .unwrap()
}

async fn items(e: &Env, id: &str, group: bool) -> Value {
    let id = id.to_string();
    e.db.read(move |c| {
        store::items(
            c,
            &id,
            &store::ItemQuery {
                limit: 500,
                group_by_post: group,
                ..Default::default()
            },
        )
    })
    .await
    .unwrap()
}

fn with_broken_link(base: &str) -> Vec<StubPost> {
    let mut posts = bike_posts(base);
    // post 4 links to a URL that does not exist (404): "bike-lock" instead of "bike-locks"
    posts[3].raw.push_str(&format!(
        "<!-- wp:paragraph -->\n<p>Our older <a href=\"{base}/bike-lock/\">bike lock review</a> covers cheap cable models too.</p>\n<!-- /wp:paragraph -->\n\n"
    ));
    posts
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_deploy_verify_repair_undo_round_trip() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = with_broken_link(&base)
        .into_iter()
        .map(|p| (p.id, p))
        .collect();
    let original: HashMap<u64, String> = wp
        .lock()
        .posts
        .values()
        .map(|p| (p.id, p.raw.clone()))
        .collect();
    let e = env("lrun");
    let site = add_site(&e, &base, true, json!({})).await;
    // link health needs posts; crawl + check once so fix_broken has rows
    run_job(&e, sf_core::types::JobKind::Crawl, &site, json!({})).await;
    run_job(
        &e,
        sf_core::types::JobKind::LinkHealth,
        &site,
        json!({"internal_only": true}),
    )
    .await;
    let broken = q_i64(
        &e.db,
        "SELECT count(*) FROM link_health WHERE is_broken=1",
        vec![],
    )
    .await;
    assert!(broken >= 1, "the stub 404 is recorded as broken");

    let o = opts(json!({}));
    let run = link_run::create_run(&e.db, &site, "first", &o, "u1", false)
        .await
        .unwrap();
    // only one active run per site
    assert!(
        link_run::create_run(&e.db, &site, "", &o, "u1", false)
            .await
            .is_err()
    );
    // deploy before ready -> conflict
    assert!(
        link_run::enqueue_step(&e.db, &e.ctx.queue, None, &run, "deploy", "t")
            .await
            .is_err()
    );
    let st = step(&e, &run, "plan").await;
    let r = run_json(&e, &run).await;
    assert_eq!(r["status"], "ready", "{st} {r}");
    assert_eq!(r["crawl_job_id"], r["plan_job_id"]);
    assert_eq!(r["progress"]["pct"], 100);
    let stats = &r["stats"];
    assert!(stats["opportunities"].as_i64().unwrap() >= 3, "{stats}");
    assert!(
        stats["by_strategy"]["semantic"]["total"].as_i64().unwrap() >= 1,
        "{stats}"
    );
    let lh = q_rows(
        &e.db,
        "SELECT href, anchor, placement, scope, is_broken FROM link_health",
        vec![],
    )
    .await;
    assert_eq!(
        stats["by_strategy"]["fix_broken"]["total"], 1,
        "{stats} {st} {lh:?}"
    );
    // every item is tagged and native (in_text only), opportunity mode = no silo items
    let n_untagged = q_i64(
        &e.db,
        "SELECT count(*) FROM link_edges WHERE run_id=?1 AND (json_extract(score_breakdown,'$.strategy') IS NULL OR insert_module<>'native' OR silo_topology IS NOT NULL)",
        vec![run.clone()],
    )
    .await;
    assert_eq!(n_untagged, 0);
    let flat = items(&e, &run, false).await;
    let first = &flat["items"][0];
    assert!(
        first["open_url"].as_str().unwrap().contains("#:~:text="),
        "{first}"
    );
    assert!(first["live_url"].as_str().unwrap().starts_with(&base));
    let grouped = items(&e, &run, true).await;
    let posts = grouped["posts"].as_array().unwrap();
    assert!(!posts.is_empty());
    assert_eq!(
        posts
            .iter()
            .map(|p| p["links"].as_array().unwrap().len() as i64)
            .sum::<i64>(),
        grouped["total_items"].as_i64().unwrap()
    );

    // untick one, deploy the rest
    let all_ids: Vec<String> = flat["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| s(&i["edge_id"]))
        .collect();
    let fixed = flat["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["strategy"] == "fix_broken")
        .unwrap()
        .clone();
    let skip_id = all_ids
        .iter()
        .find(|i| **i != s(&fixed["edge_id"]))
        .unwrap()
        .clone();
    let changed =
        e.db.write({
            let (r, id) = (run.clone(), skip_id.clone());
            move |t| store::select(t, &r, &[id], false, true)
        })
        .await
        .unwrap();
    assert_eq!(changed, 1);
    let dep = step(&e, &run, "deploy").await;
    let r = run_json(&e, &run).await;
    let applied = r["stats"]["applied"].as_i64().unwrap();
    assert!(applied >= 2, "{dep} {r}");
    assert!(
        r["status"] == "deployed" || r["status"] == "partially_deployed",
        "{r}"
    );
    assert!(r["deployed_at"].is_string());
    // the unticked item is still a suggestion
    let st_skip: Vec<Vec<Value>> = q_rows(
        &e.db,
        "SELECT status FROM link_edges WHERE id=?1",
        vec![skip_id.clone()],
    )
    .await;
    assert_eq!(st_skip[0][0], "suggested");
    // the broken link now points at /bike-locks/
    let p4 = wp.lock().posts.get(&4).unwrap().raw.clone();
    assert!(
        p4.contains(&format!(
            "<a href=\"{base}/bike-locks/\">bike lock review</a>"
        )),
        "{p4}"
    );
    let verified = q_i64(
        &e.db,
        "SELECT count(*) FROM link_edges WHERE run_id=?1 AND status='applied' AND verify_present=1",
        vec![run.clone()],
    )
    .await;
    assert_eq!(verified, applied);

    // somebody edits a post in WordPress and removes one of our links
    let victim: Vec<Vec<Value>> = q_rows(
        &e.db,
        "SELECT e.id, s.wp_post_id, t.url FROM link_edges e JOIN posts s ON s.id=e.source_post_id JOIN posts t ON t.id=e.target_post_id
         WHERE e.run_id=?1 AND e.status='applied' AND json_extract(e.score_breakdown,'$.strategy')='semantic' ORDER BY e.id LIMIT 1",
        vec![run.clone()],
    )
    .await;
    let (vid, wp_id, turl) = (
        s(&victim[0][0]),
        victim[0][1].as_u64().unwrap(),
        s(&victim[0][2]),
    );
    {
        let mut w = wp.lock();
        let p = w.posts.get_mut(&wp_id).unwrap();
        let open = format!("<a href=\"{turl}\">");
        let at = p.raw.find(&open).unwrap();
        let close = at + p.raw[at..].find("</a>").unwrap();
        let inner = p.raw[at + open.len()..close].to_string();
        p.raw = format!("{}{}{}", &p.raw[..at], inner, &p.raw[close + 4..]);
        p.modified += 7;
    }
    step(&e, &run, "verify").await;
    let flat = items(&e, &run, false).await;
    let v = flat["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["edge_id"] == vid.as_str())
        .unwrap()
        .clone();
    assert_eq!(v["status"], "not_present", "{v}");
    assert_eq!(v["verify"]["present"], false);
    // the reason follows the latest live check (E2E 2.2)
    assert_eq!(v["error"], "link not found on the live page", "{v}");
    assert_eq!(run_json(&e, &run).await["status"], "partially_deployed");

    let rep = step(&e, &run, "repair").await;
    assert!(rep["relocated"].as_u64().unwrap() >= 1, "{rep}");
    let flat = items(&e, &run, false).await;
    let v = flat["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["edge_id"] == vid.as_str())
        .unwrap()
        .clone();
    assert_eq!(v["status"], "applied", "{v}");
    assert_eq!(v["repaired"], true);
    let r = run_json(&e, &run).await;
    assert!(r["stats"]["repaired"].as_i64().unwrap() >= 1, "{r}");
    assert!(r["repair_job_id"].is_string());

    // undo the whole run: every post is byte-identical to the original
    let u = step(&e, &run, "undo").await;
    assert_eq!(run_json(&e, &run).await["status"], "undone", "{u}");
    for (id, raw) in &original {
        assert_eq!(
            &wp.lock().posts.get(id).unwrap().raw,
            raw,
            "post {id} differs after undo"
        );
    }
    // nothing left to undo / verify
    assert!(
        link_run::enqueue_step(&e.db, &e.ctx.queue, None, &run, "verify", "t")
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn filters_modes_cancel_and_repair_skips() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let e = env("lrun2");
    let site = add_site(&e, &base, true, json!({})).await;
    // reverse silo mode, silo only, approve none
    let o = opts(json!({"mode": "reverse", "approve": "none",
        "strategies": {"in_text": true, "semantic": false, "silo": true, "keyword_rules": false, "orphan_rescue": false, "fix_broken": false}}));
    let run = link_run::create_run(&e.db, &site, "", &o, "u1", false)
        .await
        .unwrap();
    step(&e, &run, "plan").await;
    let r = run_json(&e, &run).await;
    assert_eq!(r["status"], "ready");
    assert_eq!(r["stats"]["selected"], 0, "{r}");
    let other = q_i64(
        &e.db,
        "SELECT count(*) FROM link_edges WHERE run_id=?1 AND json_extract(score_breakdown,'$.strategy')<>'silo'",
        vec![run.clone()],
    )
    .await;
    assert_eq!(other, 0);
    // exclude everything by URL pattern on a fresh run: nothing adopted
    let o2 = opts(
        json!({"crawl": "skip", "filters": {"min_score": 0.0, "exclude_url_patterns": ["*"]}}),
    );
    let run2 = link_run::create_run(&e.db, &site, "", &o2, "u1", false)
        .await
        .unwrap();
    step(&e, &run2, "plan").await;
    assert_eq!(run_json(&e, &run2).await["stats"]["opportunities"], 0);

    // cancel a queued run
    let run3 = link_run::create_run(
        &e.db,
        &site,
        "",
        &opts(json!({"crawl": "skip"})),
        "u1",
        false,
    )
    .await
    .unwrap();
    link_run::enqueue_step(&e.db, &e.ctx.queue, None, &run3, "plan", "t")
        .await
        .unwrap();
    link_run::cancel_run(&e.db, &e.ctx.queue, None, &run3)
        .await
        .unwrap();
    assert_eq!(run_json(&e, &run3).await["status"], "cancelled");
    assert!(
        link_run::cancel_run(&e.db, &e.ctx.queue, None, &run3)
            .await
            .is_err()
    );

    // a run whose anchor disappears from the post: repair skips with a reason
    let run4 = link_run::create_run(
        &e.db,
        &site,
        "",
        &opts(json!({"crawl": "skip", "strategies": {"fix_broken": false}})),
        "u1",
        false,
    )
    .await
    .unwrap();
    step(&e, &run4, "plan").await;
    let flat = items(&e, &run4, false).await;
    let it = flat["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["selected"] == true)
        .unwrap()
        .clone();
    let (eid, anchor) = (s(&it["edge_id"]), s(&it["anchor"]));
    // keep only this item ticked, then remove every occurrence of the anchor (and alternates)
    e.db.write({
        let r = run4.clone();
        move |t| store::select(t, &r, &[], true, true)
    })
    .await
    .unwrap();
    e.db.write({
        let (r, id) = (run4.clone(), eid.clone());
        move |t| store::select(t, &r, &[id], false, false)
    })
    .await
    .unwrap();
    // PATCH-like edit with a bad module is refused; a free-text anchor edit works
    let bad =
        e.db.write({
            let (r, id) = (run4.clone(), eid.clone());
            move |t| store::patch_item(t, &r, &id, None, Some("magic"))
        })
        .await;
    assert!(bad.is_err());
    let src_wp: i64 = q_i64(
        &e.db,
        "SELECT s.wp_post_id FROM link_edges e JOIN posts s ON s.id=e.source_post_id WHERE e.id=?1",
        vec![eid.clone()],
    )
    .await;
    {
        let mut w = wp.lock();
        let p = w.posts.get_mut(&(src_wp as u64)).unwrap();
        let re = regex_lite_remove(&p.raw, &anchor);
        p.raw = re;
        p.modified += 3;
    }
    exec(&e.db, "UPDATE link_edges SET score_breakdown=json_remove(score_breakdown,'$.alternates') WHERE id=?1", vec![eid.clone()]).await;
    step(&e, &run4, "deploy").await;
    let st: Vec<Vec<Value>> = q_rows(
        &e.db,
        "SELECT status FROM link_edges WHERE id=?1",
        vec![eid.clone()],
    )
    .await;
    assert_ne!(st[0][0], "applied");
    assert_eq!(run_json(&e, &run4).await["status"], "failed");
    let rep = step(&e, &run4, "repair").await;
    assert_eq!(rep["skipped"], 1, "{rep}");
    let flat = items(&e, &run4, false).await;
    let it = flat["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["edge_id"] == eid.as_str())
        .unwrap()
        .clone();
    assert_eq!(it["status"], "skipped", "{it}");
    assert_eq!(it["error"], "anchor text no longer in post", "{it}");
}

/// Remove every case-insensitive occurrence of `word` (no regex crate in tests).
fn regex_lite_remove(raw: &str, word: &str) -> String {
    let lower = raw.to_lowercase();
    let w = word.to_lowercase();
    let mut out = String::new();
    let mut i = 0;
    while let Some(at) = lower[i..].find(&w) {
        out.push_str(&raw[i..i + at]);
        out.push_str("xx");
        i += at + w.len();
    }
    out.push_str(&raw[i..]);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_linker_dry_run_live_cap_pause_and_schedule() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let e = env("lauto");
    let cfg = json!({"enabled": true, "cron": "0 3 * * *", "tz": "UTC", "live": false, "max_per_day": 2,
        "options": opts(json!({"strategies": {"fix_broken": false}})).to_value()});
    let site = add_site(&e, &base, true, json!({"auto_linker": cfg})).await;
    let a = link_run::parse_auto_linker(&cfg).unwrap();
    e.db.write({
        let s = site.clone();
        move |t| link_run::sync_auto_linker_schedule(t, &s, Some(&a))
    })
    .await
    .unwrap();
    assert_eq!(
        q_i64(
            &e.db,
            "SELECT count(*) FROM site_schedules WHERE job_kind='link_run' AND active=1",
            vec![]
        )
        .await,
        1
    );
    // dry run: stops at ready
    link_run::enqueue_auto_now(&e.ctx.queue, &site, "test")
        .await
        .unwrap();
    let out = run_next(&e).await.unwrap();
    let rid = s(&out["link_run_id"]);
    let r = run_json(&e, &rid).await;
    assert_eq!(r["status"], "ready", "{out}");
    assert_eq!(r["auto"], true);
    assert!(
        r["stats"]["selected"].as_i64().unwrap() <= 2,
        "daily cap limits max_total"
    );
    // cancel the dry-run run so the next one is not blocked, then go live
    exec(
        &e.db,
        "UPDATE link_runs SET status='cancelled' WHERE id=?1",
        vec![rid],
    )
    .await;
    exec(
        &e.db,
        "UPDATE sites SET settings=json_set(settings,'$.auto_linker.live',json('true'),'$.auto_linker.pause_on_failure_rate',0.0) WHERE id=?1",
        vec![site.clone()],
    )
    .await;
    link_run::enqueue_auto_now(&e.ctx.queue, &site, "test")
        .await
        .unwrap();
    let out = run_next(&e).await.unwrap();
    let rid = s(&out["link_run_id"]);
    let r = run_json(&e, &rid).await;
    assert!(r["stats"]["applied"].as_i64().unwrap() >= 1, "{out}");
    assert!(r["stats"]["applied"].as_i64().unwrap() <= 2);
    // cap reached for today
    link_run::enqueue_auto_now(&e.ctx.queue, &site, "test")
        .await
        .unwrap();
    let out2 = run_next(&e).await.unwrap();
    if r["stats"]["applied"] == 2 {
        assert_eq!(out2["skipped"], "daily cap reached", "{out2}");
    }
    // force an auto-pause: any failure with threshold 0 pauses (simulate one not_present)
    exec(
        &e.db,
        "UPDATE link_edges SET verify_present=0 WHERE run_id=?1 AND status='applied'",
        vec![s(&out["link_run_id"])],
    )
    .await;
    let paused = out["paused"].as_bool().unwrap_or(false);
    if paused {
        let en: Vec<Vec<Value>> = q_rows(
            &e.db,
            "SELECT json_extract(settings,'$.auto_linker.enabled') FROM sites WHERE id=?1",
            vec![site.clone()],
        )
        .await;
        assert_eq!(en[0][0], 0);
    }
    // disabling the setting deactivates the schedule
    e.db.write({
        let s = site.clone();
        move |t| link_run::sync_auto_linker_schedule(t, &s, None)
    })
    .await
    .unwrap();
    assert_eq!(
        q_i64(
            &e.db,
            "SELECT count(*) FROM site_schedules WHERE job_kind='link_run' AND active=1",
            vec![]
        )
        .await,
        0
    );
    // disabled -> the scheduled job is a no-op
    exec(&e.db, "UPDATE sites SET settings=json_set(settings,'$.auto_linker.enabled',json('false')) WHERE id=?1", vec![site.clone()]).await;
    let mut nj = sf_jobs::NewJob::new(
        sf_core::types::JobKind::LinkRun,
        Some(&site),
        json!({"auto": true}),
    );
    nj.dedupe = Some(false);
    e.ctx.queue.enqueue(nj).await.unwrap();
    let out3 = run_next(&e).await.unwrap();
    assert_eq!(out3["skipped"], "auto_linker is disabled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hostile_payloads_never_panic() {
    let (base, _wp) = start_wp(vec![]).await;
    let e = env("lhost");
    let site = add_site(&e, &base, true, json!({})).await;
    for p in [
        json!({}),
        json!({"link_run_id": "nope"}),
        json!({"link_run_id": 5, "step": []}),
        json!({"auto": true}),
        json!({"auto": "yes"}),
    ] {
        let mut nj = sf_jobs::NewJob::new(sf_core::types::JobKind::LinkRun, Some(&site), p);
        nj.dedupe = Some(false);
        e.ctx.queue.enqueue(nj).await.unwrap();
        let _ = run_next(&e).await;
    }
    let id = String::from("x");
    let v = e.db.read(move |c| store::run_json(c, &id)).await.unwrap();
    assert!(v.is_none());
}

/// Regressions from the 2.2 E2E on a real local WordPress (docs/testing/link-runs-e2e.md).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e22_opportunities_alternates_anchor_edits_and_content_gaps() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let e = env("lrun-e2e22");
    let site = add_site(&e, &base, true, json!({})).await;
    run_job(&e, sf_core::types::JobKind::Crawl, &site, json!({})).await;

    // 1. right after the first crawl (no plan yet) the orphan counts come from the existing
    //    links, not from graph columns that only a plan fills (was: 0 orphans, all under-linked)
    let posts = q_i64(&e.db, "SELECT count(*) FROM posts", vec![]).await;
    let with_inbound = q_i64(
        &e.db,
        "SELECT count(DISTINCT target_post_id) FROM link_edges WHERE origin='existing' AND source_post_id<>target_post_id",
        vec![],
    )
    .await;
    assert!(with_inbound >= 1, "post 2 links post 1");
    let sid = site.clone();
    let o =
        e.db.read(move |c| store::opportunity_counts(c, &sid))
            .await
            .unwrap();
    assert_eq!(o["counts"]["orphans"], posts - with_inbound, "{o}");
    assert_eq!(
        o["counts"]["contextual_orphans"],
        posts - with_inbound,
        "{o}"
    );
    assert!(
        o["counts"]["under_linked"].as_i64().unwrap() <= posts,
        "{o}"
    );

    // 2. an in-text run offers only alternates that occur in the paragraph, and refuses an
    //    anchor edit the splicer could never place
    let run = link_run::create_run(
        &e.db,
        &site,
        "",
        &opts(json!({"crawl": "skip", "strategies": {"fix_broken": false}})),
        "u1",
        false,
    )
    .await
    .unwrap();
    step(&e, &run, "plan").await;
    let rows = q_rows(
        &e.db,
        "SELECT e.id, pa.text, e.anchor FROM link_edges e JOIN paragraphs pa ON pa.id=e.paragraph_id WHERE e.run_id=?1 AND e.status='suggested'",
        vec![run.clone()],
    )
    .await;
    assert!(!rows.is_empty());
    let flat = items(&e, &run, false).await;
    for it in flat["items"].as_array().unwrap() {
        let para = rows
            .iter()
            .find(|r| r[0] == it["edge_id"])
            .map(|r| s(&r[1]))
            .unwrap_or_default();
        for alt in it["alternates"].as_array().unwrap() {
            assert!(
                store::paragraph_has(&para, alt.as_str().unwrap()),
                "alternate {alt} is not in the paragraph {para:?}"
            );
        }
    }
    let (eid, para, anchor) = (s(&rows[0][0]), s(&rows[0][1]), s(&rows[0][2]));
    let refused =
        e.db.write({
            let (r, id) = (run.clone(), eid.clone());
            move |t| store::patch_item(t, &r, &id, Some("Totally Unrelated Phrase Xyz"), None)
        })
        .await;
    let msg = refused.unwrap_err().to_string();
    assert!(msg.contains("is not in the paragraph"), "{msg}");
    // a phrase from the paragraph is fine (here: the anchor's last word, lower-cased)
    let word = anchor.split_whitespace().last().unwrap().to_lowercase();
    assert!(store::paragraph_has(&para, &word));
    if !word.eq_ignore_ascii_case(&anchor) {
        e.db.write({
            let (r, id, w) = (run.clone(), eid.clone(), word.clone());
            move |t| store::patch_item(t, &r, &id, Some(&w), None)
        })
        .await
        .unwrap();
    }

    // 3. a deployed item that could not be placed (content gap) reports as failed, so Results
    //    and Fix show it (it used to stay "selected", in no filter and not fixable)
    exec(
        &e.db,
        "UPDATE link_edges SET status='approved', run_selected=1, insert_module='content_gap', failure_kind='content_gap', error='anchor gone' WHERE id=?1",
        vec![eid.clone()],
    )
    .await;
    let flat = items(&e, &run, false).await;
    let it = flat["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["edge_id"] == eid.as_str())
        .unwrap()
        .clone();
    assert_eq!(it["status"], "failed", "{it}");
    assert_eq!(it["failure_kind"], "content_gap");
    assert_eq!(run_json(&e, &run).await["stats"]["failed"], 1);
    // one item only (the PATCH response lookup)
    let rid = run.clone();
    let one =
        e.db.read(move |c| {
            store::items(
                c,
                &rid,
                &store::ItemQuery {
                    edge_id: Some(eid.clone()),
                    limit: 1,
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(one["total"], 1);
}

/// E2E 2.2: in a run with Contextual ticked, a native item whose anchor cannot be placed falls
/// back to the AI context module even when the site's insert modules are native only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contextual_run_falls_back_to_ai_for_native_items() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let ai = start_ai().await;
    let e = env_with_ai("lrun-ctx", &ai);
    let site = add_site(&e, &base, true, json!({})).await;
    let o = opts(json!({"strategies": {"contextual": true, "fix_broken": false}}));
    let run = link_run::create_run(&e.db, &site, "", &o, "u1", false)
        .await
        .unwrap();
    step(&e, &run, "plan").await;
    let flat = items(&e, &run, false).await;
    let it = flat["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["insert_module"] == "native")
        .expect("a native item")
        .clone();
    let eid = s(&it["edge_id"]);
    e.db.write({
        let r = run.clone();
        move |t| store::select(t, &r, &[], true, true)
    })
    .await
    .unwrap();
    e.db.write({
        let (r, id) = (run.clone(), eid.clone());
        move |t| store::select(t, &r, &[id], false, false)
    })
    .await
    .unwrap();
    // the anchor is not in the paragraph any more: native insertion must fail
    exec(
        &e.db,
        "UPDATE link_edges SET anchor='gravel trail comfort' WHERE id=?1",
        vec![eid.clone()],
    )
    .await;
    let dep = step(&e, &run, "deploy").await;
    let r = q_rows(
        &e.db,
        "SELECT status, insert_module, error FROM link_edges WHERE id=?1",
        vec![eid.clone()],
    )
    .await;
    assert_eq!(r[0][0], "applied", "{dep} {r:?}");
    assert_eq!(r[0][1], "context", "{r:?}");
    // undo takes the whole AI sentence back out, not only the <a> (byte-identical posts)
    let original: HashMap<u64, String> = bike_posts(&base)
        .into_iter()
        .map(|p| (p.id, p.raw))
        .collect();
    assert!(
        wp.lock()
            .posts
            .values()
            .any(|p| original.get(&p.id) != Some(&p.raw)),
        "the deploy changed a post"
    );
    let u = step(&e, &run, "undo").await;
    assert_eq!(run_json(&e, &run).await["status"], "undone", "{u}");
    for (id, raw) in &original {
        assert_eq!(
            &wp.lock().posts.get(id).unwrap().raw,
            raw,
            "post {id} differs after undo of an AI context link"
        );
    }
}

/// E2E 2.2: a ready run keeps its links when another run is created (it used to lose them all
/// to the new run's adoption); cancelling the ready run releases them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ready_run_keeps_its_links_until_cancelled() {
    let (base, wp) = start_wp(vec![]).await;
    wp.lock().posts = bike_posts(&base).into_iter().map(|p| (p.id, p)).collect();
    let e = env("lrun-hold");
    let site = add_site(&e, &base, true, json!({})).await;
    let o = opts(json!({"strategies": {"fix_broken": false}}));
    let a = link_run::create_run(&e.db, &site, "a", &o, "u1", true)
        .await
        .unwrap();
    step(&e, &a, "plan").await;
    let before = run_json(&e, &a).await["stats"].clone();
    assert!(before["opportunities"].as_i64().unwrap() >= 1, "{before}");
    let o2 = opts(json!({"crawl": "skip", "strategies": {"fix_broken": false}}));
    let b = link_run::create_run(&e.db, &site, "b", &o2, "u1", false)
        .await
        .unwrap();
    step(&e, &b, "plan").await;
    let after = run_json(&e, &a).await["stats"].clone();
    assert_eq!(after["opportunities"], before["opportunities"], "{after}");
    assert_eq!(after["selected"], before["selected"], "{after}");
    // cancel the ready run: allowed, and its suggestions are free for the next run
    link_run::cancel_run(&e.db, &e.ctx.queue, None, &a)
        .await
        .unwrap();
    assert_eq!(run_json(&e, &a).await["status"], "cancelled");
    let c = link_run::create_run(&e.db, &site, "c", &o2, "u1", false)
        .await
        .unwrap();
    step(&e, &c, "plan").await;
    let held_by_a = q_i64(
        &e.db,
        "SELECT count(*) FROM link_edges WHERE run_id=?1 AND status='suggested'",
        vec![a.clone()],
    )
    .await;
    assert_eq!(held_by_a, 0, "c adopted a's released suggestions");
}
