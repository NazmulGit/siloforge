//! `crawl`: WordPress REST (raw `context=edit` content when the site has an application
//! password) or sitemap crawl, streamed page by page into batched upserts, then incremental
//! chunking of the changed posts. Stats (v1 shape + extras):
//! `{upserted, skipped, paragraphs, chunked_posts, posts_seen, mode, raw_content}`.

use std::sync::Arc;

use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sf_crawl::wp::gmt_to_iso;
use sf_crawl::{CrawlError, CrawledPost, HttpClient, SitemapCrawler, SitemapOptions, WpCrawler};
use sf_store::err;
use tokio::sync::mpsc;

use super::chunk;
use super::common::{SiteInfo, content_hash, load_site};
use crate::ctx::{Job, JobContext, JobError, JobResult, compute};

#[derive(Debug, Clone, Default)]
pub struct CrawlTotals {
    pub upserted: usize,
    pub skipped: usize,
    pub posts_seen: usize,
    pub raw_content: bool,
}

pub async fn run(ctx: &Arc<JobContext>, job: &Job) -> JobResult<Value> {
    let site = load_site(ctx, job.site()?).await?;
    job.info(format!("crawl started ({} mode)", site.mode))
        .await;
    let totals = crawl_site(ctx, job, &site, (0.0, 80.0)).await?;
    job.info(format!(
        "crawled {} posts: {} upserted, {} unchanged",
        totals.posts_seen, totals.upserted, totals.skipped
    ))
    .await;
    let ch = chunk::chunk_changed(ctx, job, &site, "chunk", (80.0, 99.0)).await?;
    job.info(format!(
        "chunked {} changed posts ({} paragraphs, {} kept)",
        ch.chunked_posts, ch.paragraphs, ch.kept
    ))
    .await;
    let paragraphs = finish_sync(ctx, &site.id).await?;
    Ok(json!({
        "upserted": totals.upserted,
        "skipped": totals.skipped,
        "paragraphs": paragraphs,
        "chunked_posts": ch.chunked_posts,
        "posts_seen": totals.posts_seen,
        "mode": site.mode,
        "raw_content": totals.raw_content,
        "links_synced": ch.links,
        "progress": {"phase": "done", "pct": 100},
    }))
}

/// Stamp `last_sync_at` and return the site's paragraph count.
pub async fn finish_sync(ctx: &JobContext, site_id: &str) -> JobResult<i64> {
    let sid = site_id.to_string();
    Ok(ctx
        .db
        .write(move |t| {
            t.execute(
                "UPDATE sites SET last_sync_at=?2 WHERE id=?1",
                params![sid, sf_core::now_rfc3339()],
            )
            .map_err(err)?;
            t.query_row(
                "SELECT count(*) FROM paragraphs pa JOIN posts p ON p.id=pa.post_id WHERE p.site_id=?1",
                [&sid],
                |r| r.get(0),
            )
            .map_err(err)
        })
        .await?)
}

/// Crawl + upsert (no chunking). Used by `crawl` and the `audit` crawl stage.
pub async fn crawl_site(
    ctx: &Arc<JobContext>,
    job: &Job,
    site: &SiteInfo,
    pct: (f64, f64),
) -> JobResult<CrawlTotals> {
    let client = site.http_client(ctx)?;
    if site.mode == "sitemap" {
        return crawl_sitemap(ctx, job, site, client, pct).await;
    }
    let creds = site.has_wp_credentials();
    match crawl_wp(ctx, job, site, client.clone(), creds, pct).await {
        Err((e, true)) if creds => {
            job.warn(format!(
                "WordPress rejected the application password ({e}); crawling rendered content only"
            ))
            .await;
            crawl_wp(ctx, job, site, client, false, pct)
                .await
                .map_err(|(e, _)| e)
        }
        r => r.map_err(|(e, _)| e),
    }
}

/// Err carries `(error, is_auth_failure)`.
async fn crawl_wp(
    ctx: &Arc<JobContext>,
    job: &Job,
    site: &SiteInfo,
    client: HttpClient,
    with_creds: bool,
    pct: (f64, f64),
) -> Result<CrawlTotals, (JobError, bool)> {
    let wp = site.wp_site(with_creds).map_err(|e| (e, false))?;
    let crawler = WpCrawler::new(client, wp);
    let (tx, mut rx) = mpsc::channel(2);
    let task = tokio::spawn(async move { crawler.crawl(tx).await });
    let mut totals = CrawlTotals {
        raw_content: with_creds,
        ..Default::default()
    };
    let mut pages_done = 0u32;
    while let Some(page) = rx.recv().await {
        if let Err(e) = job.checkpoint().await {
            task.abort();
            return Err((e, false));
        }
        pages_done += 1;
        if let Err(e) = upsert_batch(ctx, site, page.posts, &mut totals).await {
            task.abort();
            return Err((e, false));
        }
        let frac = match page.total_pages {
            Some(t) if t > 0 => (f64::from(page.page) / f64::from(t)).min(1.0),
            _ => 0.5,
        };
        // posts first then pages: weight both collections roughly equally
        let base = if page.post_type == "page" { 0.5 } else { 0.0 };
        job.progress("crawl", pct.0 + (pct.1 - pct.0) * (base + frac * 0.5))
            .await;
    }
    match task.await {
        Ok(Ok(stats)) => {
            tracing::debug!(site = %site.id, pages = pages_done, ?stats, "wp crawl finished");
            Ok(totals)
        }
        Ok(Err(e)) => {
            let auth = e.is_auth();
            Err((JobError::from(e), auth))
        }
        Err(e) => Err((JobError::msg(format!("crawl task: {e}")), false)),
    }
}

async fn crawl_sitemap(
    ctx: &Arc<JobContext>,
    job: &Job,
    site: &SiteInfo,
    client: HttpClient,
    pct: (f64, f64),
) -> JobResult<CrawlTotals> {
    let opts = SitemapOptions {
        max_urls: ctx.cfg.crawl.max_pages.clamp(1, 200_000),
        ..SitemapOptions::default()
    };
    let crawler = SitemapCrawler::new(client, &site.base_url(), opts)?;
    let (tx, mut rx) = mpsc::channel::<Vec<CrawledPost>>(2);
    let task = tokio::spawn(async move { crawler.crawl(tx).await });
    let mut totals = CrawlTotals::default();
    while let Some(batch) = rx.recv().await {
        if let Err(e) = job.checkpoint().await {
            task.abort();
            return Err(e);
        }
        if let Err(e) = upsert_batch(ctx, site, batch, &mut totals).await {
            task.abort();
            return Err(e);
        }
        job.progress("crawl", pct.0 + (pct.1 - pct.0) * 0.5).await;
    }
    match task.await {
        Ok(Ok(stats)) => {
            if stats.crawled == 0 && stats.discovered > 0 {
                job.warn(format!(
                    "sitemap listed {} urls but none could be fetched",
                    stats.discovered
                ))
                .await;
            }
            Ok(totals)
        }
        Ok(Err(CrawlError::Cancelled)) => Err(JobError::cancelled()),
        Ok(Err(e)) => Err(e.into()),
        Err(e) => Err(JobError::msg(format!("crawl task: {e}"))),
    }
}

struct PostRow {
    wp_post_id: Option<i64>,
    url: String,
    slug: Option<String>,
    title: String,
    rendered: String,
    raw: Option<String>,
    text: String,
    hash: String,
    builder: String,
    post_type: String,
    status: String,
    meta: String,
    word_count: i64,
    published_at: Option<String>,
    modified_at: Option<String>,
    modified_gmt: Option<String>,
}

fn prepare(p: CrawledPost, money: &std::collections::BTreeSet<String>) -> PostRow {
    let text = sf_engine::html::html_to_text(&p.content_rendered);
    let headings: Vec<String> = sf_engine::html::analyze(&p.content_rendered, None, &[])
        .map(|a| {
            a.headings
                .into_iter()
                .filter(|h| h.level == 2 || h.level == 3)
                .map(|h| h.text)
                .filter(|t| !t.is_empty())
                .take(200)
                .collect()
        })
        .unwrap_or_default();
    let mut meta = serde_json::Map::new();
    meta.insert("h_tags".into(), json!(headings));
    if let Some(d) = p.meta_description() {
        meta.insert("description".into(), json!(d));
    }
    if let Some(k) = p.seo.focus_keyword.as_deref().filter(|k| !k.is_empty()) {
        meta.insert("focus_keyword".into(), json!(k));
    }
    if money.contains(&sf_engine::html::normalize_url(&p.url)) {
        meta.insert("money_page".into(), json!(true));
    }
    let raw = p.content_raw.filter(|r| !r.is_empty());
    let builder = sf_engine::html::detect_builder(&p.content_rendered, raw.as_deref())
        .as_str()
        .to_string();
    PostRow {
        wp_post_id: p.wp_post_id.and_then(|i| i64::try_from(i).ok()),
        hash: content_hash(&p.content_rendered, raw.as_deref()),
        word_count: sf_engine::html::word_count(&text) as i64,
        url: p.url,
        slug: p.slug,
        title: p.title,
        rendered: p.content_rendered,
        raw,
        text,
        builder,
        post_type: if p.post_type.is_empty() {
            "post".into()
        } else {
            p.post_type
        },
        status: if p.status.is_empty() {
            "publish".into()
        } else {
            p.status
        },
        meta: Value::Object(meta).to_string(),
        published_at: p.published_gmt.as_deref().map(gmt_to_iso),
        modified_at: p.modified_gmt.as_deref().map(gmt_to_iso),
        modified_gmt: p.modified_gmt,
    }
}

/// Upsert one streamed batch in a single transaction (unchanged posts are skipped).
pub async fn upsert_batch(
    ctx: &JobContext,
    site: &SiteInfo,
    posts: Vec<CrawledPost>,
    totals: &mut CrawlTotals,
) -> JobResult<()> {
    if posts.is_empty() {
        return Ok(());
    }
    totals.posts_seen += posts.len();
    let money = site.money_urls();
    let rows: Vec<PostRow> =
        compute(move || posts.into_iter().map(|p| prepare(p, &money)).collect()).await?;
    let site_id = site.id.clone();
    let (up, skip) = ctx
        .db
        .write(move |t| {
            let (mut up, mut skip) = (0usize, 0usize);
            let now = sf_core::now_rfc3339();
            for r in &rows {
                let cur: Option<String> = t
                    .query_row(
                        "SELECT content_hash FROM posts WHERE site_id=?1 AND url=?2",
                        params![site_id, r.url],
                        |x| x.get(0),
                    )
                    .optional()
                    .map_err(err)?;
                if cur.as_deref() == Some(r.hash.as_str()) {
                    // unchanged; keep the optimistic-concurrency stamp fresh
                    t.execute(
                        "UPDATE posts SET modified_gmt=coalesce(?3, modified_gmt), crawled_at=?4 WHERE site_id=?1 AND url=?2",
                        params![site_id, r.url, r.modified_gmt, now],
                    )
                    .map_err(err)?;
                    skip += 1;
                    continue;
                }
                t.execute(
                    "INSERT INTO posts(id, site_id, wp_post_id, url, slug, title, content_html, content_text, content_hash,
                        builder, post_type, post_status, meta, word_count, published_at, modified_at, crawled_at,
                        content_raw, modified_gmt)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
                     ON CONFLICT(site_id, url) DO UPDATE SET
                        wp_post_id=coalesce(excluded.wp_post_id, posts.wp_post_id), slug=excluded.slug,
                        title=excluded.title, content_html=excluded.content_html, content_text=excluded.content_text,
                        content_hash=excluded.content_hash, builder=excluded.builder, post_type=excluded.post_type,
                        post_status=excluded.post_status,
                        meta=json_patch(CASE WHEN json_valid(posts.meta) THEN posts.meta ELSE '{}' END, excluded.meta),
                        word_count=excluded.word_count,
                        published_at=coalesce(excluded.published_at, posts.published_at),
                        modified_at=coalesce(excluded.modified_at, posts.modified_at),
                        crawled_at=excluded.crawled_at, content_raw=excluded.content_raw,
                        modified_gmt=excluded.modified_gmt, embedding=NULL",
                    params![
                        sf_core::new_id(),
                        site_id,
                        r.wp_post_id,
                        r.url,
                        r.slug,
                        r.title,
                        r.rendered,
                        r.text,
                        r.hash,
                        r.builder,
                        r.post_type,
                        r.status,
                        r.meta,
                        r.word_count,
                        r.published_at,
                        r.modified_at,
                        now,
                        r.raw,
                        r.modified_gmt
                    ],
                )
                .map_err(err)?;
                up += 1;
            }
            Ok((up, skip))
        })
        .await?;
    totals.upserted += up;
    totals.skipped += skip;
    Ok(())
}
