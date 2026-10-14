//! Anchor-profile guards and reports (v1 `anchor/cannibalization.ts`), with running counts
//! instead of v1's repeated full `filter`/`find` scans (B-30).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sf_core::types::AnchorType;

/// Site-wide exact-match ceiling (LOOM).
pub const EXACT_MAX_SHARE: f64 = 0.4;
/// Site-wide generic ceiling (LOOM).
pub const GENERIC_MAX_SHARE: f64 = 0.3;
/// Minimum anchors per target before the diversity guard engages.
pub const DIVERSITY_MIN_POOL: usize = 3;
/// Max share of one anchor type within one target's pool.
pub const DIVERSITY_MAX_SHARE: f64 = 0.7;

/// An alternate anchor with its type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorAlt {
    pub anchor: String,
    pub anchor_type: AnchorType,
}

/// One anchor in the pool the guards look at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardItem {
    pub anchor: String,
    pub anchor_type: AnchorType,
    /// True = newly generated (may be downgraded); false = committed (never touched).
    pub is_new: bool,
    pub alternates: Vec<AnchorAlt>,
    /// Target post id (used by the per-target diversity guard).
    pub target_id: String,
}

/// Downgrade new anchors while the site-wide exact share > 40%, then generic > 30%.
/// Victims are taken in input order; replacement = first `partial` alt, else first `lsi`,
/// else first alt of another type. Same results as v1, O(n) per pass.
pub fn apply_over_optimization_guard(items: &[GuardItem]) -> Vec<GuardItem> {
    let mut out = items.to_vec();
    let total = out.len();
    if total == 0 {
        return out;
    }
    let mut counts: HashMap<AnchorType, usize> = HashMap::new();
    for i in &out {
        *counts.entry(i.anchor_type.clone()).or_insert(0) += 1;
    }
    for (offending, max_share) in [
        (AnchorType::Exact, EXACT_MAX_SHARE),
        (AnchorType::Generic, GENERIC_MAX_SHARE),
    ] {
        let mut cursor = 0usize;
        while (counts.get(&offending).copied().unwrap_or(0) as f64) / (total as f64) > max_share {
            let victim = out.iter().enumerate().skip(cursor).find(|(_, i)| {
                i.is_new
                    && i.anchor_type == offending
                    && i.alternates.iter().any(|a| a.anchor_type != offending)
            });
            let Some((vi, _)) = victim else { break };
            cursor = vi + 1;
            let Some(item) = out.get_mut(vi) else { break };
            let pos = item
                .alternates
                .iter()
                .position(|a| a.anchor_type == AnchorType::Partial)
                .or_else(|| {
                    item.alternates
                        .iter()
                        .position(|a| a.anchor_type == AnchorType::Lsi)
                })
                .or_else(|| {
                    item.alternates
                        .iter()
                        .position(|a| a.anchor_type != offending)
                });
            let Some(pos) = pos else { break };
            let alt = item.alternates.remove(pos);
            if let Some(c) = counts.get_mut(&offending) {
                *c = c.saturating_sub(1);
            }
            *counts.entry(alt.anchor_type.clone()).or_insert(0) += 1;
            item.anchor = alt.anchor;
            item.anchor_type = alt.anchor_type;
        }
    }
    out
}

/// Per-target diversity: within each target's pool (at least 3 anchors), while one type's
/// share exceeds 70%, swap the first new victim of the first offending type (order of first
/// appearance) to its first differently-typed alternate.
pub fn apply_diversity_guard(items: &[GuardItem]) -> Vec<GuardItem> {
    let mut out = items.to_vec();
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    let mut gindex: HashMap<String, usize> = HashMap::new();
    for (i, it) in out.iter().enumerate() {
        let g = *gindex.entry(it.target_id.clone()).or_insert_with(|| {
            groups.push((it.target_id.clone(), Vec::new()));
            groups.len() - 1
        });
        if let Some((_, v)) = groups.get_mut(g) {
            v.push(i);
        }
    }
    for (_, members) in groups {
        let total = members.len();
        if total < DIVERSITY_MIN_POOL {
            continue;
        }
        let mut counts: HashMap<AnchorType, usize> = HashMap::new();
        for &m in &members {
            if let Some(it) = out.get(m) {
                *counts.entry(it.anchor_type.clone()).or_insert(0) += 1;
            }
        }
        for _ in 0..total {
            // Types in order of first appearance in the current group state.
            let mut order: Vec<AnchorType> = Vec::new();
            for &m in &members {
                if let Some(it) = out.get(m)
                    && !order.contains(&it.anchor_type)
                {
                    order.push(it.anchor_type.clone());
                }
            }
            let Some(offending) = order.into_iter().find(|t| {
                (counts.get(t).copied().unwrap_or(0) as f64) / (total as f64) > DIVERSITY_MAX_SHARE
            }) else {
                break;
            };
            let victim = members.iter().copied().find(|&m| {
                out.get(m).is_some_and(|i| {
                    i.is_new
                        && i.anchor_type == offending
                        && i.alternates.iter().any(|a| a.anchor_type != offending)
                })
            });
            let Some(vi) = victim else { break };
            let Some(item) = out.get_mut(vi) else { break };
            let Some(pos) = item
                .alternates
                .iter()
                .position(|a| a.anchor_type != offending)
            else {
                break;
            };
            let alt = item.alternates.remove(pos);
            if let Some(c) = counts.get_mut(&offending) {
                *c = c.saturating_sub(1);
            }
            *counts.entry(alt.anchor_type.clone()).or_insert(0) += 1;
            item.anchor = alt.anchor;
            item.anchor_type = alt.anchor_type;
        }
    }
    out
}

/// Anchor distribution report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorDistribution {
    pub total: usize,
    /// Every known type is present (0 when unused); unknown stored types are added as-is.
    pub by_type: BTreeMap<String, usize>,
    /// Lowercased anchor → count.
    pub by_anchor: BTreeMap<String, usize>,
}

/// v1 `anchorDistribution`.
pub fn anchor_distribution<'a>(
    records: impl IntoIterator<Item = (&'a str, &'a AnchorType)>,
) -> AnchorDistribution {
    let mut by_type: BTreeMap<String, usize> = AnchorType::ALL
        .iter()
        .map(|t| ((*t).to_string(), 0))
        .collect();
    let mut by_anchor = BTreeMap::new();
    let mut total = 0;
    for (a, t) in records {
        total += 1;
        *by_type.entry(t.as_str().to_string()).or_insert(0) += 1;
        *by_anchor.entry(a.to_lowercase()).or_insert(0) += 1;
    }
    AnchorDistribution {
        total,
        by_type,
        by_anchor,
    }
}

/// One live anchor (status suggested/approved/applied) for cannibalization analysis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorUse {
    pub anchor: String,
    pub anchor_type: AnchorType,
    pub target_id: String,
    pub target_url: String,
}

/// The same exact anchor pointing at several targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CannibalizationWarning {
    pub anchor: String,
    pub target_count: usize,
    pub target_post_ids: Vec<String>,
    pub target_urls: Vec<String>,
}

/// v1 `getCannibalizationWarnings` as a pure function: exact anchors (grouped
/// case-insensitively, v1 was case-sensitive) with > 1 distinct target, sorted by target count
/// desc then anchor. Ids/urls sorted for determinism.
pub fn cannibalization_report(uses: &[AnchorUse]) -> Vec<CannibalizationWarning> {
    let mut groups: BTreeMap<String, (String, BTreeSet<String>, BTreeSet<String>)> =
        BTreeMap::new();
    for u in uses {
        if u.anchor_type != AnchorType::Exact || u.anchor.trim().is_empty() {
            continue;
        }
        let key = u.anchor.trim().to_lowercase();
        let e = groups.entry(key).or_insert_with(|| {
            (
                u.anchor.trim().to_string(),
                BTreeSet::new(),
                BTreeSet::new(),
            )
        });
        e.1.insert(u.target_id.clone());
        e.2.insert(u.target_url.clone());
    }
    let mut out: Vec<CannibalizationWarning> = groups
        .into_values()
        .filter(|(_, ids, _)| ids.len() > 1)
        .map(|(anchor, ids, urls)| CannibalizationWarning {
            anchor,
            target_count: ids.len(),
            target_post_ids: ids.into_iter().collect(),
            target_urls: urls.into_iter().collect(),
        })
        .collect();
    out.sort_by(|a, b| {
        b.target_count
            .cmp(&a.target_count)
            .then_with(|| a.anchor.cmp(&b.anchor))
    });
    out
}

/// Exact anchors (lowercase) already used for targets other than `target_id` — feed into
/// `PickOptions::reserved_exact` to prevent cannibalization at generation time (C.5).
pub fn reserved_exact_anchors(uses: &[AnchorUse], target_id: &str) -> HashSet<String> {
    uses.iter()
        .filter(|u| u.anchor_type == AnchorType::Exact && u.target_id != target_id)
        .map(|u| u.anchor.trim().to_lowercase())
        .filter(|a| !a.is_empty())
        .collect()
}

/// Share of `anchor_type` in a pool (0 when empty).
pub fn type_share<'a>(
    types: impl IntoIterator<Item = &'a AnchorType>,
    anchor_type: &AnchorType,
) -> f64 {
    let (mut n, mut hit) = (0usize, 0usize);
    for t in types {
        n += 1;
        if t == anchor_type {
            hit += 1;
        }
    }
    if n == 0 { 0.0 } else { hit as f64 / n as f64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(
        anchor: &str,
        t: AnchorType,
        is_new: bool,
        alts: &[(&str, AnchorType)],
        target: &str,
    ) -> GuardItem {
        GuardItem {
            anchor: anchor.into(),
            anchor_type: t,
            is_new,
            alternates: alts
                .iter()
                .map(|(a, t)| AnchorAlt {
                    anchor: (*a).into(),
                    anchor_type: t.clone(),
                })
                .collect(),
            target_id: target.into(),
        }
    }
    use AnchorType::*;

    #[test]
    fn golden_over_optimization() {
        let mut pool = vec![item("kw", Exact, false, &[], "t"); 4];
        pool.push(item(
            "kw",
            Exact,
            true,
            &[("kw alternative phrase", Partial)],
            "t",
        ));
        for x in ["x1", "x2", "x3", "x4", "x5"] {
            pool.push(item(x, Lsi, false, &[], "t"));
        }
        let out = apply_over_optimization_guard(&pool);
        assert_eq!(out[4].anchor_type, Partial);
        assert_eq!(out[4].anchor, "kw alternative phrase");
        assert_eq!(out.iter().filter(|i| i.anchor_type == Exact).count(), 4);

        let committed = vec![item("kw", Exact, false, &[], "t"); 2];
        assert_eq!(apply_over_optimization_guard(&committed), committed);

        let mut g = vec![item("read more", Generic, false, &[], "t"); 2];
        g.push(item(
            "click here",
            Generic,
            true,
            &[("topical phrase one", Lsi)],
            "t",
        ));
        g.push(item(
            "learn more",
            Generic,
            true,
            &[("topical phrase two", Lsi)],
            "t",
        ));
        for x in ["a", "b", "c", "d", "e", "f"] {
            g.push(item(x, Lsi, false, &[], "t"));
        }
        let out = apply_over_optimization_guard(&g);
        assert_eq!(out.iter().filter(|i| i.anchor_type == Generic).count(), 3);
        assert_eq!(out[2].anchor_type, Lsi);
        assert_eq!(out[3].anchor_type, Generic);

        let none = vec![item("kw", Exact, true, &[], "t"); 2];
        assert_eq!(apply_over_optimization_guard(&none), none);
        assert!(apply_over_optimization_guard(&[]).is_empty());
    }

    #[test]
    fn golden_diversity() {
        let pool = vec![
            item("a", Lsi, false, &[], "t1"),
            item("b", Lsi, false, &[], "t1"),
            item("c", Lsi, false, &[], "t1"),
            item("d", Lsi, true, &[("d-alt", Partial)], "t1"),
        ];
        let out = apply_diversity_guard(&pool);
        assert_eq!(out[3].anchor_type, Partial);
        assert_eq!(out[3].anchor, "d-alt");

        let committed = vec![item("a", Lsi, false, &[], "t1"); 3];
        assert_eq!(apply_diversity_guard(&committed), committed);

        let small = vec![
            item("a", Lsi, true, &[("alt", Partial)], "t1"),
            item("b", Lsi, true, &[("alt2", Partial)], "t1"),
        ];
        assert_eq!(apply_diversity_guard(&small), small);

        let scoped = vec![
            item("a", Lsi, false, &[], "t1"),
            item("b", Lsi, false, &[], "t1"),
            item("c", Lsi, true, &[("c-alt", Partial)], "t1"),
            item("x", Partial, false, &[], "t2"),
            item("y", Lsi, false, &[], "t2"),
            item("z", Branded, false, &[], "t2"),
        ];
        let out = apply_diversity_guard(&scoped);
        assert_eq!(out[2].anchor_type, Partial);
        assert_eq!(out[3].anchor_type, Partial);
        assert_eq!(out[4].anchor_type, Lsi);
        assert_eq!(out[5].anchor_type, Branded);

        let no_alt = vec![
            item("a", Lsi, false, &[], "t1"),
            item("b", Lsi, false, &[], "t1"),
            item("c", Lsi, true, &[], "t1"),
        ];
        assert_eq!(apply_diversity_guard(&no_alt), no_alt);
    }

    #[test]
    fn distribution_and_cannibalization() {
        let (e, g) = (Exact, Generic);
        let d = anchor_distribution([("SEO Tools", &e), ("seo tools", &e), ("read more", &g)]);
        assert_eq!(d.total, 3);
        assert_eq!(d.by_type.get("exact"), Some(&2));
        assert_eq!(d.by_type.get("cta"), Some(&0));
        assert_eq!(d.by_anchor.get("seo tools"), Some(&2));

        let u = |a: &str, t: AnchorType, id: &str| AnchorUse {
            anchor: a.into(),
            anchor_type: t,
            target_id: id.into(),
            target_url: format!("https://x/{id}"),
        };
        let uses = vec![
            u("SEO Tools", Exact, "p1"),
            u("seo tools", Exact, "p2"),
            u("seo tools", Exact, "p2"),
            u("bike lock", Exact, "p3"),
            u("seo tools", Partial, "p9"),
        ];
        let w = cannibalization_report(&uses);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].target_count, 2);
        assert_eq!(w[0].target_post_ids, vec!["p1", "p2"]);
        let r = reserved_exact_anchors(&uses, "p1");
        assert!(r.contains("seo tools") && r.contains("bike lock"));
        assert_eq!(type_share([&e, &g], &e), 0.5);
    }

    #[test]
    fn guards_scale_linearly() {
        // 50k anchors, all new exact with alternates: must finish fast (v1 was quadratic).
        let pool: Vec<GuardItem> = (0..50_000)
            .map(|i| {
                item(
                    &format!("kw{i}"),
                    Exact,
                    true,
                    &[("alt", Partial)],
                    &format!("t{}", i % 100),
                )
            })
            .collect();
        let t = std::time::Instant::now();
        let out = apply_over_optimization_guard(&pool);
        let _ = apply_diversity_guard(&out);
        assert!(t.elapsed().as_secs() < 5);
        assert!(type_share(out.iter().map(|i| &i.anchor_type), &Exact) <= EXACT_MAX_SHARE);
    }
}
