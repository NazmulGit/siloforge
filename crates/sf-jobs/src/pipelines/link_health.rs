//! `link_health`: check every link the site's posts publish (once per distinct target), upsert
//! `link_health` rows (unique site/post/href/anchor), prune rows whose link disappeared, write the
//! per-post rollups and the silo-drift summary, and detect each post's breadcrumbs from a live
//! GET of its own page (`posts.has_breadcrumbs` / `has_breadcrumb_schema` / `breadcrumb_report`).
//! Stats = v1 `LinkHealthRunStats` (camelCase) plus `distinct_urls` and `breadcrumbs`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;
use rusqlite::params;
use serde_json::{Value, json};
use sf_crawl::{
    BreadcrumbReport, HttpClient, LinkCheckInput, LinkHealthRow, TraceOptions, check_links,
    detect_breadcrumbs, extract_links,
};
use sf_engine::silo::{AppliedSiloEdge, PairSet, silo_drift};
use sf_store::err;

use super::common::{SiteInfo, load_site, path_index, site_hosts};
use crate::ctx::{Job, JobContext, JobResult, compute};

pub async fn run(ctx: &Arc<JobContext>, job: &Job) -> JobResult<Value> {
    let site = load_site(ctx, job.site()?).await?;
    let v = check_site(ctx, job, &site, (0.0, 100.0)).await?;
    Ok(v)
}

pub async fn check_site(
    ctx: &JobContext,
    job: &Job,
    site: &SiteInfo,
    pct: (f64, f64),
) -> JobResult<Value> {
    let started = sf_core::now_rfc3339();
    let (paths, urls) = path_index(ctx, &site.id).await?;
    let mut host_values = site_hosts(site, &urls);
    host_values.push(site.base_url());
    let hosts = Arc::new(sf_crawl::html::site_host_set(host_values.iter()));

    // 1. extract links post by post (bounded batches)
    let mut inputs: Vec<LinkCheckInput> = Vec::new();
    let mut links_per_post: HashMap<String, usize> = HashMap::new();
    let mut post_ids: Vec<String> = Vec::new();
    // content_hash each post had when its links were extracted (R21: a crawl may run beside us)
    let mut post_hash: HashMap<String, String> = HashMap::new();
    let mut post_pages: Vec<(String, String)> = Vec::new(); // (id, url) for breadcrumb detection
    let mut after = String::new();
    loop {
        job.checkpoint().await?;
        let (sid, a) = (site.id.clone(), after.clone());
        let rows: Vec<(String, String, String, String)> = ctx
            .db
            .read(move |c| {
                let mut st = c
                    .prepare(
                        "SELECT id, url, content_html, content_hash FROM posts WHERE site_id=?1 AND id > ?2 ORDER BY id LIMIT 200",
                    )
                    .map_err(err)?;
                let v = st
                    .query_map(params![sid, a], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                    })
                    .map_err(err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(err)?;
                Ok(v)
            })
            .await?;
        let Some(last) = rows.last() else { break };
        after = last.0.clone();
        let batch: Vec<(String, String, String)> = rows
            .into_iter()
            .map(|(id, url, html, hash)| {
                post_hash.insert(id.clone(), hash);
                (id, url, html)
            })
            .collect();
        post_pages.extend(batch.iter().map(|(id, url, _)| (id.clone(), url.clone())));
        let h = hosts.clone();
        let found: Vec<(String, Vec<sf_crawl::ExtractedLink>)> = compute(move || {
            use rayon::prelude::*;
            batch
                .par_iter()
                .map(|(id, url, html)| (id.clone(), extract_links(html, url, &h)))
                .collect()
        })
        .await?;
        for (id, links) in found {
            links_per_post.insert(id.clone(), links.len());
            post_ids.push(id.clone());
            for l in links {
                inputs.push(LinkCheckInput {
                    source_post_id: Some(id.clone()),
                    link: l,
                });
            }
        }
    }
    job.info(format!(
        "link health: {} links in {} posts",
        inputs.len(),
        post_ids.len()
    ))
    .await;

    // 2. check (progress via a ticker; the checker's callback is synchronous)
    let client = site.http_client(ctx)?;
    let done = Arc::new(AtomicUsize::new(0));
    let total = Arc::new(AtomicUsize::new(0));
    let (d2, t2) = (done.clone(), total.clone());
    let progress = move |c: usize, t: usize| {
        d2.store(c, Ordering::Relaxed);
        t2.store(t, Ordering::Relaxed);
    };
    let mut opts = ctx.link_check.clone();
    // Payload `internal_only` (or `internalOnly`) skips HTTP checks of external links for this
    // run (rows are still written, unchecked). Pilot 2026-10-13: a test site full of affiliate
    // links had no way to avoid hitting thousands of third-party URLs.
    if job.payload_bool("internal_only", "internalOnly") {
        opts.internal_only = true;
    }
    if opts.internal_only {
        job.info("external links are listed but not checked (internal_only)")
            .await;
    }
    let report = {
        let cb: &(dyn Fn(usize, usize) + Sync) = &progress;
        let check = check_links(&client, &inputs, &opts, Some(cb));
        tokio::pin!(check);
        loop {
            tokio::select! {
                r = &mut check => break r,
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                    job.checkpoint().await?;
                    let (c, t) = (done.load(Ordering::Relaxed), total.load(Ordering::Relaxed));
                    if t > 0 {
                        job.progress("check", pct.0 + (pct.1 - pct.0) * 0.75 * c as f64 / t as f64).await;
                    }
                }
            }
        }
    };
    drop(inputs);

    // 2b. breadcrumbs: they live on the rendered page (theme + wp_head), never in stored content
    let crumbs = breadcrumb_pass(ctx, job, site, &client, &hosts, post_pages, pct).await?;

    // 3. upsert rows + prune + rollups
    let rows = report.rows;
    let mut stats = json!({
        "checked": 0, "broken": 0, "nofollow": 0, "redirected": 0, "chainsOverTwo": 0, "loops": 0,
        "tempRedirects": 0, "skipped": report.stats.skipped, "unverified": 0, "blocked": 0,
        "serverError": 0, "timeouts": 0, "timeoutsRecovered": report.stats.timeouts_recovered,
        "distinct_urls": report.stats.distinct_urls,
        "breadcrumbs": crumbs,
    });
    let mut counters: HashMap<&'static str, usize> = HashMap::new();
    let mut per_post: HashMap<String, (usize, usize, usize, usize)> = HashMap::new(); // internal, broken, nofollow, redirected
    let mut live = PairSet::new();
    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    let mut to_write: Vec<(LinkHealthRow, Option<String>)> = Vec::new();
    for r in rows {
        let src = r.source_post_id.clone().unwrap_or_default();
        if !seen.insert((src.clone(), r.href.clone(), r.anchor.clone())) {
            continue;
        }
        let target = if r.scope == "internal" {
            r.resolved_url
                .as_deref()
                .or(Some(r.requested_url.as_str()))
                .and_then(|u| sf_engine::graph::url_path_key(u, None))
                .and_then(|k| paths.get(&k).cloned())
                .or_else(|| {
                    sf_engine::graph::url_path_key(&r.requested_url, None)
                        .and_then(|k| paths.get(&k).cloned())
                })
        } else {
            None
        };
        if let Some(t) = &target
            && *t != src
        {
            live.insert(&src, t);
        }
        let e = per_post.entry(src).or_default();
        if r.scope == "internal" {
            e.0 += 1;
        }
        *counters.entry("checked").or_insert(0) += 1;
        if !r.skipped {
            if r.is_broken {
                e.1 += 1;
                *counters.entry("broken").or_insert(0) += 1;
            }
            if r.is_unverified {
                *counters.entry("unverified").or_insert(0) += 1;
            }
            match r.failure_kind.as_str() {
                "blocked" => *counters.entry("blocked").or_insert(0) += 1,
                "server_error" => *counters.entry("serverError").or_insert(0) += 1,
                "timeout" => *counters.entry("timeouts").or_insert(0) += 1,
                _ => {}
            }
            if r.redirect_hops > 0 {
                e.3 += 1;
                *counters.entry("redirected").or_insert(0) += 1;
            }
            if r.redirect_hops > 2 {
                *counters.entry("chainsOverTwo").or_insert(0) += 1;
            }
            if r.has_redirect_loop {
                *counters.entry("loops").or_insert(0) += 1;
            }
            if r.has_temp_redirect {
                *counters.entry("tempRedirects").or_insert(0) += 1;
            }
        }
        if r.is_nofollow {
            e.2 += 1;
            *counters.entry("nofollow").or_insert(0) += 1;
        }
        to_write.push((r, target));
    }
    if let Some(o) = stats.as_object_mut() {
        for (k, v) in counters {
            o.insert(k.to_string(), json!(v));
        }
    }
    job.progress("save", pct.0 + (pct.1 - pct.0) * 0.92).await;
    for chunk in to_write.chunks(1000) {
        job.checkpoint().await?;
        let (sid, now, rows) = (site.id.clone(), started.clone(), chunk.to_vec());
        ctx.db
            .write(move |t| {
                let mut st = t
                    .prepare(
                        "INSERT INTO link_health(id, site_id, source_post_id, href, requested_url, resolved_url, target_post_id,
                           scope, status_code, ok, is_broken, redirect_hops, redirect_chain, has_redirect_loop,
                           has_temp_redirect, rel, is_nofollow, is_sponsored, is_ugc, anchor, placement, error, checked_at,
                           failure_kind, is_unverified)
                         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)
                         ON CONFLICT(site_id, source_post_id, href, anchor) DO UPDATE SET
                           requested_url=excluded.requested_url, resolved_url=excluded.resolved_url,
                           target_post_id=excluded.target_post_id, scope=excluded.scope, status_code=excluded.status_code,
                           ok=excluded.ok, is_broken=excluded.is_broken, redirect_hops=excluded.redirect_hops,
                           redirect_chain=excluded.redirect_chain, has_redirect_loop=excluded.has_redirect_loop,
                           has_temp_redirect=excluded.has_temp_redirect, rel=excluded.rel, is_nofollow=excluded.is_nofollow,
                           is_sponsored=excluded.is_sponsored, is_ugc=excluded.is_ugc, placement=excluded.placement,
                           error=excluded.error, checked_at=excluded.checked_at, failure_kind=excluded.failure_kind,
                           is_unverified=excluded.is_unverified",
                    )
                    .map_err(err)?;
                for (r, target) in &rows {
                    let placement = match r.placement.as_str() {
                        "body" | "nav" | "header" | "footer" | "aside" => r.placement.as_str(),
                        _ => "unknown",
                    };
                    st.execute(params![
                        sf_core::new_id(),
                        sid,
                        r.source_post_id,
                        r.href,
                        r.requested_url,
                        r.resolved_url,
                        target,
                        if r.scope == "external" { "external" } else { "internal" },
                        r.status_code.map(i64::from),
                        r.ok,
                        r.is_broken,
                        r.redirect_hops as i64,
                        r.redirect_chain_json(),
                        r.has_redirect_loop,
                        r.has_temp_redirect,
                        r.rel,
                        r.is_nofollow,
                        r.is_sponsored,
                        r.is_ugc,
                        r.anchor,
                        placement,
                        r.error,
                        now,
                        r.failure_kind.as_str(),
                        r.is_unverified
                    ])
                    .map_err(err)?;
                }
                Ok(())
            })
            .await?;
    }
    let (sid, cutoff) = (site.id.clone(), started.clone());
    let pruned = ctx
        .db
        .write(move |t| {
            t.execute(
                "DELETE FROM link_health WHERE site_id=?1 AND checked_at < ?2",
                params![sid, cutoff],
            )
            .map_err(err)
        })
        .await?;
    let mut rollup_stale = 0usize;
    for chunk in post_ids.chunks(1000) {
        let rows: Vec<Rollup> = chunk
            .iter()
            .map(|id| {
                let (internal, broken, nofollow, redirected) =
                    per_post.get(id).copied().unwrap_or_default();
                Rollup {
                    id: id.clone(),
                    content_hash: post_hash.get(id).cloned().unwrap_or_default(),
                    total: links_per_post.get(id).copied().unwrap_or(0),
                    internal,
                    broken,
                    nofollow,
                    redirected,
                }
            })
            .collect();
        let now = started.clone();
        rollup_stale += ctx.db.write(move |t| write_rollups(t, &rows, &now)).await?;
    }
    if rollup_stale > 0 {
        job.info(format!(
            "link health: {rollup_stale} posts changed during the check; their link counts are left for the next run"
        ))
        .await;
    }
    // silo drift: applied topology edges no longer backed by a live link
    let sid = site.id.clone();
    let applied: Vec<AppliedSiloEdge> = ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare(
                    "SELECT source_post_id, target_post_id, silo_topology FROM link_edges
                     WHERE site_id=?1 AND status='applied' AND silo_topology IS NOT NULL",
                )
                .map_err(err)?;
            let v = st
                .query_map([&sid], |r| {
                    Ok(AppliedSiloEdge {
                        source: r.get(0)?,
                        target: r.get(1)?,
                        topology: r.get(2)?,
                    })
                })
                .map_err(err)?
                .filter_map(|r| r.ok())
                .collect();
            Ok(v)
        })
        .await?;
    let drift = silo_drift(&applied, &live);
    if let Some(o) = stats.as_object_mut() {
        o.insert("siloApplied".into(), json!(drift.total_applied));
        o.insert("siloDrifted".into(), json!(drift.total_drifted));
        o.insert("pruned".into(), json!(pruned));
        o.insert("rollupStale".into(), json!(rollup_stale));
        o.insert("progress".into(), json!({"phase": "done", "pct": 100}));
    }
    job.info(format!(
        "link health done: {} links, {} broken, {} pruned",
        stats["checked"], stats["broken"], pruned
    ))
    .await;
    Ok(stats)
}

/// Per-post link counts computed from the HTML read at the start of the check.
pub(crate) struct Rollup {
    pub id: String,
    /// `posts.content_hash` when the links were extracted.
    pub content_hash: String,
    pub total: usize,
    pub internal: usize,
    pub broken: usize,
    pub nofollow: usize,
    pub redirected: usize,
}

/// Write the per-post rollups, skipping posts whose content changed since they were read (a crawl
/// may run beside link_health; R21). Returns how many were skipped as stale.
pub(crate) fn write_rollups(
    t: &rusqlite::Transaction<'_>,
    rows: &[Rollup],
    now: &str,
) -> sf_core::Result<usize> {
    let mut st = t
        .prepare(
            "UPDATE posts SET outbound_link_count=?2, internal_link_count=?3, broken_link_count=?4,
               nofollow_link_count=?5, redirect_link_count=?6, link_health_checked_at=?7
             WHERE id=?1 AND content_hash=?8",
        )
        .map_err(err)?;
    let mut stale = 0usize;
    for r in rows {
        let n = st
            .execute(params![
                r.id,
                r.total as i64,
                r.internal as i64,
                r.broken as i64,
                r.nofollow as i64,
                r.redirected as i64,
                now,
                r.content_hash
            ])
            .map_err(err)?;
        if n == 0 {
            stale += 1;
        }
    }
    Ok(stale)
}

/// Posts whose breadcrumb report is written per transaction (also the cancellation granularity).
const BREADCRUMB_BATCH: usize = 50;

/// GET each post's own page (internal hosts only, bounded concurrency and body size), detect
/// breadcrumbs and write the three `posts` columns in batched transactions. A failed fetch (or a
/// non-HTML / non-200 final response) leaves the post's previous values untouched.
async fn breadcrumb_pass(
    ctx: &JobContext,
    job: &Job,
    site: &SiteInfo,
    client: &HttpClient,
    hosts: &HashSet<String>,
    pages: Vec<(String, String)>,
    pct: (f64, f64),
) -> JobResult<Value> {
    let pages: Vec<(String, String)> = pages
        .into_iter()
        .filter(|(_, url)| sf_crawl::html::host_key(url).is_some_and(|h| hosts.contains(&h)))
        .collect();
    let topts = TraceOptions {
        prefer_get: true,
        capture_body: true,
        max_body_bytes: sf_crawl::MAX_BREADCRUMB_HTML_BYTES,
        ..ctx.link_check.trace.clone()
    };
    let concurrency = ctx.link_check.concurrency.clamp(1, 8);
    let total = pages.len();
    let (mut checked, mut failed, mut visible, mut schema) = (0usize, 0usize, 0usize, 0usize);
    for (n, chunk) in pages.chunks(BREADCRUMB_BATCH).enumerate() {
        job.checkpoint().await?;
        let results: Vec<(String, Option<BreadcrumbReport>)> =
            futures::stream::iter(chunk.iter().cloned())
                .map(|(id, url)| {
                    let topts = &topts;
                    async move {
                        let body = sf_crawl::trace(client, &url, topts).await.body;
                        let report = match body {
                            Some(html) => compute(move || detect_breadcrumbs(&html)).await.ok(),
                            None => None,
                        };
                        (id, report)
                    }
                })
                .buffer_unordered(concurrency)
                .collect()
                .await;
        let mut rows: Vec<(String, bool, bool, String)> = Vec::with_capacity(results.len());
        for (id, report) in results {
            match report {
                Some(r) => {
                    checked += 1;
                    visible += usize::from(r.has_breadcrumbs());
                    schema += usize::from(r.has_breadcrumb_schema());
                    rows.push((
                        id,
                        r.has_breadcrumbs(),
                        r.has_breadcrumb_schema(),
                        r.to_json_string(),
                    ));
                }
                None => failed += 1,
            }
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        if !rows.is_empty() {
            let sid = site.id.clone();
            ctx.db
                .write(move |t| {
                    let mut st = t
                        .prepare(
                            "UPDATE posts SET has_breadcrumbs=?3, has_breadcrumb_schema=?4, breadcrumb_report=?5
                              WHERE id=?1 AND site_id=?2",
                        )
                        .map_err(err)?;
                    for (id, vis, sch, rep) in &rows {
                        st.execute(params![id, sid, vis, sch, rep]).map_err(err)?;
                    }
                    Ok(())
                })
                .await?;
        }
        let done = ((n + 1) * BREADCRUMB_BATCH).min(total);
        if total > 0 {
            job.progress(
                "breadcrumbs",
                pct.0 + (pct.1 - pct.0) * (0.75 + 0.15 * done as f64 / total as f64),
            )
            .await;
        }
    }
    if failed > 0 {
        tracing::info!(site = %site.id, failed, "breadcrumbs: some pages could not be fetched");
    }
    Ok(json!({
        "checked": checked, "failed": failed, "with_visible": visible, "with_schema": schema,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn rollups_skip_posts_changed_by_a_concurrent_crawl() {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        sf_store::migrate::run(&mut c).unwrap();
        c.execute_batch(
            "INSERT INTO sites(id, domain, api_key) VALUES('s','a.com','k');
             INSERT INTO posts(id, site_id, url, content_hash) VALUES('p1','s','https://a.com/1','h1');
             INSERT INTO posts(id, site_id, url, content_hash) VALUES('p2','s','https://a.com/2','h2-new');",
        )
        .unwrap();
        let row = |id: &str, hash: &str| Rollup {
            id: id.into(),
            content_hash: hash.into(),
            total: 5,
            internal: 3,
            broken: 1,
            nofollow: 0,
            redirected: 1,
        };
        let t = c.transaction().unwrap();
        // p2 was re-crawled after link_health read it (hash h2 -> h2-new); p3 was deleted
        let stale = write_rollups(
            &t,
            &[row("p1", "h1"), row("p2", "h2"), row("p3", "h3")],
            "2026-10-14T00:00:00.000Z",
        )
        .unwrap();
        t.commit().unwrap();
        assert_eq!(stale, 2);
        let counts: Vec<(i64, Option<String>)> = c
            .prepare("SELECT outbound_link_count, link_health_checked_at FROM posts ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(counts[0], (5, Some("2026-10-14T00:00:00.000Z".into())));
        assert_eq!(counts[1], (0, None));
    }
}
