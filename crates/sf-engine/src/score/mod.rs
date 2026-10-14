//! 11-dimension composite link scorer (spec §9, v1 `score/composite.ts` + `score/weights.ts`,
//! itself a port of LOOM `class-loom-composite.php`).
//!
//! Pure: [`compute_breakdown`] maps a resolved [`DimInputs`] to the 11 dimensions (each in
//! [0, 1]); [`effective_weights`] renormalises the LOOM weights when `gsc` / `money` / `tier`
//! carry no data for the whole site (v1.4.0); [`composite_score`] is the weighted sum rounded to
//! 4 decimals with JS `Math.round` semantics. [`score`] / [`score_batch`] combine the three and
//! snapshot the weights actually used into the breakdown, whose JSON shape is identical to v1
//! `score_breakdown` (`{semantic, orphan, …, placement, weights: {…}}`).

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// The 11 dimension weights (LOOM defaults, sum 1.0). Field order = v1 JSON key order.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Weights {
    pub semantic: f64,
    pub orphan: f64,
    pub depth: f64,
    pub tier: f64,
    pub cluster: f64,
    pub velocity: f64,
    pub graph: f64,
    pub money: f64,
    pub gsc: f64,
    pub authority: f64,
    pub placement: f64,
}

/// LOOM default weights (spec §9.1).
pub const WEIGHTS: Weights = Weights {
    semantic: 0.22,
    orphan: 0.08,
    depth: 0.06,
    tier: 0.06,
    cluster: 0.06,
    velocity: 0.04,
    graph: 0.10,
    money: 0.10,
    gsc: 0.08,
    authority: 0.10,
    placement: 0.10,
};

impl Weights {
    fn as_array(&self) -> [f64; 11] {
        [
            self.semantic,
            self.orphan,
            self.depth,
            self.tier,
            self.cluster,
            self.velocity,
            self.graph,
            self.money,
            self.gsc,
            self.authority,
            self.placement,
        ]
    }

    /// Sum of all weights.
    pub fn sum(&self) -> f64 {
        self.as_array().iter().sum()
    }
}

impl Default for Weights {
    fn default() -> Self {
        WEIGHTS
    }
}

/// Site-level availability of the optional dimensions (spec §9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DimAvailability {
    pub gsc: bool,
    pub money: bool,
    pub tier: bool,
}

/// Everything available (no renormalisation, exact v1.3.0 weights).
pub const ALL_AVAILABLE: DimAvailability = DimAvailability {
    gsc: true,
    money: true,
    tier: true,
};

impl Default for DimAvailability {
    fn default() -> Self {
        ALL_AVAILABLE
    }
}

impl DimAvailability {
    /// v1 rule: gsc = any gsc keyword on the site; money = any post with `meta.money_page`;
    /// tier = more than one distinct [`tier_of`] value across the site's posts.
    pub fn for_site(any_gsc: bool, any_money: bool, tiers: impl IntoIterator<Item = i64>) -> Self {
        let mut it = tiers.into_iter();
        let first = it.next();
        let tier = first.is_some_and(|f| it.any(|t| t != f));
        Self {
            gsc: any_gsc,
            money: any_money,
            tier,
        }
    }
}

/// Resolved inputs for one candidate edge (v1 `DimInputs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DimInputs {
    /// Source→target post embedding cosine.
    pub cosine: f64,
    /// Committed inbound links to the target (status applied OR origin existing).
    /// v2 pipeline should pass DISTINCT sources (finding #14).
    pub target_incoming: u32,
    /// Target click depth; None or < 0 = unknown / unreachable.
    pub target_click_depth: Option<i64>,
    pub source_tier: i64,
    pub target_tier: i64,
    pub source_cluster_id: Option<String>,
    pub target_cluster_id: Option<String>,
    pub target_is_pillar: bool,
    /// Days since target publication; None → LOOM default 90.
    pub target_age_days: Option<f64>,
    pub target_is_orphan: bool,
    pub target_is_dead_end: bool,
    pub target_pagerank: f64,
    pub source_pagerank: f64,
    /// Mean of the finite pageranks of all site posts.
    pub avg_pagerank: f64,
    pub same_component: bool,
    pub target_is_money: bool,
    pub source_is_money: bool,
    /// LOOM money priority 1–5 (default 1).
    pub money_priority: Option<f64>,
    /// LOOM target links goal (default 10).
    pub money_links_goal: Option<f64>,
    /// Striking-distance GSC keywords on the target.
    pub striking_count: u32,
    /// Any GSC data on the site.
    pub gsc_connected: bool,
    /// Committed inbound links to the target from same-cluster sources.
    pub cluster_inbound_links: u32,
    /// Non-gsc keyword rows of the target.
    pub target_keyword_count: u32,
    /// Best-matching source paragraph, 1-based; None = no match.
    pub best_paragraph: Option<i64>,
    /// Cosine of that paragraph vs the target.
    pub paragraph_sim: f64,
    pub source_word_count: i64,
}

impl Default for DimInputs {
    fn default() -> Self {
        Self {
            cosine: 0.0,
            target_incoming: 0,
            target_click_depth: None,
            source_tier: 3,
            target_tier: 3,
            source_cluster_id: None,
            target_cluster_id: None,
            target_is_pillar: false,
            target_age_days: None,
            target_is_orphan: false,
            target_is_dead_end: false,
            target_pagerank: 0.0,
            source_pagerank: 0.0,
            avg_pagerank: 0.0,
            same_component: false,
            target_is_money: false,
            source_is_money: false,
            money_priority: None,
            money_links_goal: None,
            striking_count: 0,
            gsc_connected: false,
            cluster_inbound_links: 0,
            target_keyword_count: 0,
            best_paragraph: None,
            paragraph_sim: 0.0,
            source_word_count: 0,
        }
    }
}

/// The 11 dimensions plus the weights snapshot (v1 `ScoreBreakdown`, same JSON shape).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    pub semantic: f64,
    pub orphan: f64,
    pub depth: f64,
    pub tier: f64,
    pub cluster: f64,
    pub velocity: f64,
    pub graph: f64,
    pub money: f64,
    pub gsc: f64,
    pub authority: f64,
    pub placement: f64,
    pub weights: Weights,
}

impl ScoreBreakdown {
    fn dims(&self) -> [f64; 11] {
        [
            self.semantic,
            self.orphan,
            self.depth,
            self.tier,
            self.cluster,
            self.velocity,
            self.graph,
            self.money,
            self.gsc,
            self.authority,
            self.placement,
        ]
    }
}

/// `Math.round(x·10⁴)/10⁴` with JS semantics (`floor(x·10⁴ + 0.5)`), spec §9.3.
pub fn round4(x: f64) -> f64 {
    crate::metrics::js_round(x, 4)
}

/// Clamp to [0, 1]; NaN → 0 (JS `Math.max(0, Math.min(1, NaN))` is NaN — v2 never emits NaN).
fn clamp01(v: f64) -> f64 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

fn finite(v: f64) -> f64 {
    if v.is_finite() { v } else { 0.0 }
}

/// The 11 dimensions, each exactly as v1 `computeBreakdown` (weights = LOOM defaults).
pub fn compute_breakdown(i: &DimInputs) -> ScoreBreakdown {
    let incoming = f64::from(i.target_incoming);

    // 1. semantic
    let semantic = clamp01(i.cosine);

    // 2. orphan: 0 → 1.0, <3 → 0.5, <5 → 0.2, else 0
    let orphan = match i.target_incoming {
        0 => 1.0,
        1..=2 => 0.5,
        3..=4 => 0.2,
        _ => 0.0,
    };

    // 3. depth: unknown/<0 → 1.0, >4 → 1.0, 4 → 0.7, 3 → 0.3, else 0
    let depth = match i.target_click_depth {
        None => 1.0,
        Some(d) if !(0..=4).contains(&d) => 1.0,
        Some(4) => 0.7,
        Some(3) => 0.3,
        Some(_) => 0.0,
    };

    // 4. tier
    let tier = match i.target_tier.cmp(&i.source_tier) {
        std::cmp::Ordering::Less => 1.0,
        std::cmp::Ordering::Equal => 0.3,
        std::cmp::Ordering::Greater => 0.1,
    };

    // 5. cluster
    let same_cluster = i.source_cluster_id.is_some() && i.source_cluster_id == i.target_cluster_id;
    let cluster = if i.target_is_pillar && same_cluster {
        1.0
    } else if same_cluster {
        0.8
    } else if i.target_cluster_id.is_some() {
        0.2
    } else {
        0.0
    };

    // 6. velocity
    let age_days = finite(i.target_age_days.unwrap_or(90.0)).max(1.0);
    let age_months = (age_days / 30.0).max(1.0);
    let lpm = incoming / age_months;
    let velocity = if i.target_incoming == 0 {
        0.8
    } else if age_days > 180.0 && lpm < 0.5 {
        0.7
    } else if age_days > 90.0 && lpm < 1.0 {
        0.5
    } else if lpm < 1.0 {
        0.2
    } else {
        0.0
    };

    // 7. graph need (low-PR targets score higher)
    let (tpr, spr, avg) = (
        finite(i.target_pagerank),
        finite(i.source_pagerank),
        finite(i.avg_pagerank),
    );
    let mut graph = 0.0;
    if i.target_is_orphan {
        graph += 0.4;
    }
    if avg > 0.0 && tpr < avg * 0.5 {
        graph += 0.3;
    }
    if i.target_is_dead_end && tpr > avg {
        graph -= 0.15;
    }
    if i.same_component {
        graph += 0.15;
    }
    if avg > 0.0 && spr > avg * 2.0 {
        graph += 0.1;
    }
    let graph = clamp01(graph);

    // 8. money
    let mut money = 0.0;
    if i.target_is_money || i.source_is_money {
        if i.target_is_money {
            money += 0.5;
            money += finite(i.money_priority.unwrap_or(1.0)) * 0.05;
            let goal = finite(i.money_links_goal.unwrap_or(10.0));
            let deficit = (goal - incoming).max(0.0);
            if deficit > 0.0 {
                money += (deficit * 0.05).min(0.25);
            } else {
                money -= 0.2;
            }
        }
        if i.source_is_money && !i.target_is_money {
            money -= 0.3;
        }
        money = clamp01(money);
    }

    // 9. gsc
    let gsc = if !i.gsc_connected || i.striking_count == 0 {
        0.0
    } else {
        clamp01(0.5 + 0.25 * (f64::from(i.striking_count) - 1.0))
    };

    // 10. authority
    let mut authority = 0.0;
    if i.target_cluster_id.is_some() {
        authority += (f64::from(i.cluster_inbound_links) * 0.1).min(0.4);
    }
    if avg > 0.0 {
        let ratio = tpr / avg;
        if ratio >= 3.0 {
            authority += 0.3;
        } else if ratio >= 1.5 {
            authority += 0.2;
        } else if ratio >= 1.0 {
            authority += 0.1;
        }
    }
    authority += match i.target_keyword_count {
        0 => 0.0,
        1..=2 => 0.1,
        3..=4 => 0.2,
        _ => 0.3,
    };
    let authority = clamp01(authority);

    // 11. placement
    let sim = finite(i.paragraph_sim);
    let mut placement = 0.0;
    if let Some(bp) = i.best_paragraph.filter(|b| *b > 0)
        && sim > 0.3
    {
        // `word_count || 500`: 0 → 500
        let wc = if i.source_word_count == 0 {
            500
        } else {
            i.source_word_count
        };
        let total_paras = (wc.max(0) as f64) / 100.0;
        let ratio = if total_paras > 0.0 {
            bp as f64 / total_paras.max(1.0)
        } else {
            0.5
        };
        placement = if ratio <= 0.33 {
            (sim * 1.5).min(1.0)
        } else if ratio <= 0.66 {
            sim.min(1.0)
        } else {
            (sim * 0.5).min(1.0)
        };
    }

    ScoreBreakdown {
        semantic,
        orphan,
        depth,
        tier,
        cluster,
        velocity,
        graph,
        money,
        gsc,
        authority,
        placement,
        weights: WEIGHTS,
    }
}

/// v1 `effectiveWeights`: unavailable optional dims get 0, the rest are scaled by
/// `1/(1 − Σ excluded)` and rounded to 4 decimals; the residue goes to `semantic`.
pub fn effective_weights(av: DimAvailability) -> Weights {
    if av.gsc && av.money && av.tier {
        return WEIGHTS;
    }
    let mut removed = 0.0;
    // v1 sums excluded weights in the order gsc, money, tier
    if !av.gsc {
        removed += WEIGHTS.gsc;
    }
    if !av.money {
        removed += WEIGHTS.money;
    }
    if !av.tier {
        removed += WEIGHTS.tier;
    }
    let scale = 1.0 / (1.0 - removed);
    let s = |w: f64| round4(w * scale);
    let mut w = Weights {
        semantic: s(WEIGHTS.semantic),
        orphan: s(WEIGHTS.orphan),
        depth: s(WEIGHTS.depth),
        tier: if av.tier { s(WEIGHTS.tier) } else { 0.0 },
        cluster: s(WEIGHTS.cluster),
        velocity: s(WEIGHTS.velocity),
        graph: s(WEIGHTS.graph),
        money: if av.money { s(WEIGHTS.money) } else { 0.0 },
        gsc: if av.gsc { s(WEIGHTS.gsc) } else { 0.0 },
        authority: s(WEIGHTS.authority),
        placement: s(WEIGHTS.placement),
    };
    // Object.values order = key order; summed left to right like v1
    let sum = w.sum();
    w.semantic = round4(w.semantic + (1.0 - sum));
    w
}

/// Weighted sum of the 11 dims with `weights`, rounded to 4 decimals.
pub fn composite_score(b: &ScoreBreakdown, weights: &Weights) -> f64 {
    let d = b.dims();
    let w = weights.as_array();
    let mut s = 0.0;
    for (x, y) in d.iter().zip(&w) {
        s += x * y;
    }
    round4(s)
}

/// One scored edge.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Scored {
    pub composite_score: f64,
    /// Breakdown with `weights` = the effective weights used (persist as `score_breakdown`).
    pub score_breakdown: ScoreBreakdown,
}

/// Breakdown + renormalised composite for one edge.
pub fn score(inputs: &DimInputs, av: DimAvailability) -> Scored {
    let w = effective_weights(av);
    let mut b = compute_breakdown(inputs);
    let composite_score = composite_score(&b, &w);
    b.weights = w;
    Scored {
        composite_score,
        score_breakdown: b,
    }
}

/// [`score`] for many edges in parallel on rayon's current pool (wrap in
/// [`crate::pool::install`]). Output order = input order.
pub fn score_batch(inputs: &[DimInputs], av: DimAvailability) -> Vec<Scored> {
    let w = effective_weights(av);
    inputs
        .par_iter()
        .map(|i| {
            let mut b = compute_breakdown(i);
            let composite_score = composite_score(&b, &w);
            b.weights = w;
            Scored {
                composite_score,
                score_breakdown: b,
            }
        })
        .collect()
}

/// v1 `tierOf`: `meta.tier` when set, else from URL path depth: 0 segments → 0,
/// otherwise `min(3, segments + 1)`; unparseable URL → 3.
pub fn tier_of(url: &str, meta_tier: Option<i64>) -> i64 {
    if let Some(t) = meta_tier {
        return t;
    }
    match crate::graph::path_segments(url) {
        Some(0) => 0,
        Some(s) => (s as i64).saturating_add(1).min(3),
        None => 3,
    }
}

/// Target age in days from publication and "now" (ms since epoch), min 1; None when unknown.
pub fn age_days(published_ms: Option<i64>, now_ms: i64) -> Option<f64> {
    published_ms.map(|p| ((now_ms.saturating_sub(p)) as f64 / 86_400_000.0).max(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn base() -> DimInputs {
        DimInputs {
            cosine: 0.0,
            target_incoming: 10,
            target_click_depth: Some(1),
            source_tier: 3,
            target_tier: 3,
            target_age_days: Some(30.0),
            target_pagerank: 0.6,
            source_pagerank: 1.0,
            avg_pagerank: 1.0,
            source_word_count: 500,
            ..Default::default()
        }
    }
    fn bd(f: impl FnOnce(&mut DimInputs)) -> ScoreBreakdown {
        let mut i = base();
        f(&mut i);
        compute_breakdown(&i)
    }
    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }
    fn c1() -> Option<String> {
        Some("c1".into())
    }

    // ---- ported from v1 test/composite.test.ts ----

    #[test]
    fn weights_sum_to_one() {
        close(WEIGHTS.sum(), 1.0);
    }

    #[test]
    fn semantic() {
        close(bd(|i| i.cosine = 0.9).semantic, 0.9);
        assert_eq!(bd(|i| i.cosine = 1.4).semantic, 1.0);
        assert_eq!(bd(|i| i.cosine = -0.2).semantic, 0.0);
        assert_eq!(bd(|i| i.cosine = f64::NAN).semantic, 0.0);
    }

    #[test]
    fn orphan_curve() {
        for (n, v) in [(0, 1.0), (2, 0.5), (4, 0.2), (5, 0.0)] {
            assert_eq!(bd(|i| i.target_incoming = n).orphan, v);
        }
    }

    #[test]
    fn depth_curve() {
        for (d, v) in [
            (None, 1.0),
            (Some(-1), 1.0),
            (Some(5), 1.0),
            (Some(4), 0.7),
            (Some(3), 0.3),
            (Some(2), 0.0),
        ] {
            assert_eq!(bd(|i| i.target_click_depth = d).depth, v);
        }
    }

    #[test]
    fn tier_dim() {
        assert_eq!(
            bd(|i| {
                i.source_tier = 3;
                i.target_tier = 1
            })
            .tier,
            1.0
        );
        assert_eq!(
            bd(|i| {
                i.source_tier = 3;
                i.target_tier = 3
            })
            .tier,
            0.3
        );
        assert_eq!(
            bd(|i| {
                i.source_tier = 2;
                i.target_tier = 3
            })
            .tier,
            0.1
        );
    }

    #[test]
    fn cluster_dim() {
        assert_eq!(
            bd(|i| {
                i.source_cluster_id = c1();
                i.target_cluster_id = c1();
                i.target_is_pillar = true
            })
            .cluster,
            1.0
        );
        assert_eq!(
            bd(|i| {
                i.source_cluster_id = c1();
                i.target_cluster_id = c1()
            })
            .cluster,
            0.8
        );
        assert_eq!(
            bd(|i| {
                i.source_cluster_id = c1();
                i.target_cluster_id = Some("c2".into())
            })
            .cluster,
            0.2
        );
        assert_eq!(bd(|i| i.target_cluster_id = None).cluster, 0.0);
    }

    #[test]
    fn velocity_dim() {
        assert_eq!(bd(|i| i.target_incoming = 0).velocity, 0.8);
        assert_eq!(
            bd(|i| {
                i.target_incoming = 2;
                i.target_age_days = Some(200.0)
            })
            .velocity,
            0.7
        );
        assert_eq!(
            bd(|i| {
                i.target_incoming = 3;
                i.target_age_days = Some(120.0)
            })
            .velocity,
            0.5
        );
        assert_eq!(
            bd(|i| {
                i.target_incoming = 1;
                i.target_age_days = Some(40.0)
            })
            .velocity,
            0.2
        );
        assert_eq!(
            bd(|i| {
                i.target_incoming = 10;
                i.target_age_days = Some(30.0)
            })
            .velocity,
            0.0
        );
    }

    #[test]
    fn graph_need_dim() {
        close(bd(|i| i.target_is_orphan = true).graph, 0.4);
        close(bd(|i| i.target_pagerank = 0.4).graph, 0.3);
        assert_eq!(
            bd(|i| {
                i.target_is_dead_end = true;
                i.target_pagerank = 2.0
            })
            .graph,
            0.0
        );
        close(bd(|i| i.same_component = true).graph, 0.15);
        close(bd(|i| i.source_pagerank = 2.5).graph, 0.1);
        close(
            bd(|i| {
                i.target_is_orphan = true;
                i.target_pagerank = 0.4;
                i.same_component = true;
                i.source_pagerank = 2.5;
            })
            .graph,
            0.95,
        );
    }

    #[test]
    fn money_dim() {
        assert_eq!(bd(|_| {}).money, 0.0);
        close(
            bd(|i| {
                i.target_is_money = true;
                i.target_incoming = 0
            })
            .money,
            0.8,
        );
        close(
            bd(|i| {
                i.target_is_money = true;
                i.target_incoming = 10
            })
            .money,
            0.35,
        );
        assert_eq!(bd(|i| i.source_is_money = true).money, 0.0);
        close(
            bd(|i| {
                i.source_is_money = true;
                i.target_is_money = true;
                i.target_incoming = 10
            })
            .money,
            0.35,
        );
    }

    #[test]
    fn gsc_dim() {
        assert_eq!(
            bd(|i| {
                i.gsc_connected = false;
                i.striking_count = 2
            })
            .gsc,
            0.0
        );
        assert_eq!(bd(|i| i.gsc_connected = true).gsc, 0.0);
        close(
            bd(|i| {
                i.gsc_connected = true;
                i.striking_count = 1
            })
            .gsc,
            0.5,
        );
        assert_eq!(
            bd(|i| {
                i.gsc_connected = true;
                i.striking_count = 3
            })
            .gsc,
            1.0
        );
    }

    #[test]
    fn authority_dim() {
        close(
            bd(|i| {
                i.target_cluster_id = c1();
                i.cluster_inbound_links = 7
            })
            .authority,
            0.4,
        );
        close(bd(|i| i.target_pagerank = 3.0).authority, 0.3);
        close(bd(|i| i.target_pagerank = 1.5).authority, 0.2);
        close(bd(|i| i.target_keyword_count = 5).authority, 0.3);
        close(bd(|i| i.target_keyword_count = 1).authority, 0.1);
        assert_eq!(
            bd(|i| {
                i.target_cluster_id = c1();
                i.cluster_inbound_links = 5;
                i.target_pagerank = 4.0;
                i.target_keyword_count = 9;
            })
            .authority,
            1.0
        );
    }

    #[test]
    fn placement_dim() {
        assert_eq!(bd(|_| {}).placement, 0.0);
        let p = |para, sim| {
            bd(|i| {
                i.best_paragraph = Some(para);
                i.paragraph_sim = sim;
                i.source_word_count = 900
            })
            .placement
        };
        assert_eq!(p(1, 0.3), 0.0);
        close(p(1, 0.5), 0.75);
        close(p(5, 0.5), 0.5);
        close(p(8, 0.5), 0.25);
    }

    #[test]
    fn composite_basics() {
        let mut full = compute_breakdown(&base());
        full.semantic = 1.0;
        full.orphan = 1.0;
        full.depth = 1.0;
        full.tier = 1.0;
        full.cluster = 1.0;
        full.velocity = 1.0;
        full.graph = 1.0;
        full.money = 1.0;
        full.gsc = 1.0;
        full.authority = 1.0;
        full.placement = 1.0;
        close(composite_score(&full, &WEIGHTS), 1.0);
        let b = bd(|i| {
            i.cosine = 1.0;
            i.source_tier = 2;
            i.target_tier = 3
        });
        close(composite_score(&b, &WEIGHTS), 0.226);
        assert_eq!(compute_breakdown(&base()).weights, WEIGHTS);
    }

    #[test]
    fn tier_of_urls() {
        assert_eq!(tier_of("https://x.com/a/b/c", Some(1)), 1);
        assert_eq!(tier_of("https://x.com/", None), 0);
        assert_eq!(tier_of("https://x.com/a/b/c/d", None), 3);
        assert_eq!(tier_of("https://x.com/post", None), 2);
        assert_eq!(tier_of("garbage", None), 3);
    }

    // ---- ported from v1 test/renormalization.test.ts ----

    fn rb() -> ScoreBreakdown {
        ScoreBreakdown {
            semantic: 0.6,
            orphan: 0.5,
            depth: 1.0,
            tier: 0.3,
            cluster: 0.8,
            velocity: 0.8,
            graph: 0.55,
            money: 0.0,
            gsc: 0.0,
            authority: 0.2,
            placement: 0.6,
            weights: WEIGHTS,
        }
    }
    const NONE: DimAvailability = DimAvailability {
        gsc: false,
        money: false,
        tier: false,
    };

    #[test]
    fn renormalization() {
        assert_eq!(effective_weights(ALL_AVAILABLE), WEIGHTS);
        for gsc in [true, false] {
            for money in [true, false] {
                for tier in [true, false] {
                    close(
                        effective_weights(DimAvailability { gsc, money, tier }).sum(),
                        1.0,
                    );
                }
            }
        }
        let w = effective_weights(NONE);
        assert_eq!((w.gsc, w.money, w.tier), (0.0, 0.0, 0.0));
        assert!((w.graph - 0.10 / 0.76).abs() < 1e-3);
        assert_eq!(
            [
                w.semantic,
                w.orphan,
                w.depth,
                w.cluster,
                w.velocity,
                w.graph,
                w.authority,
                w.placement
            ],
            [
                0.2895, 0.1053, 0.0789, 0.0789, 0.0526, 0.1316, 0.1316, 0.1316
            ]
        );
        let mut b = rb();
        b.authority = 0.4;
        b.placement = 0.8;
        let capped = composite_score(&b, &WEIGHTS);
        let lifted = composite_score(&b, &effective_weights(NONE));
        assert!(capped < 0.6 && lifted >= 0.6 && lifted - capped > 0.1);
        let mut m = rb();
        m.money = 1.0;
        m.gsc = 1.0;
        assert!(
            composite_score(&m, &WEIGHTS)
                > composite_score(
                    &m,
                    &effective_weights(DimAvailability {
                        gsc: false,
                        money: false,
                        tier: true
                    })
                )
        );
        let partial = effective_weights(DimAvailability {
            gsc: false,
            money: true,
            tier: false,
        });
        let mut zero = rb();
        zero.semantic = 0.0;
        zero.orphan = 0.0;
        zero.depth = 0.0;
        zero.tier = 0.0;
        zero.cluster = 0.0;
        zero.velocity = 0.0;
        zero.graph = 0.0;
        zero.authority = 0.0;
        zero.placement = 0.0;
        assert_eq!(composite_score(&zero, &partial), 0.0);
    }

    // ---- golden values generated by running the v1 TypeScript scorer under node ----

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct TsInput {
        cosine: f64,
        target_incoming: u32,
        target_click_depth: Option<i64>,
        source_tier: i64,
        target_tier: i64,
        source_cluster_id: Option<String>,
        target_cluster_id: Option<String>,
        target_is_pillar: bool,
        target_age_days: Option<f64>,
        target_is_orphan: bool,
        target_is_dead_end: bool,
        target_pagerank: f64,
        source_pagerank: f64,
        avg_pagerank: f64,
        same_component: bool,
        target_is_money: bool,
        source_is_money: bool,
        striking_count: u32,
        gsc_connected: bool,
        cluster_inbound_links: u32,
        target_keyword_count: u32,
        best_paragraph: Option<i64>,
        paragraph_sim: f64,
        source_word_count: i64,
    }
    #[derive(Deserialize)]
    struct Row {
        input: TsInput,
        availability: DimAvailability,
        breakdown: ScoreBreakdown,
        score: f64,
    }
    #[derive(Deserialize)]
    struct WRow {
        availability: DimAvailability,
        weights: Weights,
    }
    #[derive(Deserialize)]
    struct Golden {
        weights: Vec<WRow>,
        rows: Vec<Row>,
    }

    #[test]
    fn golden_matches_v1_typescript() {
        let g: Golden =
            serde_json::from_str(include_str!("fixtures/composite_golden.json")).unwrap();
        for w in &g.weights {
            assert_eq!(
                effective_weights(w.availability),
                w.weights,
                "{:?}",
                w.availability
            );
        }
        assert_eq!(g.rows.len(), 400);
        for (n, r) in g.rows.iter().enumerate() {
            let t = &r.input;
            let i = DimInputs {
                cosine: t.cosine,
                target_incoming: t.target_incoming,
                target_click_depth: t.target_click_depth,
                source_tier: t.source_tier,
                target_tier: t.target_tier,
                source_cluster_id: t.source_cluster_id.clone(),
                target_cluster_id: t.target_cluster_id.clone(),
                target_is_pillar: t.target_is_pillar,
                target_age_days: t.target_age_days,
                target_is_orphan: t.target_is_orphan,
                target_is_dead_end: t.target_is_dead_end,
                target_pagerank: t.target_pagerank,
                source_pagerank: t.source_pagerank,
                avg_pagerank: t.avg_pagerank,
                same_component: t.same_component,
                target_is_money: t.target_is_money,
                source_is_money: t.source_is_money,
                money_priority: None,
                money_links_goal: None,
                striking_count: t.striking_count,
                gsc_connected: t.gsc_connected,
                cluster_inbound_links: t.cluster_inbound_links,
                target_keyword_count: t.target_keyword_count,
                best_paragraph: t.best_paragraph,
                paragraph_sim: t.paragraph_sim,
                source_word_count: t.source_word_count,
            };
            let s = score(&i, r.availability);
            let (a, b) = (s.score_breakdown, r.breakdown);
            for (x, y) in a.dims().iter().zip(b.dims().iter()) {
                assert!((x - y).abs() < 1e-9, "row {n}: dims {a:?} vs {b:?}");
            }
            assert_eq!(a.weights, b.weights, "row {n}");
            assert!(
                (s.composite_score - r.score).abs() < 1e-9,
                "row {n}: {} vs {}",
                s.composite_score,
                r.score
            );
        }
        // batch == single
        let inputs: Vec<DimInputs> = (0..50)
            .map(|k| DimInputs {
                cosine: k as f64 / 50.0,
                ..base()
            })
            .collect();
        let batch = score_batch(&inputs, NONE);
        for (i, s) in inputs.iter().zip(&batch) {
            assert_eq!(score(i, NONE), *s);
        }
    }

    #[test]
    fn breakdown_json_shape_matches_v1() {
        let s = score(&base(), ALL_AVAILABLE);
        let j = serde_json::to_string(&s.score_breakdown).unwrap();
        assert!(j.starts_with("{\"semantic\":"));
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        for k in [
            "semantic",
            "orphan",
            "depth",
            "tier",
            "cluster",
            "velocity",
            "graph",
            "money",
            "gsc",
            "authority",
            "placement",
            "weights",
        ] {
            assert!(keys.contains(&k), "{k}");
        }
        assert_eq!(v["weights"]["semantic"], serde_json::json!(0.22));
    }

    #[test]
    fn hostile_inputs_never_panic() {
        let i = DimInputs {
            cosine: f64::NAN,
            target_pagerank: f64::INFINITY,
            avg_pagerank: f64::NAN,
            source_pagerank: f64::NEG_INFINITY,
            target_age_days: Some(f64::NAN),
            money_priority: Some(f64::INFINITY),
            money_links_goal: Some(f64::NAN),
            target_is_money: true,
            best_paragraph: Some(i64::MAX),
            paragraph_sim: f64::NAN,
            source_word_count: i64::MIN,
            target_click_depth: Some(i64::MIN),
            target_incoming: u32::MAX,
            striking_count: u32::MAX,
            ..Default::default()
        };
        let s = score(&i, NONE);
        assert!(s.composite_score.is_finite() && (0.0..=1.0).contains(&s.composite_score));
        assert!(
            s.score_breakdown
                .dims()
                .iter()
                .all(|d| d.is_finite() && (0.0..=1.0).contains(d))
        );
        assert_eq!(age_days(None, 0), None);
        assert_eq!(age_days(Some(0), 86_400_000 * 10), Some(10.0));
        assert_eq!(age_days(Some(i64::MAX), i64::MIN), Some(1.0));
        assert!(score_batch(&[], ALL_AVAILABLE).is_empty());
        assert!(!DimAvailability::for_site(false, false, [2, 2, 2]).tier);
        assert!(DimAvailability::for_site(false, false, [0, 2]).tier);
        assert!(!DimAvailability::for_site(false, false, []).tier);
    }
}
