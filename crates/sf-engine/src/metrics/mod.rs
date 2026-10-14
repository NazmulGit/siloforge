//! Site metrics, health score and snapshot deltas (spec §17, v1 `metrics/history.ts` and
//! `api/routes/dashboard.ts`). Pure: the caller loads rows, this module does the arithmetic.
//!
//! Health score is always an **integer 0..=100** (never a 0..1 fraction):
//! `clamp(round(100·(1 − 0.5·orphanRatio − 0.2·deadEndRatio − 0.3·(1 − min(1, avgLinks/5)))), 0, 100)`
//! with `avgLinks = committed links (status applied OR origin existing) / posts`.

use serde::{Deserialize, Serialize};

/// JS `Math.round(x·10^dp)/10^dp` (half rounds toward +∞). Non-finite input returns 0.
pub fn js_round(x: f64, dp: i32) -> f64 {
    if !x.is_finite() {
        return 0.0;
    }
    let f = 10f64.powi(dp);
    let r = (x * f + 0.5).floor() / f;
    if r.is_finite() { r } else { 0.0 }
}

/// Dashboard / history health score (integer 0..=100).
pub fn health_score(posts: usize, orphans: usize, dead_ends: usize, internal_links: usize) -> i64 {
    let (orphan_ratio, dead_end_ratio, avg_links) = if posts > 0 {
        let p = posts as f64;
        (
            orphans as f64 / p,
            dead_ends as f64 / p,
            internal_links as f64 / p,
        )
    } else {
        (0.0, 0.0, 0.0)
    };
    let density = (avg_links / 5.0).min(1.0);
    let raw = 100.0 * (1.0 - 0.5 * orphan_ratio - 0.2 * dead_end_ratio - 0.3 * (1.0 - density));
    ((raw + 0.5).floor() as i64).clamp(0, 100)
}

/// One post as seen by the metrics pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PostMetricRow {
    pub is_orphan: bool,
    pub is_dead_end: bool,
    /// -1 = unknown / unreachable (excluded from depth stats).
    pub click_depth: i64,
    /// `posts.outbound_link_count` rollup from link health (None = never checked).
    pub outbound_link_count: Option<i64>,
    /// Committed outbound links (status applied OR origin existing) from this post.
    pub committed_outbound: usize,
}

/// Link-health aggregates (only when the link-health crawler has checked something).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkHealthAggregate {
    pub checked: usize,
    pub broken_links: usize,
    pub nofollow_links: usize,
    pub redirect_links: usize,
    pub redirect_chains_over_2: usize,
    pub contextual_orphans: Option<usize>,
    pub nav_only_orphans: Option<usize>,
}

/// Everything [`compute_site_metrics`] needs.
#[derive(Debug, Clone, Default)]
pub struct MetricsInput {
    pub posts: Vec<PostMetricRow>,
    pub clusters: usize,
    pub edges_suggested: usize,
    pub edges_applied: usize,
    /// `settings.link_overload_threshold` (default 100).
    pub link_overload_threshold: Option<i64>,
    /// `settings.min_links_per_post` (default 3).
    pub min_links_per_post: Option<usize>,
    pub link_health: Option<LinkHealthAggregate>,
    /// Weakly-connected component count when the graph was built.
    pub components: Option<usize>,
}

/// v1 `SiteMetrics` (all optional; absent = subsystem has not run). JSON shape identical.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SiteMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub posts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orphans: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contextual_orphans: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nav_only_orphans: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dead_ends: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_click_depth: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_click_depth: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub components: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub internal_links: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broken_links: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nofollow_links: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_links: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_chains_over_2: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_overload_pages: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_links_per_post: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub posts_under_min_links: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clusters: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edges_suggested: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edges_applied: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_score: Option<i64>,
}

/// Improvement direction of a metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricDirection {
    Up,
    Down,
    Neutral,
}

/// Canonical metric order + direction (v1 `METRIC_DIRECTION`).
pub const METRIC_DIRECTION: &[(&str, MetricDirection)] = &[
    ("posts", MetricDirection::Neutral),
    ("clusters", MetricDirection::Neutral),
    ("edges_suggested", MetricDirection::Neutral),
    ("orphans", MetricDirection::Down),
    ("contextual_orphans", MetricDirection::Down),
    ("nav_only_orphans", MetricDirection::Down),
    ("dead_ends", MetricDirection::Down),
    ("avg_click_depth", MetricDirection::Down),
    ("max_click_depth", MetricDirection::Down),
    ("components", MetricDirection::Down),
    ("broken_links", MetricDirection::Down),
    ("nofollow_links", MetricDirection::Down),
    ("redirect_links", MetricDirection::Down),
    ("redirect_chains_over_2", MetricDirection::Down),
    ("link_overload_pages", MetricDirection::Down),
    ("posts_under_min_links", MetricDirection::Down),
    ("internal_links", MetricDirection::Up),
    ("avg_links_per_post", MetricDirection::Up),
    ("edges_applied", MetricDirection::Up),
    ("health_score", MetricDirection::Up),
];

impl SiteMetrics {
    /// Value of a metric by its JSON key.
    pub fn get(&self, key: &str) -> Option<f64> {
        let int = |x: Option<i64>| x.map(|v| v as f64);
        let v = match key {
            "posts" => int(self.posts),
            "orphans" => int(self.orphans),
            "contextual_orphans" => int(self.contextual_orphans),
            "nav_only_orphans" => int(self.nav_only_orphans),
            "dead_ends" => int(self.dead_ends),
            "avg_click_depth" => self.avg_click_depth,
            "max_click_depth" => int(self.max_click_depth),
            "components" => int(self.components),
            "internal_links" => int(self.internal_links),
            "broken_links" => int(self.broken_links),
            "nofollow_links" => int(self.nofollow_links),
            "redirect_links" => int(self.redirect_links),
            "redirect_chains_over_2" => int(self.redirect_chains_over_2),
            "link_overload_pages" => int(self.link_overload_pages),
            "avg_links_per_post" => self.avg_links_per_post,
            "posts_under_min_links" => int(self.posts_under_min_links),
            "clusters" => int(self.clusters),
            "edges_suggested" => int(self.edges_suggested),
            "edges_applied" => int(self.edges_applied),
            "health_score" => int(self.health_score),
            _ => None,
        };
        v.filter(|x| x.is_finite())
    }
}

fn n(x: usize) -> Option<i64> {
    Some(i64::try_from(x).unwrap_or(i64::MAX))
}

/// Compute v1 `SiteMetrics` from rows.
pub fn compute_site_metrics(input: &MetricsInput) -> SiteMetrics {
    let overload = input.link_overload_threshold.unwrap_or(100);
    let min_links = input.min_links_per_post.unwrap_or(3);
    let posts = input.posts.len();
    let orphans = input.posts.iter().filter(|p| p.is_orphan).count();
    let dead_ends = input.posts.iter().filter(|p| p.is_dead_end).count();
    let depths: Vec<i64> = input
        .posts
        .iter()
        .map(|p| p.click_depth)
        .filter(|d| *d >= 0)
        .collect();
    let avg_depth = if depths.is_empty() {
        0.0
    } else {
        depths.iter().sum::<i64>() as f64 / depths.len() as f64
    };
    let max_depth = depths.iter().copied().max().unwrap_or(0);
    let internal_links: usize = input.posts.iter().map(|p| p.committed_outbound).sum();
    let under = input
        .posts
        .iter()
        .filter(|p| p.committed_outbound < min_links)
        .count();
    let overloaded = input
        .posts
        .iter()
        .filter(|p| p.outbound_link_count.is_some_and(|c| c > overload))
        .count();
    let avg_links = if posts > 0 {
        internal_links as f64 / posts as f64
    } else {
        0.0
    };
    let mut m = SiteMetrics {
        posts: n(posts),
        orphans: n(orphans),
        dead_ends: n(dead_ends),
        avg_click_depth: Some(js_round(avg_depth, 2)),
        max_click_depth: Some(max_depth),
        internal_links: n(internal_links),
        avg_links_per_post: Some(js_round(avg_links, 2)),
        posts_under_min_links: n(under),
        link_overload_pages: n(overloaded),
        clusters: n(input.clusters),
        edges_suggested: n(input.edges_suggested),
        edges_applied: n(input.edges_applied),
        health_score: Some(health_score(posts, orphans, dead_ends, internal_links)),
        components: input.components.and_then(n),
        ..Default::default()
    };
    if let Some(h) = input.link_health.as_ref().filter(|h| h.checked > 0) {
        m.broken_links = n(h.broken_links);
        m.nofollow_links = n(h.nofollow_links);
        m.redirect_links = n(h.redirect_links);
        m.redirect_chains_over_2 = n(h.redirect_chains_over_2);
        m.contextual_orphans = h.contextual_orphans.and_then(n);
        m.nav_only_orphans = h.nav_only_orphans.and_then(n);
    }
    m
}

/// One metric compared with the previous snapshot (v1 `MetricDeltaDetail`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricDeltaDetail {
    pub key: String,
    pub current: Option<f64>,
    pub previous: Option<f64>,
    pub delta: Option<f64>,
    pub pct: Option<f64>,
    pub improved: Option<bool>,
}

/// Did the movement improve the site? `None` for no movement, unknown key or neutral metrics.
pub fn improved_for(key: &str, delta: Option<f64>) -> Option<bool> {
    let d = delta.filter(|d| *d != 0.0)?;
    let dir = METRIC_DIRECTION.iter().find(|(k, _)| *k == key)?.1;
    match dir {
        MetricDirection::Neutral => None,
        MetricDirection::Up => Some(d > 0.0),
        MetricDirection::Down => Some(d < 0.0),
    }
}

/// Percent change (2 decimals); `None` when either side is missing or previous is 0.
pub fn pct_change(current: Option<f64>, previous: Option<f64>) -> Option<f64> {
    let (c, p) = (current?, previous?);
    if p == 0.0 {
        return None;
    }
    Some(js_round((c - p) / p * 100.0, 2))
}

/// Compare two snapshots in canonical key order; keys absent on both sides are omitted.
pub fn compute_deltas(
    current: &SiteMetrics,
    previous: Option<&SiteMetrics>,
) -> Vec<MetricDeltaDetail> {
    METRIC_DIRECTION
        .iter()
        .filter_map(|(key, _)| {
            let cur = current.get(key);
            let prev = previous.and_then(|p| p.get(key));
            if cur.is_none() && prev.is_none() {
                return None;
            }
            let delta = match (cur, prev) {
                (Some(c), Some(p)) => Some(js_round(c - p, 4)),
                _ => None,
            };
            Some(MetricDeltaDetail {
                key: (*key).to_string(),
                current: cur,
                previous: prev,
                delta,
                pct: pct_change(cur, prev),
                improved: improved_for(key, delta),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_score_is_an_integer_in_range() {
        assert_eq!(health_score(0, 0, 0, 0), 70); // 100·(1 − 0.3)
        assert_eq!(health_score(10, 0, 0, 50), 100);
        assert_eq!(health_score(10, 10, 10, 0), 0);
        // 100·(1 − .5·.2 − .2·.1 − .3·(1 − .4)) = 70
        assert_eq!(health_score(10, 2, 1, 20), 70);
        // rounding: 100·(1 − .5·(1/3) − .3) = 53.33 → 53
        assert_eq!(health_score(3, 1, 0, 0), 53);
        for p in 0..20 {
            for o in 0..=p {
                let h = health_score(p, o, p - o, o * 3);
                assert!((0..=100).contains(&h));
            }
        }
    }

    #[test]
    fn site_metrics_from_rows() {
        let row = |o, d, depth, out, links| PostMetricRow {
            is_orphan: o,
            is_dead_end: d,
            click_depth: depth,
            outbound_link_count: out,
            committed_outbound: links,
        };
        let input = MetricsInput {
            posts: vec![
                row(true, false, -1, None, 0),
                row(false, false, 0, Some(150), 5),
                row(false, true, 3, Some(10), 1),
                row(false, false, 2, Some(5), 4),
            ],
            clusters: 2,
            edges_suggested: 9,
            edges_applied: 3,
            components: Some(2),
            ..Default::default()
        };
        let m = compute_site_metrics(&input);
        assert_eq!(m.posts, Some(4));
        assert_eq!(m.avg_click_depth, Some(1.67));
        assert_eq!(m.max_click_depth, Some(3));
        assert_eq!(m.internal_links, Some(10));
        assert_eq!(m.avg_links_per_post, Some(2.5));
        assert_eq!(m.posts_under_min_links, Some(2));
        assert_eq!(m.link_overload_pages, Some(1));
        assert_eq!(m.health_score, Some(health_score(4, 1, 1, 10)));
        assert_eq!(m.broken_links, None);
        let j = serde_json::to_value(&m).unwrap();
        assert!(j.get("broken_links").is_none());
        assert_eq!(
            j["health_score"],
            serde_json::json!(health_score(4, 1, 1, 10))
        );
        assert_eq!(j["posts"].to_string(), "4");
        // empty site
        let e = compute_site_metrics(&MetricsInput::default());
        assert_eq!(e.avg_click_depth, Some(0.0));
        assert_eq!(e.health_score, Some(70));
    }

    #[test]
    fn deltas_follow_direction_table() {
        let cur = SiteMetrics {
            orphans: Some(4),
            internal_links: Some(120),
            posts: Some(10),
            broken_links: Some(3),
            ..Default::default()
        };
        let prev = SiteMetrics {
            orphans: Some(12),
            internal_links: Some(400),
            posts: Some(10),
            broken_links: Some(0),
            ..Default::default()
        };
        let d = compute_deltas(&cur, Some(&prev));
        let get = |k: &str| d.iter().find(|x| x.key == k).unwrap().clone();
        assert_eq!(get("orphans").improved, Some(true));
        assert_eq!(get("orphans").pct, Some(-66.67));
        assert_eq!(get("internal_links").improved, Some(false));
        assert_eq!(get("posts").improved, None);
        assert_eq!(get("broken_links").pct, None);
        assert_eq!(d.len(), 4);
        let first = compute_deltas(&cur, None);
        assert!(
            first
                .iter()
                .all(|x| x.delta.is_none() && x.improved.is_none())
        );
        assert_eq!(js_round(2.5, 0), 3.0);
        assert_eq!(js_round(-2.5, 0), -2.0);
        assert_eq!(js_round(f64::NAN, 2), 0.0);
    }
}
