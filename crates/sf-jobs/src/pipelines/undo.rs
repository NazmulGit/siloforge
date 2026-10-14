//! `undo`: remove injected links precisely. Per post: fresh raw → `remove_link(url, anchor,
//! near = link_start)` for each edge (later edits survive, unlike v1's snapshot restore) → one
//! update with the `modified_gmt` precondition (one re-read on conflict) → edges `reverted`.
//! A link that is no longer in the content is marked reverted with the note `link not present`.
//! `payload.mode = "snapshot"` restores the earliest pre-inject snapshot instead (every edge of
//! that post applied after the snapshot is reverted with it).
//!
//! Stats: `{reverted, not_present, failed, posts_updated}`.

use std::collections::BTreeMap;
use std::sync::Arc;

use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sf_crawl::{CrawlError, WpWriter};
use sf_engine::insert::{InsertError, remove_link};
use sf_store::err;

use super::common::{SiteInfo, load_site, placeholders};
use super::inject::{
    Journal, JournalEdge, Written, journal_abandon, journal_begin, put_checked, save_post_tx,
};
use crate::ctx::{Job, JobContext, JobResult};

#[derive(Debug, Clone)]
struct EdgeRow {
    id: String,
    source: String,
    anchor: String,
    target_url: String,
    link_start: Option<i64>,
    snapshot_id: Option<String>,
    wp_id: Option<i64>,
    post_type: String,
}

#[derive(Debug, Default, serde::Serialize)]
struct UndoStats {
    reverted: usize,
    not_present: usize,
    failed: usize,
    posts_updated: usize,
    /// Snapshot restores refused because the post changed after SiloForge wrote it (R9).
    refused: usize,
}

pub async fn run(ctx: &Arc<JobContext>, job: &Job) -> JobResult<Value> {
    let site = load_site(ctx, job.site()?).await?;
    // R4: finish unfinished WordPress writes of this site first (we hold the site lock).
    super::inject::reconcile_for_job(ctx, job, &site).await?;
    let ids = job.payload_ids("edge_ids", "edgeIds", super::inject::MAX_EDGES);
    let snapshot_mode = job.payload_str("mode", "mode").as_deref() == Some("snapshot");
    let force = job.payload_bool("force", "force");
    let edges = load_applied(ctx, &site.id, &ids).await?;
    let mut stats = UndoStats::default();
    if edges.is_empty() {
        return done(stats);
    }
    if !site.has_wp_credentials() {
        let msg = "cannot undo without a WordPress application password (link stays applied)";
        let ids: Vec<String> = edges.iter().map(|e| e.id.clone()).collect();
        mark(ctx, &ids, "applied", Some(msg)).await?;
        stats.failed = ids.len();
        return done(stats);
    }
    let writer = WpWriter::new(site.http_client(ctx)?, site.wp_site(true)?)?;
    let mut groups: BTreeMap<String, Vec<EdgeRow>> = BTreeMap::new();
    for e in edges {
        groups.entry(e.source.clone()).or_default().push(e);
    }
    let total = groups.len().max(1);
    for (i, (source, group)) in groups.into_iter().enumerate() {
        job.checkpoint().await?;
        let r = if snapshot_mode {
            undo_snapshot(ctx, job, &writer, &site, &source, &group, force, &mut stats).await
        } else {
            undo_precise(ctx, job, &writer, &site, &source, &group, &mut stats).await
        };
        if let Err(e) = r {
            if e.is_cancelled() {
                return Err(e);
            }
            let ids: Vec<String> = group.iter().map(|g| g.id.clone()).collect();
            mark(ctx, &ids, "applied", Some(&format!("undo failed: {e}"))).await?;
            stats.failed += ids.len();
        }
        job.progress("undo", 100.0 * (i + 1) as f64 / total as f64)
            .await;
    }
    job.info(format!(
        "undo done: {} reverted ({} not present), {} failed",
        stats.reverted, stats.not_present, stats.failed
    ))
    .await;
    done(stats)
}

fn done(stats: UndoStats) -> JobResult<Value> {
    let mut v = serde_json::to_value(&stats).unwrap_or_else(|_| json!({}));
    if let Some(o) = v.as_object_mut() {
        o.insert("progress".into(), json!({"phase": "done", "pct": 100}));
    }
    Ok(v)
}

async fn load_applied(ctx: &JobContext, site_id: &str, ids: &[String]) -> JobResult<Vec<EdgeRow>> {
    let mut out = Vec::new();
    for chunk in ids.chunks(500) {
        let sid = site_id.to_string();
        let ids = chunk.to_vec();
        let rows: Vec<EdgeRow> = ctx
            .db
            .read(move |c| {
                let sql = format!(
                    "SELECT e.id, e.source_post_id, e.anchor, t.url, e.link_start, e.snapshot_id, s.wp_post_id, s.post_type
                     FROM link_edges e JOIN posts s ON s.id=e.source_post_id JOIN posts t ON t.id=e.target_post_id
                     WHERE e.site_id=?1 AND e.status='applied' AND e.origin <> 'existing' AND e.id IN ({})
                     ORDER BY e.source_post_id, e.id",
                    placeholders(2, ids.len())
                );
                let mut st = c.prepare(&sql).map_err(err)?;
                let mut p: Vec<&dyn rusqlite::ToSql> = vec![&sid];
                for id in &ids {
                    p.push(id);
                }
                let v = st
                    .query_map(p.as_slice(), |r| {
                        Ok(EdgeRow {
                            id: r.get(0)?,
                            source: r.get(1)?,
                            anchor: r.get(2)?,
                            target_url: r.get(3)?,
                            link_start: r.get(4)?,
                            snapshot_id: r.get(5)?,
                            wp_id: r.get(6)?,
                            post_type: r.get(7)?,
                        })
                    })
                    .map_err(err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(err)?;
                Ok(v)
            })
            .await?;
        out.extend(rows);
    }
    Ok(out)
}

/// Remove every edge's link from `raw`. Returns (new content, reverted ids, not-present ids, failed).
fn remove_all(
    raw: &str,
    group: &[EdgeRow],
) -> (String, Vec<String>, Vec<String>, Vec<(String, String)>) {
    let mut content = raw.to_string();
    let mut order: Vec<&EdgeRow> = group.iter().collect();
    // last link first, so earlier offsets stay valid
    order.sort_by_key(|e| std::cmp::Reverse(e.link_start.unwrap_or(-1)));
    let (mut ok, mut gone, mut failed) = (Vec::new(), Vec::new(), Vec::new());
    for e in order {
        let near = e.link_start.and_then(|s| usize::try_from(s).ok());
        match remove_link(&content, &e.target_url, &e.anchor, near) {
            Ok(new) => {
                content = new;
                ok.push(e.id.clone());
            }
            Err(InsertError::LinkNotFound) => gone.push(e.id.clone()),
            Err(other) => failed.push((e.id.clone(), other.to_string())),
        }
    }
    (content, ok, gone, failed)
}

#[allow(clippy::too_many_arguments)]
async fn undo_precise(
    ctx: &JobContext,
    job: &Job,
    writer: &WpWriter,
    site: &SiteInfo,
    source: &str,
    group: &[EdgeRow],
    stats: &mut UndoStats,
) -> JobResult<()> {
    let Some(first) = group.first() else {
        return Ok(());
    };
    let Some(wp_id) = first.wp_id.and_then(|i| u64::try_from(i).ok()) else {
        let ids: Vec<String> = group.iter().map(|g| g.id.clone()).collect();
        mark(ctx, &ids, "applied", Some("source post has no wp_post_id")).await?;
        stats.failed += ids.len();
        return Ok(());
    };
    let mut raw = writer.get_post_raw(wp_id, &first.post_type).await?;
    let (mut content, mut ok, mut gone, mut failed) = remove_all(&raw.raw, group);
    let mut written: Option<(sf_crawl::UpdateOutcome, Journal)> = None;
    if !ok.is_empty() {
        let mut jr = undo_journal(
            job, site, source, wp_id, first, &content, &raw.raw, group, &ok, &gone,
        );
        journal_begin(ctx, &jr).await?;
        let mut res =
            put_checked(writer, wp_id, &first.post_type, &content, &raw.modified_gmt).await;
        if matches!(res, Written::NotWritten(CrawlError::Conflict { .. })) {
            raw = writer.get_post_raw(wp_id, &first.post_type).await?;
            (content, ok, gone, failed) = remove_all(&raw.raw, group);
            res = if ok.is_empty() {
                Written::NotWritten(CrawlError::Other("nothing to remove after re-read".into()))
            } else {
                let j2 = undo_journal(
                    job, site, source, wp_id, first, &content, &raw.raw, group, &ok, &gone,
                );
                journal_abandon(ctx, &jr).await?;
                jr = j2;
                journal_begin(ctx, &jr).await?;
                put_checked(writer, wp_id, &first.post_type, &content, &raw.modified_gmt).await
            };
        }
        match res {
            Written::Done(o) => written = Some((o, jr)),
            Written::NotWritten(CrawlError::Other(_)) if ok.is_empty() => {
                journal_abandon(ctx, &jr).await?;
            }
            Written::NotWritten(e) => {
                journal_abandon(ctx, &jr).await?;
                let mut ids = ok.clone();
                ids.extend(failed.iter().map(|f| f.0.clone()));
                mark(ctx, &ids, "applied", Some(&format!("undo failed: {e}"))).await?;
                stats.failed += ids.len();
                if !gone.is_empty() {
                    mark(ctx, &gone, "reverted", Some("link not present")).await?;
                    stats.reverted += gone.len();
                    stats.not_present += gone.len();
                }
                return Ok(());
            }
            Written::Unknown(e) => {
                // journal kept: the next inject/undo of this site reconciles it
                let msg = format!(
                    "undo outcome unknown ({e}); reconciled against the live post on the next inject/undo"
                );
                job.warn(format!("post {wp_id}: {msg}")).await;
                let mut ids = ok.clone();
                ids.extend(failed.iter().map(|f| f.0.clone()));
                ids.extend(gone.iter().cloned());
                mark(ctx, &ids, "applied", Some(&msg)).await?;
                stats.failed += ids.len();
                return Ok(());
            }
        }
    }
    if let Some((o, jr)) = written {
        // one transaction: local copy, reverted edges, journal row gone (R4)
        let (src, c2, rendered, modified, jid) = (
            source.to_string(),
            o.raw.clone().unwrap_or_else(|| content.clone()),
            o.rendered.clone(),
            o.modified_gmt.clone(),
            jr.id.clone(),
        );
        let (ok2, gone2) = (ok.clone(), gone.clone());
        ctx.db
            .write(move |t| {
                save_post_tx(t, &src, &c2, &rendered, &modified)?;
                mark_tx(t, &ok2, "reverted", None)?;
                mark_tx(t, &gone2, "reverted", Some("link not present"))?;
                t.execute("DELETE FROM inject_journal WHERE id=?1", [&jid])
                    .map_err(err)?;
                Ok(())
            })
            .await?;
        stats.posts_updated += 1;
    } else {
        mark(ctx, &ok, "reverted", None).await?;
        mark(ctx, &gone, "reverted", Some("link not present")).await?;
    }
    for (id, e) in &failed {
        mark(
            ctx,
            std::slice::from_ref(id),
            "applied",
            Some(&format!("undo failed: {e}")),
        )
        .await?;
    }
    stats.reverted += ok.len() + gone.len();
    stats.not_present += gone.len();
    stats.failed += failed.len();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn undo_journal(
    job: &Job,
    site: &SiteInfo,
    source: &str,
    wp_id: u64,
    first: &EdgeRow,
    content: &str,
    previous: &str,
    group: &[EdgeRow],
    ok: &[String],
    gone: &[String],
) -> Journal {
    let mut jr = Journal::new(
        "undo",
        &site.id,
        source,
        Some(job),
        wp_id,
        &first.post_type,
        content,
        previous,
    );
    jr.edges = group
        .iter()
        .filter_map(|e| {
            let result = if ok.contains(&e.id) {
                "reverted"
            } else if gone.contains(&e.id) {
                "not_present"
            } else {
                return None;
            };
            Some(JournalEdge {
                id: e.id.clone(),
                url: e.target_url.clone(),
                text: e.anchor.clone(),
                link_start: e.link_start,
                module: None,
                result: Some(result.into()),
            })
        })
        .collect();
    jr
}

/// `mode=snapshot`: restore the earliest pre-inject snapshot of the post. R9: refused unless
/// `force` when the post was modified in WordPress after SiloForge's last write to it (those
/// edits would be silently lost); precise undo is the safe default.
#[allow(clippy::too_many_arguments)]
async fn undo_snapshot(
    ctx: &JobContext,
    job: &Job,
    writer: &WpWriter,
    site: &SiteInfo,
    source: &str,
    group: &[EdgeRow],
    force: bool,
    stats: &mut UndoStats,
) -> JobResult<()> {
    let Some(first) = group.first() else {
        return Ok(());
    };
    let snap_ids: Vec<String> = group.iter().filter_map(|g| g.snapshot_id.clone()).collect();
    let wp_id = first.wp_id.and_then(|i| u64::try_from(i).ok());
    let (Some(wp_id), false) = (wp_id, snap_ids.is_empty()) else {
        let ids: Vec<String> = group.iter().map(|g| g.id.clone()).collect();
        mark(
            ctx,
            &ids,
            "applied",
            Some("no snapshot / wp_post_id to restore from"),
        )
        .await?;
        stats.failed += ids.len();
        return Ok(());
    };
    let src0 = source.to_string();
    let (snap, written_gmt): (Option<(String, String)>, Option<String>) = ctx
        .db
        .read(move |c| {
            let sql = format!(
                "SELECT content_html, taken_at FROM snapshots WHERE id IN ({}) ORDER BY taken_at ASC LIMIT 1",
                placeholders(1, snap_ids.len())
            );
            let mut st = c.prepare(&sql).map_err(err)?;
            // R20: a real DB error propagates; only "no row" means the snapshot is gone
            let r = st
                .query_row(rusqlite::params_from_iter(snap_ids.iter()), |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()
                .map_err(err)?;
            let w: Option<String> = c
                .query_row(
                    "SELECT sf_written_gmt FROM posts WHERE id=?1",
                    [&src0],
                    |r| r.get(0),
                )
                .optional()
                .map_err(err)?
                .flatten();
            Ok((r, w))
        })
        .await?;
    let Some((content, taken_at)) = snap else {
        let ids: Vec<String> = group.iter().map(|g| g.id.clone()).collect();
        mark(ctx, &ids, "applied", Some("snapshot row is gone")).await?;
        stats.failed += ids.len();
        return Ok(());
    };
    let cur = writer.get_post_raw(wp_id, &first.post_type).await?;
    let changed = written_gmt.as_deref().map(str::trim) != Some(cur.modified_gmt.trim());
    if changed && !force {
        let msg = format!(
            "snapshot undo refused: the post was modified in WordPress after SiloForge's last write (live modified_gmt {}, SiloForge wrote {}); restoring the snapshot would discard those edits. Use precise undo, or pass force=true",
            cur.modified_gmt,
            written_gmt.as_deref().unwrap_or("unknown")
        );
        job.warn(format!("post {wp_id}: {msg}")).await;
        let ids: Vec<String> = group.iter().map(|g| g.id.clone()).collect();
        mark(ctx, &ids, "applied", Some(&msg)).await?;
        stats.failed += ids.len();
        stats.refused += ids.len();
        return Ok(());
    }
    // edges this restore reverts (every SiloForge edge of the post applied after the snapshot)
    let (src1, ta) = (source.to_string(), taken_at.clone());
    let reverted: Vec<(String, String, String, Option<i64>)> = ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare(
                    "SELECT e.id, t.url, e.anchor, e.link_start FROM link_edges e JOIN posts t ON t.id=e.target_post_id
                     WHERE e.source_post_id=?1 AND e.status='applied' AND e.origin <> 'existing' AND e.applied_at >= ?2",
                )
                .map_err(err)?;
            let v = st
                .query_map(params![src1, ta], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(v)
        })
        .await?;
    let mut jr = Journal::new(
        "undo",
        &site.id,
        source,
        Some(job),
        wp_id,
        &first.post_type,
        &content,
        &cur.raw,
    );
    jr.edges = reverted
        .iter()
        .map(|(id, url, text, ls)| JournalEdge {
            id: id.clone(),
            url: url.clone(),
            text: text.clone(),
            link_start: *ls,
            module: None,
            result: Some("reverted".into()),
        })
        .collect();
    journal_begin(ctx, &jr).await?;
    let o = match put_checked(writer, wp_id, &first.post_type, &content, &cur.modified_gmt).await {
        Written::Done(o) => o,
        Written::NotWritten(e) => {
            journal_abandon(ctx, &jr).await?;
            return Err(e.into());
        }
        Written::Unknown(e) => return Err(e.into()),
    };
    let (src, c2, rendered, modified, jid) = (
        source.to_string(),
        content.clone(),
        o.rendered.clone(),
        o.modified_gmt.clone(),
        jr.id.clone(),
    );
    let ids: Vec<String> = reverted.iter().map(|r| r.0.clone()).collect();
    let n = ids.len();
    ctx.db
        .write(move |t| {
            save_post_tx(t, &src, &c2, &rendered, &modified)?;
            let mut st = t
                .prepare(
                    "UPDATE link_edges SET status='reverted', error='reverted by snapshot restore'
                     WHERE id=?1 AND status='applied'",
                )
                .map_err(err)?;
            for id in &ids {
                st.execute([id]).map_err(err)?;
            }
            t.execute("DELETE FROM inject_journal WHERE id=?1", [&jid])
                .map_err(err)?;
            Ok(())
        })
        .await?;
    stats.posts_updated += 1;
    stats.reverted += n;
    Ok(())
}

fn mark_tx(
    t: &rusqlite::Transaction<'_>,
    ids: &[String],
    status: &str,
    note: Option<&str>,
) -> sf_core::Result<()> {
    let mut st = t
        .prepare("UPDATE link_edges SET status=?2, error=?3 WHERE id=?1 AND status='applied'")
        .map_err(err)?;
    for id in ids {
        st.execute(params![id, status, note]).map_err(err)?;
    }
    Ok(())
}

async fn mark(ctx: &JobContext, ids: &[String], status: &str, note: Option<&str>) -> JobResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let (ids, status, note) = (ids.to_vec(), status.to_string(), note.map(str::to_string));
    ctx.db
        .write(move |t| {
            let mut st = t
                .prepare(
                    "UPDATE link_edges SET status=?2, error=?3 WHERE id=?1 AND status='applied'",
                )
                .map_err(err)?;
            for id in &ids {
                st.execute(params![id, status, note]).map_err(err)?;
            }
            Ok(())
        })
        .await?;
    Ok(())
}
