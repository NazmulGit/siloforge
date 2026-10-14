//! Topic clustering of post embeddings (spec §6, v1 `silo/cluster.ts` + `rust-engine/kmeans.rs`).
//!
//! * [`kmeans`]: k-means++ seeding (FNV-1a seed hash over UTF-16 code units + mulberry32) and
//!   Lloyd iterations, bit-for-bit parity with v1 for the same vector order (`ORDER BY id`), k
//!   and seed (= site id). Seeding keeps a running min distance (O(n·k·d) instead of
//!   O(n·k²·d), finding #38) which yields identical values.
//! * [`default_k`]: `clamp(round(sqrt(n/2)), 2, 25)`.
//! * [`select_pillar`]: member closest to its centroid, |Δdist| ≤ 1e-6 ties broken by
//!   higher pagerank, then by member order.
//! * [`cluster_label`] (v1 title-frequency label) and [`tfidf_labels`] (top TF-IDF terms of each
//!   cluster against the other clusters, from titles and/or keywords).

use std::collections::HashMap;

use rayon::prelude::*;

/// Largest vector count accepted.
pub const MAX_VECTORS: usize = 200_000;
/// Largest dimension accepted.
pub const MAX_DIMENSIONS: usize = 4_096;
/// Largest total value count (n · dim).
pub const MAX_VALUES: usize = 80_000_000;
/// v1 `MAX_ITERS`.
pub const DEFAULT_MAX_ITERATIONS: usize = 50;

/// v1 label stopwords (spec §6).
pub const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "of", "for", "to", "in", "on", "at", "by", "with", "is",
    "are", "was", "were", "be", "been", "it", "its", "this", "that", "these", "those", "you",
    "your", "we", "our", "how", "what", "why", "when", "where", "which", "who", "best", "top",
    "guide", "vs", "from", "as", "can", "do", "does", "not", "no",
];

/// Clustering input/argument errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClusterError {
    Empty,
    TooMany,
    BadK,
    BadDimensions,
    RaggedVectors,
    NonFinite,
    BadIterations,
}

impl std::fmt::Display for ClusterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Empty => "vectors must not be empty",
            Self::TooMany => "too many vectors or values",
            Self::BadK => "k must be between 1 and the vector count",
            Self::BadDimensions => "vector dimensions must be between 1 and 4096",
            Self::RaggedVectors => "all vectors must have equal dimensions",
            Self::NonFinite => "vectors must contain only finite values",
            Self::BadIterations => "max_iterations must be between 1 and 10000",
        };
        f.write_str(s)
    }
}

impl std::error::Error for ClusterError {}

/// FNV-1a 32-bit over JavaScript UTF-16 code units (v1 `hashSeed`).
pub fn hash_seed(seed: &str) -> u32 {
    let mut hash = 2_166_136_261u32;
    for cu in seed.encode_utf16() {
        hash ^= u32::from(cu);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash
}

/// mulberry32 PRNG, exact u32 form of the v1 implementation.
#[derive(Debug, Clone)]
pub struct Mulberry32(u32);

impl Mulberry32 {
    /// Seeded generator.
    pub fn new(seed: u32) -> Self {
        Self(seed)
    }

    /// Next value in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6d2b_79f5);
        let a = self.0;
        let mut t = (a ^ (a >> 15)).wrapping_mul(1 | a);
        t = t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t)) ^ t;
        f64::from(t ^ (t >> 14)) / 4_294_967_296.0
    }
}

/// v1 `defaultK(n) = clamp(round(sqrt(n/2)), 2, 25)` (JS rounding, .5 up).
pub fn default_k(n: usize) -> usize {
    let r = ((n as f64 / 2.0).sqrt() + 0.5).floor();
    (r as usize).clamp(2, 25)
}

/// Squared Euclidean distance (0 for mismatched lengths is never reached: inputs are validated).
pub fn sq_dist(a: &[f64], b: &[f64]) -> f64 {
    let mut d = 0.0;
    for (x, y) in a.iter().zip(b) {
        let diff = x - y;
        d += diff * diff;
    }
    d
}

/// k-means parameters.
#[derive(Debug, Clone)]
pub struct KmeansParams {
    pub k: usize,
    /// Seed string; v1 uses the site id.
    pub seed: String,
    pub max_iterations: usize,
    /// v1 behaviour: when float residue leaves the k-means++ draw unresolved, pick index 0
    /// (finding #24). `false` picks the last row with a non-zero distance instead.
    pub legacy_draw_fallback: bool,
}

impl KmeansParams {
    /// v1 defaults (50 iterations, legacy fallback on).
    pub fn new(k: usize, seed: impl Into<String>) -> Self {
        Self {
            k,
            seed: seed.into(),
            max_iterations: DEFAULT_MAX_ITERATIONS,
            legacy_draw_fallback: true,
        }
    }
}

/// k-means output.
#[derive(Debug, Clone, PartialEq)]
pub struct KmeansResult {
    /// Cluster index per input vector.
    pub assignments: Vec<usize>,
    /// k centroids (empty clusters keep their previous centroid).
    pub centroids: Vec<Vec<f64>>,
    /// Lloyd iterations run.
    pub iterations: usize,
}

fn validate<V: AsRef<[f64]>>(vectors: &[V], p: &KmeansParams) -> Result<usize, ClusterError> {
    let n = vectors.len();
    if n == 0 {
        return Err(ClusterError::Empty);
    }
    if n > MAX_VECTORS {
        return Err(ClusterError::TooMany);
    }
    if p.k == 0 || p.k > n {
        return Err(ClusterError::BadK);
    }
    if p.max_iterations == 0 || p.max_iterations > 10_000 {
        return Err(ClusterError::BadIterations);
    }
    let dim = vectors.first().map_or(0, |v| v.as_ref().len());
    if dim == 0 || dim > MAX_DIMENSIONS {
        return Err(ClusterError::BadDimensions);
    }
    if n.checked_mul(dim).is_none_or(|t| t > MAX_VALUES) {
        return Err(ClusterError::TooMany);
    }
    for v in vectors {
        let v = v.as_ref();
        if v.len() != dim {
            return Err(ClusterError::RaggedVectors);
        }
        if v.iter().any(|x| !x.is_finite()) {
            return Err(ClusterError::NonFinite);
        }
    }
    Ok(dim)
}

fn draw_index(rng: &mut Mulberry32, n: usize) -> usize {
    ((rng.next_f64() * n as f64).floor() as usize).min(n.saturating_sub(1))
}

/// Deterministic k-means (v1 parity). Use [`kmeans_f32`] for stored f32 embeddings.
/// The assignment step runs on rayon's current pool (results are order-independent).
pub fn kmeans<V: AsRef<[f64]> + Sync>(
    vectors: &[V],
    params: &KmeansParams,
) -> Result<KmeansResult, ClusterError> {
    let dim = validate(vectors, params)?;
    let n = vectors.len();
    let k = params.k;
    let row = |i: usize| vectors.get(i).map_or(&[][..], |v| v.as_ref());

    let mut rng = Mulberry32::new(hash_seed(&params.seed));
    let mut centroids: Vec<Vec<f64>> = Vec::with_capacity(k);
    centroids.push(row(draw_index(&mut rng, n)).to_vec());
    // running min squared distance to the chosen centroids
    let mut mins: Vec<f64> = vectors
        .iter()
        .map(|v| sq_dist(v.as_ref(), &centroids[0]))
        .collect();
    while centroids.len() < k {
        let total: f64 = mins.iter().sum();
        let next = if total == 0.0 {
            draw_index(&mut rng, n)
        } else {
            let mut r = rng.next_f64() * total;
            let mut idx = None;
            for (i, d) in mins.iter().enumerate() {
                r -= d;
                if r <= 0.0 {
                    idx = Some(i);
                    break;
                }
            }
            match idx {
                Some(i) => i,
                None if params.legacy_draw_fallback => 0,
                None => mins.iter().rposition(|d| *d > 0.0).unwrap_or(0),
            }
        };
        let c = row(next).to_vec();
        for (m, v) in mins.iter_mut().zip(vectors) {
            let d = sq_dist(v.as_ref(), &c);
            if d < *m {
                *m = d;
            }
        }
        centroids.push(c);
    }

    let mut assignments = vec![0usize; n];
    let mut iterations = 0;
    for iter in 0..params.max_iterations {
        iterations = iter + 1;
        let next: Vec<usize> = vectors
            .par_iter()
            .map(|v| {
                let v = v.as_ref();
                let mut best = 0;
                let mut best_d = f64::INFINITY;
                for (c, centroid) in centroids.iter().enumerate() {
                    let d = sq_dist(v, centroid);
                    if d < best_d {
                        best_d = d;
                        best = c;
                    }
                }
                best
            })
            .collect();
        let changed = next != assignments;
        assignments = next;
        if !changed && iter > 0 {
            break;
        }
        let mut sums = vec![vec![0.0; dim]; k];
        let mut counts = vec![0usize; k];
        for (v, &c) in vectors.iter().zip(&assignments) {
            if let (Some(s), Some(cnt)) = (sums.get_mut(c), counts.get_mut(c)) {
                *cnt += 1;
                for (acc, x) in s.iter_mut().zip(v.as_ref()) {
                    *acc += x;
                }
            }
        }
        for ((centroid, s), &cnt) in centroids.iter_mut().zip(&sums).zip(&counts) {
            if cnt == 0 {
                continue;
            }
            for (c, x) in centroid.iter_mut().zip(s) {
                *c = x / cnt as f64;
            }
        }
    }
    Ok(KmeansResult {
        assignments,
        centroids,
        iterations,
    })
}

/// [`kmeans`] over f32 embeddings (widened to f64 exactly).
pub fn kmeans_f32<V: AsRef<[f32]>>(
    vectors: &[V],
    params: &KmeansParams,
) -> Result<KmeansResult, ClusterError> {
    let wide: Vec<Vec<f64>> = vectors
        .iter()
        .map(|v| v.as_ref().iter().map(|&x| f64::from(x)).collect())
        .collect();
    kmeans(&wide, params)
}

/// One cluster member considered for pillar.
#[derive(Debug, Clone, Copy)]
pub struct PillarCandidate<'a> {
    pub vector: &'a [f64],
    pub pagerank: f64,
}

/// Index (into `members`) of the pillar: smallest squared distance to `centroid`; when two
/// distances are within 1e-6 the higher pagerank wins, then the earlier member.
/// `None` for an empty slice.
pub fn select_pillar(members: &[PillarCandidate<'_>], centroid: &[f64]) -> Option<usize> {
    let mut best: Option<(usize, f64, f64)> = None;
    for (i, m) in members.iter().enumerate() {
        let d = sq_dist(m.vector, centroid);
        let pr = if m.pagerank.is_finite() {
            m.pagerank
        } else {
            0.0
        };
        best = match best {
            None => Some((i, d, pr)),
            Some((bi, bd, bpr)) => {
                let better = if (d - bd).abs() > 1e-6 {
                    d < bd
                } else {
                    pr > bpr
                };
                if better {
                    Some((i, d, pr))
                } else {
                    Some((bi, bd, bpr))
                }
            }
        };
    }
    best.map(|b| b.0)
}

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '\''))
        .filter(|w| w.len() >= 3 && !STOPWORDS.contains(w))
        .map(str::to_string)
}

/// v1 label: words of the lower-cased member titles split on `[^a-z0-9']+`, length ≥ 3, no
/// stopwords; top 3 by frequency (ties keep first-seen order), joined by a space; else `"cluster"`.
pub fn cluster_label<S: AsRef<str>>(titles: &[S]) -> String {
    let mut order: Vec<String> = Vec::new();
    let mut freq: HashMap<String, usize> = HashMap::new();
    for t in titles {
        let lower = t.as_ref().to_lowercase();
        for w in words(&lower) {
            let e = freq.entry(w.clone()).or_insert(0);
            if *e == 0 {
                order.push(w);
            }
            *e += 1;
        }
    }
    let mut ranked: Vec<(usize, &String)> = order
        .iter()
        .map(|w| (freq.get(w).copied().unwrap_or(0), w))
        .collect();
    // stable sort by count desc keeps first-seen order for ties (JS Array.sort is stable)
    ranked.sort_by_key(|r| std::cmp::Reverse(r.0));
    let top: Vec<&str> = ranked.iter().take(3).map(|(_, w)| w.as_str()).collect();
    if top.is_empty() {
        "cluster".to_string()
    } else {
        top.join(" ")
    }
}

/// TF-IDF labels: each cluster is one document made of its members' texts (titles and/or
/// keyword phrases). Term score = `tf · ln(1 + K / df)` where K = cluster count; the top
/// `max_terms` terms (ties: higher tf, then alphabetical) form the label; `"cluster"` when none.
pub fn tfidf_labels<S: AsRef<str>>(clusters: &[Vec<S>], max_terms: usize) -> Vec<String> {
    let docs: Vec<HashMap<String, usize>> = clusters
        .iter()
        .map(|texts| {
            let mut tf = HashMap::new();
            for t in texts {
                for w in words(&t.as_ref().to_lowercase()) {
                    *tf.entry(w).or_insert(0usize) += 1;
                }
            }
            tf
        })
        .collect();
    let mut df: HashMap<&str, usize> = HashMap::new();
    for d in &docs {
        for w in d.keys() {
            *df.entry(w.as_str()).or_insert(0) += 1;
        }
    }
    let k = docs.len().max(1) as f64;
    docs.iter()
        .map(|d| {
            let mut scored: Vec<(f64, usize, &str)> = d
                .iter()
                .map(|(w, &tf)| {
                    let dfw = df.get(w.as_str()).copied().unwrap_or(1).max(1) as f64;
                    (tf as f64 * (1.0 + k / dfw).ln(), tf, w.as_str())
                })
                .collect();
            scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));
            let top: Vec<&str> = scored.iter().take(max_terms.max(1)).map(|s| s.2).collect();
            if top.is_empty() {
                "cluster".to_string()
            } else {
                top.join(" ")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Case {
        name: String,
        vectors: Vec<Vec<f64>>,
        k: usize,
        seed: String,
        assignments: Vec<usize>,
        centroids: Vec<Vec<f64>>,
    }
    #[derive(Deserialize)]
    struct Fixture {
        hash: Vec<(String, u32)>,
        #[serde(rename = "defaultK")]
        default_k: Vec<(usize, usize)>,
        cases: Vec<Case>,
    }

    /// Fixture produced by running the v1 TypeScript kmeans/hashSeed/defaultK under node
    /// (generator: `fixtures/gen_v1_fixtures.mjs`, v1 functions copied verbatim; run
    /// `node gen_v1_fixtures.mjs <crates/sf-engine/src>`).
    #[test]
    fn matches_v1_typescript_fixture_exactly() {
        let f: Fixture = serde_json::from_str(include_str!("fixtures/kmeans_parity.json")).unwrap();
        for (s, h) in &f.hash {
            assert_eq!(hash_seed(s), *h, "hash {s}");
        }
        for (n, k) in &f.default_k {
            assert_eq!(default_k(*n), *k, "defaultK({n})");
        }
        for c in &f.cases {
            let r = kmeans(&c.vectors, &KmeansParams::new(c.k, c.seed.clone())).unwrap();
            assert_eq!(r.assignments, c.assignments, "case {}", c.name);
            assert_eq!(
                r.centroids, c.centroids,
                "case {} centroids (bit-exact)",
                c.name
            );
        }
    }

    #[test]
    fn hash_seed_unicode_code_units() {
        assert_eq!(hash_seed("😀"), 3_409_036_472);
    }

    #[test]
    fn hostile_inputs_are_errors_not_panics() {
        let p = |k| KmeansParams::new(k, "x");
        let empty: Vec<Vec<f64>> = vec![];
        assert_eq!(kmeans(&empty, &p(1)), Err(ClusterError::Empty));
        assert_eq!(kmeans(&[vec![1.0]], &p(2)), Err(ClusterError::BadK)); // k > n
        assert_eq!(kmeans(&[vec![1.0]], &p(0)), Err(ClusterError::BadK));
        assert_eq!(
            kmeans(&[vec![1.0], vec![1.0, 2.0]], &p(1)),
            Err(ClusterError::RaggedVectors)
        );
        assert_eq!(
            kmeans(&[vec![f64::NAN]], &p(1)),
            Err(ClusterError::NonFinite)
        );
        assert_eq!(
            kmeans(&[Vec::<f64>::new()], &p(1)),
            Err(ClusterError::BadDimensions)
        );
        let mut q = p(1);
        q.max_iterations = 0;
        assert_eq!(kmeans(&[vec![1.0]], &q), Err(ClusterError::BadIterations));
        // all-identical vectors with k = n
        let same = vec![vec![0.5, 0.5]; 4];
        let r = kmeans(&same, &p(4)).unwrap();
        assert_eq!(r.assignments.len(), 4);
        let mut fixed = p(3);
        fixed.legacy_draw_fallback = false;
        assert!(kmeans_f32(&[vec![1.0f32, 0.0], vec![0.0, 1.0], vec![1.0, 1.0]], &fixed).is_ok());
    }

    #[test]
    fn pillar_closest_then_pagerank() {
        let a = [0.0, 0.0];
        let b = [1.0, 0.0];
        let c = [0.0, 1.0];
        let m = [
            PillarCandidate {
                vector: &b,
                pagerank: 0.1,
            },
            PillarCandidate {
                vector: &c,
                pagerank: 0.9,
            },
            PillarCandidate {
                vector: &a,
                pagerank: 0.0,
            },
        ];
        assert_eq!(select_pillar(&m, &[0.0, 0.0]), Some(2));
        // b and c tie on distance to (0.5,0.5): higher pagerank wins
        assert_eq!(select_pillar(&m[..2], &[0.5, 0.5]), Some(1));
        assert_eq!(select_pillar(&[], &[0.0]), None);
    }

    #[test]
    fn labels() {
        assert_eq!(
            cluster_label(&[
                "Best Bike Locks for 2024",
                "How to pick a bike lock",
                "Bike lock guide"
            ]),
            "bike lock locks"
        );
        assert_eq!(cluster_label(&["The best of", ""]), "cluster");
        let l = tfidf_labels(
            &[
                vec!["bike locks review", "bike lock types"],
                vec!["bike helmets review", "helmet sizes"],
            ],
            2,
        );
        // "bike" is in both clusters but tf=2 still outweighs the single-use terms
        assert_eq!(l[0], "bike lock");
        assert_eq!(l[1], "helmet helmets");
        assert_eq!(
            tfidf_labels::<&str>(&[vec![]], 3),
            vec!["cluster".to_string()]
        );
    }
}
