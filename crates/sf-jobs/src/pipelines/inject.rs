//! `inject`: apply approved edges through the WordPress REST API, one write per post.
//!
//! Per source post: fresh `context=edit` raw → every edge spliced into the RAW content with the
//! block-aware hash-checked splicer (native first; LLM rephrase/context only with validated
//! output) → local sanity checks → snapshot of the fresh raw → ONE `update_post_content` with the
//! `modified_gmt` precondition (a conflict re-fetches once and recomputes) → verification
//! (rendered response, then the live page unless `skip_inject_verify`) → automatic rollback of
//! the post when the link did not make it into the page. Per-edge failures never abort the batch.
//!
//! Sites without an application password keep v1 plugin-pull semantics: edges stay `approved`
//! (`marked`), with `insert_module` set, for the WP plugin to pull.
//!
//! Stats (v1 `InjectStats` + extras): `{applied, marked, failed, verified, rolled_back,
//! verify_skipped, posts_updated, conflicts}`.

use std::collections::BTreeMap;
use std::sync::Arc;

use rusqlite::params;
use serde_json::{Value, json};
use sf_ai::ChatRequest;
use sf_core::types::InsertModule;
use sf_crawl::{CrawlError, HttpClient, WpWriter, verify_live};
use sf_engine::anchor::find_unblocked;
use sf_engine::insert::{
    InsertError, InsertRequest, LLM_MAX_TOKENS, LLM_TEMPERATURE, LlmKind, LlmLimits,
    append_to_paragraph, context_prompt, count_links, insert_link, locate_paragraph,
    rephrase_prompt, replace_paragraph, validate_llm_output,
};
use sf_store::err;

use super::chunk::SPLIT_MODE;
use super::common::{SiteInfo, content_hash, load_site, placeholders};
use crate::ctx::{Job, JobContext, JobResult};

/// Most edges accepted by one inject job.
pub const MAX_EDGES: usize = 50_000;

#[derive(Debug, Clone)]
pub struct EdgeRow {
    pub id: String,
    pub source: String,
    pub anchor: String,
    pub insert_module: Option<String>,
    pub para_idx: Option<i64>,
    pub para_hash: Option<String>,
    pub target_url: String,
    pub target_title: String,
    pub src_wp_id: Option<i64>,
    pub src_type: String,
    pub src_url: String,
    /// Fix-broken edge: the broken href whose link is re-pointed (`score_breakdown.replace_href`)
    /// instead of wrapping new text.
    pub replace_href: Option<String>,
    /// The target is a money page (`posts.meta.money_page`): AI work runs on the `money_page` tier.
    pub target_money: bool,
}

/// Inject behaviour switches (link runs).
#[derive(Debug, Clone)]
pub struct InjectOpts {
    /// Allow the AI rephrase/context modules when the native insertion fails.
    pub allow_llm: bool,
    /// Fall back to the AI context module for *any* edge whose anchor cannot be placed natively,
    /// even when the site's insert modules do not list rephrase/context (a link run with the
    /// Contextual strategy ticked; E2E 2.2: native items of a contextual run ended as
    /// "no AI insertion model" content gaps with an AI chain active).
    pub llm_fallback: bool,
    /// "Apply properly" pass: AI insertions are accounted as task `repair_insert`.
    pub repair: bool,
    /// Run/site marked high priority: AI insertions use the `priority` tier.
    pub high_priority: bool,
    /// Link Run id (or job id) for AI accounting.
    pub run_id: Option<String>,
}

impl Default for InjectOpts {
    fn default() -> Self {
        Self {
            allow_llm: true,
            llm_fallback: false,
            repair: false,
            high_priority: false,
            run_id: None,
        }
    }
}

#[derive(Debug, Clone)]
enum Outcome {
    Applied {
        link_start: usize,
        module: InsertModule,
        /// What an AI module changed, so undo can take it back byte for byte
        /// (`score_breakdown.llm_edit`): `{kind: context, inserted}` or
        /// `{kind: rephrase, before, after}`.
        llm: Option<Value>,
    },
    Failed {
        error: String,
        kind: &'static str,
    },
    Marked {
        module: InsertModule,
        /// Why the edge could not be written now (stored in `link_edges.error`).
        reason: String,
    },
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct InjectStats {
    pub applied: usize,
    pub marked: usize,
    pub failed: usize,
    pub verified: usize,
    pub rolled_back: usize,
    pub verify_skipped: usize,
    pub posts_updated: usize,
    pub conflicts: usize,
}

pub async fn run(ctx: &Arc<JobContext>, job: &Job) -> JobResult<Value> {
    let site = load_site(ctx, job.site()?).await?;
    // R4: finish any unfinished WordPress write of this site first (we hold the site lock).
    let reconciled = reconcile_for_job(ctx, job, &site).await?;
    if job.payload_bool("reconcile_only", "reconcileOnly") {
        return Ok(json!({
            "reconcile": reconciled,
            "progress": {"phase": "done", "pct": 100},
        }));
    }
    let ids = job.payload_ids("edge_ids", "edgeIds", MAX_EDGES);
    let opts = InjectOpts {
        high_priority: site.setting_bool("ai_high_priority"),
        run_id: Some(job.id.clone()),
        ..InjectOpts::default()
    };
    let stats = inject_edges(ctx, job, &site, &ids, opts).await?;
    finish(stats)
}

/// Inject these approved edges (ids of other statuses are ignored). The caller holds the site
/// lock and has reconciled the journal ([`reconcile_for_job`]).
pub async fn inject_edges(
    ctx: &JobContext,
    job: &Job,
    site: &SiteInfo,
    ids: &[String],
    opts: InjectOpts,
) -> JobResult<InjectStats> {
    let site = site.clone();
    let edges = load_approved(ctx, &site.id, ids).await?;
    let mut stats = InjectStats::default();
    job.info(format!(
        "inject: {} of {} requested edges are approved",
        edges.len(),
        ids.len()
    ))
    .await;
    if edges.is_empty() {
        return Ok(stats);
    }
    if !site.has_wp_credentials() {
        // Plugin-pull mode (v1 'marked'): keep approved, record how to insert.
        let ids: Vec<String> = edges.iter().map(|e| e.id.clone()).collect();
        let n = ctx
            .db
            .write(move |t| {
                let mut st = t
                    .prepare(
                        "UPDATE link_edges SET insert_module=coalesce(insert_module,'native'), failure_kind='pending_plugin'
                         WHERE id=?1 AND status='approved'",
                    )
                    .map_err(err)?;
                let mut n = 0;
                for id in &ids {
                    n += st.execute([id]).map_err(err)?;
                }
                Ok(n)
            })
            .await?;
        stats.marked = n;
        job.info(format!(
            "no WordPress application password: {n} edges left approved for the plugin to pull"
        ))
        .await;
        return Ok(stats);
    }
    let client = site.http_client(ctx)?;
    let writer = WpWriter::new(client.clone(), site.wp_site(true)?)?;
    let mut groups: BTreeMap<String, Vec<EdgeRow>> = BTreeMap::new();
    for e in edges {
        groups.entry(e.source.clone()).or_default().push(e);
    }
    let total = groups.len().max(1);
    for (i, (source, group)) in groups.into_iter().enumerate() {
        job.checkpoint().await?;
        let res = process_post(
            ctx, job, &site, &writer, &client, &source, &group, &mut stats, &opts,
        )
        .await;
        if let Err(e) = res {
            if e.is_cancelled() {
                return Err(e);
            }
            let msg = e.to_string();
            let outs: Vec<(String, Outcome)> = group
                .iter()
                .map(|g| {
                    (
                        g.id.clone(),
                        Outcome::Failed {
                            error: msg.clone(),
                            kind: "internal",
                        },
                    )
                })
                .collect();
            persist(ctx, &outs, None, &mut stats).await?;
        }
        job.progress("inject", 100.0 * (i + 1) as f64 / total as f64)
            .await;
    }
    job.info(format!(
        "inject done: {} applied, {} failed, {} marked, {} verified, {} rolled back",
        stats.applied, stats.failed, stats.marked, stats.verified, stats.rolled_back
    ))
    .await;
    if stats.marked > 0 {
        job.warn(format!(
            "{} edges were NOT written: their anchor text is not in the post (content_gap) and no AI rephrase/context model is available; they stay approved with the reason in `error` (edit the anchor, configure a model, or reject them)",
            stats.marked
        ))
        .await;
    }
    Ok(stats)
}

/// Reason stored on an edge that could not be inserted natively (v1 kept these silently approved).
fn content_gap_reason(e: &EdgeRow) -> String {
    if e.insert_module.as_deref() == Some("native") {
        format!(
            "anchor \"{}\" is no longer in the post (edited since planning?); needs a new anchor or an AI insertion model",
            e.anchor.chars().take(120).collect::<String>()
        )
    } else {
        "content_gap: the anchor text is not in the paragraph; an AI rephrase/context model (or a manual edit) is needed".into()
    }
}

fn finish(stats: InjectStats) -> JobResult<Value> {
    let mut v = serde_json::to_value(&stats).unwrap_or_else(|_| json!({}));
    if let Some(o) = v.as_object_mut() {
        o.insert("progress".into(), json!({"phase": "done", "pct": 100}));
    }
    Ok(v)
}

async fn load_approved(ctx: &JobContext, site_id: &str, ids: &[String]) -> JobResult<Vec<EdgeRow>> {
    let mut out = Vec::new();
    for chunk in ids.chunks(500) {
        let sid = site_id.to_string();
        let ids = chunk.to_vec();
        let rows: Vec<EdgeRow> = ctx
            .db
            .read(move |c| {
                let sql = format!(
                    "SELECT e.id, e.source_post_id, e.anchor, e.insert_module, pa.idx, pa.hash,
                        t.url, t.title, s.wp_post_id, s.post_type, s.url,
                        json_extract(CASE WHEN json_valid(e.score_breakdown) THEN e.score_breakdown ELSE '{{}}' END, '$.replace_href'),
                        CASE WHEN json_valid(t.meta) AND json_extract(t.meta, '$.money_page') IN (1, 'true') THEN 1 ELSE 0 END
                     FROM link_edges e
                     JOIN posts s ON s.id = e.source_post_id
                     JOIN posts t ON t.id = e.target_post_id
                     LEFT JOIN paragraphs pa ON pa.id = e.paragraph_id
                     WHERE e.site_id=?1 AND e.status='approved' AND e.id IN ({})
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
                            insert_module: r.get(3)?,
                            para_idx: r.get(4)?,
                            para_hash: r.get(5)?,
                            target_url: r.get(6)?,
                            target_title: r.get(7)?,
                            src_wp_id: r.get(8)?,
                            src_type: r.get(9)?,
                            src_url: r.get(10)?,
                            replace_href: r.get::<_, Option<String>>(11).ok().flatten(),
                            target_money: r.get::<_, i64>(12).unwrap_or(0) != 0,
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
    out.sort_by(|a, b| a.source.cmp(&b.source).then(a.id.cmp(&b.id)));
    out.dedup_by(|a, b| a.id == b.id);
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
async fn process_post(
    ctx: &JobContext,
    job: &Job,
    site: &SiteInfo,
    writer: &WpWriter,
    client: &HttpClient,
    source: &str,
    group: &[EdgeRow],
    stats: &mut InjectStats,
    opts: &InjectOpts,
) -> JobResult<()> {
    let fail_all = |error: String, kind: &'static str| -> Vec<(String, Outcome)> {
        group
            .iter()
            .map(|g| {
                (
                    g.id.clone(),
                    Outcome::Failed {
                        error: error.clone(),
                        kind,
                    },
                )
            })
            .collect()
    };
    let Some(first) = group.first() else {
        return Ok(());
    };
    let Some(wp_id) = first.src_wp_id.and_then(|i| u64::try_from(i).ok()) else {
        let outs = fail_all("source post has no wp_post_id".into(), "no_wp_id");
        return persist(ctx, &outs, None, stats).await;
    };
    let post_type = first.src_type.clone();
    let raw = match writer.get_post_raw(wp_id, &post_type).await {
        Ok(r) => r,
        Err(e) => {
            let outs = fail_all(
                format!("could not read the post from WordPress: {e}"),
                "fetch",
            );
            return persist(ctx, &outs, None, stats).await;
        }
    };
    let (mut new_content, mut outcomes) = apply_all(ctx, site, &raw.raw, group, opts).await;
    let mut base = raw.clone();
    if !outcomes
        .iter()
        .any(|(_, o)| matches!(o, Outcome::Applied { .. }))
    {
        return persist(ctx, &outcomes, None, stats).await;
    }
    if let Some(reason) = local_damage(&raw.raw, &new_content) {
        let outs = fail_all(format!("refused: {reason}"), "local_verify");
        return persist(ctx, &outs, None, stats).await;
    }
    // R4: snapshot + write-ahead journal in one transaction before the PUT (R13: the snapshot is
    // dropped again whenever the write certainly did not happen).
    let snapshot_id = sf_core::new_id();
    let mut texts = applied_texts(group, &outcomes, &new_content);
    let mut jr = Journal::new(
        "inject",
        &site.id,
        source,
        Some(job),
        wp_id,
        &post_type,
        &new_content,
        &raw.raw,
    );
    jr.snapshot_id = Some(snapshot_id.clone());
    jr.edges = journal_edges(group, &outcomes, &texts);
    begin_inject(ctx, &jr, source, &raw.raw).await?;
    let mut result = put_checked(writer, wp_id, &post_type, &new_content, &raw.modified_gmt).await;
    if matches!(result, Written::NotWritten(CrawlError::Conflict { .. })) {
        stats.conflicts += 1;
        job.warn(format!(
            "post {wp_id} changed in WordPress while injecting; re-reading once"
        ))
        .await;
        match writer.get_post_raw(wp_id, &post_type).await {
            Ok(fresh) => {
                let (c2, o2) = apply_all(ctx, site, &fresh.raw, group, opts).await;
                new_content = c2;
                outcomes = o2;
                if !outcomes
                    .iter()
                    .any(|(_, o)| matches!(o, Outcome::Applied { .. }))
                {
                    journal_abandon(ctx, &jr).await?;
                    return persist(ctx, &outcomes, None, stats).await;
                }
                if let Some(reason) = local_damage(&fresh.raw, &new_content) {
                    journal_abandon(ctx, &jr).await?;
                    let outs = fail_all(format!("refused: {reason}"), "local_verify");
                    return persist(ctx, &outs, None, stats).await;
                }
                texts = applied_texts(group, &outcomes, &new_content);
                jr.expected = new_content.clone();
                jr.previous = fresh.raw.clone();
                jr.edges = journal_edges(group, &outcomes, &texts);
                restart_inject(ctx, &jr, &snapshot_id, &fresh.raw).await?;
                result =
                    put_checked(writer, wp_id, &post_type, &new_content, &fresh.modified_gmt).await;
                base = fresh;
            }
            Err(e) => result = Written::NotWritten(e),
        }
    }
    let outcome = match result {
        Written::Done(o) => o,
        Written::NotWritten(CrawlError::Conflict { .. }) => {
            journal_abandon(ctx, &jr).await?;
            let outs = fail_all(
                "post was modified in WordPress during injection (conflict); nothing was written"
                    .into(),
                "conflict",
            );
            return persist(ctx, &outs, None, stats).await;
        }
        Written::NotWritten(e) => {
            journal_abandon(ctx, &jr).await?;
            let outs = fail_all(format!("WordPress update failed: {e}"), "wp_update");
            return persist(ctx, &outs, None, stats).await;
        }
        Written::Unknown(e) => {
            // The PUT may or may not have landed and the post can't be re-read: keep the journal
            // row (the next inject/undo of this site reconciles it) and leave the edges approved.
            let msg = format!(
                "WordPress update outcome unknown ({e}); it is reconciled against the live post on the next inject/undo"
            );
            job.warn(format!("post {wp_id}: {msg}")).await;
            let ids: Vec<String> = group.iter().map(|g| g.id.clone()).collect();
            ctx.db
                .write(move |t| {
                    for id in &ids {
                        t.execute(
                            "UPDATE link_edges SET error=?2, failure_kind='wp_unknown' WHERE id=?1 AND status='approved'",
                            params![id, msg],
                        )
                        .map_err(err)?;
                    }
                    Ok(())
                })
                .await?;
            stats.failed += group.len();
            return Ok(());
        }
    };
    // R10: WordPress reports what it stored; a filter or a save that slipped in between the
    // precondition GET and the POST shows up as a difference.
    let stored = match outcome.raw.as_deref() {
        Some(r) if r != new_content => {
            job.warn(format!(
                "post {wp_id}: WordPress stored different content than SiloForge sent (content filter or a concurrent edit); keeping what WordPress stored"
            ))
            .await;
            r.to_string()
        }
        _ => new_content.clone(),
    };
    commit_inject(
        ctx,
        &jr,
        source,
        &stored,
        &outcome.rendered,
        &outcome.modified_gmt,
        &outcomes,
        stats,
    )
    .await?;
    stats.posts_updated += 1;
    record_link_texts(ctx, group, &texts).await?;
    let anchor_of = |e: &EdgeRow| {
        texts
            .get(&e.id)
            .cloned()
            .unwrap_or_else(|| e.anchor.clone())
    };

    // ---- verification -------------------------------------------------------------------
    let applied: Vec<&EdgeRow> = group
        .iter()
        .filter(|g| {
            outcomes
                .iter()
                .any(|(id, o)| id == &g.id && matches!(o, Outcome::Applied { .. }))
        })
        .collect();
    let mut damage: Option<String> = None;
    if !outcome.rendered.trim().is_empty() {
        for e in &applied {
            if count_links(&outcome.rendered, &e.target_url, &anchor_of(e)) == 0 {
                damage = Some(format!(
                    "link to {} missing from the rendered content returned by WordPress",
                    e.target_url
                ));
                break;
            }
        }
    }
    let mut inconclusive: Vec<(String, String)> = Vec::new();
    let mut verified = 0usize;
    if damage.is_none() && !site.setting_bool("skip_inject_verify") {
        let page = [
            outcome.link.as_str(),
            base.link.as_str(),
            first.src_url.as_str(),
        ]
        .into_iter()
        .find(|u| !u.trim().is_empty())
        .unwrap_or("")
        .to_string();
        for e in &applied {
            match verify_live(client, &page, &e.target_url, &anchor_of(e), &ctx.verify).await {
                Ok(v) if v.found() => verified += 1,
                Ok(v) if v.status == Some(200) && !v.stale_cache => {
                    damage = Some(format!(
                        "link to {} not found on the live page",
                        e.target_url
                    ));
                    break;
                }
                Ok(v) => inconclusive.push((
                    e.id.clone(),
                    format!(
                        "verify_skipped{}: http {:?}{}",
                        if v.stale_cache { " (stale)" } else { "" },
                        v.status,
                        v.error.map(|x| format!(", {x}")).unwrap_or_default()
                    ),
                )),
                Err(err) => inconclusive.push((e.id.clone(), format!("verify_skipped: {err}"))),
            }
        }
    }
    if let Some(reason) = damage {
        let ids: Vec<String> = applied.iter().map(|e| e.id.clone()).collect();
        let mut jr = Journal::new(
            "rollback",
            &site.id,
            source,
            Some(job),
            wp_id,
            &post_type,
            &base.raw,
            &stored,
        );
        jr.edges = applied
            .iter()
            .map(|e| JournalEdge {
                id: e.id.clone(),
                url: e.target_url.clone(),
                text: anchor_of(e),
                link_start: None,
                module: None,
                result: None,
            })
            .collect();
        journal_begin(ctx, &jr).await?;
        let restored =
            put_checked(writer, wp_id, &post_type, &base.raw, &outcome.modified_gmt).await;
        let n = ids.len();
        let (msg, rolled) = match &restored {
            Written::Done(_) => (format!("verify failed (rolled back): {reason}"), true),
            Written::NotWritten(e) => (
                format!("verify failed; rollback failed ({e}): {reason}"),
                false,
            ),
            Written::Unknown(e) => (
                format!(
                    "verify failed; rollback outcome unknown ({e}), reconciled on the next inject/undo: {reason}"
                ),
                false,
            ),
        };
        let (m2, jid) = (msg.clone(), jr.id.clone());
        let keep_journal = matches!(restored, Written::Unknown(_));
        let back = match restored {
            Written::Done(o) => Some(o),
            _ => None,
        };
        let (src, raw_back) = (source.to_string(), base.raw.clone());
        ctx.db
            .write(move |t| {
                if let Some(o) = &back {
                    save_post_tx(t, &src, &raw_back, &o.rendered, &o.modified_gmt)?;
                }
                let mut st = t
                    .prepare(
                        "UPDATE link_edges SET status=CASE WHEN ?3 THEN 'failed' ELSE status END,
                           applied_at=CASE WHEN ?3 THEN NULL ELSE applied_at END, error=?2, failure_kind='verify_failed'
                         WHERE id=?1",
                    )
                    .map_err(err)?;
                for id in &ids {
                    st.execute(params![id, m2, rolled]).map_err(err)?;
                }
                if !keep_journal {
                    t.execute("DELETE FROM inject_journal WHERE id=?1", [&jid])
                        .map_err(err)?;
                }
                Ok(())
            })
            .await?;
        if rolled {
            stats.applied = stats.applied.saturating_sub(n);
            stats.failed += n;
            stats.rolled_back += n;
        }
        job.warn(format!("post {wp_id}: {msg}")).await;
        return Ok(());
    }
    stats.verified += verified;
    stats.verify_skipped += inconclusive.len();
    if !inconclusive.is_empty() {
        ctx.db
            .write(move |t| {
                for (id, msg) in &inconclusive {
                    t.execute(
                        "UPDATE link_edges SET error=?2 WHERE id=?1",
                        params![id, msg.chars().take(1000).collect::<String>()],
                    )
                    .map_err(err)?;
                }
                Ok(())
            })
            .await?;
    }
    Ok(())
}

/// Refuse writes that would damage the post: block comments lost/added, or content emptied.
fn local_damage(before: &str, after: &str) -> Option<String> {
    let blocks = |s: &str| s.matches("<!-- wp:").count() + s.matches("<!-- /wp:").count();
    if blocks(before) != blocks(after) {
        return Some("Gutenberg block comments changed".into());
    }
    if after.trim().is_empty() && !before.trim().is_empty() {
        return Some("content would be emptied".into());
    }
    None
}

/// Apply every edge of one post to `content`: native insertions first (they never change
/// paragraph hashes), then LLM modules. Returns the new content and one outcome per edge.
async fn apply_all(
    ctx: &JobContext,
    site: &SiteInfo,
    content: &str,
    group: &[EdgeRow],
    opts: &InjectOpts,
) -> (String, Vec<(String, Outcome)>) {
    let mut cur = content.to_string();
    let mut outs: Vec<(String, Outcome)> = Vec::new();
    let mut starts: Vec<(usize, usize)> = Vec::new(); // (outs index, link_start)
    let allowed = site.insert_modules();
    let mut llm_queue: Vec<&EdgeRow> = Vec::new();
    for e in group {
        if let Some(old) = e.replace_href.as_deref() {
            // fix-broken edge: re-point the broken link, keep its text
            match sf_engine::insert::replace_link(&cur, old, &e.target_url, Some(&e.anchor), None) {
                Ok(o) => {
                    let delta = o.content.len().abs_diff(cur.len());
                    let grew = o.content.len() >= cur.len();
                    if grew {
                        shift(&mut starts, &mut outs, o.link_start + 1, delta);
                    } else {
                        unshift(&mut starts, &mut outs, o.link_start + 1, delta);
                    }
                    cur = o.content;
                    starts.push((outs.len(), o.link_start));
                    outs.push((
                        e.id.clone(),
                        Outcome::Applied {
                            link_start: o.link_start,
                            module: InsertModule::Native,
                            llm: None,
                        },
                    ));
                }
                Err(err) => outs.push((
                    e.id.clone(),
                    Outcome::Failed {
                        error: format!("broken link to {old} not found in the post: {err}"),
                        kind: "replace",
                    },
                )),
            }
            continue;
        }
        match try_native(&cur, e) {
            Ok(o) => {
                let delta = o.content.len().saturating_sub(cur.len());
                shift(&mut starts, &mut outs, o.link_start, delta);
                cur = o.content;
                starts.push((outs.len(), o.link_start));
                outs.push((
                    e.id.clone(),
                    Outcome::Applied {
                        link_start: o.link_start,
                        module: InsertModule::Native,
                        llm: None,
                    },
                ));
            }
            Err(InsertError::AnchorNotFound) => {
                let wants_llm = opts.llm_fallback
                    || matches!(
                        e.insert_module.as_deref(),
                        Some("rephrase") | Some("context")
                    )
                    || allowed
                        .iter()
                        .any(|m| matches!(m, InsertModule::Rephrase | InsertModule::Context));
                if wants_llm && opts.allow_llm && ctx.ai.is_some() {
                    llm_queue.push(e);
                } else {
                    outs.push((
                        e.id.clone(),
                        Outcome::Marked {
                            module: InsertModule::ContentGap,
                            reason: content_gap_reason(e),
                        },
                    ));
                }
            }
            Err(InsertError::AlreadyLinked) => outs.push((
                e.id.clone(),
                Outcome::Failed {
                    error: "the paragraph already links to the target".into(),
                    kind: "already_linked",
                },
            )),
            Err(other) => outs.push((
                e.id.clone(),
                Outcome::Failed {
                    error: other.to_string(),
                    kind: "insert",
                },
            )),
        }
    }
    for e in llm_queue {
        let kind = match e.insert_module.as_deref() {
            Some("context") => LlmKind::Context,
            Some("rephrase") => LlmKind::Rephrase,
            _ if allowed.contains(&InsertModule::Rephrase) => LlmKind::Rephrase,
            _ => LlmKind::Context,
        };
        match llm_insert(ctx, site, &cur, e, kind, opts).await {
            Ok((new, start, edit)) => {
                let delta = new.len().saturating_sub(cur.len());
                shift(&mut starts, &mut outs, start, delta);
                cur = new;
                starts.push((outs.len(), start));
                outs.push((
                    e.id.clone(),
                    Outcome::Applied {
                        link_start: start,
                        module: match kind {
                            LlmKind::Rephrase => InsertModule::Rephrase,
                            LlmKind::Context => InsertModule::Context,
                        },
                        llm: Some(edit),
                    },
                ));
            }
            Err((error, kind)) => outs.push((e.id.clone(), Outcome::Failed { error, kind })),
        }
    }
    (cur, outs)
}

/// Move recorded link starts at or after `pos` back by `delta` bytes (a change got shorter).
fn unshift(
    starts: &mut [(usize, usize)],
    outs: &mut [(String, Outcome)],
    pos: usize,
    delta: usize,
) {
    for (oi, s) in starts.iter_mut() {
        if *s >= pos {
            *s = s.saturating_sub(delta);
            if let Some((_, Outcome::Applied { link_start, .. })) = outs.get_mut(*oi) {
                *link_start = *s;
            }
        }
    }
}

/// Shift recorded link starts that lie after a change at `pos` by `delta` bytes.
fn shift(starts: &mut [(usize, usize)], outs: &mut [(String, Outcome)], pos: usize, delta: usize) {
    for (oi, s) in starts.iter_mut() {
        if *s >= pos {
            *s += delta;
            if let Some((_, Outcome::Applied { link_start, .. })) = outs.get_mut(*oi) {
                *link_start = *s;
            }
        }
    }
}

/// Paragraph (idx, hash) for an edge: the planned one, else the first paragraph of `content`
/// containing the anchor outside links/headings/code.
fn locate(content: &str, e: &EdgeRow) -> Option<(usize, String)> {
    if let (Some(i), Some(h)) = (e.para_idx, e.para_hash.as_ref())
        && let Ok(i) = usize::try_from(i)
    {
        return Some((i, h.clone()));
    }
    search(content, &e.anchor)
}

fn search(content: &str, anchor: &str) -> Option<(usize, String)> {
    sf_engine::html::paragraphs(content, SPLIT_MODE)
        .ok()?
        .into_iter()
        .find(|p| p.link_count == 0 && find_unblocked(&p.text, anchor, &p.blocked).is_some())
        .or_else(|| {
            sf_engine::html::paragraphs(content, SPLIT_MODE)
                .ok()?
                .into_iter()
                .find(|p| find_unblocked(&p.text, anchor, &p.blocked).is_some())
        })
        .map(|p| (p.index, p.hash))
}

fn try_native(content: &str, e: &EdgeRow) -> Result<sf_engine::insert::InsertOutcome, InsertError> {
    let Some((idx, hash)) = locate(content, e) else {
        return Err(InsertError::AnchorNotFound);
    };
    let mut req = InsertRequest {
        paragraph_index: idx,
        expected_hash: hash,
        anchor: e.anchor.clone(),
        target_url: e.target_url.clone(),
        mode: SPLIT_MODE,
    };
    match insert_link(content, &req) {
        Err(InsertError::ContentChanged {
            found_at: Some(i), ..
        }) => {
            req.paragraph_index = i;
            insert_link(content, &req)
        }
        Err(InsertError::ContentChanged { found_at: None, .. })
        | Err(InsertError::ParagraphNotFound { .. })
        | Err(InsertError::AnchorNotFound)
            if e.para_idx.is_some() =>
        {
            // planned paragraph is gone or lacks the anchor now: look for it elsewhere once
            match search(content, &e.anchor) {
                Some((i, h)) if i != req.paragraph_index || h != req.expected_hash => {
                    req.paragraph_index = i;
                    req.expected_hash = h;
                    insert_link(content, &req)
                }
                _ => Err(InsertError::AnchorNotFound),
            }
        }
        r => r,
    }
}

async fn llm_insert(
    ctx: &JobContext,
    site: &SiteInfo,
    content: &str,
    e: &EdgeRow,
    kind: LlmKind,
    opts: &InjectOpts,
) -> Result<(String, usize, Value), (String, &'static str)> {
    let Some(ai) = ctx.ai.as_ref() else {
        return Err(("no AI model configured".into(), "llm_unavailable"));
    };
    let (idx, hash) = match (e.para_idx, e.para_hash.as_ref()) {
        (Some(i), Some(h)) => (usize::try_from(i).unwrap_or(0), h.clone()),
        _ => {
            return Err((
                "edge has no paragraph for an LLM insertion".into(),
                "content_gap",
            ));
        }
    };
    let para = locate_paragraph(content, idx, &hash, SPLIT_MODE)
        .map_err(|e| (format!("paragraph changed: {e}"), "content_changed"))?;
    let msgs = match kind {
        LlmKind::Rephrase => rephrase_prompt(&para.html, &e.anchor, &e.target_url, &e.target_title),
        LlmKind::Context => context_prompt(&para.text, &e.anchor, &e.target_url, &e.target_title),
    };
    // v2.3 task routing: repair pass / rephrase / context sentence; money-page targets and
    // high-priority runs override the tier.
    let task = if opts.repair {
        sf_core::config::ai_tasks::REPAIR_INSERT
    } else {
        match kind {
            LlmKind::Rephrase => sf_core::config::ai_tasks::REPHRASE,
            LlmKind::Context => sf_core::config::ai_tasks::CONTEXT_SENTENCE,
        }
    };
    let tier = if e.target_money {
        ai.tier_for_task(sf_core::config::ai_tasks::MONEY_PAGE)
    } else if opts.high_priority {
        ai.tier_for_task(sf_core::config::ai_tasks::PRIORITY)
    } else {
        ai.tier_for_task(task)
    };
    let req = ChatRequest {
        messages: msgs
            .into_iter()
            .map(|m| sf_ai::ChatMessage {
                role: m.role,
                content: m.content,
            })
            .collect(),
        max_tokens: llm_max_tokens(kind, &para.text),
        temperature: LLM_TEMPERATURE,
        site_id: Some(site.id.clone()),
        json_mode: false,
        task: Some(task.into()),
        tier: Some(tier),
        run_id: opts.run_id.clone(),
        ..Default::default()
    };
    let reply = ai.chat(req).await.map_err(|err| match err {
        sf_ai::AiError::BudgetExhausted(m) => {
            (format!("AI budget exhausted: {m}"), "llm_unavailable")
        }
        other => (format!("AI call failed: {other}"), "llm_error"),
    })?;
    let clean = validate_llm_output(
        kind,
        &para.html,
        &reply.text,
        &e.anchor,
        &e.target_url,
        &LlmLimits::default(),
    )
    .map_err(|r| (format!("AI output rejected: {r}"), "llm_rejected"))?;
    let new = match kind {
        LlmKind::Rephrase => replace_paragraph(content, idx, &hash, &clean, SPLIT_MODE),
        LlmKind::Context => append_to_paragraph(content, idx, &hash, &clean, SPLIT_MODE),
    }
    .map_err(|e| (e.to_string(), "insert"))?;
    let start = new
        .get(para.start..)
        .and_then(|rest| {
            let href = format!(
                "href=\"{}\"",
                sf_engine::html::escape_attr(e.target_url.trim())
            );
            let h = rest.find(&href)?;
            rest.get(..h)?.rfind("<a").map(|a| a + para.start)
        })
        .unwrap_or(para.start);
    // exactly what changed (append_to_paragraph inserts " " + sentence)
    let edit = match kind {
        LlmKind::Context => json!({"kind": "context", "inserted": format!(" {clean}")}),
        LlmKind::Rephrase => json!({"kind": "rephrase", "before": para.html, "after": clean}),
    };
    Ok((new, start, edit))
}

/// Output budget of an AI insertion (v2.3 tight max_tokens): a context sentence gets 220 tokens;
/// a rephrase returns the whole paragraph, so it gets ~1 token per 3 chars of it + 60 (at least
/// 220, at most the engine cap).
fn llm_max_tokens(kind: LlmKind, para_text: &str) -> u32 {
    const SENTENCE: u32 = 220;
    match kind {
        LlmKind::Context => SENTENCE,
        LlmKind::Rephrase => {
            let chars = u32::try_from(para_text.chars().count()).unwrap_or(u32::MAX);
            (chars / 3)
                .saturating_add(60)
                .clamp(SENTENCE, LLM_MAX_TOKENS)
        }
    }
}

/// Snapshot of the pre-inject raw + the journal row, in one transaction (R4/R13).
async fn begin_inject(ctx: &JobContext, jr: &Journal, post_id: &str, raw: &str) -> JobResult<()> {
    let (jr, pid, raw) = (jr.clone(), post_id.to_string(), raw.to_string());
    ctx.db
        .write(move |t| {
            if let Some(snap) = &jr.snapshot_id {
                t.execute(
                    "INSERT INTO snapshots(id, post_id, content_html, reason) VALUES(?1,?2,?3,'pre-inject')",
                    params![snap, pid, raw],
                )
                .map_err(err)?;
            }
            journal_insert(t, &jr)
        })
        .await?;
    Ok(())
}

/// After a conflict re-read: refresh the snapshot and the journal row for the second attempt.
async fn restart_inject(
    ctx: &JobContext,
    jr: &Journal,
    snapshot_id: &str,
    raw: &str,
) -> JobResult<()> {
    let (jr, snap, raw) = (jr.clone(), snapshot_id.to_string(), raw.to_string());
    ctx.db
        .write(move |t| {
            t.execute(
                "UPDATE snapshots SET content_html=?2, taken_at=?3 WHERE id=?1",
                params![snap, raw, sf_core::now_rfc3339()],
            )
            .map_err(err)?;
            t.execute(
                "UPDATE inject_journal SET expected=?2, previous=?3, edges=?4 WHERE id=?1",
                params![
                    jr.id,
                    jr.expected,
                    jr.previous,
                    serde_json::to_string(&jr.edges).unwrap_or_else(|_| "[]".into())
                ],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// Result of a content PUT as far as SiloForge can tell.
#[derive(Debug)]
pub enum Written {
    Done(sf_crawl::UpdateOutcome),
    /// WordPress certainly holds the old content (conflict, or confirmed by a re-read).
    NotWritten(CrawlError),
    /// The PUT failed at transport level and the post could not be re-read.
    Unknown(CrawlError),
}

/// PUT with the `modified_gmt` precondition. A failed PUT (other than a precondition conflict,
/// which is raised before anything is sent) is never trusted: the post is re-read, and the write
/// counts as done when WordPress now holds exactly `content` (R4: a timeout after WordPress
/// committed used to mark live links as failed).
pub async fn put_checked(
    writer: &WpWriter,
    wp_id: u64,
    post_type: &str,
    content: &str,
    modified_gmt: &str,
) -> Written {
    match writer
        .update_post_content(wp_id, post_type, content, modified_gmt)
        .await
    {
        Ok(o) => Written::Done(o),
        Err(e @ CrawlError::Conflict { .. }) => Written::NotWritten(e),
        Err(e) => match writer.get_post_raw(wp_id, post_type).await {
            Ok(live) if live.raw == content => Written::Done(sf_crawl::UpdateOutcome {
                id: wp_id,
                modified_gmt: live.modified_gmt,
                rendered: live.rendered,
                link: live.link,
                raw: Some(live.raw),
            }),
            Ok(_) => Written::NotWritten(e),
            Err(_) => Written::Unknown(e),
        },
    }
}

/// The text actually linked per applied edge (LLM modules may link a longer phrase).
fn applied_texts(
    group: &[EdgeRow],
    outcomes: &[(String, Outcome)],
    content: &str,
) -> std::collections::HashMap<String, String> {
    outcomes
        .iter()
        .filter_map(|(id, o)| match o {
            Outcome::Applied { link_start, .. } => {
                let e = group.iter().find(|g| &g.id == id)?;
                Some((
                    id.clone(),
                    link_text_at(content, *link_start, &e.target_url)
                        .unwrap_or_else(|| e.anchor.clone()),
                ))
            }
            _ => None,
        })
        .collect()
}

fn journal_edges(
    group: &[EdgeRow],
    outcomes: &[(String, Outcome)],
    texts: &std::collections::HashMap<String, String>,
) -> Vec<JournalEdge> {
    outcomes
        .iter()
        .filter_map(|(id, o)| match o {
            Outcome::Applied {
                link_start, module, ..
            } => {
                let e = group.iter().find(|g| &g.id == id)?;
                Some(JournalEdge {
                    id: id.clone(),
                    url: e.target_url.clone(),
                    text: texts.get(id).cloned().unwrap_or_else(|| e.anchor.clone()),
                    link_start: i64::try_from(*link_start).ok(),
                    module: Some(module.as_str().to_string()),
                    result: None,
                })
            }
            _ => None,
        })
        .collect()
}

/// Bookkeeping of a confirmed inject write in ONE transaction: local post copy, every edge
/// outcome, and the journal row deleted (R4).
#[allow(clippy::too_many_arguments)]
async fn commit_inject(
    ctx: &JobContext,
    jr: &Journal,
    source: &str,
    raw: &str,
    rendered: &str,
    modified: &str,
    outs: &[(String, Outcome)],
    stats: &mut InjectStats,
) -> JobResult<()> {
    count_outcomes(outs, true, stats);
    let (outs, snap, jid) = (outs.to_vec(), jr.snapshot_id.clone(), jr.id.clone());
    let (src, raw, rendered, modified) = (
        source.to_string(),
        raw.to_string(),
        rendered.to_string(),
        modified.to_string(),
    );
    ctx.db
        .write(move |t| {
            save_post_tx(t, &src, &raw, &rendered, &modified)?;
            persist_tx(t, &outs, snap.as_deref())?;
            t.execute("DELETE FROM inject_journal WHERE id=?1", [&jid])
                .map_err(err)?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// Visible text of the `<a>` that starts at `start` in `content`, if it links to `url`.
fn link_text_at(content: &str, start: usize, url: &str) -> Option<String> {
    let rest = content.get(start..)?;
    if !rest.starts_with("<a") {
        return None;
    }
    let open_end = rest.find('>')?;
    let open = rest.get(..open_end)?;
    let href = format!("href=\"{}\"", sf_engine::html::escape_attr(url.trim()));
    if !open.contains(&href) {
        return None;
    }
    let close = rest.find("</a>")?;
    let inner = rest.get(open_end + 1..close)?;
    let text = sf_engine::html::html_to_text(inner);
    (!text.trim().is_empty()).then_some(text)
}

/// Store the actually linked text as the edge anchor when it differs (needed for precise undo).
async fn record_link_texts(
    ctx: &JobContext,
    group: &[EdgeRow],
    texts: &std::collections::HashMap<String, String>,
) -> JobResult<()> {
    let changes: Vec<(String, String)> = group
        .iter()
        .filter_map(|e| {
            let t = texts.get(&e.id)?;
            (t.to_lowercase() != e.anchor.to_lowercase()).then(|| (e.id.clone(), t.clone()))
        })
        .collect();
    if changes.is_empty() {
        return Ok(());
    }
    ctx.db
        .write(move |t| {
            for (id, text) in &changes {
                match t.execute(
                    "UPDATE link_edges SET anchor=?2 WHERE id=?1",
                    params![id, super::common::truncate_utf16(text, 255)],
                ) {
                    Ok(_) => {}
                    // same (source, target, anchor) already exists: keep the planned anchor
                    Err(rusqlite::Error::SqliteFailure(f, _))
                        if f.code == rusqlite::ErrorCode::ConstraintViolation => {}
                    Err(e) => return Err(err(e)),
                }
            }
            Ok(())
        })
        .await?;
    Ok(())
}

fn count_outcomes(outs: &[(String, Outcome)], written: bool, stats: &mut InjectStats) {
    for (_, o) in outs {
        match o {
            Outcome::Applied { .. } if written => stats.applied += 1,
            Outcome::Applied { .. } => {}
            Outcome::Failed { .. } => stats.failed += 1,
            Outcome::Marked { .. } => stats.marked += 1,
        }
    }
}

async fn persist(
    ctx: &JobContext,
    outs: &[(String, Outcome)],
    snapshot_id: Option<&str>,
    stats: &mut InjectStats,
) -> JobResult<()> {
    count_outcomes(outs, snapshot_id.is_some(), stats);
    let outs = outs.to_vec();
    let snap = snapshot_id.map(str::to_string);
    ctx.db
        .write(move |t| persist_tx(t, &outs, snap.as_deref()))
        .await?;
    Ok(())
}

fn persist_tx(
    t: &rusqlite::Transaction<'_>,
    outs: &[(String, Outcome)],
    snap: Option<&str>,
) -> sf_core::Result<()> {
    let now = sf_core::now_rfc3339();
    for (id, o) in outs {
        match o {
            Outcome::Applied {
                link_start,
                module,
                llm,
            } => {
                let Some(snap) = snap else { continue };
                t.execute(
                    "UPDATE link_edges SET status='applied', applied_at=?2, snapshot_id=?3, link_start=?4,
                       insert_module=?5, error=NULL, failure_kind=NULL,
                       score_breakdown=CASE WHEN ?6 IS NULL THEN score_breakdown
                         ELSE json_set(CASE WHEN json_valid(score_breakdown) THEN score_breakdown ELSE '{}' END,
                                       '$.llm_edit', json(?6)) END
                     WHERE id=?1 AND status='approved'",
                    params![
                        id,
                        now,
                        snap,
                        *link_start as i64,
                        module.as_str(),
                        llm.as_ref().map(Value::to_string)
                    ],
                )
                .map_err(err)?;
            }
            Outcome::Failed { error, kind } => {
                t.execute(
                    "UPDATE link_edges SET status='failed', error=?2, failure_kind=?3 WHERE id=?1 AND status='approved'",
                    params![id, error.chars().take(1000).collect::<String>(), kind],
                )
                .map_err(err)?;
            }
            Outcome::Marked { module, reason } => {
                t.execute(
                    "UPDATE link_edges SET insert_module=?2, failure_kind='content_gap', error=?3 WHERE id=?1 AND status='approved'",
                    params![id, module.as_str(), reason],
                )
                .map_err(err)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// R4: write-ahead journal for WordPress writes (inject, verify rollback, precise/snapshot undo).
//
// A journal row is written before every content PUT and deleted in the same transaction as the
// bookkeeping that follows a confirmed write (or when the write certainly did not happen). An
// abort at any `.await` in between (cancel grace, hard timeout, shutdown, crash) or a transport
// error whose outcome is unknown leaves the row; `reconcile_site` then compares the live raw
// content with the journal and finishes (or drops) the bookkeeping.

/// One edge recorded in a journal row.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JournalEdge {
    pub id: String,
    /// Target URL and linked text (used to check the live content when it is not byte-equal).
    pub url: String,
    pub text: String,
    #[serde(default)]
    pub link_start: Option<i64>,
    #[serde(default)]
    pub module: Option<String>,
    /// For undo: "reverted" (removed by this write) or "not_present" (already gone).
    #[serde(default)]
    pub result: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Journal {
    pub id: String,
    pub site_id: String,
    pub post_id: String,
    pub job_run_id: Option<String>,
    pub op: &'static str,
    pub snapshot_id: Option<String>,
    pub wp_post_id: u64,
    pub post_type: String,
    pub edges: Vec<JournalEdge>,
    pub expected: String,
    pub previous: String,
}

impl Journal {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        op: &'static str,
        site_id: &str,
        post_id: &str,
        job: Option<&Job>,
        wp_post_id: u64,
        post_type: &str,
        expected: &str,
        previous: &str,
    ) -> Self {
        Self {
            id: sf_core::new_id(),
            site_id: site_id.to_string(),
            post_id: post_id.to_string(),
            job_run_id: job.map(|j| j.id.clone()),
            op,
            snapshot_id: None,
            wp_post_id,
            post_type: post_type.to_string(),
            edges: Vec::new(),
            expected: expected.to_string(),
            previous: previous.to_string(),
        }
    }
}

fn journal_insert(t: &rusqlite::Transaction<'_>, j: &Journal) -> sf_core::Result<()> {
    t.execute(
        "INSERT INTO inject_journal(id, site_id, post_id, job_run_id, op, snapshot_id, wp_post_id, post_type, edges, expected, previous)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            j.id,
            j.site_id,
            j.post_id,
            j.job_run_id,
            j.op,
            j.snapshot_id,
            i64::try_from(j.wp_post_id).unwrap_or(i64::MAX),
            j.post_type,
            serde_json::to_string(&j.edges).unwrap_or_else(|_| "[]".into()),
            j.expected,
            j.previous,
        ],
    )
    .map_err(err)?;
    Ok(())
}

/// Writes the intent row (before the PUT).
pub async fn journal_begin(ctx: &JobContext, j: &Journal) -> JobResult<()> {
    let j = j.clone();
    ctx.db.write(move |t| journal_insert(t, &j)).await?;
    Ok(())
}

/// Drops an intent row whose write certainly did not land (and its unused snapshot, R13).
pub async fn journal_abandon(ctx: &JobContext, j: &Journal) -> JobResult<()> {
    let (id, snap) = (j.id.clone(), j.snapshot_id.clone());
    ctx.db
        .write(move |t| {
            t.execute("DELETE FROM inject_journal WHERE id=?1", [&id])
                .map_err(err)?;
            if let Some(s) = snap {
                delete_unused_snapshot(t, &s)?;
            }
            Ok(())
        })
        .await?;
    Ok(())
}

fn delete_unused_snapshot(t: &rusqlite::Transaction<'_>, snap: &str) -> sf_core::Result<()> {
    t.execute(
        "DELETE FROM snapshots WHERE id=?1
           AND NOT EXISTS (SELECT 1 FROM link_edges WHERE snapshot_id=?1)
           AND NOT EXISTS (SELECT 1 FROM inject_journal WHERE snapshot_id=?1)",
        [snap],
    )
    .map_err(err)?;
    Ok(())
}

/// Sites that have unfinished journal rows (the runner queues a reconcile job for each at boot).
pub async fn journal_sites(db: &sf_store::Db) -> sf_core::Result<Vec<String>> {
    db.read(|c| {
        let mut st = c
            .prepare("SELECT DISTINCT site_id FROM inject_journal ORDER BY site_id")
            .map_err(err)?;
        let v = st
            .query_map([], |r| r.get(0))
            .map_err(err)?
            .collect::<rusqlite::Result<Vec<String>>>()
            .map_err(err)?;
        Ok(v)
    })
    .await
}

/// Local copy of the post after a confirmed write (also records `sf_written_gmt`, R9).
pub fn save_post_tx(
    t: &rusqlite::Transaction<'_>,
    post_id: &str,
    raw: &str,
    rendered: &str,
    modified: &str,
) -> sf_core::Result<()> {
    let cur: String = t
        .query_row(
            "SELECT content_html FROM posts WHERE id=?1",
            [post_id],
            |r| r.get(0),
        )
        .map_err(err)?;
    let html = if rendered.trim().is_empty() {
        cur
    } else {
        rendered.to_string()
    };
    t.execute(
        "UPDATE posts SET content_raw=?2, content_html=?3, modified_gmt=?4, modified_at=?5, content_hash=?6,
           sf_written_gmt=?4 WHERE id=?1",
        params![
            post_id,
            raw,
            html,
            modified,
            sf_core::now_rfc3339(),
            content_hash(&html, Some(raw))
        ],
    )
    .map_err(err)?;
    Ok(())
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ReconcileStats {
    pub journal_rows: usize,
    pub landed: usize,
    pub not_landed: usize,
    pub unreachable: usize,
}

struct JournalRow {
    id: String,
    post_id: String,
    op: String,
    snapshot_id: Option<String>,
    wp_post_id: i64,
    post_type: String,
    edges: Vec<JournalEdge>,
    expected: String,
    previous: String,
}

/// Finishes the bookkeeping of every unfinished WordPress write of `site_id` by comparing the
/// journal with the live raw content. Rows whose post can't be read stay for the next run.
pub async fn reconcile_site(
    ctx: &JobContext,
    job: Option<&Job>,
    site_id: &str,
    writer: &WpWriter,
) -> JobResult<ReconcileStats> {
    let sid = site_id.to_string();
    let rows: Vec<JournalRow> = ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare(
                    "SELECT id, post_id, op, snapshot_id, wp_post_id, post_type, edges, expected, previous
                     FROM inject_journal WHERE site_id=?1 ORDER BY created_at, id",
                )
                .map_err(err)?;
            let v = st
                .query_map([&sid], |r| {
                    let edges: String = r.get(6)?;
                    Ok(JournalRow {
                        id: r.get(0)?,
                        post_id: r.get(1)?,
                        op: r.get(2)?,
                        snapshot_id: r.get(3)?,
                        wp_post_id: r.get(4)?,
                        post_type: r.get(5)?,
                        edges: serde_json::from_str(&edges).unwrap_or_default(),
                        expected: r.get(7)?,
                        previous: r.get(8)?,
                    })
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(v)
        })
        .await?;
    let mut stats = ReconcileStats {
        journal_rows: rows.len(),
        ..Default::default()
    };
    for row in rows {
        let Ok(wp_id) = u64::try_from(row.wp_post_id) else {
            continue;
        };
        let live = match writer.get_post_raw(wp_id, &row.post_type).await {
            Ok(l) => l,
            Err(e) => {
                stats.unreachable += 1;
                if let Some(j) = job {
                    j.warn(format!(
                        "reconcile: post {wp_id} unreadable ({e}); unfinished write kept for the next run"
                    ))
                    .await;
                }
                continue;
            }
        };
        let landed = journal_landed(&row, &live.raw);
        if landed {
            stats.landed += 1;
        } else {
            stats.not_landed += 1;
        }
        if let Some(j) = job {
            j.warn(format!(
                "reconcile: unfinished {} on post {wp_id} {} WordPress",
                row.op,
                if landed { "reached" } else { "did not reach" }
            ))
            .await;
        }
        apply_reconciled(ctx, row, landed, live).await?;
    }
    Ok(stats)
}

fn journal_landed(row: &JournalRow, live: &str) -> bool {
    if live == row.expected {
        return true;
    }
    if live == row.previous {
        return false;
    }
    // edited since: decide by the links themselves
    let present = |e: &JournalEdge| count_links(live, &e.url, &e.text) > 0;
    match row.op.as_str() {
        "inject" => !row.edges.is_empty() && row.edges.iter().all(present),
        "rollback" => row.edges.iter().all(|e| !present(e)),
        _ => row
            .edges
            .iter()
            .filter(|e| e.result.as_deref() == Some("reverted"))
            .all(|e| !present(e)),
    }
}

async fn apply_reconciled(
    ctx: &JobContext,
    row: JournalRow,
    landed: bool,
    live: sf_crawl::RawPost,
) -> JobResult<()> {
    ctx.db
        .write(move |t| {
            let now = sf_core::now_rfc3339();
            if landed {
                save_post_tx(t, &row.post_id, &live.raw, &live.rendered, &live.modified_gmt)?;
                for e in &row.edges {
                    match row.op.as_str() {
                        "inject" => {
                            t.execute(
                                "UPDATE link_edges SET status='applied', applied_at=?2, snapshot_id=?3, link_start=?4,
                                   insert_module=coalesce(?5, insert_module), error=NULL, failure_kind=NULL
                                 WHERE id=?1 AND status IN ('approved','failed')",
                                params![e.id, now, row.snapshot_id, e.link_start, e.module],
                            )
                            .map_err(err)?;
                        }
                        "rollback" => {
                            t.execute(
                                "UPDATE link_edges SET status='failed', applied_at=NULL,
                                   error='verify failed (rolled back; confirmed on reconcile)', failure_kind='verify_failed'
                                 WHERE id=?1 AND status='applied'",
                                [&e.id],
                            )
                            .map_err(err)?;
                        }
                        _ => {
                            let note = (e.result.as_deref() == Some("not_present"))
                                .then_some("link not present");
                            t.execute(
                                "UPDATE link_edges SET status='reverted', error=?2 WHERE id=?1 AND status='applied'",
                                params![e.id, note],
                            )
                            .map_err(err)?;
                        }
                    }
                }
            }
            t.execute("DELETE FROM inject_journal WHERE id=?1", [&row.id])
                .map_err(err)?;
            if !landed
                && row.op == "inject"
                && let Some(s) = &row.snapshot_id
            {
                delete_unused_snapshot(t, s)?;
            }
            Ok(())
        })
        .await?;
    Ok(())
}

/// Reconciles the site's journal when it has WordPress credentials (inject and undo call this
/// first). Returns the stats as JSON (`null` when there was nothing to do).
pub async fn reconcile_for_job(ctx: &JobContext, job: &Job, site: &SiteInfo) -> JobResult<Value> {
    let sid = site.id.clone();
    let pending: i64 = ctx
        .db
        .read(move |c| {
            c.query_row(
                "SELECT count(*) FROM inject_journal WHERE site_id=?1",
                [&sid],
                |r| r.get(0),
            )
            .map_err(err)
        })
        .await?;
    if pending == 0 || !site.has_wp_credentials() {
        return Ok(Value::Null);
    }
    let writer = WpWriter::new(site.wp_http_client(ctx)?, site.wp_site(true)?)?;
    let rep = reconcile_site(ctx, Some(job), &site.id, &writer).await?;
    job.info(format!(
        "reconciled {} unfinished WordPress write(s): {} landed, {} did not, {} unreadable",
        rep.journal_rows, rep.landed, rep.not_landed, rep.unreachable
    ))
    .await;
    Ok(serde_json::to_value(rep).unwrap_or(Value::Null))
}
