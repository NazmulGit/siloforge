//! Incremental embedding: posts (`title + "\n" + content_text[:2000]`, v1 §2.2) and paragraphs
//! that have no vector yet, streamed in bounded chunks through `EmbedChain::embed_batched`, every
//! batch persisted immediately (a late failure never loses earlier work, fixes B-32).
//! Stats: `{posts, paragraphs}`.
//!
//! Post vectors: v1 embedded `title + "\n" + content_text[:2000]` (UTF-16 units) on whichever
//! rung served. OpenRouter truncated at 256 word-pieces, v1's local transformers.js rung at 512,
//! so some migrated sites (site30, site69.example) hold 512-token post vectors and others 256.
//! Before re-embedding posts, a few stored long-post vectors are re-embedded at both caps and the
//! cap that reproduces them (cos >= 0.999) is used, so new post vectors match the old ones.
//! Paragraph vectors were always 256 (remote) and stay at 256.

use std::sync::Arc;

use rusqlite::params;
use serde::Serialize;
use serde_json::{Value, json};
use sf_core::types::vec_to_blob;
use sf_store::err;

use super::chunk;
use super::common::{load_site, truncate_utf16};
use crate::ctx::{Job, JobContext, JobError, JobResult};

/// Rows read per chunk (each chunk is embedded in `embed.batch_size` batches). Rows are sorted by
/// text length inside the chunk, so each embed batch holds texts of similar length: the local
/// backends pad a batch to its longest text, and with id-ordered (random-length) batches most of
/// the GPU/CPU time went into padding (docs/testing/performance.md).
const READ_CHUNK: usize = 2048;

/// Sort `(id, text)` pairs by text length (stable: ties keep id order).
fn by_length(pairs: &mut [(String, String)]) {
    pairs.sort_by_key(|(_, t)| t.len());
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct EmbedStats {
    pub posts: usize,
    pub paragraphs: usize,
}

/// `embed` job: chunk changed posts, then embed what is missing.
pub async fn run(ctx: &Arc<JobContext>, job: &Job) -> JobResult<Value> {
    let site = load_site(ctx, job.site()?).await?;
    let ch = chunk::chunk_changed(ctx, job, &site, "chunk", (0.0, 10.0)).await?;
    let s = embed_site(ctx, job, &site.id, "embed", (10.0, 100.0)).await?;
    Ok(json!({
        "posts": s.posts,
        "paragraphs": s.paragraphs,
        "chunked_posts": ch.chunked_posts,
        "progress": {"phase": "done", "pct": 100},
    }))
}

/// Embed every post and paragraph of the site that lacks a vector.
pub async fn embed_site(
    ctx: &JobContext,
    job: &Job,
    site_id: &str,
    phase: &str,
    pct: (f64, f64),
) -> JobResult<EmbedStats> {
    let sid = site_id.to_string();
    let (np, npar): (i64, i64) = ctx
        .db
        .read(move |c| {
            let a = c
                .query_row(
                    "SELECT count(*) FROM posts WHERE site_id=?1 AND embedding IS NULL",
                    [&sid],
                    |r| r.get(0),
                )
                .map_err(err)?;
            let b = c
                .query_row(
                    "SELECT count(*) FROM paragraphs pa JOIN posts p ON p.id=pa.post_id
                     WHERE p.site_id=?1 AND pa.embedding IS NULL",
                    [&sid],
                    |r| r.get(0),
                )
                .map_err(err)?;
            Ok((a, b))
        })
        .await?;
    let total = (np + npar).max(1) as f64;
    let mut stats = EmbedStats::default();
    if np + npar == 0 {
        return Ok(stats);
    }
    let max_chars = 2000usize;
    let post_cap = if np > 0 {
        let cap = detect_post_cap(ctx, site_id).await;
        if cap != sf_embed::MAX_TOKENS {
            job.info(format!(
                "embed: stored post vectors use a {cap}-token cap (v1 local rung); new post vectors match it"
            ))
            .await;
        }
        cap
    } else {
        sf_embed::MAX_TOKENS
    };
    // posts
    let mut after = String::new();
    loop {
        job.checkpoint().await?;
        let (sid, a) = (site_id.to_string(), after.clone());
        let rows: Vec<(String, String, String)> = ctx
            .db
            .read(move |c| {
                let mut st = c
                    .prepare(
                        "SELECT id, title, content_text FROM posts WHERE site_id=?1 AND embedding IS NULL AND id > ?2
                         ORDER BY id LIMIT ?3",
                    )
                    .map_err(err)?;
                let v = st
                    .query_map(params![sid, a, READ_CHUNK as i64], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })
                    .map_err(err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(err)?;
                Ok(v)
            })
            .await?;
        let Some(last) = rows.last() else { break };
        after = last.0.clone();
        let mut pairs: Vec<(String, String)> = rows
            .into_iter()
            .map(|(id, t, body)| (id, format!("{t}\n{}", truncate_utf16(&body, max_chars))))
            .collect();
        by_length(&mut pairs);
        let (ids, texts): (Vec<String>, Vec<String>) = pairs.into_iter().unzip();
        let n = embed_and_store(
            ctx, job, "posts", &ids, &texts, post_cap, &mut stats, total, phase, pct,
        )
        .await?;
        stats.posts += n;
    }
    // paragraphs
    let mut after = String::new();
    loop {
        job.checkpoint().await?;
        let (sid, a) = (site_id.to_string(), after.clone());
        let rows: Vec<(String, String)> = ctx
            .db
            .read(move |c| {
                let mut st = c
                    .prepare(
                        "SELECT pa.id, pa.text FROM paragraphs pa JOIN posts p ON p.id=pa.post_id
                         WHERE p.site_id=?1 AND pa.embedding IS NULL AND pa.id > ?2 ORDER BY pa.id LIMIT ?3",
                    )
                    .map_err(err)?;
                let v = st
                    .query_map(params![sid, a, READ_CHUNK as i64], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })
                    .map_err(err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(err)?;
                Ok(v)
            })
            .await?;
        let Some(last) = rows.last() else { break };
        after = last.0.clone();
        let mut rows = rows;
        by_length(&mut rows);
        let (ids, texts): (Vec<String>, Vec<String>) = rows.into_iter().unzip();
        let n = embed_and_store(
            ctx,
            job,
            "paragraphs",
            &ids,
            &texts,
            sf_embed::MAX_TOKENS,
            &mut stats,
            total,
            phase,
            pct,
        )
        .await?;
        stats.paragraphs += n;
    }
    if stats.posts + stats.paragraphs > 0 {
        ctx.vectors.invalidate(site_id);
    }
    Ok(stats)
}

/// Posts sampled to detect the stored post-vector cap.
const CAP_SAMPLE: i64 = 6;

/// Word-piece cap ([`sf_embed::MAX_TOKENS`] or [`sf_embed::MAX_TOKENS_LONG`]) that reproduces the
/// site's stored post vectors. Long posts only (shorter texts embed the same at both caps); no
/// sample, an embedding error or a tie gives the default 256.
async fn detect_post_cap(ctx: &JobContext, site_id: &str) -> usize {
    let sid = site_id.to_string();
    let rows: Vec<(String, String, Vec<u8>)> = ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare(
                    "SELECT title, content_text, embedding FROM posts
                     WHERE site_id=?1 AND embedding IS NOT NULL AND length(content_text) > 1600
                     ORDER BY id LIMIT ?2",
                )
                .map_err(err)?;
            let v = st
                .query_map(params![sid, CAP_SAMPLE], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .map_err(err)?
                .filter_map(|r| r.ok())
                .collect();
            Ok(v)
        })
        .await
        .unwrap_or_default();
    let stored: Vec<Vec<f32>> = rows
        .iter()
        .filter_map(|(_, _, b)| sf_core::types::blob_to_vec(b))
        .collect();
    if stored.is_empty() || stored.len() != rows.len() {
        return sf_embed::MAX_TOKENS;
    }
    let texts: Vec<String> = rows
        .iter()
        .map(|(t, body, _)| format!("{t}\n{}", truncate_utf16(body, 2000)))
        .collect();
    let matches = |cap: usize| {
        let texts = texts.clone();
        let stored = &stored;
        async move {
            match ctx.embed.embed_cap(&texts, cap).await {
                Ok(v) => v
                    .iter()
                    .zip(stored)
                    .filter(|(a, b)| sf_engine::vector::cosine(a, b) >= 0.999)
                    .count(),
                Err(e) => {
                    tracing::warn!(error = %e, "post-vector cap detection skipped");
                    0
                }
            }
        }
    };
    let short = matches(sf_embed::MAX_TOKENS).await;
    let long = matches(sf_embed::MAX_TOKENS_LONG).await;
    if long > short {
        sf_embed::MAX_TOKENS_LONG
    } else {
        sf_embed::MAX_TOKENS
    }
}

#[allow(clippy::too_many_arguments)]
async fn embed_and_store(
    ctx: &JobContext,
    job: &Job,
    table: &'static str,
    ids: &[String],
    texts: &[String],
    max_tokens: usize,
    stats: &mut EmbedStats,
    total: f64,
    phase: &str,
    pct: (f64, f64),
) -> JobResult<usize> {
    let base_done = (stats.posts + stats.paragraphs) as f64;
    let db = ctx.db.clone();
    let res = ctx
        .embed
        .embed_batched_cap(texts, max_tokens, |b| {
            let db = db.clone();
            let rows: Vec<(String, Vec<u8>)> = b
                .vectors
                .iter()
                .enumerate()
                .filter_map(|(i, v)| ids.get(b.offset + i).map(|id| (id.clone(), vec_to_blob(v))))
                .collect();
            let done = b.done;
            async move {
                db.write(move |t| {
                    let sql = format!("UPDATE {table} SET embedding=?2 WHERE id=?1");
                    let mut st = t.prepare(&sql).map_err(err)?;
                    for (id, blob) in &rows {
                        st.execute(params![id, blob]).map_err(err)?;
                    }
                    Ok(())
                })
                .await?;
                job.progress(
                    phase,
                    pct.0 + (pct.1 - pct.0) * ((base_done + done as f64) / total).min(1.0),
                )
                .await;
                if job.cancel_requested() {
                    return Err(sf_core::Error::Cancelled);
                }
                Ok(())
            }
        })
        .await;
    match res {
        Ok(n) => Ok(n),
        Err(sf_core::Error::Cancelled) => Err(JobError::cancelled()),
        // Every rung failed (or timed out): a later retry may succeed.
        Err(e @ sf_core::Error::Upstream(_)) => Err(JobError::retry(e)),
        Err(e) => Err(JobError::fatal(e)),
    }
}
