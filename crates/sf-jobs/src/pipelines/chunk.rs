//! Incremental chunking: only posts whose `content_hash` differs from `chunked_hash` are split
//! (with the same `SplitMode` the injector uses), paragraph rows are matched by hash first so
//! unchanged paragraphs keep their id and embedding (fixes index-shift churn, B-35), and the
//! post's live in-content links are re-synced as `origin='existing'` edges (prunes stale ones, B-11).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rusqlite::params;
use serde::Serialize;
use sf_engine::html::{self, SplitMode};
use sf_store::err;

use super::common::{SiteInfo, path_index, site_hosts, truncate_utf16};
use crate::ctx::{Job, JobContext, JobResult, compute};

/// The splitter used for chunking AND insertion. Never change one without the other.
pub const SPLIT_MODE: SplitMode = SplitMode::Prose;

const BATCH: usize = 50;

#[derive(Debug, Clone, Default, Serialize)]
pub struct ChunkStats {
    pub chunked_posts: usize,
    /// Paragraph rows of the chunked posts after chunking.
    pub paragraphs: usize,
    pub kept: usize,
    pub inserted: usize,
    pub deleted: usize,
    /// Live internal links recorded as `origin='existing'` edges.
    pub links: usize,
}

struct Chunked {
    id: String,
    hash: String,
    paras: Vec<(String, String)>,
    links: Vec<(String, String)>,
}

/// Chunk every changed post of the site. `progress(lo, hi)` maps this phase onto a pct range.
pub async fn chunk_changed(
    ctx: &JobContext,
    job: &Job,
    site: &SiteInfo,
    phase: &str,
    pct_range: (f64, f64),
) -> JobResult<ChunkStats> {
    let sid = site.id.clone();
    let total: i64 = ctx
        .db
        .read(move |c| {
            c.query_row(
                "SELECT count(*) FROM posts WHERE site_id=?1 AND content_hash <> chunked_hash",
                [&sid],
                |r| r.get(0),
            )
            .map_err(err)
        })
        .await?;
    let mut stats = ChunkStats::default();
    if total == 0 {
        return Ok(stats);
    }
    let (paths, urls) = path_index(ctx, &site.id).await?;
    let paths = Arc::new(paths);
    let hosts = Arc::new(site_hosts(site, &urls));
    let mut after = String::new();
    loop {
        job.checkpoint().await?;
        let sid = site.id.clone();
        let a = after.clone();
        let batch: Vec<(String, String, String, Option<String>, String)> = ctx
            .db
            .read(move |c| {
                let mut st = c
                    .prepare(
                        "SELECT id, url, content_html, content_raw, content_hash FROM posts
                         WHERE site_id=?1 AND content_hash <> chunked_hash AND id > ?2 ORDER BY id LIMIT ?3",
                    )
                    .map_err(err)?;
                let v = st
                    .query_map(params![sid, a, BATCH as i64], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                    })
                    .map_err(err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(err)?;
                Ok(v)
            })
            .await?;
        let Some(last) = batch.last() else { break };
        after = last.0.clone();
        let (p2, h2) = (paths.clone(), hosts.clone());
        let chunked: Vec<Chunked> = compute(move || {
            use rayon::prelude::*;
            batch
                .par_iter()
                .map(|(id, url, rendered, raw, hash)| {
                    chunk_one(id, url, rendered, raw.as_deref(), hash, &p2, &h2)
                })
                .collect()
        })
        .await?;
        let site_id = site.id.clone();
        let s = ctx
            .db
            .write(move |t| {
                let mut s = ChunkStats::default();
                for c in &chunked {
                    persist_one(t, &site_id, c, &mut s)?;
                }
                Ok(s)
            })
            .await?;
        stats.chunked_posts += s.chunked_posts;
        stats.paragraphs += s.paragraphs;
        stats.kept += s.kept;
        stats.inserted += s.inserted;
        stats.deleted += s.deleted;
        stats.links += s.links;
        let frac = stats.chunked_posts as f64 / total.max(1) as f64;
        job.progress(
            phase,
            pct_range.0 + (pct_range.1 - pct_range.0) * frac.min(1.0),
        )
        .await;
    }
    if stats.chunked_posts > 0 {
        // Re-chunking inserts, deletes and re-indexes paragraphs even when nothing needs a new
        // embedding, so cached `para_meta` ids/idx would point at the wrong text (R16).
        ctx.vectors.invalidate(&site.id);
    }
    Ok(stats)
}

fn chunk_one(
    id: &str,
    url: &str,
    rendered: &str,
    raw: Option<&str>,
    hash: &str,
    paths: &HashMap<String, String>,
    hosts: &[String],
) -> Chunked {
    let source = match raw {
        Some(r) if !r.trim().is_empty() => r,
        _ => rendered,
    };
    let paras = html::paragraphs(source, SPLIT_MODE)
        .map(|v| v.into_iter().map(|p| (p.text, p.hash)).collect())
        .unwrap_or_default();
    let mut links = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    if let Ok(a) = html::analyze(rendered, Some(url), hosts) {
        for l in a.links {
            if l.is_external != Some(false) {
                continue;
            }
            let Some(resolved) = l.resolved_url.as_deref() else {
                continue;
            };
            let Some(target) =
                sf_engine::graph::url_path_key(resolved, None).and_then(|k| paths.get(&k))
            else {
                continue;
            };
            if target == id {
                continue;
            }
            let anchor = truncate_utf16(&l.text, 255);
            if seen.insert((target.clone(), anchor.clone())) {
                links.push((target.clone(), anchor));
            }
        }
    }
    Chunked {
        id: id.to_string(),
        hash: hash.to_string(),
        paras,
        links,
    }
}

fn persist_one(
    t: &rusqlite::Transaction<'_>,
    site_id: &str,
    c: &Chunked,
    s: &mut ChunkStats,
) -> sf_core::Result<()> {
    let mut existing: HashMap<String, String> = HashMap::new();
    let mut dup_ids: Vec<String> = Vec::new();
    {
        let mut st = t
            .prepare("SELECT id, hash FROM paragraphs WHERE post_id=?1 ORDER BY idx")
            .map_err(err)?;
        let rows = st
            .query_map([&c.id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(err)?;
        for row in rows {
            let (id, hash) = row.map_err(err)?;
            match existing.entry(hash) {
                std::collections::hash_map::Entry::Occupied(_) => dup_ids.push(id),
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(id);
                }
            }
        }
    }
    t.execute(
        "UPDATE paragraphs SET idx = -1 - idx WHERE post_id=?1",
        [&c.id],
    )
    .map_err(err)?;
    for (i, (text, hash)) in c.paras.iter().enumerate() {
        if let Some(pid) = existing.remove(hash) {
            t.execute(
                "UPDATE paragraphs SET idx=?2 WHERE id=?1",
                params![pid, i as i64],
            )
            .map_err(err)?;
            s.kept += 1;
        } else {
            t.execute(
                "INSERT INTO paragraphs(id, post_id, idx, text, hash) VALUES(?1,?2,?3,?4,?5)",
                params![super::ids::row_id(), c.id, i as i64, text, hash],
            )
            .map_err(err)?;
            s.inserted += 1;
        }
    }
    for pid in existing.into_values().chain(dup_ids) {
        t.execute("DELETE FROM paragraphs WHERE id=?1", [&pid])
            .map_err(err)?;
        s.deleted += 1;
    }
    t.execute(
        "UPDATE posts SET chunked_hash=?2 WHERE id=?1",
        params![c.id, c.hash],
    )
    .map_err(err)?;
    t.execute(
        "DELETE FROM link_edges WHERE source_post_id=?1 AND origin='existing'",
        [&c.id],
    )
    .map_err(err)?;
    let now = sf_core::now_rfc3339();
    for (target, anchor) in &c.links {
        let n = t
            .execute(
                "INSERT INTO link_edges(id, site_id, source_post_id, target_post_id, anchor, status, origin, applied_at)
                 VALUES(?1,?2,?3,?4,?5,'applied','existing',?6)
                 ON CONFLICT(site_id, source_post_id, target_post_id, anchor) DO NOTHING",
                params![super::ids::row_id(), site_id, c.id, target, anchor, now],
            )
            .map_err(err)?;
        s.links += n;
    }
    s.chunked_posts += 1;
    s.paragraphs += c.paras.len();
    Ok(())
}
