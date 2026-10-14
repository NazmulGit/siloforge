//! Anchor generation for planned edges (spec §10, fixes B-9/B-10/B-29/B-30, accuracy C.1/C.3/C.8):
//! source content is re-split per batch of source posts (same splitter as insertion, so anchors
//! respect blocked spans), each paragraph is analysed once (`ParagraphNlp` with the site's term
//! statistics: POS-filtered noun phrases, brand/model detection), targets get one IDF-weighted
//! `TargetProfile`, and `PickOptions` carry the DB-derived constraints (anchors already used in
//! the source, exact anchors reserved by other targets, per-target counts, exact share per site
//! AND per target). The site-wide and per-target guards run over committed + new anchors.
//!
//! Semantic phrase scoring (C.1): per edge the top lexical candidates are embedded once (batched
//! across the edges of a source batch, cached by phrase text in `ctx.phrases`) and their cosine
//! to the target post vector re-ranks them. When the embed chain fails the plan carries on with
//! lexical scores (one warning).
//!
//! AI refinement (optional): for the top-N new edges by composite score, `ctx.ai.chat_json` picks
//! or proposes a verbatim anchor and rates relevance. Validated replies replace the heuristic
//! anchor; anything else keeps it (`fallback_used`). Bounded concurrency, a per-plan timeout and
//! a circuit (3 failures before any success) keep the plan from waiting on a dead provider.
//!
//! Perf: per batch of sources the HTML split + paragraph NLP runs in parallel on the rayon pool;
//! only the picks (which share the running anchor pools) are sequential.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use rusqlite::params;
use serde_json::{Value, json};
use sf_ai::{ChatMessage, ChatRequest};
use sf_core::types::AnchorType;
use sf_engine::anchor::refine::{RefineReply, refine_prompt, relevance_factor, validate_refined};
use sf_engine::anchor::{
    AnchorAlt, GuardItem, PickOptions, PickSource, STRIKING_WEIGHT, TargetProfile,
    apply_diversity_guard, apply_over_optimization_guard, brand_from_domain, classify_anchor,
    find_unblocked, pick_anchor_across, top_phrases,
};
use sf_engine::html::{self, Paragraph};
use sf_engine::nlp::{ParagraphNlp, SiteTerms, TermsBuilder};
use sf_store::err;

use super::chunk::SPLIT_MODE;
use super::common::{SiteInfo, placeholders};
use crate::ctx::{Job, JobContext, JobResult, SiteVectors, compute};

const SOURCE_BATCH: usize = 100;
/// Paragraphs tried per edge (v1 top-5 by cosine).
const TOP_PARAGRAPHS: usize = 5;
/// Prefer non-exact anchors once the site's exact share is this close to the 40% cap.
const AVOID_EXACT_SHARE: f64 = 0.35;
/// Per-target exact-match share cap (diversity of a target's incoming anchors).
const TARGET_EXACT_SHARE: f64 = 0.35;
/// Candidates per edge embedded for semantic scoring.
const SEMANTIC_TOP_K: usize = 3;
/// Paragraphs (best first) offered to the AI per edge.
const AI_PARAGRAPHS: usize = 3;
/// Consecutive AI failures (before any success) that stop asking for the rest of the plan.
const AI_CIRCUIT: usize = 3;
/// Rows read per chunk when building site term statistics.
const TERMS_CHUNK: i64 = 500;

/// One edge that needs an anchor (or a placement check for an anchor it already has).
#[derive(Debug, Clone)]
pub struct AnchorInput {
    /// Caller's index (echoed back).
    pub key: usize,
    pub source: String,
    pub target: String,
    /// Already chosen anchor (existing suggestion), empty = pick one.
    pub anchor: String,
    /// Candidate paragraph idx values, best (highest cosine to the target) first.
    pub paragraphs: Vec<i64>,
    /// Composite score (orders the AI refinement budget).
    pub score: f64,
}

#[derive(Debug, Clone)]
pub struct AnchorOutput {
    pub key: usize,
    pub anchor: String,
    pub anchor_type: AnchorType,
    /// Anchor occurs (unblocked) in `paragraph_idx` → native insertion possible.
    pub in_paragraph: bool,
    pub paragraph_idx: Option<i64>,
    pub paragraph_id: Option<String>,
    /// How the anchor was chosen (`{source, semantic?}`) — merged into `score_breakdown.anchor`.
    pub meta: Option<Value>,
    /// AI judgement (`{anchor, relevance, reason, model, fallback_used}`) for `score_breakdown.ai`.
    pub ai: Option<Value>,
    /// Factor for the composite score (AI relevance < 0.3 scales it down; else 1).
    pub score_factor: f64,
}

/// AI refinement budget.
#[derive(Debug, Clone)]
pub struct AiRefineOptions {
    /// New edges refined per plan (top by composite score).
    pub max_edges: usize,
    pub concurrency: usize,
    /// Wall-clock budget for all AI calls of one plan.
    pub timeout: Duration,
}

impl Default for AiRefineOptions {
    fn default() -> Self {
        Self {
            max_edges: 200,
            concurrency: 4,
            timeout: Duration::from_secs(300),
        }
    }
}

/// Options for one `generate` run.
#[derive(Clone, Default)]
pub struct AnchorRunOptions {
    /// Site vectors (target post vectors for semantic scoring); None = lexical only.
    pub vectors: Option<Arc<SiteVectors>>,
    /// Embed candidate phrases and re-rank by cosine to the target (C.1).
    pub semantic: bool,
    /// AI refinement (None = off).
    pub ai: Option<AiRefineOptions>,
    /// Semantic floor for in-paragraph phrase picks (None = `MIN_PHRASE_SEMANTIC`, 0 = off).
    pub min_semantic: Option<f32>,
}

#[derive(Debug, Clone)]
struct Located {
    anchor: String,
    anchor_type: AnchorType,
    in_paragraph: bool,
    paragraph_idx: Option<i64>,
}

#[derive(Default)]
struct PoolState {
    used_in_source: HashMap<String, HashSet<String>>,
    /// Exact anchor (lowercase) → targets it points at.
    exact_targets: HashMap<String, HashSet<String>>,
    /// Every key of `exact_targets` (handed to `PickOptions::reserved_exact` minus `exclusive`).
    reserved_all: HashSet<String>,
    /// Target → exact anchors that point at that target only (not reserved for it).
    exclusive: HashMap<String, HashSet<String>>,
    target_counts: HashMap<String, HashMap<String, usize>>,
    /// Target → (exact anchors, all anchors) pointing at it.
    target_exact: HashMap<String, (usize, usize)>,
    total: usize,
    exact: usize,
}

impl PoolState {
    /// Record an exact anchor → target pair, keeping `reserved_all` / `exclusive` in sync.
    fn add_exact(&mut self, anchor: String, target: &str) {
        let set = self.exact_targets.entry(anchor.clone()).or_default();
        if set.contains(target) {
            return;
        }
        match set.len() {
            0 => {
                self.exclusive
                    .entry(target.to_string())
                    .or_default()
                    .insert(anchor.clone());
                self.reserved_all.insert(anchor);
            }
            1 => {
                if let Some(ex) = set.iter().next().and_then(|p| self.exclusive.get_mut(p)) {
                    ex.remove(&anchor);
                }
            }
            _ => {}
        }
        set.insert(target.to_string());
    }

    /// Exact anchors pointing at a target other than `target` (moved out of `reserved_all`; give
    /// it back with `restore_reserved`).
    fn take_reserved(&mut self, target: &str) -> (HashSet<String>, Vec<String>) {
        let mut reserved = std::mem::take(&mut self.reserved_all);
        let own: Vec<String> = self
            .exclusive
            .get(target)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        for a in &own {
            reserved.remove(a);
        }
        (reserved, own)
    }

    fn restore_reserved(&mut self, mut reserved: HashSet<String>, own: Vec<String>) {
        reserved.extend(own);
        self.reserved_all = reserved;
    }

    /// Count one anchor for a target (pool + per-target shares).
    fn count(&mut self, target: &str, lower: &str, exact: bool) {
        *self
            .target_counts
            .entry(target.to_string())
            .or_default()
            .entry(lower.to_string())
            .or_insert(0) += 1;
        let te = self
            .target_exact
            .entry(target.to_string())
            .or_insert((0, 0));
        te.1 += 1;
        self.total += 1;
        if exact {
            te.0 += 1;
            self.exact += 1;
            self.add_exact(lower.to_string(), target);
        }
    }

    fn avoid_exact(&self, target: &str) -> bool {
        let site = self.total > 0 && (self.exact as f64 / self.total as f64) >= AVOID_EXACT_SHARE;
        let tgt = self
            .target_exact
            .get(target)
            .is_some_and(|&(e, t)| t >= 2 && (e as f64 / t as f64) >= TARGET_EXACT_SHARE);
        site || tgt
    }
}

struct Profiles {
    targets: HashMap<String, TargetProfile>,
    source_kw: HashMap<String, String>,
}

/// Context kept for an edge that will be sent to the AI.
struct AiItem {
    key: usize,
    source: String,
    target: String,
    paras: Vec<(i64, ParagraphNlp)>,
    candidates: Vec<String>,
    opts: PickOptions,
}

/// Generate anchors. Output order = input order (entries whose pick failed have an empty anchor).
#[allow(clippy::too_many_arguments)]
pub async fn generate(
    ctx: &JobContext,
    job: &Job,
    site: &SiteInfo,
    titles: &HashMap<String, String>,
    inputs: Vec<AnchorInput>,
    run: &AnchorRunOptions,
    phase: &str,
    pct: (f64, f64),
) -> JobResult<Vec<AnchorOutput>> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    let brand =
        brand_from_domain(&sf_engine::graph::configured_host(&site.domain).unwrap_or_default());
    let sid = site.id.clone();
    let (pool, committed, ignore) = ctx.db.read(move |c| load_pool(c, &sid)).await?;
    let t_terms = Instant::now();
    let terms: Option<Arc<SiteTerms>> = match site_terms(ctx, &site.id).await {
        Ok(t) => Some(t),
        Err(e) => {
            job.warn(format!(
                "anchors: site term statistics unavailable ({e}); uniform weights"
            ))
            .await;
            None
        }
    };
    let terms_ms = t_terms.elapsed().as_millis();
    let mut ids: Vec<String> = inputs
        .iter()
        .flat_map(|i| [i.source.clone(), i.target.clone()])
        .collect();
    ids.sort();
    ids.dedup();
    let titles2: HashMap<String, String> = ids
        .iter()
        .filter_map(|id| titles.get(id).map(|t| (id.clone(), t.clone())))
        .collect();
    let mut profiles = ctx
        .db
        .read(move |c| load_profiles(c, &ids, &titles2))
        .await?;
    if let Some(t) = &terms {
        for p in profiles.targets.values_mut() {
            p.apply_idf(t);
        }
    }
    let profiles = Arc::new(profiles);
    let ignore = Arc::new(ignore);

    // AI budget: top-N new edges (with candidate paragraphs) by composite score.
    let ai_keys: HashSet<usize> = match &run.ai {
        Some(o) if o.max_edges > 0 => {
            let mut v: Vec<(f64, usize)> = inputs
                .iter()
                .filter(|i| i.anchor.is_empty() && !i.paragraphs.is_empty())
                .map(|i| (i.score, i.key))
                .collect();
            v.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            v.into_iter().take(o.max_edges).map(|x| x.1).collect()
        }
        _ => HashSet::new(),
    };
    let ai_keys = Arc::new(ai_keys);

    // group inputs by source (stable: first appearance order of sources is the caller's order)
    let mut order: Vec<String> = Vec::new();
    let mut by_source: HashMap<String, Vec<AnchorInput>> = HashMap::new();
    for i in inputs {
        if !by_source.contains_key(&i.source) {
            order.push(i.source.clone());
        }
        by_source.entry(i.source.clone()).or_default().push(i);
    }
    let total_sources = order.len().max(1);
    let t_start = Instant::now();
    let (mut load_ms, mut pick_ms, mut sem_ms) = (0u128, 0u128, 0u128);
    let mut semantic_on = run.semantic && run.vectors.is_some();
    let mut sem_embedded = 0usize;
    let mut state = pool;
    let mut picked: Vec<Picked> = Vec::new();
    let mut para_ids: HashMap<String, HashMap<i64, String>> = HashMap::new();
    let mut ai_items: Vec<AiItem> = Vec::new();
    for (bi, batch) in order.chunks(SOURCE_BATCH).enumerate() {
        job.checkpoint().await?;
        let batch_ids: Vec<String> = batch.to_vec();
        let t_load = Instant::now();
        let contents = {
            let ids = batch_ids.clone();
            ctx.db.read(move |c| load_sources(c, &ids)).await?
        };
        load_ms += t_load.elapsed().as_millis();
        let t_pick = Instant::now();
        let items: Vec<(String, Vec<AnchorInput>)> = batch_ids
            .iter()
            .map(|s| (s.clone(), by_source.remove(s).unwrap_or_default()))
            .collect();
        // 1. split + NLP (parallel) + semantic candidate lists
        let (prof, ign, tm) = (profiles.clone(), ignore.clone(), terms.clone());
        let want_sem = semantic_on;
        let (prepared, wanted) =
            compute(move || prepare_batch(items, &contents, &prof, &ign, tm.as_deref(), want_sem))
                .await?;
        pick_ms += t_pick.elapsed().as_millis();
        // 2. embed missing candidate phrases (cached by text)
        let mut phrase_vecs: HashMap<String, Arc<[f32]>> = HashMap::new();
        if semantic_on && !wanted.is_empty() {
            let t_sem = Instant::now();
            let mut missing: Vec<String> = Vec::new();
            for w in wanted {
                match ctx.phrases.get(&w) {
                    Some(v) => {
                        phrase_vecs.insert(w, v);
                    }
                    None => missing.push(w),
                }
            }
            if !missing.is_empty() {
                match ctx.embed.embed(&missing).await {
                    Ok(vs) if vs.len() == missing.len() => {
                        sem_embedded += missing.len();
                        for (w, v) in missing.into_iter().zip(vs) {
                            ctx.phrases.insert(w.clone(), v.clone());
                            phrase_vecs.insert(w, Arc::from(v));
                        }
                    }
                    Ok(_) | Err(_) => {
                        semantic_on = false;
                        phrase_vecs.clear();
                        job.warn(
                            "anchors: phrase embedding failed; semantic scoring off for this plan (lexical anchors)",
                        )
                        .await;
                    }
                }
            }
            sem_ms += t_sem.elapsed().as_millis();
        }
        // 3. sequential picks
        let t_pick = Instant::now();
        let (prof, ign, br, keys) = (
            profiles.clone(),
            ignore.clone(),
            brand.clone(),
            ai_keys.clone(),
        );
        let vectors = run.vectors.clone().filter(|_| semantic_on);
        let min_sem = run
            .min_semantic
            .filter(|v| v.is_finite())
            .unwrap_or(sf_engine::anchor::MIN_PHRASE_SEMANTIC)
            .clamp(0.0, 1.0);
        let st = std::mem::take(&mut state);
        let (st, out, ids, ai) = compute(move || {
            pick_batch(
                prepared,
                &prof,
                &ign,
                &br,
                st,
                &phrase_vecs,
                vectors.as_deref(),
                &keys,
                min_sem,
            )
        })
        .await?;
        pick_ms += t_pick.elapsed().as_millis();
        state = st;
        picked.extend(out);
        para_ids.extend(ids);
        ai_items.extend(ai);
        let done = ((bi + 1) * SOURCE_BATCH).min(total_sources);
        job.progress(
            phase,
            pct.0 + (pct.1 - pct.0) * 0.8 * done as f64 / total_sources as f64,
        )
        .await;
    }

    // Guards over committed + new anchors.
    let mut guard_items: Vec<GuardItem> = committed;
    let first_new = guard_items.len();
    for (inp, loc, alts, _) in &picked {
        guard_items.push(GuardItem {
            anchor: loc.anchor.clone(),
            anchor_type: loc.anchor_type.clone(),
            is_new: inp.anchor.is_empty() && !loc.anchor.is_empty(),
            alternates: alts.iter().map(|(a, _)| a.clone()).collect(),
            target_id: inp.target.clone(),
        });
    }
    let t_guard = Instant::now();
    let guarded = compute(move || {
        let g = apply_over_optimization_guard(&guard_items);
        apply_diversity_guard(&g)
    })
    .await?;
    let guard_ms = t_guard.elapsed().as_millis();
    let mut out = Vec::with_capacity(picked.len());
    // One new native link per paragraph must survive the guards: a guard swap can move an edge
    // onto a paragraph another edge of the same source already claimed (pilot 2026-10-13 found
    // 5 paragraphs with two native suggestions). Applies to re-located existing suggestions too.
    let mut claimed: HashMap<String, HashSet<i64>> = HashMap::new();
    let mut sources: Vec<(String, String)> = Vec::with_capacity(picked.len());
    for (k, (inp, loc, alts, meta)) in picked.into_iter().enumerate() {
        let g = guarded.get(first_new + k);
        let swapped = match g {
            Some(g) if g.anchor != loc.anchor => alts
                .iter()
                .find(|(a, _)| a.anchor == g.anchor)
                .map(|(_, l)| l.clone()),
            _ => None,
        };
        let taken = claimed.entry(inp.source.clone()).or_default();
        let free = |l: &Located| l.paragraph_idx.is_none_or(|i| !taken.contains(&i));
        let was_swapped = swapped.is_some();
        let final_loc = match swapped {
            Some(s) if free(&s) => s,
            // The guard's anchor wins; it just can't be placed natively in a claimed paragraph.
            Some(s) => Located {
                in_paragraph: false,
                paragraph_idx: None,
                ..s
            },
            None if free(&loc) => loc,
            None => Located {
                in_paragraph: false,
                paragraph_idx: None,
                ..loc
            },
        };
        if let Some(i) = final_loc.paragraph_idx.filter(|_| final_loc.in_paragraph) {
            taken.insert(i);
        }
        let paragraph_id = final_loc
            .paragraph_idx
            .or_else(|| inp.paragraphs.first().copied())
            .and_then(|i| para_ids.get(&inp.source).and_then(|m| m.get(&i)).cloned());
        let meta = meta.map(|mut m| {
            if was_swapped && let Some(o) = m.as_object_mut() {
                o.insert("guard_swapped".into(), json!(true));
            }
            m
        });
        sources.push((inp.source.clone(), inp.target.clone()));
        out.push(AnchorOutput {
            key: inp.key,
            anchor: final_loc.anchor,
            anchor_type: final_loc.anchor_type,
            in_paragraph: final_loc.in_paragraph,
            paragraph_idx: final_loc.paragraph_idx,
            paragraph_id,
            meta,
            ai: None,
            score_factor: 1.0,
        });
    }
    job.info(format!(
        "anchors: {} edges from {total_sources} sources (terms {terms_ms} ms, load {load_ms} ms, split+nlp+pick {pick_ms} ms, semantic {sem_ms} ms / {sem_embedded} phrases embedded{}, guards {guard_ms} ms, total {} ms)",
        out.len(),
        if run.semantic && !semantic_on { " (off)" } else { "" },
        t_start.elapsed().as_millis()
    ))
    .await;

    if run.ai.is_some() && !ai_items.is_empty() {
        let opts = run.ai.clone().unwrap_or_default();
        refine_with_ai(
            ctx,
            job,
            site,
            &profiles,
            terms.as_deref(),
            &brand,
            &para_ids,
            ai_items,
            &mut out,
            &mut claimed,
            &opts,
        )
        .await;
    }
    job.progress(phase, pct.1).await;
    Ok(out)
}

/// `(input, located, alternates, meta)`.
type Picked = (
    AnchorInput,
    Located,
    Vec<(AnchorAlt, Located)>,
    Option<Value>,
);
type ParaIds = HashMap<String, HashMap<i64, String>>;
type SourceNlp = HashMap<i64, ParagraphNlp>;
type Prepared = (String, Vec<AnchorInput>, SourceNlp, HashMap<i64, String>);

fn base_options(ignore: &[String], source_kw: Option<String>) -> PickOptions {
    let mut opts = PickOptions::new();
    opts.ignore = ignore.to_vec();
    opts.source_keyword = source_kw;
    opts
}

/// Paragraph idx values tried for an edge (inputs without candidates use the first ones).
fn tops_of(inp: &AnchorInput, nlp: &SourceNlp) -> Vec<i64> {
    if inp.paragraphs.is_empty() {
        let mut v: Vec<i64> = nlp.keys().copied().collect();
        v.sort_unstable();
        v.truncate(TOP_PARAGRAPHS);
        v
    } else {
        inp.paragraphs
            .iter()
            .take(TOP_PARAGRAPHS)
            .copied()
            .collect()
    }
}

/// Split + NLP every source of the batch (parallel, order kept) and collect the phrases to embed.
fn prepare_batch(
    items: Vec<(String, Vec<AnchorInput>)>,
    contents: &HashMap<String, SourceContent>,
    prof: &Profiles,
    ignore: &[String],
    terms: Option<&SiteTerms>,
    semantic: bool,
) -> (Vec<Prepared>, Vec<String>) {
    use rayon::prelude::*;
    let prepared: Vec<(Prepared, Vec<String>)> = items
        .into_par_iter()
        .map(|(source, inputs)| {
            let (nlp, para_ids) = prepare_source(contents.get(&source), &inputs, terms);
            let mut want: Vec<String> = Vec::new();
            if semantic {
                let opts = base_options(ignore, prof.source_kw.get(&source).cloned());
                for inp in inputs.iter().filter(|i| i.anchor.is_empty()) {
                    let Some(target) = prof.targets.get(&inp.target) else {
                        continue;
                    };
                    let tops = tops_of(inp, &nlp);
                    let paras: Vec<(usize, &ParagraphNlp)> = tops
                        .iter()
                        .filter_map(|i| nlp.get(i).map(|p| (usize::try_from(*i).unwrap_or(0), p)))
                        .collect();
                    want.extend(top_phrases(&paras, target, &opts, SEMANTIC_TOP_K));
                }
            }
            ((source, inputs, nlp, para_ids), want)
        })
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut wanted = Vec::new();
    let mut out = Vec::with_capacity(prepared.len());
    for (p, want) in prepared {
        for w in want {
            if seen.insert(w.clone()) {
                wanted.push(w);
            }
        }
        out.push(p);
    }
    (out, wanted)
}

#[allow(clippy::too_many_arguments)]
fn pick_batch(
    prepared: Vec<Prepared>,
    prof: &Profiles,
    ignore: &[String],
    brand: &str,
    mut st: PoolState,
    phrase_vecs: &HashMap<String, Arc<[f32]>>,
    vectors: Option<&SiteVectors>,
    ai_keys: &HashSet<usize>,
    min_semantic: f32,
) -> (PoolState, Vec<Picked>, ParaIds, Vec<AiItem>) {
    let mut out = Vec::new();
    let mut all_ids: ParaIds = HashMap::new();
    let mut ai_items = Vec::new();
    for (source, inputs, nlp, para_ids) in prepared {
        let source_kw = prof.source_kw.get(&source).cloned();
        // At most one new link per paragraph (accuracy C.8): existing suggestions claim their
        // paragraph first, then new picks only consider paragraphs that are still free.
        let mut used_paras: HashSet<i64> = HashSet::new();
        let mut inputs = inputs;
        inputs.sort_by_key(|i| i.anchor.is_empty());
        for inp in inputs {
            let tops = tops_of(&inp, &nlp);
            // Existing suggestions are re-located among free paragraphs too, so a re-plan never
            // turns two of them into native links in one paragraph (pilot 2026-10-13).
            let paras: Vec<(usize, &ParagraphNlp)> = tops
                .iter()
                .filter(|i| !used_paras.contains(i))
                .filter_map(|i| nlp.get(i).map(|p| (usize::try_from(*i).unwrap_or(0), p)))
                .collect();
            let Some(target) = prof.targets.get(&inp.target) else {
                out.push((inp, empty_loc(), Vec::new(), None));
                continue;
            };
            let top_kw = target.top_keyword.clone();
            let locate = |anchor: &str| -> Located {
                let hit = paras
                    .iter()
                    .find(|(_, p)| find_unblocked(&p.text, anchor, &p.blocked).is_some());
                Located {
                    anchor: anchor.to_string(),
                    anchor_type: classify_anchor(anchor, top_kw.as_deref(), brand),
                    in_paragraph: hit.is_some(),
                    paragraph_idx: hit.map(|(i, _)| *i as i64),
                }
            };
            if !inp.anchor.is_empty() {
                // Existing suggestion: only re-evaluate placement.
                let loc = locate(&inp.anchor);
                if let Some(i) = loc.paragraph_idx {
                    used_paras.insert(i);
                }
                out.push((inp, loc, Vec::new(), None));
                continue;
            }
            let used = st.used_in_source.entry(source.clone()).or_default().clone();
            let (reserved, own_exact) = st.take_reserved(&inp.target);
            let mut opts = base_options(ignore, source_kw.clone());
            opts.min_semantic = min_semantic;
            opts.used_in_source = used;
            opts.reserved_exact = reserved;
            opts.target_anchor_counts = st
                .target_counts
                .get(&inp.target)
                .cloned()
                .unwrap_or_default();
            opts.avoid_exact = st.avoid_exact(&inp.target);
            // semantic: cosine of each embedded candidate phrase to the target post vector
            let tvec = vectors.and_then(|v| {
                v.posts
                    .position(&inp.target)
                    .and_then(|i| v.posts.vector(i))
            });
            if let Some(tv) = tvec {
                for (_, p) in &paras {
                    for ph in &p.phrases {
                        let l = ph.text.to_lowercase();
                        if let Some(pv) = phrase_vecs.get(&l)
                            && !opts.semantic.contains_key(&l)
                        {
                            let c = sf_engine::vector::dot(pv, tv);
                            opts.semantic.insert(l, c);
                        }
                    }
                }
            }
            let pick = pick_anchor_across(&paras, target, &opts);
            let ai_item = ai_keys.contains(&inp.key).then(|| {
                let mut o = opts.clone();
                o.semantic = HashMap::new();
                AiItem {
                    key: inp.key,
                    source: source.clone(),
                    target: inp.target.clone(),
                    paras: tops
                        .iter()
                        .filter_map(|i| nlp.get(i).map(|p| (*i, p.clone())))
                        .take(AI_PARAGRAPHS)
                        .collect(),
                    candidates: top_phrases(&paras, target, &opts, 5),
                    opts: o,
                }
            });
            st.restore_reserved(std::mem::take(&mut opts.reserved_exact), own_exact);
            let anchor = pick.anchor.trim().to_string();
            let lower = anchor.to_lowercase();
            if anchor.is_empty() || opts.is_ignored(&lower) || opts.used_in_source.contains(&lower)
            {
                out.push((inp, empty_loc(), Vec::new(), None));
                continue;
            }
            let loc = Located {
                anchor_type: classify_anchor(&anchor, top_kw.as_deref(), brand),
                in_paragraph: pick.in_paragraph,
                paragraph_idx: pick
                    .paragraph_idx
                    .map(|i| i as i64)
                    .filter(|_| pick.in_paragraph),
                anchor: anchor.clone(),
            };
            let mut meta = serde_json::Map::new();
            meta.insert(
                "source".into(),
                json!(match pick.source {
                    PickSource::Phrase => "phrase",
                    PickSource::Keyword => "keyword",
                    PickSource::Title => "title",
                    PickSource::CleanTitle => "clean_title",
                }),
            );
            if let Some(c) = opts.semantic.get(&lower) {
                meta.insert(
                    "semantic".into(),
                    json!((f64::from(*c) * 1000.0).round() / 1000.0),
                );
            }
            let alts: Vec<(AnchorAlt, Located)> = pick
                .alternates
                .iter()
                .map(|a| a.trim())
                .filter(|a| !a.is_empty() && *a != anchor)
                .filter(|a| {
                    let l = a.to_lowercase();
                    !opts.is_ignored(&l) && !opts.used_in_source.contains(&l)
                })
                .map(|a| {
                    let l = locate(a);
                    (
                        AnchorAlt {
                            anchor: l.anchor.clone(),
                            anchor_type: l.anchor_type.clone(),
                        },
                        l,
                    )
                })
                .collect();
            // update running state
            st.used_in_source
                .entry(source.clone())
                .or_default()
                .insert(lower.clone());
            st.count(&inp.target, &lower, loc.anchor_type == AnchorType::Exact);
            if let Some(i) = loc.paragraph_idx {
                used_paras.insert(i);
            }
            if let Some(item) = ai_item {
                ai_items.push(item);
            }
            out.push((inp, loc, alts, Some(Value::Object(meta))));
        }
        all_ids.insert(source, para_ids);
    }
    (st, out, all_ids, ai_items)
}

/// Re-split one source post and analyse the paragraphs its edges may use. Only paragraphs whose
/// hash still matches the stored row are kept (idx → paragraph id for all of them).
fn prepare_source(
    content: Option<&SourceContent>,
    inputs: &[AnchorInput],
    terms: Option<&SiteTerms>,
) -> (SourceNlp, HashMap<i64, String>) {
    let mut nlp: SourceNlp = HashMap::new();
    let mut para_ids: HashMap<i64, String> = HashMap::new();
    let Some(c) = content else {
        return (nlp, para_ids);
    };
    let split: Vec<Paragraph> = html::paragraphs(&c.source, SPLIT_MODE).unwrap_or_default();
    let wanted: HashSet<i64> = inputs
        .iter()
        .flat_map(|i| i.paragraphs.iter().copied())
        .collect();
    let all_wanted = inputs.iter().any(|i| i.paragraphs.is_empty());
    for p in &split {
        let idx = p.index as i64;
        let Some((pid, hash)) = c.rows.get(&idx) else {
            continue;
        };
        if *hash != p.hash {
            continue;
        }
        para_ids.insert(idx, pid.clone());
        if wanted.contains(&idx) || (all_wanted && idx < TOP_PARAGRAPHS as i64) {
            nlp.insert(idx, ParagraphNlp::from_paragraph_with(p, terms));
        }
    }
    (nlp, para_ids)
}

fn empty_loc() -> Located {
    Located {
        anchor: String::new(),
        anchor_type: AnchorType::Partial,
        in_paragraph: false,
        paragraph_idx: None,
    }
}

// ---------------------------------------------------------------------------------------------
// AI refinement
// ---------------------------------------------------------------------------------------------

struct AiCall {
    key: usize,
    para_idx: i64,
    messages: Vec<ChatMessage>,
}

#[allow(clippy::too_many_arguments)]
async fn refine_with_ai(
    ctx: &JobContext,
    job: &Job,
    site: &SiteInfo,
    profiles: &Profiles,
    terms: Option<&SiteTerms>,
    brand: &str,
    para_ids: &ParaIds,
    items: Vec<AiItem>,
    out: &mut [AnchorOutput],
    claimed: &mut HashMap<String, HashSet<i64>>,
    opts: &AiRefineOptions,
) {
    let t0 = Instant::now();
    let fallback = |reason: &str| json!({"fallback_used": true, "error": reason});
    let pos: HashMap<usize, usize> = out.iter().enumerate().map(|(i, o)| (o.key, i)).collect();
    let Some(ai) = ctx.ai.clone() else {
        job.warn(format!(
            "anchors: AI refinement requested for {} edges but no AI is configured; heuristic anchors kept",
            items.len()
        ))
        .await;
        for it in &items {
            if let Some(o) = pos.get(&it.key).and_then(|&i| out.get_mut(i)) {
                o.ai = Some(fallback("ai not configured"));
            }
        }
        return;
    };
    // Build one call per edge: the edge's own paragraph when it is native, else the first
    // candidate paragraph no other native link of the source claimed.
    let mut calls: Vec<AiCall> = Vec::new();
    let mut no_place: Vec<usize> = Vec::new();
    for it in &items {
        let Some(o) = pos.get(&it.key).and_then(|&i| out.get(i)) else {
            continue;
        };
        if o.anchor.is_empty() {
            continue;
        }
        let taken = claimed.get(&it.source);
        let para = if o.in_paragraph {
            o.paragraph_idx
                .and_then(|i| it.paras.iter().find(|(pi, _)| *pi == i))
        } else {
            it.paras
                .iter()
                .find(|(pi, _)| taken.is_none_or(|t| !t.contains(pi)))
        };
        let Some((pi, p)) = para else {
            no_place.push(it.key);
            continue;
        };
        let Some(target) = profiles.targets.get(&it.target) else {
            continue;
        };
        let mut cands = it.candidates.clone();
        if !cands.iter().any(|c| c.eq_ignore_ascii_case(&o.anchor)) {
            cands.insert(0, o.anchor.clone());
        }
        let (sys, user) = refine_prompt(&p.text, &target.title, &target.keywords, &cands);
        calls.push(AiCall {
            key: it.key,
            para_idx: *pi,
            messages: vec![ChatMessage::system(sys), ChatMessage::user(user)],
        });
    }
    for k in no_place {
        if let Some(o) = pos.get(&k).and_then(|&i| out.get_mut(i)) {
            o.ai = Some(fallback("no free paragraph"));
        }
    }
    let asked = calls.len();
    let deadline = tokio::time::Instant::now() + opts.timeout;
    let site_id = site.id.clone();
    let mut stream = futures::stream::iter(calls.into_iter().map(|c| {
        let ai = ai.clone();
        let site_id = site_id.clone();
        async move {
            let req = ChatRequest {
                messages: c.messages,
                max_tokens: 300,
                temperature: 0.0,
                site_id: Some(site_id),
                json_mode: true,
            };
            let r = ai.chat_json::<RefineReply>(req).await;
            (c.key, c.para_idx, r)
        }
    }))
    .buffer_unordered(opts.concurrency.clamp(1, 16));
    let mut results: Vec<(usize, i64, Result<(RefineReply, String), String>)> = Vec::new();
    let (mut ok_any, mut consecutive_fail, mut stop) = (false, 0usize, None::<&str>);
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some((key, pi, r))) => {
                match r {
                    Ok((reply, resp)) => {
                        ok_any = true;
                        consecutive_fail = 0;
                        results.push((key, pi, Ok((reply, resp.model))));
                    }
                    Err(e) => {
                        consecutive_fail += 1;
                        results.push((key, pi, Err(e.to_string())));
                        if !ok_any && consecutive_fail >= AI_CIRCUIT {
                            stop = Some("ai unavailable");
                            break;
                        }
                    }
                }
                if job.cancel_requested() {
                    stop = Some("cancelled");
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {
                stop = Some("ai timeout");
                break;
            }
        }
    }
    drop(stream);
    // Apply in key order (deterministic).
    results.sort_by_key(|r| r.0);
    let answered: HashSet<usize> = results.iter().map(|r| r.0).collect();
    let mut used: HashMap<String, HashSet<String>> = HashMap::new();
    for o in out.iter() {
        if let Some(it) = items.iter().find(|i| i.key == o.key) {
            used.entry(it.source.clone())
                .or_default()
                .insert(o.anchor.to_lowercase());
        }
    }
    let (mut changed, mut kept, mut errors, mut low) = (0usize, 0usize, 0usize, 0usize);
    let mut first_error: Option<String> = None;
    for (key, pi, r) in results {
        let Some(it) = items.iter().find(|i| i.key == key) else {
            continue;
        };
        let Some(o) = pos.get(&key).and_then(|&i| out.get_mut(i)) else {
            continue;
        };
        let (reply, model) = match r {
            Ok(x) => x,
            Err(e) => {
                errors += 1;
                if first_error.is_none() {
                    first_error = Some(e.chars().take(200).collect());
                }
                o.ai = Some(fallback("ai call failed"));
                continue;
            }
        };
        o.score_factor = relevance_factor(reply.relevance);
        if reply
            .relevance
            .is_some_and(|r| r < sf_engine::anchor::refine::LOW_RELEVANCE)
        {
            low += 1;
        }
        let reason: String = reply.reason.chars().take(200).collect();
        let current = o.anchor.to_lowercase();
        let (Some(target), Some((_, p))) = (
            profiles.targets.get(&it.target),
            it.paras.iter().find(|(i, _)| *i == pi),
        ) else {
            continue;
        };
        let mut vopts = it.opts.clone();
        vopts.used_in_source = used.get(&it.source).cloned().unwrap_or_default();
        vopts.used_in_source.remove(&current);
        let verdict = validate_refined(&reply.anchor, p, target, &vopts, terms);
        let mut fallback_used = true;
        let mut reject: Option<&str> = None;
        match verdict {
            Ok((surface, _)) => {
                let lower = surface.to_lowercase();
                let same_place = o.in_paragraph && o.paragraph_idx == Some(pi);
                let free = same_place || claimed.get(&it.source).is_none_or(|t| !t.contains(&pi));
                if lower == current && same_place {
                    fallback_used = false; // the model agrees with the heuristic
                } else if !free {
                    reject = Some("paragraph_taken");
                } else {
                    let top_kw = target.top_keyword.clone();
                    let taken = claimed.entry(it.source.clone()).or_default();
                    if o.in_paragraph
                        && let Some(old) = o.paragraph_idx
                    {
                        taken.remove(&old);
                    }
                    taken.insert(pi);
                    let u = used.entry(it.source.clone()).or_default();
                    u.remove(&current);
                    u.insert(lower);
                    o.anchor_type = classify_anchor(&surface, top_kw.as_deref(), brand);
                    o.anchor = surface;
                    o.in_paragraph = true;
                    o.paragraph_idx = Some(pi);
                    o.paragraph_id = para_ids.get(&it.source).and_then(|m| m.get(&pi)).cloned();
                    fallback_used = false;
                    changed += 1;
                }
            }
            Err(r) => reject = Some(r.as_str()),
        }
        if fallback_used {
            kept += 1;
        }
        let mut ai_meta = json!({
            "anchor": reply.anchor.chars().take(120).collect::<String>(),
            "relevance": reply.relevance,
            "reason": reason,
            "model": model,
            "fallback_used": fallback_used,
        });
        if let (Some(r), Some(m)) = (reject, ai_meta.as_object_mut()) {
            m.insert("reject".into(), json!(r));
        }
        o.ai = Some(ai_meta);
    }
    for it in &items {
        if !answered.contains(&it.key)
            && let Some(o) = pos.get(&it.key).and_then(|&i| out.get_mut(i))
            && o.ai.is_none()
        {
            o.ai = Some(fallback(stop.unwrap_or("not asked")));
        }
    }
    let msg = format!(
        "ai anchors: {asked} asked, {changed} changed, {kept} kept the heuristic anchor, {low} rated low relevance, {errors} errors{} ({} ms)",
        stop.map(|s| format!(", stopped: {s}")).unwrap_or_default(),
        t0.elapsed().as_millis()
    );
    if errors > 0 || stop.is_some() {
        job.warn(format!(
            "{msg}; first error: {}",
            first_error.unwrap_or_default()
        ))
        .await;
    } else {
        job.info(msg).await;
    }
}

// ---------------------------------------------------------------------------------------------
// Site term statistics
// ---------------------------------------------------------------------------------------------

/// IDF + proper-noun statistics of a site's post texts, cached per content fingerprint.
pub async fn site_terms(ctx: &JobContext, site_id: &str) -> JobResult<Arc<SiteTerms>> {
    let sid = site_id.to_string();
    let fp: String = ctx
        .db
        .read(move |c| {
            c.query_row(
                "SELECT count(*), coalesce(max(crawled_at), ''), total(word_count), coalesce(max(id), '')
                 FROM posts WHERE site_id=?1",
                [&sid],
                |r| {
                    Ok(format!(
                        "{}|{}|{}|{}",
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1).unwrap_or_default(),
                        r.get::<_, f64>(2)?,
                        r.get::<_, String>(3)?
                    ))
                },
            )
            .map_err(err)
        })
        .await?;
    if let Some(t) = ctx.terms.get(site_id, &fp) {
        return Ok(t);
    }
    let mut builder = TermsBuilder::new();
    let mut after = String::new();
    loop {
        let (sid, a) = (site_id.to_string(), after.clone());
        let rows: Vec<(String, String)> = ctx
            .db
            .read(move |c| {
                let mut st = c
                    .prepare(
                        "SELECT id, content_text FROM posts WHERE site_id=?1 AND id > ?2 ORDER BY id LIMIT ?3",
                    )
                    .map_err(err)?;
                let v = st
                    .query_map(params![sid, a, TERMS_CHUNK], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map_err(err)?
                    .filter_map(|r| r.ok())
                    .collect();
                Ok(v)
            })
            .await?;
        let Some(last) = rows.last() else { break };
        after = last.0.clone();
        let b = std::mem::take(&mut builder);
        builder = compute(move || {
            use rayon::prelude::*;
            let part = rows
                .par_iter()
                .fold(TermsBuilder::new, |mut b, (_, t)| {
                    b.add_document(t);
                    b
                })
                .reduce(TermsBuilder::new, TermsBuilder::merge);
            b.merge(part)
        })
        .await?;
    }
    let terms = Arc::new(builder.finish());
    ctx.terms.put(site_id, fp, terms.clone());
    Ok(terms)
}

/// Committed anchor pool (suggested/approved/applied, excluding existing body links) → running
/// state + guard items + ignore phrases.
fn load_pool(
    c: &rusqlite::Connection,
    site_id: &str,
) -> sf_core::Result<(PoolState, Vec<GuardItem>, Vec<String>)> {
    let mut st = PoolState::default();
    let mut items = Vec::new();
    let mut q = c
        .prepare(
            "SELECT source_post_id, target_post_id, anchor, anchor_type, origin FROM link_edges
             WHERE site_id=?1 AND status IN ('suggested','approved','applied') AND anchor <> ''
             ORDER BY id",
        )
        .map_err(err)?;
    let rows = q
        .query_map([site_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })
        .map_err(err)?;
    for row in rows {
        let (s, t, a, ty, origin) = row.map_err(err)?;
        let lower = a.trim().to_lowercase();
        st.used_in_source
            .entry(s)
            .or_default()
            .insert(lower.clone());
        if origin == "existing" {
            continue; // body links never feed the anchor guards (B-10)
        }
        let ty = AnchorType::parse(&ty);
        st.count(&t, &lower, ty == AnchorType::Exact);
        items.push(GuardItem {
            anchor: a,
            anchor_type: ty,
            is_new: false,
            alternates: Vec::new(),
            target_id: t,
        });
    }
    let mut q = c
        .prepare("SELECT phrase FROM ignore_keywords WHERE site_id=?1 ORDER BY phrase")
        .map_err(err)?;
    let ignore = q
        .query_map([site_id], |r| r.get::<_, String>(0))
        .map_err(err)?
        .filter_map(|r| r.ok())
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    Ok((st, items, ignore))
}

fn load_profiles(
    c: &rusqlite::Connection,
    ids: &[String],
    titles: &HashMap<String, String>,
) -> sf_core::Result<Profiles> {
    let mut kws: HashMap<String, Vec<(String, f64)>> = HashMap::new();
    let mut striking: HashMap<String, Vec<(String, f64)>> = HashMap::new();
    for chunk in ids.chunks(500) {
        let sql = format!(
            "SELECT post_id, keyword, score, source, striking FROM keywords WHERE post_id IN ({})",
            placeholders(1, chunk.len())
        );
        let mut q = c.prepare(&sql).map_err(err)?;
        let rows = q
            .query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .map_err(err)?;
        for row in rows {
            let (pid, kw, score, source, strike) = row.map_err(err)?;
            if source == "gsc" {
                if strike != 0 {
                    striking.entry(pid).or_default().push((kw, STRIKING_WEIGHT));
                }
            } else {
                kws.entry(pid).or_default().push((kw, score));
            }
        }
    }
    let mut targets = HashMap::new();
    let mut source_kw = HashMap::new();
    for id in ids {
        let mut list = kws.remove(id).unwrap_or_default();
        // best first: score desc, more words first, then lexical (deterministic, C.2)
        list.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| {
                    b.0.split_whitespace()
                        .count()
                        .cmp(&a.0.split_whitespace().count())
                })
                .then_with(|| a.0.cmp(&b.0))
        });
        list.dedup_by(|a, b| a.0 == b.0);
        let words: Vec<String> = list.iter().take(10).map(|k| k.0.clone()).collect();
        if let Some(top) = words.first() {
            source_kw.insert(id.clone(), top.to_lowercase());
        }
        let weighted = striking.remove(id).unwrap_or_default();
        let title = titles.get(id).map(String::as_str).unwrap_or("");
        targets.insert(id.clone(), TargetProfile::new(title, &words, &weighted));
    }
    Ok(Profiles { targets, source_kw })
}

struct SourceContent {
    source: String,
    /// idx → (paragraph id, hash)
    rows: HashMap<i64, (String, String)>,
}

fn load_sources(
    c: &rusqlite::Connection,
    ids: &[String],
) -> sf_core::Result<HashMap<String, SourceContent>> {
    let mut out = HashMap::new();
    let mut q = c
        .prepare("SELECT content_html, content_raw FROM posts WHERE id=?1")
        .map_err(err)?;
    let mut pq = c
        .prepare("SELECT id, idx, hash FROM paragraphs WHERE post_id=?1")
        .map_err(err)?;
    for id in ids {
        let row: Option<(String, Option<String>)> = q
            .query_row(params![id], |r| Ok((r.get(0)?, r.get(1)?)))
            .ok();
        let Some((rendered, raw)) = row else { continue };
        let source = match raw {
            Some(r) if !r.trim().is_empty() => r,
            _ => rendered,
        };
        let rows = pq
            .query_map(params![id], |r| {
                Ok((
                    r.get::<_, i64>(1)?,
                    (r.get::<_, String>(0)?, r.get::<_, String>(2)?),
                ))
            })
            .map_err(err)?
            .filter_map(|r| r.ok())
            .collect();
        out.insert(id.clone(), SourceContent { source, rows });
    }
    Ok(out)
}
