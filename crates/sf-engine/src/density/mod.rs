//! Link density rules (spec §8, v1 `rules/density.ts`), applied **after** scoring.
//!
//! v1 ran `applyDensityCaps` before `scoreCandidates`, so every new candidate had score 0 and
//! "drop the lowest score" degenerated to dropping by random UUID (finding #7). The v2 pipeline
//! must call [`apply_density`] on *scored* candidates. Selection is one greedy pass in
//! (score desc, id asc, input order) order: a candidate is kept while its source still has
//! budget `min(max_per_post − existing, max_new_per_pass)` and — when
//! `max_new_inbound_per_target` is set — its target still has inbound budget (finding #44).
//! Money-page targets are exempt from the inbound cap so they can collect their links goal.
//! For the per-source rule alone this equals v1 `capCandidatesPerSource` (top-budget by score).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// Effective, clamped density limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DensityLimits {
    /// Hard ceiling of contextual internal links per post (D2).
    pub max_per_post: usize,
    /// Floor below which a post is reported under-linked (D2).
    pub min_per_post: usize,
    /// New links added to one post per pass (T8 SOP).
    pub max_new_per_pass: usize,
    /// Total outbound links above which a page is overloaded (D4).
    pub overload_threshold: usize,
    /// Optional per-target cap of new inbound links per pass (None = unlimited, v1 behaviour).
    pub max_new_inbound_per_target: Option<usize>,
}

/// v1 `LINK_DENSITY` defaults.
pub const LINK_DENSITY: DensityLimits = DensityLimits {
    max_per_post: 10,
    min_per_post: 3,
    max_new_per_pass: 5,
    overload_threshold: 100,
    max_new_inbound_per_target: None,
};

impl Default for DensityLimits {
    fn default() -> Self {
        LINK_DENSITY
    }
}

/// Raw site settings (`max_links_per_post`, … as stored in `sites.settings`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DensitySettings {
    pub max_links_per_post: Option<f64>,
    pub min_links_per_post: Option<f64>,
    pub max_new_links_per_pass: Option<f64>,
    pub link_overload_threshold: Option<f64>,
    /// v2 setting; absent = no inbound cap.
    pub max_new_inbound_per_target: Option<f64>,
}

fn clamp_int(v: Option<f64>, min: usize, max: usize, fallback: usize) -> usize {
    match v {
        Some(x) if x.is_finite() => {
            let t = x.trunc();
            if t <= min as f64 {
                min
            } else if t >= max as f64 {
                max
            } else {
                t as usize
            }
        }
        _ => fallback,
    }
}

/// v1 `resolveLimits`: merge over defaults, trunc + clamp to [1,200], [0,200], [1,200],
/// [1,10000]; `min ≤ max`, `max_new ≤ max`. Inbound cap clamped to [1,10000] when set.
pub fn resolve_limits(s: &DensitySettings) -> DensityLimits {
    let d = LINK_DENSITY;
    let max_per_post = clamp_int(s.max_links_per_post, 1, 200, d.max_per_post);
    let min_per_post = clamp_int(s.min_links_per_post, 0, 200, d.min_per_post).min(max_per_post);
    let max_new_per_pass =
        clamp_int(s.max_new_links_per_pass, 1, 200, d.max_new_per_pass).min(max_per_post);
    let overload_threshold = clamp_int(s.link_overload_threshold, 1, 10_000, d.overload_threshold);
    let max_new_inbound_per_target = s
        .max_new_inbound_per_target
        .filter(|x| x.is_finite())
        .map(|x| clamp_int(Some(x), 1, 10_000, 1));
    DensityLimits {
        max_per_post,
        min_per_post,
        max_new_per_pass,
        overload_threshold,
        max_new_inbound_per_target,
    }
}

/// What a candidate must expose to be density-capped.
pub trait DensityItem {
    fn id(&self) -> &str;
    fn source(&self) -> &str;
    fn target(&self) -> &str;
    /// Composite score; NaN ranks last.
    fn score(&self) -> f64;
}

/// A minimal owned candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub id: String,
    pub source: String,
    pub target: String,
    pub score: f64,
}

impl DensityItem for Candidate {
    fn id(&self) -> &str {
        &self.id
    }
    fn source(&self) -> &str {
        &self.source
    }
    fn target(&self) -> &str {
        &self.target
    }
    fn score(&self) -> f64 {
        self.score
    }
}

/// Kept and dropped candidates, both in input order.
#[derive(Debug, Clone, PartialEq)]
pub struct CapResult<T> {
    pub kept: Vec<T>,
    pub dropped: Vec<T>,
}

/// Context for [`apply_density`].
#[derive(Debug, Clone, Default)]
pub struct DensityContext {
    /// Committed outbound links per source (status applied/approved OR origin existing).
    pub existing_outbound: HashMap<String, usize>,
    /// Money-page post ids (exempt from the inbound cap).
    pub money_pages: HashSet<String>,
}

/// Apply the density rules to scored candidates (see module docs). Deterministic.
pub fn apply_density<T: DensityItem>(
    candidates: Vec<T>,
    ctx: &DensityContext,
    limits: &DensityLimits,
) -> CapResult<T> {
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    let key = |i: usize| {
        candidates.get(i).map(|c| {
            (
                if c.score().is_nan() {
                    f64::NEG_INFINITY
                } else {
                    c.score()
                },
                c.id(),
            )
        })
    };
    order.sort_by(|&a, &b| match (key(a), key(b)) {
        (Some((sa, ia)), Some((sb, ib))) => {
            sb.total_cmp(&sa).then_with(|| ia.cmp(ib)).then(a.cmp(&b))
        }
        _ => a.cmp(&b),
    });
    let mut src_budget: HashMap<&str, usize> = HashMap::new();
    let mut tgt_used: HashMap<&str, usize> = HashMap::new();
    let mut keep = vec![false; candidates.len()];
    for i in order {
        let Some(c) = candidates.get(i) else { continue };
        let budget = src_budget.entry(c.source()).or_insert_with(|| {
            let existing = ctx.existing_outbound.get(c.source()).copied().unwrap_or(0);
            limits
                .max_per_post
                .saturating_sub(existing)
                .min(limits.max_new_per_pass)
        });
        if *budget == 0 {
            continue;
        }
        if let Some(cap) = limits.max_new_inbound_per_target
            && !ctx.money_pages.contains(c.target())
        {
            let used = tgt_used.entry(c.target()).or_insert(0);
            if *used >= cap {
                continue;
            }
            *used += 1;
        }
        *budget -= 1;
        if let Some(k) = keep.get_mut(i) {
            *k = true;
        }
    }
    let mut out = CapResult {
        kept: Vec::new(),
        dropped: Vec::new(),
    };
    for (c, k) in candidates.into_iter().zip(keep) {
        if k {
            out.kept.push(c);
        } else {
            out.dropped.push(c);
        }
    }
    out
}

/// A post's link counts for the under-linked / overloaded reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PostLinkCount {
    pub post_id: String,
    pub url: String,
    pub title: String,
    pub post_status: String,
    pub post_type: String,
    /// Committed outbound (applied/approved OR existing).
    pub committed_outbound: usize,
    /// `posts.outbound_link_count` rollup (None = link health never ran).
    pub outbound_link_count: Option<usize>,
}

/// v1 `findUnderLinkedPosts`: published non-archive posts with committed outbound < min,
/// ordered by count asc, then url.
pub fn find_under_linked<'a>(
    posts: &'a [PostLinkCount],
    limits: &DensityLimits,
) -> Vec<&'a PostLinkCount> {
    let mut v: Vec<&PostLinkCount> = posts
        .iter()
        .filter(|p| p.post_status == "publish" && p.post_type != "archive")
        .filter(|p| p.committed_outbound < limits.min_per_post)
        .collect();
    v.sort_by(|a, b| {
        a.committed_outbound
            .cmp(&b.committed_outbound)
            .then_with(|| a.url.cmp(&b.url))
    });
    v
}

/// v1 `findOverloadedPosts`: outbound rollup > threshold, ordered by count desc, then url.
pub fn find_overloaded<'a>(
    posts: &'a [PostLinkCount],
    limits: &DensityLimits,
) -> Vec<&'a PostLinkCount> {
    let mut v: Vec<&PostLinkCount> = posts
        .iter()
        .filter(|p| p.outbound_link_count.unwrap_or(0) > limits.overload_threshold)
        .collect();
    v.sort_by(|a, b| {
        b.outbound_link_count
            .cmp(&a.outbound_link_count)
            .then_with(|| a.url.cmp(&b.url))
    });
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(source: &str, score: f64, id: &str) -> Candidate {
        Candidate {
            id: id.into(),
            source: source.into(),
            target: format!("t-{id}"),
            score,
        }
    }
    fn c(source: &str, score: f64) -> Candidate {
        cand(source, score, &format!("{source}:{score}"))
    }
    fn ctx(existing: &[(&str, usize)]) -> DensityContext {
        DensityContext {
            existing_outbound: existing
                .iter()
                .map(|(k, v)| ((*k).to_string(), *v))
                .collect(),
            ..Default::default()
        }
    }
    fn lim(max_new: usize) -> DensityLimits {
        DensityLimits {
            max_new_per_pass: max_new,
            ..LINK_DENSITY
        }
    }
    fn scores(v: &[Candidate]) -> Vec<f64> {
        v.iter().map(|c| c.score).collect()
    }

    // ---- ported from v1 test/density.test.ts ----

    #[test]
    fn resolve_limits_like_v1() {
        assert_eq!(resolve_limits(&DensitySettings::default()), LINK_DENSITY);
        let s = |f: fn(&mut DensitySettings)| {
            let mut d = DensitySettings::default();
            f(&mut d);
            resolve_limits(&d)
        };
        let r = s(|d| {
            d.max_links_per_post = Some(8.0);
            d.min_links_per_post = Some(2.0);
            d.max_new_links_per_pass = Some(3.0);
            d.link_overload_threshold = Some(150.0);
        });
        assert_eq!(
            (
                r.max_per_post,
                r.min_per_post,
                r.max_new_per_pass,
                r.overload_threshold
            ),
            (8, 2, 3, 150)
        );
        assert_eq!(s(|d| d.max_links_per_post = Some(20.0)).max_per_post, 20);
        assert_eq!(s(|d| d.max_links_per_post = Some(0.0)).max_per_post, 1);
        assert_eq!(s(|d| d.max_links_per_post = Some(-5.0)).max_per_post, 1);
        assert_eq!(
            s(|d| d.max_links_per_post = Some(99999.0)).max_per_post,
            200
        );
        assert_eq!(
            s(|d| d.max_new_links_per_pass = Some(0.0)).max_new_per_pass,
            1
        );
        assert_eq!(s(|d| d.min_links_per_post = Some(-3.0)).min_per_post, 0);
        assert_eq!(
            s(|d| d.link_overload_threshold = Some(0.0)).overload_threshold,
            1
        );
        assert_eq!(
            s(|d| d.link_overload_threshold = Some(1e9)).overload_threshold,
            10_000
        );
        let r = s(|d| {
            d.max_links_per_post = Some(4.0);
            d.min_links_per_post = Some(9.0);
            d.max_new_links_per_pass = Some(50.0);
        });
        assert_eq!(
            (r.max_per_post, r.min_per_post, r.max_new_per_pass),
            (4, 4, 4)
        );
        assert_eq!(
            s(|d| d.max_links_per_post = Some(f64::NAN)).max_per_post,
            10
        );
        assert_eq!(
            s(|d| d.max_links_per_post = Some(f64::INFINITY)).max_per_post,
            10
        );
        assert_eq!(s(|d| d.max_links_per_post = Some(7.9)).max_per_post, 7);
        assert_eq!(
            s(|d| d.max_new_inbound_per_target = Some(0.0)).max_new_inbound_per_target,
            Some(1)
        );
    }

    #[test]
    fn caps_like_v1() {
        let cs = vec![c("a", 0.9), c("b", 0.5), c("c", 0.1)];
        let r = apply_density(cs.clone(), &ctx(&[]), &LINK_DENSITY);
        assert_eq!(r.kept, cs);
        assert!(r.dropped.is_empty());

        let r = apply_density(
            vec![c("a", 0.1), c("a", 0.9), c("a", 0.5), c("a", 0.7)],
            &ctx(&[]),
            &lim(2),
        );
        assert_eq!(scores(&r.kept), vec![0.9, 0.7]);
        assert_eq!(scores(&r.dropped), vec![0.1, 0.5]);

        let cs: Vec<Candidate> = (0..9).map(|i| c("a", f64::from(i) / 10.0)).collect();
        let r = apply_density(cs, &ctx(&[]), &LINK_DENSITY);
        assert_eq!(scores(&r.kept), vec![0.4, 0.5, 0.6, 0.7, 0.8]);

        let cs: Vec<Candidate> = (0..6).map(|i| c("a", f64::from(i))).collect();
        let r = apply_density(cs, &ctx(&[("a", 8)]), &LINK_DENSITY);
        assert_eq!(scores(&r.kept), vec![4.0, 5.0]);

        let cs = vec![c("a", 1.0), c("a", 2.0), c("b", 3.0)];
        assert_eq!(
            apply_density(cs.clone(), &ctx(&[("a", 10)]), &LINK_DENSITY).kept,
            vec![c("b", 3.0)]
        );
        assert_eq!(
            apply_density(cs, &ctx(&[("a", 42)]), &LINK_DENSITY).kept,
            vec![c("b", 3.0)]
        );

        let cs = vec![c("a", 1.0), c("a", 2.0), c("b", 1.0), c("b", 2.0)];
        let r = apply_density(cs, &ctx(&[("a", 9)]), &LINK_DENSITY);
        assert_eq!(
            r.kept.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["a:2", "b:1", "b:2"]
        );
        assert!(
            apply_density(Vec::<Candidate>::new(), &ctx(&[]), &LINK_DENSITY)
                .kept
                .is_empty()
        );
    }

    #[test]
    fn ties_break_by_id_then_input_order() {
        let cs = vec![
            cand("a", 0.5, "z"),
            cand("a", 0.5, "m"),
            cand("a", 0.5, "b"),
        ];
        let r = apply_density(cs, &ctx(&[]), &lim(2));
        assert_eq!(
            r.kept.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["m", "b"]
        );
        assert_eq!(r.dropped[0].id, "z");
    }

    /// Finding #7: density must see real scores. With scores applied, the best candidates win
    /// regardless of id order; with all scores 0 (v1 order bug) the outcome is id order.
    #[test]
    fn density_after_scoring_keeps_best() {
        use crate::score::{ALL_AVAILABLE, DimInputs, score_batch};
        let inputs: Vec<DimInputs> = (0..8)
            .map(|k| DimInputs {
                cosine: f64::from(k) / 8.0,
                target_incoming: 10,
                target_click_depth: Some(1),
                ..Default::default()
            })
            .collect();
        let scored = score_batch(&inputs, ALL_AVAILABLE);
        // id0 is the worst candidate, id7 the best
        let cands: Vec<Candidate> = scored
            .iter()
            .enumerate()
            .map(|(k, s)| cand("src", s.composite_score, &format!("id{k}")))
            .collect();
        let r = apply_density(cands.clone(), &ctx(&[]), &LINK_DENSITY);
        let kept: Vec<&str> = r.kept.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(kept, vec!["id3", "id4", "id5", "id6", "id7"]);
        // capping before scoring (all scores 0, v1 bug) keeps the first five ids instead
        let unscored: Vec<Candidate> = cands
            .iter()
            .map(|c| Candidate {
                score: 0.0,
                ..c.clone()
            })
            .collect();
        let r0 = apply_density(unscored, &ctx(&[]), &LINK_DENSITY);
        let kept0: Vec<&str> = r0.kept.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(kept0, vec!["id0", "id1", "id2", "id3", "id4"]);
    }

    #[test]
    fn inbound_cap_and_money_exemption() {
        let mk = |id: &str, s: &str, t: &str, score: f64| Candidate {
            id: id.into(),
            source: s.into(),
            target: t.into(),
            score,
        };
        let cs = vec![
            mk("1", "a", "pillar", 0.9),
            mk("2", "b", "pillar", 0.8),
            mk("3", "c", "pillar", 0.7),
            mk("4", "a", "money", 0.6),
            mk("5", "b", "money", 0.5),
            mk("6", "c", "money", 0.4),
        ];
        let limits = DensityLimits {
            max_new_inbound_per_target: Some(2),
            ..LINK_DENSITY
        };
        let mut cx = ctx(&[]);
        cx.money_pages.insert("money".into());
        let r = apply_density(cs, &cx, &limits);
        assert_eq!(
            r.kept.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["1", "2", "4", "5", "6"]
        );
    }

    #[test]
    fn nan_scores_rank_last() {
        let r = apply_density(vec![c("a", f64::NAN), c("a", 0.1)], &ctx(&[]), &lim(1));
        assert_eq!(scores(&r.kept), vec![0.1]);
    }

    #[test]
    fn reports() {
        let p = |id: &str, status: &str, ty: &str, out: usize, roll: Option<usize>| PostLinkCount {
            post_id: id.into(),
            url: format!("https://x.com/{id}"),
            title: id.into(),
            post_status: status.into(),
            post_type: ty.into(),
            committed_outbound: out,
            outbound_link_count: roll,
        };
        let posts = vec![
            p("b", "publish", "post", 1, Some(150)),
            p("a", "publish", "post", 1, Some(120)),
            p("c", "draft", "post", 0, None),
            p("d", "publish", "archive", 0, Some(400)),
            p("e", "publish", "page", 5, Some(10)),
        ];
        let u: Vec<&str> = find_under_linked(&posts, &LINK_DENSITY)
            .iter()
            .map(|p| p.post_id.as_str())
            .collect();
        assert_eq!(u, vec!["a", "b"]);
        let o: Vec<&str> = find_overloaded(&posts, &LINK_DENSITY)
            .iter()
            .map(|p| p.post_id.as_str())
            .collect();
        assert_eq!(o, vec!["d", "b", "a"]);
    }
}
