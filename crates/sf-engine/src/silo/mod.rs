//! Silo candidate generation and drift (spec §7, v1 `silo/topologies.ts`, `customClusters.ts`,
//! `drift.ts`, semantic candidates from `pipeline/plan.ts`).
//!
//! * [`edges_for_topology`]: the five topologies over one cluster (pillar excluded from members,
//!   member order = `ORDER BY id`): reverse, serial, priority (top-3 by pagerank), bidirectional,
//!   circle. `Topology::Unknown` behaves like `reverse` (v1 default).
//! * [`build_topology_candidates`]: all clusters (auto + custom), de-duplicated by
//!   (source, target), self-pairs dropped, and **pairs that already link or were rejected are
//!   skipped regardless of anchor** ([`SkipPairs`]) — v1 deduped on anchor `''` only and re-suggested
//!   applied/rejected pairs on every plan (finding #6). Rank = target pagerank.
//! * [`semantic_candidates`] / [`archive_candidates`]: the other two generators, same skip rules.
//! * [`resolve_custom_clusters`] / [`export_custom_clusters`]: v1 custom-cluster JSON.
//! * [`silo_drift`]: v1's SQL matched each applied edge against *itself* (`OR live.id = e.id`),
//!   so drift was always 0 (finding #12). Here an applied silo edge is intact only when the latest
//!   crawl found a live in-content link with the same (source, target).
//!
//! Density caps are **not** applied here; run [`crate::density::apply_density`] after scoring.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sf_core::types::Topology;

/// Set of directed (source, target) pairs with allocation-free lookups.
#[derive(Debug, Clone, Default)]
pub struct PairSet {
    map: HashMap<String, HashSet<String>>,
    len: usize,
}

impl PairSet {
    /// Empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a pair; returns true when it was new.
    pub fn insert(&mut self, source: &str, target: &str) -> bool {
        let new = self
            .map
            .entry(source.to_string())
            .or_default()
            .insert(target.to_string());
        self.len += usize::from(new);
        new
    }

    /// True when the pair is present.
    pub fn contains(&self, source: &str, target: &str) -> bool {
        self.map.get(source).is_some_and(|t| t.contains(target))
    }

    /// Number of pairs.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<S: AsRef<str>, T: AsRef<str>> FromIterator<(S, T)> for PairSet {
    fn from_iter<I: IntoIterator<Item = (S, T)>>(iter: I) -> Self {
        let mut p = Self::new();
        for (s, t) in iter {
            p.insert(s.as_ref(), t.as_ref());
        }
        p
    }
}

/// Pairs that must never be suggested again, keyed by (source, target) only.
#[derive(Debug, Clone, Default)]
pub struct SkipPairs {
    /// Already linked: status applied/approved, origin existing (live body links).
    pub linked: PairSet,
    /// Rejected by an operator (durable rejection memory, accuracy idea C.17).
    pub rejected: PairSet,
    /// Already pending as a suggestion (status suggested).
    pub pending: PairSet,
}

/// Why a pair was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    Linked,
    Rejected,
    Pending,
}

impl SkipPairs {
    /// Reason to skip `(source, target)`, if any.
    pub fn reason(&self, source: &str, target: &str) -> Option<SkipReason> {
        if self.linked.contains(source, target) {
            Some(SkipReason::Linked)
        } else if self.rejected.contains(source, target) {
            Some(SkipReason::Rejected)
        } else if self.pending.contains(source, target) {
            Some(SkipReason::Pending)
        } else {
            None
        }
    }

    /// Classify an existing `link_edges` row by status/origin into the right set.
    /// `reverted` / `failed` rows are not remembered (they may be suggested again).
    pub fn add_edge(&mut self, source: &str, target: &str, status: &str, origin: &str) {
        if origin == "existing" || matches!(status, "applied" | "approved") {
            self.linked.insert(source, target);
        } else if status == "rejected" {
            self.rejected.insert(source, target);
        } else if status == "suggested" {
            self.pending.insert(source, target);
        }
    }
}

/// Counts of what the generators dropped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SkipStats {
    pub self_pairs: usize,
    pub duplicates: usize,
    pub linked: usize,
    pub rejected: usize,
    pub pending: usize,
}

impl SkipStats {
    fn count(&mut self, r: SkipReason) {
        match r {
            SkipReason::Linked => self.linked += 1,
            SkipReason::Rejected => self.rejected += 1,
            SkipReason::Pending => self.pending += 1,
        }
    }
}

fn pr_of(pagerank: &HashMap<String, f64>, id: &str) -> f64 {
    pagerank
        .get(id)
        .copied()
        .filter(|v| v.is_finite())
        .unwrap_or(0.0)
}

/// v1 `edgesForTopology`. `members` may contain the pillar (it is removed). Empty member list
/// → no edges.
pub fn edges_for_topology(
    topology: &Topology,
    members: &[String],
    pillar: &str,
    pagerank: &HashMap<String, f64>,
) -> Vec<(String, String)> {
    let ms: Vec<&str> = members
        .iter()
        .map(String::as_str)
        .filter(|m| *m != pillar)
        .collect();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push = |s: &str, t: &str| out.push((s.to_string(), t.to_string()));
    let Some(last) = ms.last() else {
        return out;
    };
    match topology {
        Topology::Serial => {
            for w in ms.windows(2) {
                if let [a, b] = w {
                    push(a, b);
                }
            }
            push(last, pillar);
        }
        Topology::Priority => {
            for m in &ms {
                push(m, pillar);
            }
            let mut top = ms.clone();
            // stable sort, pagerank desc (missing → 0) — same as v1 Array.sort
            top.sort_by(|a, b| pr_of(pagerank, b).total_cmp(&pr_of(pagerank, a)));
            for m in top.iter().take(3) {
                push(pillar, m);
            }
        }
        Topology::Bidirectional => {
            for m in &ms {
                push(m, pillar);
            }
            for m in &ms {
                push(pillar, m);
            }
        }
        Topology::Circle => {
            let ring: Vec<&str> = std::iter::once(pillar).chain(ms.iter().copied()).collect();
            for (i, s) in ring.iter().enumerate() {
                if let Some(t) = ring.get((i + 1) % ring.len()) {
                    push(s, t);
                }
            }
        }
        Topology::Reverse | Topology::Unknown(_) => {
            for m in &ms {
                push(m, pillar);
            }
        }
    }
    out
}

/// One cluster as stored (`clusters` row + its member posts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SiloCluster {
    pub id: String,
    pub pillar: Option<String>,
    /// Member post ids, `ORDER BY id` (may include the pillar).
    pub members: Vec<String>,
    /// `source = 'custom'` (user-pinned).
    #[serde(default)]
    pub custom: bool,
}

/// A topology candidate before scoring.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TopologyCandidate {
    pub source: String,
    pub target: String,
    pub cluster_id: String,
    /// Target pagerank (pre-score tie-break only; density uses the composite score).
    pub rank: f64,
}

/// Candidates + what was skipped.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CandidateSet<T> {
    pub candidates: Vec<T>,
    pub skipped: SkipStats,
}

/// Topology candidates for every cluster that has a pillar (spec §7 `buildTopologyEdges`,
/// without the pre-scoring density cap).
pub fn build_topology_candidates(
    topology: &Topology,
    clusters: &[SiloCluster],
    pagerank: &HashMap<String, f64>,
    skip: &SkipPairs,
) -> CandidateSet<TopologyCandidate> {
    let mut seen = PairSet::new();
    let mut stats = SkipStats::default();
    let mut candidates = Vec::new();
    for c in clusters {
        let Some(pillar) = c.pillar.as_deref() else {
            continue;
        };
        for (s, t) in edges_for_topology(topology, &c.members, pillar, pagerank) {
            if s == t {
                stats.self_pairs += 1;
                continue;
            }
            if let Some(r) = skip.reason(&s, &t) {
                stats.count(r);
                continue;
            }
            if !seen.insert(&s, &t) {
                stats.duplicates += 1;
                continue;
            }
            candidates.push(TopologyCandidate {
                rank: pr_of(pagerank, &t),
                source: s,
                target: t,
                cluster_id: c.id.clone(),
            });
        }
    }
    CandidateSet {
        candidates,
        skipped: stats,
    }
}

/// A semantic-neighbour candidate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SemanticCandidate {
    pub source: String,
    pub target: String,
    pub similarity: f64,
}

/// v1 semantic candidates (`plan.ts`, `SEMANTIC_K = 5`, keep `score > min_score` = 0.5): for each
/// source, its nearest posts (best first) minus self, minus ids not in `current_posts`, minus
/// skipped pairs. Use [`crate::vector::VectorIndex::similar_batch`] to produce `neighbors`.
pub fn semantic_candidates(
    neighbors: &[(String, Vec<(String, f64)>)],
    min_score: f64,
    current_posts: &HashSet<String>,
    skip: &SkipPairs,
) -> CandidateSet<SemanticCandidate> {
    let mut seen = PairSet::new();
    let mut stats = SkipStats::default();
    let mut candidates = Vec::new();
    for (source, near) in neighbors {
        for (target, score) in near {
            if !(score.is_finite() && *score > min_score) || !current_posts.contains(target) {
                continue;
            }
            if source == target {
                stats.self_pairs += 1;
                continue;
            }
            if let Some(r) = skip.reason(source, target) {
                stats.count(r);
                continue;
            }
            if !seen.insert(source, target) {
                stats.duplicates += 1;
                continue;
            }
            candidates.push(SemanticCandidate {
                source: source.clone(),
                target: target.clone(),
                similarity: *score,
            });
        }
    }
    CandidateSet {
        candidates,
        skipped: stats,
    }
}

/// One archive/taxonomy page for [`archive_candidates`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ArchiveInput {
    pub id: String,
    /// Most similar posts first (any type; archives are filtered out).
    pub similar: Vec<String>,
    /// Same-cluster non-archive posts by pagerank desc (used when `similar` yields nothing).
    pub cluster_fallback: Vec<String>,
}

/// An archive-silo candidate (`origin = 'archive'`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArchiveCandidate {
    pub source: String,
    pub target: String,
    /// v1 rank `len − i` (most related first).
    pub rank: f64,
}

/// v1 `buildArchiveSilo` (without the pre-scoring cap): per archive, top `top_n` related
/// non-archive posts, emitting archive→post and post→archive.
pub fn archive_candidates(
    archives: &[ArchiveInput],
    top_n: usize,
    skip: &SkipPairs,
) -> CandidateSet<ArchiveCandidate> {
    let archive_ids: HashSet<&str> = archives.iter().map(|a| a.id.as_str()).collect();
    let mut seen = PairSet::new();
    let mut stats = SkipStats::default();
    let mut candidates = Vec::new();
    for a in archives {
        let mut targets: Vec<&str> = a
            .similar
            .iter()
            .map(String::as_str)
            .filter(|t| !archive_ids.contains(t))
            .take(top_n)
            .collect();
        if targets.is_empty() {
            targets = a
                .cluster_fallback
                .iter()
                .map(String::as_str)
                .filter(|t| !archive_ids.contains(t) && *t != a.id)
                .take(top_n)
                .collect();
        }
        let len = targets.len();
        for (i, t) in targets.iter().enumerate() {
            let rank = (len - i) as f64;
            for (s, d) in [(a.id.as_str(), *t), (*t, a.id.as_str())] {
                if s == d {
                    stats.self_pairs += 1;
                    continue;
                }
                if let Some(r) = skip.reason(s, d) {
                    stats.count(r);
                    continue;
                }
                if !seen.insert(s, d) {
                    stats.duplicates += 1;
                    continue;
                }
                candidates.push(ArchiveCandidate {
                    source: s.to_string(),
                    target: d.to_string(),
                    rank,
                });
            }
        }
    }
    CandidateSet {
        candidates,
        skipped: stats,
    }
}

// ---------------------------------------------------------------------------------------------
// Custom clusters
// ---------------------------------------------------------------------------------------------

/// v1 custom-cluster export entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomCluster {
    pub label: String,
    pub pillar_url: Option<String>,
    pub member_urls: Vec<String>,
}

/// v1 custom-cluster export document `{clusters: [...]}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CustomClusterExport {
    pub clusters: Vec<CustomCluster>,
}

/// A custom cluster resolved to post ids.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResolvedCustomCluster {
    pub label: String,
    pub pillar: Option<String>,
    /// Matched member ids (input order, de-duplicated).
    pub members: Vec<String>,
    /// URLs that matched no post.
    pub unmatched: Vec<String>,
}

/// Build the v1 export (member URLs sorted).
pub fn export_custom_clusters(clusters: &[CustomCluster]) -> CustomClusterExport {
    CustomClusterExport {
        clusters: clusters
            .iter()
            .map(|c| {
                let mut m = c.member_urls.clone();
                m.sort();
                CustomCluster {
                    label: c.label.clone(),
                    pillar_url: c.pillar_url.clone(),
                    member_urls: m,
                }
            })
            .collect(),
    }
}

/// Resolve an import against the site's posts (`url → id`). Exact URL first (v1), then the
/// [`crate::graph::url_path_key`] identity so `http`/`www`/trailing-slash variants still match.
/// The pillar is added to the members when missing.
pub fn resolve_custom_clusters(
    export: &CustomClusterExport,
    url_to_id: &HashMap<String, String>,
) -> Vec<ResolvedCustomCluster> {
    let by_path: HashMap<String, &String> = url_to_id
        .iter()
        .filter_map(|(u, id)| crate::graph::url_path_key(u, None).map(|k| (k, id)))
        .collect();
    let lookup = |u: &str| -> Option<String> {
        url_to_id.get(u).cloned().or_else(|| {
            crate::graph::url_path_key(u, None).and_then(|k| by_path.get(&k).map(|s| (*s).clone()))
        })
    };
    export
        .clusters
        .iter()
        .map(|c| {
            let mut members = Vec::new();
            let mut seen = HashSet::new();
            let mut unmatched = Vec::new();
            for u in &c.member_urls {
                match lookup(u) {
                    Some(id) => {
                        if seen.insert(id.clone()) {
                            members.push(id);
                        }
                    }
                    None => unmatched.push(u.clone()),
                }
            }
            let pillar = c.pillar_url.as_deref().and_then(|u| {
                let p = lookup(u);
                if p.is_none() {
                    unmatched.push(u.to_string());
                }
                p
            });
            if let Some(p) = &pillar
                && seen.insert(p.clone())
            {
                members.push(p.clone());
            }
            ResolvedCustomCluster {
                label: c.label.clone(),
                pillar,
                members,
                unmatched,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Drift
// ---------------------------------------------------------------------------------------------

/// An applied edge that carries a `silo_topology`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedSiloEdge {
    pub source: String,
    pub target: String,
    pub topology: String,
}

/// Per-topology drift counts (v1 shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiloDriftTopology {
    pub topology: String,
    pub applied: usize,
    pub intact: usize,
    pub drifted: usize,
}

/// v1 `SiloDriftReport` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiloDriftReport {
    pub topologies: Vec<SiloDriftTopology>,
    pub total_applied: usize,
    pub total_drifted: usize,
    pub ok: bool,
}

/// Fixed drift check. `live_links` must be the (source, target) pairs found in post content by
/// the **latest** crawl (parsed links / fresh `origin='existing'` rows) — never the managed
/// applied rows themselves. Topologies are sorted by name.
pub fn silo_drift(applied: &[AppliedSiloEdge], live_links: &PairSet) -> SiloDriftReport {
    let mut by: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for e in applied {
        let entry = by.entry(e.topology.as_str()).or_insert((0, 0));
        entry.0 += 1;
        if live_links.contains(&e.source, &e.target) {
            entry.1 += 1;
        }
    }
    let topologies: Vec<SiloDriftTopology> = by
        .into_iter()
        .map(|(t, (a, i))| SiloDriftTopology {
            topology: t.to_string(),
            applied: a,
            intact: i,
            drifted: a - i,
        })
        .collect();
    let total_applied = topologies.iter().map(|t| t.applied).sum();
    let total_drifted: usize = topologies.iter().map(|t| t.drifted).sum();
    SiloDriftReport {
        topologies,
        total_applied,
        total_drifted,
        ok: total_drifted == 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }
    fn pairs(v: &[(String, String)]) -> Vec<(&str, &str)> {
        v.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect()
    }
    fn none() -> HashMap<String, f64> {
        HashMap::new()
    }

    // ---- ported from v1 test/topologies.test.ts ----

    #[test]
    fn topologies_like_v1() {
        let m = s(&["a", "b", "c"]);
        assert_eq!(
            pairs(&edges_for_topology(&Topology::Reverse, &m, "p", &none())),
            vec![("a", "p"), ("b", "p"), ("c", "p")]
        );
        assert_eq!(
            pairs(&edges_for_topology(&Topology::Serial, &m, "p", &none())),
            vec![("a", "b"), ("b", "c"), ("c", "p")]
        );
        let pr: HashMap<String, f64> = [("a", 0.1), ("b", 0.3), ("c", 0.2)]
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect();
        assert_eq!(
            pairs(&edges_for_topology(&Topology::Priority, &m, "p", &pr)),
            vec![
                ("a", "p"),
                ("b", "p"),
                ("c", "p"),
                ("p", "b"),
                ("p", "c"),
                ("p", "a")
            ]
        );
        let many = s(&["a", "b", "c", "d", "e"]);
        let pr: HashMap<String, f64> = [("a", 5.0), ("b", 4.0), ("c", 3.0), ("d", 2.0), ("e", 1.0)]
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect();
        let e = edges_for_topology(&Topology::Priority, &many, "p", &pr);
        let from_p: Vec<&str> = e
            .iter()
            .filter(|x| x.0 == "p")
            .map(|x| x.1.as_str())
            .collect();
        assert_eq!(from_p, vec!["a", "b", "c"]);
        assert_eq!(
            pairs(&edges_for_topology(&Topology::Circle, &m, "p", &none())),
            vec![("p", "a"), ("a", "b"), ("b", "c"), ("c", "p")]
        );
        assert_eq!(
            pairs(&edges_for_topology(
                &Topology::Bidirectional,
                &m,
                "p",
                &none()
            )),
            vec![
                ("a", "p"),
                ("b", "p"),
                ("c", "p"),
                ("p", "a"),
                ("p", "b"),
                ("p", "c")
            ]
        );
        let seven = s(&["a", "b", "c", "d", "e", "f", "g"]);
        let e = edges_for_topology(&Topology::Bidirectional, &seven, "p", &none());
        assert_eq!(e.len(), 14);
        assert!(e.iter().all(|(a, b)| a != b));
        assert_eq!(
            edges_for_topology(&Topology::Bidirectional, &s(&["a", "p", "b"]), "p", &none()).len(),
            4
        );
        assert!(edges_for_topology(&Topology::Bidirectional, &[], "p", &none()).is_empty());
        assert!(edges_for_topology(&Topology::Circle, &s(&["p"]), "p", &none()).is_empty());
        let r = edges_for_topology(&Topology::Reverse, &s(&["a", "p", "b"]), "p", &none());
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|x| x.0 != "p"));
        assert_eq!(
            edges_for_topology(&Topology::Unknown("zigzag".into()), &m, "p", &none()).len(),
            3
        );
    }

    /// Finding #6: pairs that are linked, rejected or already pending are never re-suggested,
    /// whatever anchor they carry.
    #[test]
    fn candidates_skip_linked_rejected_and_pending_pairs() {
        let clusters = vec![
            SiloCluster {
                id: "c1".into(),
                pillar: Some("p".into()),
                members: s(&["a", "b", "c", "p"]),
                custom: false,
            },
            SiloCluster {
                id: "c2".into(),
                pillar: None,
                members: s(&["x", "y"]),
                custom: false,
            },
            SiloCluster {
                id: "c3".into(),
                pillar: Some("p".into()),
                members: s(&["a", "q"]),
                custom: true,
            },
        ];
        let mut skip = SkipPairs::default();
        skip.add_edge("a", "p", "applied", "plan");
        skip.add_edge("b", "p", "rejected", "plan");
        skip.add_edge("x", "y", "suggested", "plan");
        skip.add_edge("c", "p", "reverted", "plan"); // forgotten → may be suggested again
        skip.add_edge("q", "zz", "suggested", "existing"); // existing origin counts as linked
        let pr: HashMap<String, f64> = [("p".to_string(), 0.7)].into_iter().collect();
        let r = build_topology_candidates(&Topology::Reverse, &clusters, &pr, &skip);
        let got: Vec<(&str, &str)> = r
            .candidates
            .iter()
            .map(|c| (c.source.as_str(), c.target.as_str()))
            .collect();
        assert_eq!(got, vec![("c", "p"), ("q", "p")]);
        assert_eq!(r.candidates[0].rank, 0.7);
        assert_eq!(r.candidates[1].cluster_id, "c3");
        assert_eq!(r.skipped.linked, 2); // a→p in c1 and again in c3
        assert_eq!(r.skipped.rejected, 1);
        assert!(skip.linked.contains("q", "zz"));
    }

    #[test]
    fn semantic_and_archive_candidates() {
        let current: HashSet<String> = s(&["a", "b", "c"]).into_iter().collect();
        let mut skip = SkipPairs::default();
        skip.add_edge("a", "c", "rejected", "plan");
        let near = vec![
            (
                "a".to_string(),
                vec![
                    ("a".into(), 1.0),
                    ("b".into(), 0.8),
                    ("c".into(), 0.7),
                    ("gone".into(), 0.9),
                    ("b".into(), 0.6),
                ],
            ),
            (
                "b".to_string(),
                vec![("c".into(), 0.4), ("a".into(), f64::NAN)],
            ),
        ];
        let r = semantic_candidates(&near, 0.5, &current, &skip);
        let got: Vec<(&str, &str)> = r
            .candidates
            .iter()
            .map(|c| (c.source.as_str(), c.target.as_str()))
            .collect();
        assert_eq!(got, vec![("a", "b")]);
        assert_eq!(
            (
                r.skipped.self_pairs,
                r.skipped.rejected,
                r.skipped.duplicates
            ),
            (1, 1, 1)
        );

        let archives = vec![
            ArchiveInput {
                id: "cat1".into(),
                similar: s(&["cat2", "p1", "p2", "p3"]),
                cluster_fallback: vec![],
            },
            ArchiveInput {
                id: "cat2".into(),
                similar: vec![],
                cluster_fallback: s(&["p9", "cat2"]),
            },
        ];
        let r = archive_candidates(&archives, 2, &SkipPairs::default());
        let got: Vec<(&str, &str, f64)> = r
            .candidates
            .iter()
            .map(|c| (c.source.as_str(), c.target.as_str(), c.rank))
            .collect();
        assert_eq!(
            got,
            vec![
                ("cat1", "p1", 2.0),
                ("p1", "cat1", 2.0),
                ("cat1", "p2", 1.0),
                ("p2", "cat1", 1.0),
                ("cat2", "p9", 1.0),
                ("p9", "cat2", 1.0)
            ]
        );
    }

    /// Finding #12: v1 counted every applied edge as its own live evidence (drift always 0).
    #[test]
    fn drift_uses_live_links_only() {
        let applied = vec![
            AppliedSiloEdge {
                source: "a".into(),
                target: "p".into(),
                topology: "reverse".into(),
            },
            AppliedSiloEdge {
                source: "b".into(),
                target: "p".into(),
                topology: "reverse".into(),
            },
            AppliedSiloEdge {
                source: "p".into(),
                target: "a".into(),
                topology: "priority".into(),
            },
        ];
        let live: PairSet = [("a", "p"), ("p", "a")].into_iter().collect();
        let r = silo_drift(&applied, &live);
        assert!(!r.ok);
        assert_eq!(r.total_applied, 3);
        assert_eq!(r.total_drifted, 1);
        assert_eq!(
            r.topologies[0],
            SiloDriftTopology {
                topology: "priority".into(),
                applied: 1,
                intact: 1,
                drifted: 0
            }
        );
        assert_eq!(r.topologies[1].drifted, 1);
        let empty = silo_drift(&[], &PairSet::new());
        assert_eq!(
            empty,
            SiloDriftReport {
                topologies: vec![],
                total_applied: 0,
                total_drifted: 0,
                ok: true
            }
        );
        let j = serde_json::to_value(&empty).unwrap();
        assert_eq!(
            j,
            serde_json::json!({"topologies": [], "total_applied": 0, "total_drifted": 0, "ok": true})
        );
    }

    #[test]
    fn custom_clusters_roundtrip() {
        let urls: HashMap<String, String> = [
            ("https://x.com/a/", "A"),
            ("https://x.com/b/", "B"),
            ("https://x.com/pillar/", "P"),
        ]
        .iter()
        .map(|(u, i)| ((*u).to_string(), (*i).to_string()))
        .collect();
        let exp = export_custom_clusters(&[CustomCluster {
            label: "Locks".into(),
            pillar_url: Some("https://x.com/pillar/".into()),
            member_urls: s(&[
                "https://x.com/b/",
                "http://www.x.com/A",
                "https://x.com/missing/",
            ]),
        }]);
        assert_eq!(exp.clusters[0].member_urls[0], "http://www.x.com/A");
        let r = resolve_custom_clusters(&exp, &urls);
        assert_eq!(r[0].members, s(&["A", "B", "P"]));
        assert_eq!(r[0].pillar.as_deref(), Some("P"));
        assert_eq!(r[0].unmatched, s(&["https://x.com/missing/"]));
        let parsed: CustomClusterExport = serde_json::from_str(
            r#"{"clusters":[{"label":"x","pillar_url":null,"member_urls":[]}]}"#,
        )
        .unwrap();
        assert!(
            resolve_custom_clusters(&parsed, &HashMap::new())[0]
                .members
                .is_empty()
        );
    }
}
