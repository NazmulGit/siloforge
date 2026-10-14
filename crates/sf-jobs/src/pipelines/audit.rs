//! `audit`: crawl (+chunk, unless `skip_crawl`) → GSC pull (when connected) → link health → plan →
//! metrics snapshot. A GSC failure is a warning (`stats.gsc = {ok: false, error}`), not a stage.
//! v1 outcome rules: every stage ok → `done` + one `site_metrics_history` row; some failed →
//! `done` with `degraded: true` and no snapshot; the plan stage failed → job failed.
//! `stats.stages = [{stage, ok, ms, stats?, error?}]` is published as each stage ends.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use rusqlite::params;
use serde_json::{Value, json};
use sf_engine::metrics::{LinkHealthAggregate, MetricsInput, PostMetricRow, compute_site_metrics};
use sf_store::err;

use super::common::{SiteInfo, load_site};
use super::{chunk, crawl, gsc, link_health, plan};
use crate::ctx::{Job, JobContext, JobError, JobResult};

pub async fn run(ctx: &Arc<JobContext>, job: &Job) -> JobResult<Value> {
    let site = load_site(ctx, job.site()?).await?;
    let skip_crawl = job.payload_bool("skip_crawl", "skipCrawl");
    let topology = job
        .payload_str("topology", "topology")
        .or_else(|| site.setting_str("silo_topology"))
        .unwrap_or_else(|| "reverse".into());
    let mut stages: Vec<Value> = Vec::new();

    if !skip_crawl {
        let r = async {
            let t = crawl::crawl_site(ctx, job, &site, (0.0, 20.0)).await?;
            let ch = chunk::chunk_changed(ctx, job, &site, "chunk", (20.0, 25.0)).await?;
            let paragraphs = crawl::finish_sync(ctx, &site.id).await?;
            Ok::<Value, JobError>(json!({
                "upserted": t.upserted, "skipped": t.skipped, "paragraphs": paragraphs,
                "chunked_posts": ch.chunked_posts,
            }))
        };
        stage(job, &mut stages, "crawl", r).await?;
    }
    // GSC pull (G2-2) after the crawl so new posts can match; a failure is only a warning.
    let gsc_stats = gsc::audit_pull(ctx, job, &site).await?;
    if let Some(g) = &gsc_stats {
        job.merge_stats(json!({ "gsc": g })).await;
    }
    let r = link_health::check_site(ctx, job, &site, (25.0, 50.0));
    stage(job, &mut stages, "link_health", r).await?;
    let r = plan_stage(ctx, job, &site, &topology);
    let plan_ok = stage(job, &mut stages, "plan", r).await?;
    if !plan_ok {
        let reason = stages
            .iter()
            .find(|s| s["stage"] == "plan")
            .and_then(|s| s["error"].as_str())
            .unwrap_or("plan stage failed")
            .to_string();
        return Err(JobError::msg(format!("audit failed: {reason}")));
    }
    let degraded = stages.iter().any(|s| s["ok"] != json!(true));
    let metrics = if degraded {
        None
    } else {
        Some(capture_metrics(ctx, &site, "audit", Some(&job.id)).await?)
    };
    let mut out = json!({
        "stages": stages,
        "degraded": degraded,
        "metrics_captured": metrics.is_some(),
        "progress": {"phase": "done", "pct": 100},
    });
    if let (Some(g), Some(o)) = (gsc_stats, out.as_object_mut()) {
        o.insert("gsc".into(), g);
    }
    if let (Some(m), Some(o)) = (metrics, out.as_object_mut()) {
        o.insert("metrics".into(), m);
    }
    Ok(out)
}

async fn plan_stage(
    ctx: &JobContext,
    job: &Job,
    site: &SiteInfo,
    topology: &str,
) -> JobResult<Value> {
    let archive = job.payload_bool("archive", "archive");
    plan::plan_site(ctx, job, site, topology, archive, None).await
}

/// Run one stage; record `{stage, ok, ms, stats|error}`. Cancellation propagates.
async fn stage(
    job: &Job,
    stages: &mut Vec<Value>,
    name: &str,
    fut: impl std::future::Future<Output = JobResult<Value>>,
) -> JobResult<bool> {
    let t = Instant::now();
    job.info(format!("audit stage {name} started")).await;
    let res = fut.await;
    let ms = t.elapsed().as_millis() as u64;
    let ok = match res {
        Ok(mut stats) => {
            if let Some(o) = stats.as_object_mut() {
                o.remove("progress");
            }
            stages.push(json!({"stage": name, "ok": true, "ms": ms, "stats": stats}));
            true
        }
        Err(e) if e.is_cancelled() => return Err(e),
        Err(e) => {
            job.warn(format!("audit stage {name} failed: {e}")).await;
            stages.push(json!({"stage": name, "ok": false, "ms": ms, "error": e.to_string()}));
            false
        }
    };
    job.merge_stats(json!({ "stages": stages })).await;
    Ok(ok)
}

/// Compute v1 `SiteMetrics` from the DB and append a `site_metrics_history` row.
pub async fn capture_metrics(
    ctx: &JobContext,
    site: &SiteInfo,
    source: &str,
    job_run_id: Option<&str>,
) -> JobResult<Value> {
    let sid = site.id.clone();
    let (input_rows, clusters, sugg, applied, lh, edges) = ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare(
                    "SELECT id, is_orphan, is_dead_end, click_depth, outbound_link_count, link_health_checked_at,
                       is_contextual_orphan, is_nav_only
                     FROM posts WHERE site_id=?1 ORDER BY id",
                )
                .map_err(err)?;
            let rows: Vec<(String, bool, bool, i64, i64, Option<String>, bool, bool)> = st
                .query_map([&sid], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            let q = |sql: &str| -> sf_core::Result<i64> {
                c.query_row(sql, [&sid], |r| r.get(0)).map_err(err)
            };
            let clusters = q("SELECT count(*) FROM clusters WHERE site_id=?1")?;
            let sugg = q("SELECT count(*) FROM link_edges WHERE site_id=?1 AND status='suggested'")?;
            let applied = q("SELECT count(*) FROM link_edges WHERE site_id=?1 AND status='applied' AND origin <> 'existing'")?;
            let lh: (i64, i64, i64, i64, i64) = c
                .query_row(
                    "SELECT count(*), ifnull(sum(is_broken),0), ifnull(sum(is_nofollow),0),
                       ifnull(sum(redirect_hops>0),0), ifnull(sum(redirect_hops>2),0)
                     FROM link_health WHERE site_id=?1",
                    [&sid],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .map_err(err)?;
            let mut st = c
                .prepare(
                    "SELECT DISTINCT source_post_id, target_post_id FROM link_edges
                     WHERE site_id=?1 AND (status='applied' OR origin='existing') AND source_post_id<>target_post_id",
                )
                .map_err(err)?;
            let edges: Vec<(String, String)> = st
                .query_map([&sid], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(err)?
                .filter_map(|r| r.ok())
                .collect();
            Ok((rows, clusters, sugg, applied, lh, edges))
        })
        .await?;
    let mut outbound: HashMap<&str, usize> = HashMap::new();
    for (s, _) in &edges {
        *outbound.entry(s.as_str()).or_insert(0) += 1;
    }
    let ids: Vec<&str> = input_rows.iter().map(|r| r.0.as_str()).collect();
    let components = sf_engine::graph::LinkGraph::build(
        &ids,
        edges.iter().map(|(s, t)| (s.as_str(), t.as_str())),
    )
    .ok()
    .map(|(g, _)| sf_engine::graph::weak_components(&g).1);
    let to_us = |v: i64| usize::try_from(v).unwrap_or(0);
    let input = MetricsInput {
        posts: input_rows
            .iter()
            .map(|r| PostMetricRow {
                is_orphan: r.1,
                is_dead_end: r.2,
                click_depth: r.3,
                outbound_link_count: r.5.as_ref().map(|_| r.4),
                committed_outbound: outbound.get(r.0.as_str()).copied().unwrap_or(0),
            })
            .collect(),
        clusters: to_us(clusters),
        edges_suggested: to_us(sugg),
        edges_applied: to_us(applied),
        link_overload_threshold: site
            .setting_f64("link_overload_threshold")
            .map(|v| v as i64),
        min_links_per_post: site
            .setting_f64("min_links_per_post")
            .map(|v| v.max(0.0) as usize),
        link_health: Some(LinkHealthAggregate {
            checked: to_us(lh.0),
            broken_links: to_us(lh.1),
            nofollow_links: to_us(lh.2),
            redirect_links: to_us(lh.3),
            redirect_chains_over_2: to_us(lh.4),
            contextual_orphans: Some(input_rows.iter().filter(|r| r.6).count()),
            nav_only_orphans: Some(input_rows.iter().filter(|r| r.7).count()),
        }),
        components,
    };
    let metrics = serde_json::to_value(compute_site_metrics(&input)).unwrap_or_else(|_| json!({}));
    let (sid, m, src, jid) = (
        site.id.clone(),
        metrics.to_string(),
        source.to_string(),
        job_run_id.map(str::to_string),
    );
    ctx.db
        .write(move |t| {
            t.execute(
                "INSERT INTO site_metrics_history(id, site_id, job_run_id, source, metrics) VALUES(?1,?2,?3,?4,?5)",
                params![sf_core::new_id(), sid, jid, src, m],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    Ok(metrics)
}
