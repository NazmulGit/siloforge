//! `plan`: the full planning pipeline (v1 `planSite` order with the v2 fixes):
//! chunk → embed → keywords → graph → cluster → topology + semantic (+ archive) candidates →
//! score (rayon) → density AFTER scoring → anchors → module prediction → save.
//!
//! * Candidates never repeat a (source, target) pair that is already linked, approved, pending or
//!   rejected (any anchor), fixes B-6.
//! * Existing `suggested` plan/archive edges are rescored every run (B-13) and compete with the new
//!   candidates for the density budget (B-7); dropped ones are deleted.
//! * Vectors come from the per-site cache; paragraph HTML is streamed per batch of source posts.
//!
//! Stats = v1 `PlanStats` `{posts, paragraphs, embedded, candidates, scored, archive_candidates}`
//! plus `{chunked_posts, keyword_posts, clusters, rescored, dropped_by_density, anchors_in_paragraph,
//! deleted_stale, skipped, graph, topology, progress}`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rusqlite::params;
use serde_json::{Value, json};
use sf_core::types::{InsertModule, Topology};
use sf_engine::cluster::{
    KmeansParams, PillarCandidate, cluster_label, default_k, kmeans, select_pillar,
};
use sf_engine::density::{
    Candidate, DensityContext, DensitySettings, apply_density, resolve_limits,
};
use sf_engine::insert::pick_module;
use sf_engine::score::{DimAvailability, DimInputs, Scored, age_days, score_batch, tier_of};
use sf_engine::silo::{
    ArchiveInput, SiloCluster, SkipPairs, SkipStats, archive_candidates, build_topology_candidates,
    semantic_candidates,
};
use sf_store::err;

use super::anchors::{self, AiRefineOptions, AnchorInput, AnchorRunOptions};
use super::chunk;
use super::common::{SiteInfo, load_site};
use super::embed;
use super::graph::{self, GraphData};
use super::keywords;
use crate::ctx::{Job, JobContext, JobError, JobResult, SiteVectors, compute};

const SEMANTIC_K: usize = 5;
const SEMANTIC_MIN_SCORE: f64 = 0.5;
const ARCHIVE_TOP_N: usize = 5;
const TOP_PARAGRAPHS: usize = 5;
const SAVE_BATCH: usize = 500;

#[derive(Debug, Clone)]
struct PostInfo {
    id: String,
    title: String,
    post_type: String,
    word_count: i64,
    cluster_id: Option<String>,
    published_ms: Option<i64>,
    money: bool,
    tier: i64,
}

#[derive(Debug, Clone)]
struct Cand {
    /// Existing suggested edge id (None = new candidate).
    edge_id: Option<String>,
    source: String,
    target: String,
    origin: &'static str,
    topology: Option<String>,
    anchor: String,
    scored: Option<Scored>,
    /// Paragraph idx values ranked by cosine to the target (best first).
    top_paras: Vec<i64>,
    has_best: bool,
    /// `score_breakdown.ai` of an existing suggestion (kept, and its relevance factor re-applied,
    /// when the edge is rescored).
    prev_ai: Option<Value>,
}

/// `plan` job entry point.
pub async fn run(ctx: &Arc<JobContext>, job: &Job) -> JobResult<Value> {
    let site = load_site(ctx, job.site()?).await?;
    let topology = job
        .payload_str("topology", "topology")
        .or_else(|| site.setting_str("silo_topology"))
        .unwrap_or_else(|| "reverse".into());
    let archive = job.payload_bool("archive", "archive");
    let k = job
        .payload
        .get("k")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok());
    let stats = plan_site(ctx, job, &site, &topology, archive, k).await?;
    Ok(stats)
}

/// Which candidate sources a plan uses (link runs narrow it; the default is everything).
#[derive(Debug, Clone)]
pub struct PlanScope {
    /// Silo topology candidates (`topology`).
    pub topology: bool,
    /// Semantic (embedding similarity) candidates.
    pub semantic: bool,
    /// Density overrides (`None` = the site setting).
    pub max_links_per_post: Option<f64>,
    pub max_new_links_per_pass: Option<f64>,
    /// AI anchor refinement override (link run option `ai_refine`); None = payload / site setting.
    pub ai_refine: Option<bool>,
    /// v2.3: AI work on the `priority` tier.
    pub high_priority: bool,
    /// Link Run id for AI accounting (None = this job's id).
    pub run_id: Option<String>,
}

impl Default for PlanScope {
    fn default() -> Self {
        Self {
            topology: true,
            semantic: true,
            max_links_per_post: None,
            max_new_links_per_pass: None,
            ai_refine: None,
            high_priority: false,
            run_id: None,
        }
    }
}

/// The whole pipeline for one site (used by `plan` and the `audit` plan stage).
pub async fn plan_site(
    ctx: &JobContext,
    job: &Job,
    site: &SiteInfo,
    topology: &str,
    archive: bool,
    k: Option<usize>,
) -> JobResult<Value> {
    plan_site_scoped(ctx, job, site, topology, archive, k, &PlanScope::default()).await
}

/// [`plan_site`] with a candidate scope (link runs).
pub async fn plan_site_scoped(
    ctx: &JobContext,
    job: &Job,
    site: &SiteInfo,
    topology: &str,
    archive: bool,
    k: Option<usize>,
    scope: &PlanScope,
) -> JobResult<Value> {
    let topo = Topology::parse(topology);
    job.info(format!(
        "plan started (topology {topology}, archive {archive})"
    ))
    .await;

    let mut tm = PhaseTimer::new();
    // 1. chunk
    job.progress("chunk", 0.0).await;
    let ch = chunk::chunk_changed(ctx, job, site, "chunk", (0.0, 5.0)).await?;

    tm.lap("chunk");
    // 2. embed
    job.progress("embed", 5.0).await;
    let emb = embed::embed_site(ctx, job, &site.id, "embed", (5.0, 30.0)).await?;
    job.info(format!(
        "chunked {} posts, embedded {} posts + {} paragraphs",
        ch.chunked_posts, emb.posts, emb.paragraphs
    ))
    .await;

    tm.lap("embed");
    // 3. keywords
    job.progress("keywords", 30.0).await;
    let kw_posts = keywords::extract_site(ctx, job, &site.id, "keywords", (30.0, 45.0)).await?;

    tm.lap("keywords");
    // 4. graph
    job.progress("graph", 45.0).await;
    let g = graph::build_graph(ctx, job, site, true).await?;
    job.info(format!(
        "graph: {} nodes, {} edges, {} orphans, roots {:?}",
        g.summary.nodes, g.summary.edges, g.summary.orphans, g.summary.root_strategy
    ))
    .await;

    tm.lap("graph");
    // 5. cluster
    job.progress("cluster", 60.0).await;
    let vectors = ctx.vectors.get(&site.id).await?;
    tm.lap("vectors");
    let pagerank: HashMap<String, f64> = g
        .metrics
        .iter()
        .map(|(id, m)| (id.clone(), m.pagerank))
        .collect();
    let n_clusters = cluster_site(ctx, job, site, &vectors, &pagerank, k).await?;

    tm.lap("cluster");
    // 6. candidates
    job.progress("topology", 70.0).await;
    let posts = load_posts(ctx, site).await?;
    let mut skip = SkipPairs::default();
    let (existing, skip_rows) = load_edges(ctx, &site.id).await?;
    for (s, t, status, origin) in &skip_rows {
        skip.add_edge(s, t, status, origin);
    }
    // #6: a pending suggestion whose pair got linked meanwhile (by an editor, the plugin or an
    // applied duplicate) is not rescored but deleted: approving it would add a second link.
    let linked = linked_pairs(&skip_rows);
    let (existing, stale_linked): (Vec<Cand>, Vec<Cand>) = existing
        .into_iter()
        .partition(|c| !linked.contains(&(c.source.clone(), c.target.clone())));
    let stale_linked: Vec<String> = stale_linked.into_iter().filter_map(|c| c.edge_id).collect();
    if !stale_linked.is_empty() {
        job.info(format!(
            "{} pending suggestion(s) dropped: their pair is already linked",
            stale_linked.len()
        ))
        .await;
    }
    let clusters = load_clusters(ctx, &site.id, &posts).await?;
    let mut topo_set = build_topology_candidates(&topo, &clusters, &pagerank, &skip);
    if !scope.topology {
        topo_set.candidates.clear();
    }
    let mut skipped = topo_set.skipped;
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut cands: Vec<Cand> = Vec::new();
    for c in topo_set.candidates {
        if seen.insert((c.source.clone(), c.target.clone())) {
            cands.push(new_cand(
                c.source,
                c.target,
                "plan",
                Some(topo.as_str().to_string()),
            ));
        }
    }
    let current: HashSet<String> = posts.iter().map(|p| p.id.clone()).collect();
    let post_ids: Vec<String> = posts
        .iter()
        .filter(|p| vectors.posts.position(&p.id).is_some())
        .map(|p| p.id.clone())
        .collect();
    let v2 = vectors.clone();
    let ids2 = post_ids.clone();
    let neighbors: Vec<(String, Vec<(String, f64)>)> = compute(move || {
        let refs: Vec<&str> = ids2.iter().map(String::as_str).collect();
        let hits = v2.posts.similar_batch(&refs, SEMANTIC_K);
        ids2.iter()
            .zip(hits)
            .map(|(id, h)| {
                (
                    id.clone(),
                    v2.posts
                        .resolve(&h)
                        .into_iter()
                        .map(|(t, s)| (t.to_string(), f64::from(s)))
                        .collect(),
                )
            })
            .collect()
    })
    .await?;
    let mut sem = semantic_candidates(&neighbors, SEMANTIC_MIN_SCORE, &current, &skip);
    if !scope.semantic {
        sem.candidates.clear();
    }
    add_skips(&mut skipped, &sem.skipped);
    for c in sem.candidates {
        if seen.insert((c.source.clone(), c.target.clone())) {
            cands.push(new_cand(c.source, c.target, "plan", None));
        }
    }
    let mut archive_new = 0usize;
    if archive {
        let arch = archive_inputs(&posts, &vectors, &pagerank).await?;
        let set = archive_candidates(&arch, ARCHIVE_TOP_N, &skip);
        add_skips(&mut skipped, &set.skipped);
        for c in set.candidates {
            if seen.insert((c.source.clone(), c.target.clone())) {
                cands.push(new_cand(c.source, c.target, "archive", None));
                archive_new += 1;
            }
        }
    }
    let new_count = cands.len();
    let rescored = existing.len();
    let repicked = existing.iter().filter(|e| e.anchor.is_empty()).count();
    for e in existing {
        cands.push(e);
    }
    job.info(format!(
        "candidates: {new_count} new, {rescored} pending suggestions rescored ({repicked} get a new anchor)"
    ))
    .await;
    job.checkpoint().await?;

    tm.lap("candidates");
    // 7. score (rayon)
    job.progress("score", 78.0).await;
    let scored_n = cands.len();
    let cands = score_candidates(ctx, site, cands, &posts, &g, &vectors).await?;
    job.checkpoint().await?;

    tm.lap("score");
    // 8. density (after scoring)
    let mut dset = density_settings(site);
    if scope.max_links_per_post.is_some() {
        dset.max_links_per_post = scope.max_links_per_post;
    }
    if scope.max_new_links_per_pass.is_some() {
        dset.max_new_links_per_pass = scope.max_new_links_per_pass;
    }
    let limits = resolve_limits(&dset);
    let mut dctx = DensityContext::default();
    // committed outbound = distinct targets linked live/applied or approved (about to be applied)
    let mut committed: HashSet<(&str, &str)> = g
        .edges
        .iter()
        .map(|(s, t)| (s.as_str(), t.as_str()))
        .collect();
    for (s, t, status, _) in &skip_rows {
        if status == "approved" && s != t {
            committed.insert((s.as_str(), t.as_str()));
        }
    }
    for (s, _) in committed {
        *dctx.existing_outbound.entry(s.to_string()).or_insert(0) += 1;
    }
    dctx.money_pages = posts
        .iter()
        .filter(|p| p.money)
        .map(|p| p.id.clone())
        .collect();
    let dens_in: Vec<Candidate> = cands
        .iter()
        .enumerate()
        .map(|(i, c)| Candidate {
            id: c.edge_id.clone().unwrap_or_else(|| format!("~new{i:09}")),
            source: c.source.clone(),
            target: c.target.clone(),
            score: c.scored.map_or(0.0, |s| s.composite_score),
        })
        .collect();
    let capped = apply_density(dens_in, &dctx, &limits);
    let kept_ids: HashSet<String> = capped.kept.into_iter().map(|c| c.id).collect();
    let mut kept: Vec<Cand> = Vec::new();
    let mut drop_existing: Vec<String> = stale_linked;
    for (i, c) in cands.into_iter().enumerate() {
        let key = c.edge_id.clone().unwrap_or_else(|| format!("~new{i:09}"));
        if kept_ids.contains(&key) {
            kept.push(c);
        } else if let Some(id) = c.edge_id {
            drop_existing.push(id);
        }
    }
    let dropped_by_density = scored_n.saturating_sub(kept.len());
    // deterministic order for anchors: source, score desc, target
    kept.sort_by(|a, b| {
        a.source
            .cmp(&b.source)
            .then_with(|| {
                let (sa, sb) = (
                    a.scored.map_or(0.0, |s| s.composite_score),
                    b.scored.map_or(0.0, |s| s.composite_score),
                );
                sb.total_cmp(&sa)
            })
            .then_with(|| a.target.cmp(&b.target))
    });

    tm.lap("density");
    // 9. anchors
    job.progress("anchors", 85.0).await;
    let titles: HashMap<String, String> = posts
        .iter()
        .map(|p| (p.id.clone(), p.title.clone()))
        .collect();
    let inputs: Vec<AnchorInput> = kept
        .iter()
        .enumerate()
        .map(|(i, c)| AnchorInput {
            key: i,
            source: c.source.clone(),
            target: c.target.clone(),
            anchor: c.anchor.clone(),
            paragraphs: c.top_paras.clone(),
            score: c.scored.map_or(0.0, |s| s.composite_score),
            relevance: c.scored.map(|s| s.score_breakdown.semantic),
        })
        .collect();
    let run = anchor_run_options(ctx, job, site, &vectors, scope);
    if let Some(ai) = &run.ai {
        job.info(format!(
            "anchors: AI refinement on (top {} new edges, concurrency {}, budget {} s)",
            ai.max_edges,
            ai.concurrency,
            ai.timeout.as_secs()
        ))
        .await;
    }
    let picks = anchors::generate(
        ctx,
        job,
        site,
        &titles,
        inputs,
        &run,
        "anchors",
        (85.0, 95.0),
    )
    .await?;

    tm.lap("anchors");
    // 10. save
    job.progress("save", 95.0).await;
    let allowed = site.insert_modules();
    let preset = site.setting_str("preset");
    let mut new_rows: Vec<Value> = Vec::new();
    let mut upd_rows: Vec<Value> = Vec::new();
    let mut in_para = 0usize;
    for p in picks {
        let Some(c) = kept.get(p.key) else { continue };
        if p.anchor.trim().is_empty() {
            if let Some(id) = &c.edge_id {
                drop_existing.push(id.clone());
            }
            continue;
        }
        if p.in_paragraph {
            in_para += 1;
        }
        // AI relevance (this run, or kept from the previous one) scales the composite score
        let ai = p.ai.clone().or_else(|| c.prev_ai.clone());
        let factor = if p.ai.is_some() {
            p.score_factor
        } else {
            sf_engine::anchor::refine::relevance_factor(
                ai.as_ref()
                    .and_then(|a| a.get("relevance"))
                    .and_then(Value::as_f64),
            )
        };
        let composite = c.scored.map_or(0.0, |s| s.composite_score) * factor;
        let score100 = if c.has_best { composite * 100.0 } else { 0.0 };
        let module = pick_module(score100, &allowed, preset.as_deref(), p.in_paragraph);
        let mut breakdown = c
            .scored
            .map(|s| serde_json::to_value(s.score_breakdown).unwrap_or_else(|_| json!({})))
            .unwrap_or_else(|| json!({}));
        if let Some(b) = breakdown.as_object_mut() {
            if let Some(m) = &p.meta {
                b.insert("anchor".into(), m.clone());
            }
            if !p.alternates.is_empty() {
                b.insert("alternates".into(), Value::Array(p.alternates.clone()));
            }
            if let Some(a) = ai {
                b.insert("ai".into(), a);
            }
            if factor != 1.0 {
                b.insert("ai_factor".into(), json!(factor));
            }
        }
        let row = json!({
            "id": c.edge_id,
            "source": c.source,
            "target": c.target,
            "origin": c.origin,
            "topology": c.topology,
            "anchor": crate::pipelines::common::truncate_utf16(&p.anchor, 255),
            "anchor_type": p.anchor_type.as_str(),
            "score": composite,
            "breakdown": breakdown.to_string(),
            "module": module.as_str(),
            "paragraph_id": p.paragraph_id,
            "was_blank": c.anchor.is_empty(),
        });
        if c.edge_id.is_some() {
            upd_rows.push(row);
        } else {
            new_rows.push(row);
        }
    }
    let saved = save_edges(
        ctx,
        job,
        &site.id,
        new_rows,
        upd_rows,
        drop_existing.clone(),
    )
    .await?;
    let paragraphs: i64 = {
        let sid = site.id.clone();
        ctx.db
            .read(move |c| {
                c.query_row(
                    "SELECT count(*) FROM paragraphs pa JOIN posts p ON p.id=pa.post_id WHERE p.site_id=?1",
                    [&sid],
                    |r| r.get(0),
                )
                .map_err(err)
            })
            .await?
    };
    tm.lap("save");
    job.info(format!(
        "plan saved: {} new suggestions ({} archive), {} rescored, {} dropped",
        saved.inserted, saved.archive, saved.updated, saved.deleted
    ))
    .await;
    job.info(format!("phase timings (ms): {}", tm.summary()))
        .await;
    Ok(json!({
        "posts": posts.len(),
        "paragraphs": paragraphs,
        "embedded": emb.posts + emb.paragraphs,
        "candidates": saved.inserted,
        "scored": scored_n,
        "archive_candidates": saved.archive.min(archive_new),
        "chunked_posts": ch.chunked_posts,
        "keyword_posts": kw_posts,
        "clusters": n_clusters,
        "rescored": saved.updated,
        "dropped_by_density": dropped_by_density,
        "deleted_stale": saved.deleted,
        "anchors_in_paragraph": in_para,
        "skipped": {
            "self_pairs": skipped.self_pairs, "duplicates": skipped.duplicates,
            "linked": skipped.linked, "rejected": skipped.rejected, "pending": skipped.pending,
        },
        "graph": {
            "nodes": g.summary.nodes, "edges": g.summary.edges, "orphans": g.summary.orphans,
            "dead_ends": g.summary.dead_ends, "avg_depth": g.summary.avg_depth,
            "components": g.summary.components, "root_strategy": g.summary.root_strategy,
        },
        "topology": topology,
        "timings_ms": tm.to_json(),
        "progress": {"phase": "done", "pct": 100},
    }))
}

/// Anchor options for a plan: semantic scoring when vectors exist; AI refinement from the link
/// run option, the payload (`ai_refine` / `aiRefine`, overrides) or the site setting
/// `ai_anchor_refine` (default off) — always on with `ai.always_on` (v2.3) — budget from
/// `ai_anchor_refine_max` (default 200, 5000 with always_on), `ai_anchor_refine_concurrency` (4)
/// and `ai_anchor_refine_timeout_s` (300). Batch size and skip thresholds come from `[ai]`.
fn anchor_run_options(
    ctx: &JobContext,
    job: &Job,
    site: &SiteInfo,
    vectors: &Arc<SiteVectors>,
    scope: &PlanScope,
) -> AnchorRunOptions {
    let ai_cfg = ctx.live.get().ai.clone();
    let payload = scope.ai_refine.or_else(|| {
        job.payload
            .get("ai_refine")
            .or_else(|| job.payload.get("aiRefine"))
            .and_then(Value::as_bool)
    });
    let enabled =
        ai_cfg.always_on || payload.unwrap_or_else(|| site.setting_bool("ai_anchor_refine"));
    let default_max = if ai_cfg.always_on { 5000.0 } else { 200.0 };
    let num = |k: &str, d: f64, lo: f64, hi: f64| {
        site.setting_f64(k)
            .filter(|v| v.is_finite())
            .unwrap_or(d)
            .clamp(lo, hi)
    };
    let run_id = scope.run_id.clone().unwrap_or_else(|| job.id.clone());
    let ai = enabled.then(|| AiRefineOptions {
        max_edges: num("ai_anchor_refine_max", default_max, 0.0, 5000.0) as usize,
        batch_size: ai_cfg.batch(),
        skip_anchor: ai_cfg.confidence_skip.anchor,
        skip_relevance: ai_cfg.confidence_skip.relevance,
        run_id: Some(run_id),
        high_priority: scope.high_priority || site.setting_bool("ai_high_priority"),
        concurrency: num("ai_anchor_refine_concurrency", 4.0, 1.0, 16.0) as usize,
        timeout: std::time::Duration::from_secs(num(
            "ai_anchor_refine_timeout_s",
            300.0,
            5.0,
            3600.0,
        ) as u64),
    });
    AnchorRunOptions {
        vectors: Some(vectors.clone()),
        semantic: site.setting_str("anchor_semantic").as_deref() != Some("false")
            && !site.setting_bool("anchor_semantic_off"),
        ai,
        min_semantic: site
            .setting_f64("anchor_semantic_min")
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(0.0, 1.0) as f32),
    }
}

/// Wall time per pipeline phase (reported in the job log and `stats.timings_ms`).
struct PhaseTimer {
    last: std::time::Instant,
    laps: Vec<(&'static str, u64)>,
}

impl PhaseTimer {
    fn new() -> Self {
        Self {
            last: std::time::Instant::now(),
            laps: Vec::new(),
        }
    }

    fn lap(&mut self, phase: &'static str) {
        let now = std::time::Instant::now();
        let ms = u64::try_from(now.duration_since(self.last).as_millis()).unwrap_or(u64::MAX);
        self.laps.push((phase, ms));
        self.last = now;
    }

    fn summary(&self) -> String {
        self.laps
            .iter()
            .map(|(p, ms)| format!("{p} {ms}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn to_json(&self) -> Value {
        Value::Object(
            self.laps
                .iter()
                .map(|(p, ms)| ((*p).to_string(), json!(ms)))
                .collect(),
        )
    }
}

fn new_cand(
    source: String,
    target: String,
    origin: &'static str,
    topology: Option<String>,
) -> Cand {
    Cand {
        edge_id: None,
        source,
        target,
        origin,
        topology,
        anchor: String::new(),
        scored: None,
        top_paras: Vec::new(),
        has_best: false,
        prev_ai: None,
    }
}

fn add_skips(a: &mut SkipStats, b: &SkipStats) {
    a.self_pairs += b.self_pairs;
    a.duplicates += b.duplicates;
    a.linked += b.linked;
    a.rejected += b.rejected;
    a.pending += b.pending;
}

pub(crate) fn density_settings(site: &SiteInfo) -> DensitySettings {
    DensitySettings {
        max_links_per_post: site.setting_f64("max_links_per_post"),
        min_links_per_post: site.setting_f64("min_links_per_post"),
        max_new_links_per_pass: site.setting_f64("max_new_links_per_pass"),
        link_overload_threshold: site.setting_f64("link_overload_threshold"),
        max_new_inbound_per_target: site.setting_f64("max_new_inbound_per_target"),
    }
}

// ---------------------------------------------------------------------------------------------
// loading
// ---------------------------------------------------------------------------------------------

async fn load_posts(ctx: &JobContext, site: &SiteInfo) -> JobResult<Vec<PostInfo>> {
    let sid = site.id.clone();
    let money = site.money_urls();
    let rows: Vec<(String, String, String, String, i64, Option<String>, Option<String>, String)> = ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare(
                    "SELECT id, url, title, post_type, word_count, cluster_id, published_at,
                       json_object('money_page', json_extract(CASE WHEN json_valid(meta) THEN meta ELSE '{}' END, '$.money_page'),
                                   'tier', json_extract(CASE WHEN json_valid(meta) THEN meta ELSE '{}' END, '$.tier'))
                     FROM posts WHERE site_id=?1 ORDER BY id",
                )
                .map_err(err)?;
            let v = st
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
            Ok(v)
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, url, title, post_type, wc, cluster_id, published, meta)| {
                let meta: Value = serde_json::from_str(&meta).unwrap_or(Value::Null);
                let money_flag = meta
                    .get("money_page")
                    .is_some_and(|v| v.as_bool() == Some(true) || v.as_i64() == Some(1));
                let tier = tier_of(&url, meta.get("tier").and_then(Value::as_i64));
                PostInfo {
                    money: money_flag || money.contains(&sf_engine::html::normalize_url(&url)),
                    published_ms: published
                        .as_deref()
                        .and_then(|p| chrono::DateTime::parse_from_rfc3339(p).ok())
                        .map(|d| d.timestamp_millis()),
                    tier,
                    id,
                    title,
                    post_type,
                    word_count: wc,
                    cluster_id,
                }
            },
        )
        .collect())
}

type EdgeRow = (String, String, String, String);

/// (source, target) pairs that are live or about to be: discovered links, applied or approved.
fn linked_pairs(rows: &[EdgeRow]) -> HashSet<(String, String)> {
    rows.iter()
        .filter(|(_, _, status, origin)| {
            origin == "existing" || matches!(status.as_str(), "applied" | "approved")
        })
        .map(|(s, t, _, _)| (s.clone(), t.clone()))
        .collect()
}

/// Existing suggested plan/archive edges (to rescore) + every edge row for the skip sets.
async fn load_edges(ctx: &JobContext, site_id: &str) -> JobResult<(Vec<Cand>, Vec<EdgeRow>)> {
    let sid = site_id.to_string();
    type EdgeDbRow = (
        String,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<EdgeDbRow> = ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare(
                    "SELECT id, source_post_id, target_post_id, status, origin, anchor, silo_topology,
                            CASE WHEN status='suggested' THEN score_breakdown END
                     FROM link_edges WHERE site_id=?1 ORDER BY id",
                )
                .map_err(err)?;
            let v = st
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
            Ok(v)
        })
        .await?;
    let mut existing = Vec::new();
    let mut skip = Vec::with_capacity(rows.len());
    let mut seen_pairs: HashSet<(String, String)> = HashSet::new();
    for (id, s, t, status, origin, anchor, topo, breakdown) in rows {
        if status == "suggested" && (origin == "plan" || origin == "archive") && s != t {
            // Pending anchors from older pickers that are spec fragments ("V kits perfect",
            // "inch cordless Impact Wrenches": v1 split "20V") are re-picked like blank ones.
            let anchor = if sf_engine::anchor::is_fragment_anchor(&anchor) {
                String::new()
            } else {
                anchor
            };
            // one rescored suggestion per pair (older duplicates are left as they are)
            if seen_pairs.insert((s.clone(), t.clone())) {
                existing.push(Cand {
                    edge_id: Some(id),
                    source: s.clone(),
                    target: t.clone(),
                    origin: if origin == "archive" {
                        "archive"
                    } else {
                        "plan"
                    },
                    topology: topo,
                    anchor,
                    scored: None,
                    top_paras: Vec::new(),
                    has_best: false,
                    prev_ai: breakdown
                        .as_deref()
                        .and_then(|b| serde_json::from_str::<Value>(b).ok())
                        .and_then(|v| v.get("ai").cloned()),
                });
            }
        }
        skip.push((s, t, status, origin));
    }
    Ok((existing, skip))
}

async fn load_clusters(
    ctx: &JobContext,
    site_id: &str,
    posts: &[PostInfo],
) -> JobResult<Vec<SiloCluster>> {
    let sid = site_id.to_string();
    let rows: Vec<(String, Option<String>, String)> = ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare(
                    "SELECT id, pillar_post_id, source FROM clusters WHERE site_id=?1 ORDER BY id",
                )
                .map_err(err)?;
            let v = st
                .query_map([&sid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(v)
        })
        .await?;
    let mut members: HashMap<&str, Vec<String>> = HashMap::new();
    for p in posts {
        if let Some(c) = p.cluster_id.as_deref() {
            members.entry(c).or_default().push(p.id.clone());
        }
    }
    Ok(rows
        .into_iter()
        .map(|(id, pillar, source)| SiloCluster {
            members: members.get(id.as_str()).cloned().unwrap_or_default(),
            id,
            pillar,
            custom: source == "custom",
        })
        .collect())
}

async fn archive_inputs(
    posts: &[PostInfo],
    vectors: &Arc<SiteVectors>,
    pagerank: &HashMap<String, f64>,
) -> JobResult<Vec<ArchiveInput>> {
    let archives: Vec<String> = posts
        .iter()
        .filter(|p| p.post_type == "archive")
        .map(|p| p.id.clone())
        .collect();
    if archives.is_empty() {
        return Ok(Vec::new());
    }
    let n_arch = archives.len();
    let v = vectors.clone();
    let a2 = archives.clone();
    let similar: Vec<Vec<String>> = compute(move || {
        let refs: Vec<&str> = a2.iter().map(String::as_str).collect();
        v.posts
            .similar_batch(&refs, ARCHIVE_TOP_N + n_arch)
            .into_iter()
            .map(|h| {
                v.posts
                    .resolve(&h)
                    .into_iter()
                    .map(|(id, _)| id.to_string())
                    .collect()
            })
            .collect()
    })
    .await?;
    let by_id: HashMap<&str, &PostInfo> = posts.iter().map(|p| (p.id.as_str(), p)).collect();
    Ok(archives
        .into_iter()
        .zip(similar)
        .map(|(id, similar)| {
            let cluster = by_id.get(id.as_str()).and_then(|p| p.cluster_id.clone());
            let mut fallback: Vec<&PostInfo> = posts
                .iter()
                .filter(|p| {
                    p.post_type != "archive" && p.cluster_id.is_some() && p.cluster_id == cluster
                })
                .collect();
            fallback.sort_by(|a, b| {
                let pa = pagerank.get(&a.id).copied().unwrap_or(0.0);
                let pb = pagerank.get(&b.id).copied().unwrap_or(0.0);
                pb.total_cmp(&pa).then_with(|| a.id.cmp(&b.id))
            });
            ArchiveInput {
                id,
                similar,
                cluster_fallback: fallback
                    .into_iter()
                    .take(ARCHIVE_TOP_N)
                    .map(|p| p.id.clone())
                    .collect(),
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// clustering
// ---------------------------------------------------------------------------------------------

/// k-means over post embeddings (custom clusters respected), labels + pillars. Returns the
/// number of auto clusters written.
async fn cluster_site(
    ctx: &JobContext,
    job: &Job,
    site: &SiteInfo,
    vectors: &Arc<SiteVectors>,
    pagerank: &HashMap<String, f64>,
    k: Option<usize>,
) -> JobResult<usize> {
    let sid = site.id.clone();
    let members: Vec<(String, String)> = ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare(
                    "SELECT p.id, p.title FROM posts p LEFT JOIN clusters c ON c.id = p.cluster_id
                     WHERE p.site_id=?1 AND p.embedding IS NOT NULL AND (p.cluster_id IS NULL OR c.source IS NULL OR c.source <> 'custom')
                     ORDER BY p.id",
                )
                .map_err(err)?;
            let v = st
                .query_map([&sid], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(v)
        })
        .await?;
    let members: Vec<(String, String)> = members
        .into_iter()
        .filter(|(id, _)| vectors.posts.position(id).is_some())
        .collect();
    if members.len() < 2 {
        return Ok(0);
    }
    job.checkpoint().await?;
    let n = members.len();
    let k = k.unwrap_or_else(|| default_k(n)).clamp(1, n);
    let v = vectors.clone();
    let seed = site.id.clone();
    let prs: Vec<f64> = members
        .iter()
        .map(|(id, _)| pagerank.get(id).copied().unwrap_or(0.0))
        .collect();
    let ids: Vec<String> = members.iter().map(|m| m.0.clone()).collect();
    let titles: Vec<String> = members.iter().map(|m| m.1.clone()).collect();
    type ClusterOut = Vec<(String, Option<String>, Vec<String>)>;
    let result: Result<ClusterOut, String> = compute(move || {
        let wide: Vec<Vec<f64>> = ids
            .iter()
            .map(|id| {
                v.posts
                    .position(id)
                    .and_then(|r| v.posts.vector(r))
                    .map(|x| x.iter().map(|&f| f64::from(f)).collect())
                    .unwrap_or_default()
            })
            .collect();
        // #24: the fixed k-means++ draw (an unresolved draw picks the last row with a non-zero
        // distance, never a duplicate centroid at index 0). Identical output whenever v1's draw
        // resolved, which is every run without float residue.
        let mut params = KmeansParams::new(k, seed);
        params.legacy_draw_fallback = false;
        let res = kmeans(&wide, &params).map_err(|e| format!("{e:?}"))?;
        let mut out = Vec::new();
        for (ci, centroid) in res.centroids.iter().enumerate() {
            let idx: Vec<usize> = res
                .assignments
                .iter()
                .enumerate()
                .filter(|(_, a)| **a == ci)
                .map(|(i, _)| i)
                .collect();
            if idx.is_empty() {
                continue;
            }
            let cands: Vec<PillarCandidate<'_>> = idx
                .iter()
                .filter_map(|&i| {
                    wide.get(i).map(|vec| PillarCandidate {
                        vector: vec,
                        pagerank: prs.get(i).copied().unwrap_or(0.0),
                    })
                })
                .collect();
            let pillar = select_pillar(&cands, centroid)
                .and_then(|p| idx.get(p))
                .and_then(|&i| ids.get(i).cloned());
            let tl: Vec<&str> = idx
                .iter()
                .filter_map(|&i| titles.get(i).map(String::as_str))
                .collect();
            let label = cluster_label(&tl);
            let mem: Vec<String> = idx.iter().filter_map(|&i| ids.get(i).cloned()).collect();
            out.push((label, pillar, mem));
        }
        Ok(out)
    })
    .await?;
    let clusters = result.map_err(JobError::msg)?;
    let count = clusters.len();
    let sid = site.id.clone();
    ctx.db
        .write(move |t| write_clusters(t, &sid, &clusters))
        .await?;
    Ok(count)
}

/// Replace the site's auto clusters. An auto cluster whose member set is unchanged keeps its id
/// (label/pillar refreshed) and its posts are not touched: every `posts` UPDATE rewrites the whole
/// row (content + embedding, ~40 KB on site30), and the old delete-all (FK SET NULL) +
/// re-assign rewrote every post twice per plan. k-means is seeded, so a re-plan of an unchanged
/// site now writes no post rows here.
fn write_clusters(
    t: &rusqlite::Transaction<'_>,
    sid: &str,
    clusters: &[(String, Option<String>, Vec<String>)],
) -> sf_core::Result<()> {
    let mut old: HashMap<String, Vec<String>> = HashMap::new();
    {
        let mut q = t
            .prepare(
                "SELECT c.id, p.id FROM clusters c JOIN posts p ON p.cluster_id = c.id
                 WHERE c.site_id=?1 AND c.source='auto'",
            )
            .map_err(err)?;
        let rows = q
            .query_map([sid], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(err)?;
        for row in rows {
            let (cid, pid) = row.map_err(err)?;
            old.entry(cid).or_default().push(pid);
        }
    }
    let mut by_members: HashMap<Vec<String>, String> = old
        .into_iter()
        .map(|(cid, mut m)| {
            m.sort();
            (m, cid)
        })
        .collect();
    let mut plan: Vec<(Option<String>, &(String, Option<String>, Vec<String>))> = Vec::new();
    for c in clusters {
        let mut key = c.2.clone();
        key.sort();
        plan.push((by_members.remove(&key), c));
    }
    let keep: HashSet<&str> = plan.iter().filter_map(|(id, _)| id.as_deref()).collect();
    let mut stale: Vec<String> = Vec::new();
    {
        let mut q = t
            .prepare("SELECT id FROM clusters WHERE site_id=?1 AND source='auto'")
            .map_err(err)?;
        let rows = q.query_map([sid], |r| r.get::<_, String>(0)).map_err(err)?;
        for row in rows {
            let id = row.map_err(err)?;
            if !keep.contains(id.as_str()) {
                stale.push(id);
            }
        }
    }
    let mut del = t.prepare("DELETE FROM clusters WHERE id=?1").map_err(err)?;
    for id in &stale {
        del.execute([id]).map_err(err)?;
    }
    let mut up = t
        .prepare("UPDATE posts SET cluster_id=?2 WHERE id=?1 AND cluster_id IS NOT ?2")
        .map_err(err)?;
    for (reuse, (label, pillar, mem)) in plan {
        match reuse {
            Some(cid) => {
                t.execute(
                    "UPDATE clusters SET label=?2, pillar_post_id=?3 WHERE id=?1
                       AND (label IS NOT ?2 OR pillar_post_id IS NOT ?3)",
                    params![cid, label, pillar],
                )
                .map_err(err)?;
            }
            None => {
                let cid = super::ids::row_id();
                t.execute(
                    "INSERT INTO clusters(id, site_id, label, pillar_post_id, source) VALUES(?1,?2,?3,?4,'auto')",
                    params![cid, sid, label, pillar],
                )
                .map_err(err)?;
                for m in mem {
                    up.execute(params![m, cid]).map_err(err)?;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// scoring
// ---------------------------------------------------------------------------------------------

struct ScoreTables {
    posts: HashMap<String, PostInfo>,
    metrics: HashMap<String, (f64, i64, bool, bool, usize)>,
    avg_pr: f64,
    incoming: HashMap<String, u32>,
    cluster_inbound: HashMap<String, u32>,
    pillars: HashSet<String>,
    striking: HashMap<String, u32>,
    kw_count: HashMap<String, u32>,
    gsc_connected: bool,
    now_ms: i64,
    av: DimAvailability,
}

async fn score_candidates(
    ctx: &JobContext,
    site: &SiteInfo,
    cands: Vec<Cand>,
    posts: &[PostInfo],
    g: &GraphData,
    vectors: &Arc<SiteVectors>,
) -> JobResult<Vec<Cand>> {
    let sid = site.id.clone();
    let (pillars, striking, kw_count, gsc_connected): (
        HashSet<String>,
        HashMap<String, u32>,
        HashMap<String, u32>,
        bool,
    ) = ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare("SELECT pillar_post_id FROM clusters WHERE site_id=?1 AND pillar_post_id IS NOT NULL")
                .map_err(err)?;
            let pillars = st
                .query_map([&sid], |r| r.get::<_, String>(0))
                .map_err(err)?
                .filter_map(|r| r.ok())
                .collect();
            let mut st = c
                .prepare(
                    "SELECT k.post_id, sum(k.source='gsc' AND k.striking<>0), sum(k.source<>'gsc'), sum(k.source='gsc')
                     FROM keywords k JOIN posts p ON p.id=k.post_id WHERE p.site_id=?1 GROUP BY k.post_id",
                )
                .map_err(err)?;
            let mut striking = HashMap::new();
            let mut kw = HashMap::new();
            let mut gsc = false;
            let rows = st
                .query_map([&sid], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                        r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    ))
                })
                .map_err(err)?;
            for row in rows {
                let (pid, s, k, gc) = row.map_err(err)?;
                if gc > 0 {
                    gsc = true;
                }
                striking.insert(pid.clone(), u32::try_from(s).unwrap_or(u32::MAX));
                kw.insert(pid, u32::try_from(k).unwrap_or(u32::MAX));
            }
            Ok((pillars, striking, kw, gsc))
        })
        .await?;
    let posts_map: HashMap<String, PostInfo> =
        posts.iter().map(|p| (p.id.clone(), p.clone())).collect();
    let mut incoming: HashMap<String, u32> = HashMap::new();
    let mut cluster_inbound: HashMap<String, u32> = HashMap::new();
    for (s, t) in &g.edges {
        *incoming.entry(t.clone()).or_insert(0) += 1;
        let sc = posts_map.get(s).and_then(|p| p.cluster_id.as_deref());
        let tc = posts_map.get(t).and_then(|p| p.cluster_id.as_deref());
        if sc.is_some() && sc == tc {
            *cluster_inbound.entry(t.clone()).or_insert(0) += 1;
        }
    }
    let prs: Vec<f64> = g
        .metrics
        .values()
        .map(|m| m.pagerank)
        .filter(|p| p.is_finite())
        .collect();
    let avg_pr = if prs.is_empty() {
        0.0
    } else {
        prs.iter().sum::<f64>() / prs.len() as f64
    };
    let av = DimAvailability::for_site(
        gsc_connected,
        posts.iter().any(|p| p.money),
        posts.iter().map(|p| p.tier),
    );
    let tables = ScoreTables {
        metrics: g
            .metrics
            .iter()
            .map(|(id, m)| {
                (
                    id.clone(),
                    (
                        m.pagerank,
                        m.click_depth,
                        m.is_orphan,
                        m.is_dead_end,
                        m.component,
                    ),
                )
            })
            .collect(),
        posts: posts_map,
        avg_pr,
        incoming,
        cluster_inbound,
        pillars,
        striking,
        kw_count,
        gsc_connected,
        now_ms: chrono::Utc::now().timestamp_millis(),
        av,
    };
    let v = vectors.clone();
    compute(move || score_all(cands, &tables, &v)).await
}

fn score_all(mut cands: Vec<Cand>, t: &ScoreTables, v: &SiteVectors) -> Vec<Cand> {
    use rayon::prelude::*;
    // best paragraphs + inputs (parallel)
    let prepared: Vec<(DimInputs, Vec<i64>, bool)> = cands
        .par_iter()
        .map(|c| {
            let sp = t.posts.get(&c.source);
            let tp = t.posts.get(&c.target);
            let svec = v.posts.position(&c.source).and_then(|r| v.posts.vector(r));
            let tvec = v.posts.position(&c.target).and_then(|r| v.posts.vector(r));
            let cosine = match (svec, tvec) {
                (Some(a), Some(b)) => f64::from(sf_engine::vector::dot(a, b)).clamp(0.0, 1.0),
                _ => 0.0,
            };
            let mut ranked: Vec<(f32, i64)> = Vec::new();
            if let Some(tv) = tvec {
                for &row in v.paragraphs_of(&c.source) {
                    if let (Some(pv), Some((_, idx))) = (v.paras.vector(row), v.para_meta.get(row))
                    {
                        ranked.push((sf_engine::vector::dot(pv, tv), *idx));
                    }
                }
            }
            ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            let best = ranked.first().copied();
            let top: Vec<i64> = ranked.iter().take(TOP_PARAGRAPHS).map(|r| r.1).collect();
            let m = |id: &str| t.metrics.get(id).copied();
            let sm = m(&c.source);
            let tm = m(&c.target);
            let s_cluster = sp.and_then(|p| p.cluster_id.clone());
            let t_cluster = tp.and_then(|p| p.cluster_id.clone());
            let inputs = DimInputs {
                cosine,
                target_incoming: t.incoming.get(&c.target).copied().unwrap_or(0),
                target_click_depth: tm.map(|x| x.1),
                source_tier: sp.map_or(3, |p| p.tier),
                target_tier: tp.map_or(3, |p| p.tier),
                target_is_pillar: t.pillars.contains(&c.target),
                target_age_days: age_days(tp.and_then(|p| p.published_ms), t.now_ms),
                target_is_orphan: tm.is_some_and(|x| x.2),
                target_is_dead_end: tm.is_some_and(|x| x.3),
                target_pagerank: tm.map_or(0.0, |x| x.0),
                source_pagerank: sm.map_or(0.0, |x| x.0),
                avg_pagerank: t.avg_pr,
                same_component: matches!((sm, tm), (Some(a), Some(b)) if a.4 == b.4),
                target_is_money: tp.is_some_and(|p| p.money),
                source_is_money: sp.is_some_and(|p| p.money),
                money_priority: None,
                money_links_goal: None,
                striking_count: t.striking.get(&c.target).copied().unwrap_or(0),
                gsc_connected: t.gsc_connected,
                cluster_inbound_links: t.cluster_inbound.get(&c.target).copied().unwrap_or(0),
                target_keyword_count: t.kw_count.get(&c.target).copied().unwrap_or(0),
                best_paragraph: best.map(|b| b.1 + 1),
                paragraph_sim: best.map_or(0.0, |b| f64::from(b.0)),
                source_word_count: sp.map_or(0, |p| p.word_count),
                source_cluster_id: s_cluster,
                target_cluster_id: t_cluster,
            };
            (inputs, top, best.is_some())
        })
        .collect();
    let inputs: Vec<DimInputs> = prepared.iter().map(|p| p.0.clone()).collect();
    let scored = score_batch(&inputs, t.av);
    for ((c, s), (_, top, has_best)) in cands.iter_mut().zip(scored).zip(prepared) {
        c.scored = Some(s);
        c.top_paras = top;
        c.has_best = has_best;
    }
    cands
}

// ---------------------------------------------------------------------------------------------
// save
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Saved {
    inserted: usize,
    archive: usize,
    updated: usize,
    deleted: usize,
}

async fn save_edges(
    ctx: &JobContext,
    job: &Job,
    site_id: &str,
    new_rows: Vec<Value>,
    upd_rows: Vec<Value>,
    mut drop_ids: Vec<String>,
) -> JobResult<Saved> {
    let mut saved = Saved::default();
    drop_ids.sort();
    drop_ids.dedup();
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    for chunk in new_rows.chunks(SAVE_BATCH) {
        job.checkpoint().await?;
        let rows = chunk.to_vec();
        let sid = site_id.to_string();
        let (n, a) = ctx
            .db
            .write(move |t| {
                let mut st = t
                    .prepare(
                        "INSERT INTO link_edges(id, site_id, source_post_id, target_post_id, paragraph_id, anchor, anchor_type,
                           composite_score, score_breakdown, silo_topology, insert_module, status, origin)
                         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'suggested',?12)
                         ON CONFLICT(site_id, source_post_id, target_post_id, anchor) DO NOTHING",
                    )
                    .map_err(err)?;
                let (mut n, mut a) = (0usize, 0usize);
                for r in &rows {
                    let origin = s(r, "origin").unwrap_or_else(|| "plan".into());
                    let done = st
                        .execute(params![
                            super::ids::row_id(),
                            sid,
                            s(r, "source"),
                            s(r, "target"),
                            s(r, "paragraph_id"),
                            s(r, "anchor"),
                            s(r, "anchor_type"),
                            r.get("score").and_then(Value::as_f64).unwrap_or(0.0),
                            s(r, "breakdown").unwrap_or_else(|| "{}".into()),
                            s(r, "topology"),
                            s(r, "module").unwrap_or_else(|| InsertModule::Native.as_str().into()),
                            origin
                        ])
                        .map_err(err)?;
                    n += done;
                    if done > 0 && origin == "archive" {
                        a += 1;
                    }
                }
                Ok((n, a))
            })
            .await?;
        saved.inserted += n;
        saved.archive += a;
    }
    for chunk in upd_rows.chunks(SAVE_BATCH) {
        job.checkpoint().await?;
        let rows = chunk.to_vec();
        let n = ctx
            .db
            .write(move |t| {
                let mut n = 0usize;
                for r in &rows {
                    let blank = r.get("was_blank").and_then(Value::as_bool).unwrap_or(false);
                    let sql = if blank {
                        "UPDATE link_edges SET composite_score=?2, score_breakdown=?3, insert_module=?4,
                           paragraph_id=coalesce(?5, paragraph_id), anchor=?6, anchor_type=?7
                         WHERE id=?1 AND status='suggested'"
                    } else {
                        "UPDATE link_edges SET composite_score=?2, score_breakdown=?3, insert_module=?4,
                           paragraph_id=coalesce(?5, paragraph_id)
                         WHERE id=?1 AND status='suggested' AND ?6 IS NOT NULL AND ?7 IS NOT NULL"
                    };
                    // a re-anchored blank edge may collide with an existing anchor: skip that one
                    match t.execute(
                        sql,
                        params![
                            s(r, "id"),
                            r.get("score").and_then(Value::as_f64).unwrap_or(0.0),
                            s(r, "breakdown").unwrap_or_else(|| "{}".into()),
                            s(r, "module"),
                            s(r, "paragraph_id"),
                            s(r, "anchor"),
                            s(r, "anchor_type")
                        ],
                    ) {
                        Ok(k) => n += k,
                        Err(rusqlite::Error::SqliteFailure(f, _))
                            if f.code == rusqlite::ErrorCode::ConstraintViolation => {}
                        Err(e) => return Err(err(e)),
                    }
                }
                Ok(n)
            })
            .await?;
        saved.updated += n;
    }
    for chunk in drop_ids.chunks(SAVE_BATCH) {
        let ids = chunk.to_vec();
        let n = ctx
            .db
            .write(move |t| {
                let mut st = t
                    .prepare("DELETE FROM link_edges WHERE id=?1 AND status='suggested'")
                    .map_err(err)?;
                let mut n = 0;
                for id in &ids {
                    n += st.execute([id]).map_err(err)?;
                }
                Ok(n)
            })
            .await?;
        saved.deleted += n;
    }
    // blank-anchor safety net (v1): suggested edges without an anchor are never kept
    let sid = site_id.to_string();
    saved.deleted += ctx
        .db
        .write(move |t| {
            t.execute(
                "DELETE FROM link_edges WHERE site_id=?1 AND status='suggested' AND trim(anchor)=''",
                [&sid],
            )
            .map_err(err)
        })
        .await?;
    Ok(saved)
}
