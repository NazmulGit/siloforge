//! Site link graph (spec §4, v1 `graph/engine.ts` + `rust-engine/graph.rs`).
//!
//! * URL identity: [`url_path_key`], [`url_host_key`], [`site_hosts`] (spec §4.1).
//! * [`LinkGraph`]: directed, de-duplicated, no self-loops, node order = caller order
//!   (pass posts `ORDER BY id` for determinism).
//! * [`pagerank`] (alpha 0.85, 100 iterations, tolerance 1e-6, v1 summation order),
//!   [`weak_components`] (ids 1.. in node order), [`click_depths`] (multi-source BFS).
//! * **Root fix (finding #8 "click depth always 0")**: v1 made every URL with ≤ 1 path segment a
//!   BFS root, so on `/%postname%/` sites every post had depth 0. [`select_roots`] now uses, in
//!   order: operator-configured roots, the home page (path `/`), then nav-linked pages at depth 1
//!   (when a home page exists) — never "every single-segment URL". With none of those it picks one
//!   hub (highest in-degree, then shallowest URL, then node order).
//! * [`compute`] runs everything and returns per-node metrics (+ orphan / dead-end /
//!   contextual-orphan / nav-only flags, spec §4.4) and a summary.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use serde::Serialize;
use url::Url;

use crate::metrics::js_round;

/// Largest node count accepted.
pub const MAX_NODES: usize = 500_000;
/// Largest edge count accepted (before de-duplication).
pub const MAX_EDGES: usize = 5_000_000;

/// Graph construction errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    TooManyNodes,
    TooManyEdges,
    DuplicateNode(String),
    BadNodeId,
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyNodes => write!(f, "graph exceeds {MAX_NODES} nodes"),
            Self::TooManyEdges => write!(f, "graph exceeds {MAX_EDGES} edges"),
            Self::DuplicateNode(id) => write!(f, "duplicate node id: {id}"),
            Self::BadNodeId => write!(f, "node ids must contain 1..=512 bytes"),
        }
    }
}

impl std::error::Error for GraphError {}

// ---------------------------------------------------------------------------------------------
// URL identity (spec §4.1)
// ---------------------------------------------------------------------------------------------

/// JS `decodeURIComponent`: `None` on a malformed escape or invalid UTF-8.
fn decode_uri_component(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hs = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hs, 16).ok()?);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn parse(raw: &str, base: Option<&str>) -> Option<Url> {
    match base {
        Some(b) => Url::parse(b).ok()?.join(raw).ok(),
        None => Url::parse(raw).ok(),
    }
}

/// Site-relative identity key of a URL: path only (scheme/host/port/query/fragment dropped),
/// trailing slashes stripped (`""` → `"/"`), percent-decoded (raw kept when malformed),
/// lower-cased. `None` when unparseable. Matches v1 `urlPathKey`, except (#23) WordPress "plain"
/// permalinks: on `/` or `/index.php`, a numeric `p`, `page_id` or `attachment_id` query is kept
/// (`/?p=12`), so `https://x.com/?p=12` is that post and not the home page.
pub fn url_path_key(raw: &str, base: Option<&str>) -> Option<String> {
    let u = parse(raw, base)?;
    if u.cannot_be_a_base() {
        return None;
    }
    let mut p = u.path().trim_end_matches('/').to_string();
    if p.is_empty() {
        p.push('/');
    }
    let p = decode_uri_component(&p).unwrap_or(p);
    let p = p.to_lowercase();
    if p == "/" || p == "/index.php" {
        for (k, v) in u.query_pairs() {
            let k = k.to_ascii_lowercase();
            if matches!(k.as_str(), "p" | "page_id" | "attachment_id")
                && !v.is_empty()
                && v.len() <= 20
                && v.bytes().all(|b| b.is_ascii_digit())
            {
                return Some(format!("/?{k}={v}"));
            }
        }
    }
    Some(p)
}

/// Host of an absolute URL: lower-cased, `www.` stripped, no port. Matches v1 `urlHostKey`.
pub fn url_host_key(raw: &str) -> Option<String> {
    let u = Url::parse(raw).ok()?;
    let h = u.host_str()?.to_lowercase();
    let h = h.strip_prefix("www.").unwrap_or(&h).to_string();
    (!h.is_empty()).then_some(h)
}

/// Host of the configured site domain (`https://www.Example.com:8443/x` → `example.com`).
pub fn configured_host(domain: &str) -> Option<String> {
    let d = domain.trim().to_lowercase();
    let d = d
        .strip_prefix("https://")
        .or_else(|| d.strip_prefix("http://"))
        .unwrap_or(&d);
    let d = d.split('/').next().unwrap_or("");
    let d = match d.rsplit_once(':') {
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => d,
    };
    let d = d.strip_prefix("www.").unwrap_or(d);
    (!d.is_empty()).then(|| d.to_string())
}

/// Every host the site is served on: configured domain ∪ hosts of all post URLs.
pub fn site_hosts<'a>(
    domain: &str,
    post_urls: impl IntoIterator<Item = &'a str>,
) -> BTreeSet<String> {
    let mut hosts = BTreeSet::new();
    if let Some(h) = configured_host(domain) {
        hosts.insert(h);
    }
    for u in post_urls {
        if let Some(h) = url_host_key(u) {
            hosts.insert(h);
        }
    }
    hosts
}

/// Number of non-empty path segments of an absolute URL.
pub fn path_segments(url: &str) -> Option<usize> {
    let u = Url::parse(url).ok()?;
    Some(u.path().split('/').filter(|s| !s.is_empty()).count())
}

/// True for the site home page (path key `/`). Query-string URLs such as `/?p=123` are *not*
/// home pages (finding #23).
pub fn is_home_url(url: &str) -> bool {
    match Url::parse(url) {
        Ok(u) => url_path_key(url, None).as_deref() == Some("/") && u.query().is_none(),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Graph
// ---------------------------------------------------------------------------------------------

/// PageRank parameters (spec §4.3 defaults).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphParams {
    pub alpha: f64,
    pub max_iterations: usize,
    pub tolerance: f64,
}

impl Default for GraphParams {
    fn default() -> Self {
        Self {
            alpha: 0.85,
            max_iterations: 100,
            tolerance: 1e-6,
        }
    }
}

impl GraphParams {
    /// Out-of-range values replaced by the defaults (alpha ∈ [0,1], iterations 1..=10000,
    /// tolerance ∈ (0,1]).
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        Self {
            alpha: if self.alpha.is_finite() && (0.0..=1.0).contains(&self.alpha) {
                self.alpha
            } else {
                d.alpha
            },
            max_iterations: if (1..=10_000).contains(&self.max_iterations) {
                self.max_iterations
            } else {
                d.max_iterations
            },
            tolerance: if self.tolerance.is_finite()
                && self.tolerance > 0.0
                && self.tolerance <= 1.0
            {
                self.tolerance
            } else {
                d.tolerance
            },
        }
    }
}

/// What [`LinkGraph::build`] dropped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct BuildStats {
    pub self_loops: usize,
    pub duplicates: usize,
    pub unknown_endpoints: usize,
}

/// Directed simple graph over string ids.
#[derive(Debug, Clone, Default)]
pub struct LinkGraph {
    ids: Vec<String>,
    index: HashMap<String, usize>,
    out: Vec<Vec<usize>>,
    inc: Vec<Vec<usize>>,
    edges: usize,
}

impl LinkGraph {
    /// Build from node ids (order kept) and `(source, target)` edges. Self-loops, duplicate
    /// edges and edges with an unknown endpoint are skipped and counted; duplicate node ids
    /// are an error.
    pub fn build<S, A, B, I>(nodes: &[S], edges: I) -> Result<(Self, BuildStats), GraphError>
    where
        S: AsRef<str>,
        A: AsRef<str>,
        B: AsRef<str>,
        I: IntoIterator<Item = (A, B)>,
    {
        if nodes.len() > MAX_NODES {
            return Err(GraphError::TooManyNodes);
        }
        let n = nodes.len();
        let mut g = Self {
            ids: Vec::with_capacity(n),
            index: HashMap::with_capacity(n),
            out: vec![Vec::new(); n],
            inc: vec![Vec::new(); n],
            edges: 0,
        };
        for (i, id) in nodes.iter().enumerate() {
            let id = id.as_ref();
            if id.is_empty() || id.len() > 512 {
                return Err(GraphError::BadNodeId);
            }
            if g.index.insert(id.to_string(), i).is_some() {
                return Err(GraphError::DuplicateNode(id.to_string()));
            }
            g.ids.push(id.to_string());
        }
        let mut stats = BuildStats::default();
        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        let mut total = 0usize;
        for (s, t) in edges {
            total += 1;
            if total > MAX_EDGES {
                return Err(GraphError::TooManyEdges);
            }
            let (Some(&s), Some(&t)) = (g.index.get(s.as_ref()), g.index.get(t.as_ref())) else {
                stats.unknown_endpoints += 1;
                continue;
            };
            if s == t {
                stats.self_loops += 1;
                continue;
            }
            if !seen.insert((s, t)) {
                stats.duplicates += 1;
                continue;
            }
            if let (Some(o), Some(i)) = (g.out.get_mut(s), g.inc.get_mut(t)) {
                o.push(t);
                i.push(s);
                g.edges += 1;
            }
        }
        Ok((g, stats))
    }

    /// Node count.
    pub fn node_count(&self) -> usize {
        self.ids.len()
    }

    /// Unique edge count.
    pub fn edge_count(&self) -> usize {
        self.edges
    }

    /// Node ids in order.
    pub fn ids(&self) -> &[String] {
        &self.ids
    }

    /// Index of a node id.
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.index.get(id).copied()
    }

    /// Out-neighbours (insertion order).
    pub fn out_neighbors(&self, i: usize) -> &[usize] {
        self.out.get(i).map_or(&[], Vec::as_slice)
    }

    /// In-neighbours (insertion order).
    pub fn in_neighbors(&self, i: usize) -> &[usize] {
        self.inc.get(i).map_or(&[], Vec::as_slice)
    }
}

/// PageRank result. When `converged` is false the ranks of the last iteration are returned
/// (v1 threw and failed the whole plan; v2 logs and continues).
#[derive(Debug, Clone, PartialEq)]
pub struct PageRank {
    pub ranks: Vec<f64>,
    pub iterations: usize,
    pub converged: bool,
}

/// v1 PageRank: all zero when there are no nodes or no edges; otherwise start at 1/n,
/// redistribute dangling mass uniformly, stop when Σ|Δ| < n·tolerance.
pub fn pagerank(g: &LinkGraph, params: GraphParams) -> PageRank {
    let p = params.sanitized();
    let n = g.node_count();
    if n == 0 || g.edge_count() == 0 {
        return PageRank {
            ranks: vec![0.0; n],
            iterations: 0,
            converged: true,
        };
    }
    let inv = 1.0 / n as f64;
    let mut rank = vec![inv; n];
    for it in 0..p.max_iterations {
        let dangling: f64 = g
            .out
            .iter()
            .zip(&rank)
            .filter(|(o, _)| o.is_empty())
            .map(|(_, r)| *r)
            .sum::<f64>()
            * p.alpha;
        let base = dangling * inv + (1.0 - p.alpha) * inv;
        let mut next = vec![base; n];
        for (targets, r) in g.out.iter().zip(&rank) {
            if targets.is_empty() {
                continue;
            }
            let c = p.alpha * r / targets.len() as f64;
            for &t in targets {
                if let Some(x) = next.get_mut(t) {
                    *x += c;
                }
            }
        }
        let err: f64 = next.iter().zip(&rank).map(|(a, b)| (a - b).abs()).sum();
        rank = next;
        if err < n as f64 * p.tolerance {
            return PageRank {
                ranks: rank,
                iterations: it + 1,
                converged: true,
            };
        }
    }
    tracing::warn!(nodes = n, "PageRank did not converge; using last iteration");
    PageRank {
        ranks: rank,
        iterations: p.max_iterations,
        converged: false,
    }
}

/// Weakly connected components: `(component id per node (1-based, numbered in node order),
/// component count)`.
pub fn weak_components(g: &LinkGraph) -> (Vec<usize>, usize) {
    let n = g.node_count();
    let mut comp = vec![0usize; n];
    let mut count = 0;
    let mut queue = VecDeque::new();
    for start in 0..n {
        if comp.get(start).copied().unwrap_or(1) != 0 {
            continue;
        }
        count += 1;
        if let Some(c) = comp.get_mut(start) {
            *c = count;
        }
        queue.push_back(start);
        while let Some(node) = queue.pop_front() {
            for &nb in g.out_neighbors(node).iter().chain(g.in_neighbors(node)) {
                if let Some(c) = comp.get_mut(nb)
                    && *c == 0
                {
                    *c = count;
                    queue.push_back(nb);
                }
            }
        }
    }
    (comp, count)
}

/// How the BFS roots were chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RootStrategy {
    /// Operator roots and/or the home page (nav targets at depth 1).
    Configured,
    /// No home page or configured root; nav-linked pages at depth 0.
    NavTargets,
    /// Single best hub (highest in-degree, shallowest URL, node order).
    Hub,
    /// Empty graph.
    None,
}

/// BFS seeds: `primary` start at depth 0, `secondary` at depth 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    pub primary: Vec<usize>,
    pub secondary: Vec<usize>,
    pub strategy: RootStrategy,
}

/// Choose BFS roots (fixes finding #8). `urls[i]` is node i's URL (missing → not a home page).
/// `configured` are operator root ids; `nav_targets` are ids linked from site navigation.
pub fn select_roots<S: AsRef<str>>(
    g: &LinkGraph,
    urls: &[S],
    configured: &[S],
    nav_targets: &[S],
) -> Roots {
    let mut primary: Vec<usize> = Vec::new();
    let mut seen = HashSet::new();
    for id in configured {
        if let Some(i) = g.index_of(id.as_ref())
            && seen.insert(i)
        {
            primary.push(i);
        }
    }
    for (i, u) in urls.iter().enumerate().take(g.node_count()) {
        if is_home_url(u.as_ref()) && seen.insert(i) {
            primary.push(i);
        }
    }
    let mut nav: Vec<usize> = Vec::new();
    for id in nav_targets {
        if let Some(i) = g.index_of(id.as_ref())
            && !seen.contains(&i)
            && !nav.contains(&i)
        {
            nav.push(i);
        }
    }
    nav.sort_unstable();
    if !primary.is_empty() {
        return Roots {
            primary,
            secondary: nav,
            strategy: RootStrategy::Configured,
        };
    }
    if !nav.is_empty() {
        return Roots {
            primary: nav,
            secondary: Vec::new(),
            strategy: RootStrategy::NavTargets,
        };
    }
    if g.node_count() == 0 {
        return Roots {
            primary: Vec::new(),
            secondary: Vec::new(),
            strategy: RootStrategy::None,
        };
    }
    let depth_of = |i: usize| {
        urls.get(i)
            .and_then(|u| path_segments(u.as_ref()))
            .unwrap_or(usize::MAX)
    };
    let mut best = 0usize;
    for i in 1..g.node_count() {
        let (bi, bd) = (g.in_neighbors(best).len(), depth_of(best));
        let (ci, cd) = (g.in_neighbors(i).len(), depth_of(i));
        if ci > bi || (ci == bi && cd < bd) {
            best = i;
        }
    }
    Roots {
        primary: vec![best],
        secondary: Vec::new(),
        strategy: RootStrategy::Hub,
    }
}

/// Multi-source BFS along out-edges. Unreached nodes get -1.
pub fn click_depths(g: &LinkGraph, roots: &Roots) -> Vec<i64> {
    let n = g.node_count();
    let mut depth = vec![-1i64; n];
    let mut queue = VecDeque::new();
    for (seeds, d) in [(&roots.primary, 0i64), (&roots.secondary, 1i64)] {
        for &r in seeds {
            if let Some(x) = depth.get_mut(r)
                && *x < 0
            {
                *x = d;
                queue.push_back(r);
            }
        }
    }
    // secondary seeds may be reached at depth 1 from a primary anyway; BFS order is still
    // valid because all depth-0 seeds are queued before depth-1 seeds.
    while let Some(node) = queue.pop_front() {
        let d = depth.get(node).copied().unwrap_or(0);
        for &nb in g.out_neighbors(node) {
            if let Some(x) = depth.get_mut(nb)
                && *x < 0
            {
                *x = d + 1;
                queue.push_back(nb);
            }
        }
    }
    depth
}

// ---------------------------------------------------------------------------------------------
// Full computation
// ---------------------------------------------------------------------------------------------

/// One post in the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNode {
    pub id: String,
    pub url: String,
}

/// Everything [`compute`] needs.
#[derive(Debug, Clone, Default)]
pub struct GraphInput {
    /// Posts, `ORDER BY id`.
    pub nodes: Vec<GraphNode>,
    /// Body (in-content) links: committed edges ∪ links parsed from content.
    pub edges: Vec<(String, String)>,
    /// Operator-configured BFS root ids.
    pub configured_roots: Vec<String>,
    /// Distinct navigational inbound sources per target id (link_health nav/header/footer/aside).
    pub nav_inbound: HashMap<String, usize>,
    pub params: GraphParams,
}

/// Per-node metrics (column names match `posts`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeMetric {
    pub id: String,
    pub pagerank: f64,
    pub component: usize,
    pub click_depth: i64,
    pub in_degree: usize,
    pub out_degree: usize,
    /// No inbound link of any provenance (body or nav).
    pub is_orphan: bool,
    pub is_dead_end: bool,
    /// No inbound body link.
    pub is_contextual_orphan: bool,
    /// Inbound links exist but all are navigational.
    pub is_nav_only: bool,
    pub body_inbound_count: usize,
    pub nav_inbound_count: usize,
}

/// Graph-wide summary.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GraphSummary {
    pub nodes: usize,
    pub edges: usize,
    pub orphans: usize,
    pub contextual_orphans: usize,
    pub nav_only_orphans: usize,
    pub nav_linked_posts: usize,
    pub dead_ends: usize,
    /// Mean depth over reached nodes, 2 decimals.
    pub avg_depth: f64,
    pub max_depth: i64,
    pub unreachable: usize,
    pub components: usize,
    pub pagerank_converged: bool,
    pub root_strategy: RootStrategy,
    pub dropped: BuildStats,
}

/// [`compute`] output.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GraphOutput {
    pub nodes: Vec<NodeMetric>,
    pub summary: GraphSummary,
}

/// Reachability flags for one node: `(is_orphan, is_contextual_orphan, is_nav_only)`.
pub fn classify_reachability(has_body_inbound: bool, has_nav_inbound: bool) -> (bool, bool, bool) {
    (
        !has_body_inbound && !has_nav_inbound,
        !has_body_inbound,
        !has_body_inbound && has_nav_inbound,
    )
}

/// Build the graph and compute every metric.
pub fn compute(input: &GraphInput) -> Result<GraphOutput, GraphError> {
    let ids: Vec<&str> = input.nodes.iter().map(|n| n.id.as_str()).collect();
    let urls: Vec<&str> = input.nodes.iter().map(|n| n.url.as_str()).collect();
    let (g, dropped) = LinkGraph::build(
        &ids,
        input.edges.iter().map(|(s, t)| (s.as_str(), t.as_str())),
    )?;
    let pr = pagerank(&g, input.params);
    let (comp, components) = weak_components(&g);
    let configured: Vec<&str> = input.configured_roots.iter().map(String::as_str).collect();
    let mut nav_targets: Vec<&str> = input
        .nav_inbound
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(id, _)| id.as_str())
        .collect();
    nav_targets.sort_unstable();
    let roots = select_roots(&g, &urls, &configured, &nav_targets);
    let depth = click_depths(&g, &roots);

    let mut s = GraphSummary {
        nodes: g.node_count(),
        edges: g.edge_count(),
        orphans: 0,
        contextual_orphans: 0,
        nav_only_orphans: 0,
        nav_linked_posts: 0,
        dead_ends: 0,
        avg_depth: 0.0,
        max_depth: 0,
        unreachable: 0,
        components,
        pagerank_converged: pr.converged,
        root_strategy: roots.strategy,
        dropped,
    };
    let (mut dsum, mut dcount) = (0i64, 0usize);
    let mut nodes = Vec::with_capacity(g.node_count());
    for (i, id) in g.ids().iter().enumerate() {
        let in_degree = g.in_neighbors(i).len();
        let out_degree = g.out_neighbors(i).len();
        let nav = input.nav_inbound.get(id).copied().unwrap_or(0);
        let (is_orphan, is_ctx, is_nav_only) = classify_reachability(in_degree > 0, nav > 0);
        let d = depth.get(i).copied().unwrap_or(-1);
        s.orphans += usize::from(is_orphan);
        s.contextual_orphans += usize::from(is_ctx);
        s.nav_only_orphans += usize::from(is_nav_only);
        s.nav_linked_posts += usize::from(nav > 0);
        s.dead_ends += usize::from(out_degree == 0);
        if d >= 0 {
            dsum += d;
            dcount += 1;
            s.max_depth = s.max_depth.max(d);
        } else {
            s.unreachable += 1;
        }
        nodes.push(NodeMetric {
            id: id.clone(),
            pagerank: pr.ranks.get(i).copied().unwrap_or(0.0),
            component: comp.get(i).copied().unwrap_or(0),
            click_depth: d,
            in_degree,
            out_degree,
            is_orphan,
            is_dead_end: out_degree == 0,
            is_contextual_orphan: is_ctx,
            is_nav_only,
            body_inbound_count: in_degree,
            nav_inbound_count: nav,
        });
    }
    if dcount > 0 {
        s.avg_depth = js_round(dsum as f64 / dcount as f64, 2);
    }
    Ok(GraphOutput { nodes, summary: s })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, url: &str) -> GraphNode {
        GraphNode {
            id: id.into(),
            url: url.into(),
        }
    }
    fn e(s: &str, t: &str) -> (String, String) {
        (s.into(), t.into())
    }

    #[test]
    fn url_keys_match_v1() {
        assert_eq!(
            url_path_key("https://example.com/blog/post/", None).as_deref(),
            Some("/blog/post")
        );
        assert_eq!(
            url_path_key("http://site01.example:8391/blog/post?utm=1#top", None).as_deref(),
            Some("/blog/post")
        );
        assert_eq!(
            url_path_key("https://example.com", None).as_deref(),
            Some("/")
        );
        assert_eq!(
            url_path_key("https://example.com/", None).as_deref(),
            Some("/")
        );
        assert_eq!(
            url_path_key("https://example.com/Bike%20Locks/", None).as_deref(),
            Some("/bike locks")
        );
        assert_eq!(
            url_path_key("https://example.com/bad%zz", None).as_deref(),
            Some("/bad%zz")
        );
        assert_eq!(
            url_path_key("https://example.com/%E2%82%AC", None).as_deref(),
            Some("/€")
        );
        assert_eq!(
            url_path_key("/other", Some("https://example.com/blog/post")).as_deref(),
            Some("/other")
        );
        assert_eq!(
            url_path_key("../sibling/", Some("https://example.com/blog/post/")).as_deref(),
            Some("/blog/sibling")
        );
        assert_eq!(url_path_key("not a url", None), None);
        assert_eq!(url_path_key("", None), None);
        // #23: plain permalinks keep their post id; other queries are still dropped.
        assert_eq!(
            url_path_key("https://example.com/?p=12", None).as_deref(),
            Some("/?p=12")
        );
        assert_eq!(
            url_path_key("http://www.example.com/index.php?utm=x&page_id=7#c", None).as_deref(),
            Some("/?page_id=7")
        );
        assert_eq!(
            url_path_key("https://example.com/?s=bike", None).as_deref(),
            Some("/")
        );
        assert_eq!(
            url_path_key("https://example.com/?p=12abc", None).as_deref(),
            Some("/")
        );
        assert_eq!(
            url_path_key("https://example.com/blog/?p=12", None).as_deref(),
            Some("/blog")
        );
        assert_eq!(
            url_host_key("https://WWW.Example.com:8443/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            url_host_key("http://site01.example:8391/x").as_deref(),
            Some("site01.example")
        );
        assert_eq!(url_host_key("/relative/path"), None);
        assert_eq!(url_host_key("nonsense"), None);
        assert_eq!(
            configured_host("https://www.Bike.com:8080/blog").as_deref(),
            Some("bike.com")
        );
        let hosts = site_hosts(
            "bike.com",
            ["https://www.bike.com/a", "http://clone.local:81/b"],
        );
        assert_eq!(
            hosts.into_iter().collect::<Vec<_>>(),
            vec!["bike.com", "clone.local"]
        );
        assert!(is_home_url("https://bike.com"));
        assert!(!is_home_url("https://bike.com/?p=12"));
        assert!(!is_home_url("https://bike.com/post"));
    }

    #[test]
    fn computes_directed_metrics_like_v1() {
        let input = GraphInput {
            nodes: vec![
                node("home", "https://x.com/"),
                node("a", "https://x.com/a"),
                node("b", "https://x.com/b"),
                node("island", "https://x.com/island"),
            ],
            edges: vec![
                e("home", "a"),
                e("a", "b"),
                e("a", "b"),
                e("b", "b"),
                e("zz", "a"),
            ],
            ..Default::default()
        };
        let r = compute(&input).unwrap();
        assert_eq!(r.summary.edges, 2);
        assert_eq!(r.summary.components, 2);
        assert_eq!(r.summary.orphans, 2);
        assert_eq!(r.summary.dead_ends, 2);
        assert_eq!(r.summary.avg_depth, 1.0);
        assert_eq!(
            r.summary.dropped,
            BuildStats {
                self_loops: 1,
                duplicates: 1,
                unknown_endpoints: 1
            }
        );
        assert_eq!(
            r.nodes.iter().map(|n| n.click_depth).collect::<Vec<_>>(),
            vec![0, 1, 2, -1]
        );
        let sum: f64 = r.nodes.iter().map(|n| n.pagerank).sum();
        assert!((sum - 1.0).abs() < 1e-9, "pagerank sums to {sum}");
        assert!(r.summary.pagerank_converged);
    }

    #[test]
    fn pagerank_sums_to_one_on_a_larger_graph() {
        let n = 300;
        let ids: Vec<String> = (0..n).map(|i| format!("p{i:03}")).collect();
        let edges: Vec<(String, String)> = (0..n)
            .flat_map(|i| [(i, (i * 7 + 3) % n), (i, (i * 13 + 1) % n)])
            .filter(|(i, _)| i % 11 != 0) // some dangling nodes
            .map(|(a, b)| (ids[a].clone(), ids[b].clone()))
            .collect();
        let (g, _) = LinkGraph::build(&ids, edges.iter().map(|(a, b)| (a, b))).unwrap();
        let pr = pagerank(&g, GraphParams::default());
        assert!(pr.converged);
        assert!((pr.ranks.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        let edgeless = LinkGraph::build(&ids, std::iter::empty::<(&str, &str)>())
            .unwrap()
            .0;
        assert!(
            pagerank(&edgeless, GraphParams::default())
                .ranks
                .iter()
                .all(|r| *r == 0.0)
        );
    }

    /// Finding #8: on a /%postname%/ site every post has one path segment. v1 made all of them
    /// roots (depth 0 everywhere). Now the home page is the only root.
    #[test]
    fn click_depth_root_fix() {
        let input = GraphInput {
            nodes: vec![
                node("a", "https://x.com/post-a/"),
                node("b", "https://x.com/post-b/"),
                node("c", "https://x.com/post-c/"),
                node("home", "https://x.com/"),
            ],
            edges: vec![e("home", "a"), e("a", "b"), e("b", "c")],
            ..Default::default()
        };
        let r = compute(&input).unwrap();
        let depth: HashMap<&str, i64> = r
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n.click_depth))
            .collect();
        assert_eq!(depth["home"], 0);
        assert_eq!(depth["a"], 1);
        assert_eq!(depth["b"], 2);
        assert_eq!(depth["c"], 3);
        assert_eq!(r.summary.root_strategy, RootStrategy::Configured);
        assert_eq!(r.summary.avg_depth, 1.5);
        assert_eq!(r.summary.max_depth, 3);

        // no home page: nav targets become roots
        let mut no_home = input.clone();
        no_home.nodes.pop();
        no_home.edges.remove(0);
        no_home.nav_inbound.insert("b".into(), 4);
        let r = compute(&no_home).unwrap();
        assert_eq!(r.summary.root_strategy, RootStrategy::NavTargets);
        let d: Vec<i64> = r.nodes.iter().map(|n| n.click_depth).collect();
        assert_eq!(d, vec![-1, 0, 1]);
        // nav-linked b is not an orphan but is a contextual orphan only if no body link: a→b exists
        assert!(!r.nodes[1].is_orphan && !r.nodes[1].is_contextual_orphan);
        assert!(r.nodes[0].is_orphan);

        // nothing at all: single hub = highest in-degree
        let hub = GraphInput {
            nodes: vec![
                node("a", "https://x.com/a/"),
                node("b", "https://x.com/b/"),
                node("c", "https://x.com/c/"),
            ],
            edges: vec![e("a", "b"), e("c", "b"), e("b", "c")],
            ..Default::default()
        };
        let r = compute(&hub).unwrap();
        assert_eq!(r.summary.root_strategy, RootStrategy::Hub);
        assert_eq!(
            r.nodes.iter().map(|n| n.click_depth).collect::<Vec<_>>(),
            vec![-1, 0, 1]
        );

        // configured roots win over the home page and nav targets sit at depth 1
        let mut cfg = input.clone();
        cfg.configured_roots = vec!["c".into()];
        cfg.nav_inbound.insert("b".into(), 1);
        let r = compute(&cfg).unwrap();
        let depth: HashMap<&str, i64> = r
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n.click_depth))
            .collect();
        assert_eq!(
            (depth["c"], depth["home"], depth["b"], depth["a"]),
            (0, 0, 1, 1)
        );
    }

    #[test]
    fn reachability_flags() {
        assert_eq!(classify_reachability(true, false), (false, false, false));
        assert_eq!(classify_reachability(false, true), (false, true, true));
        assert_eq!(classify_reachability(true, true), (false, false, false));
        assert_eq!(classify_reachability(false, false), (true, true, false));
    }

    #[test]
    fn hostile_inputs_never_panic() {
        let r = compute(&GraphInput::default()).unwrap();
        assert_eq!(r.summary.nodes, 0);
        assert_eq!(r.summary.root_strategy, RootStrategy::None);
        let dup = GraphInput {
            nodes: vec![node("a", "x"), node("a", "y")],
            ..Default::default()
        };
        assert!(matches!(compute(&dup), Err(GraphError::DuplicateNode(_))));
        let bad = GraphInput {
            nodes: vec![node("", "x")],
            ..Default::default()
        };
        assert_eq!(compute(&bad).unwrap_err(), GraphError::BadNodeId);
        let weird = GraphInput {
            nodes: vec![node("a", "::::"), node("b", "")],
            edges: vec![e("a", "b"), e("b", "a")],
            params: GraphParams {
                alpha: f64::NAN,
                max_iterations: 0,
                tolerance: -1.0,
            },
            ..Default::default()
        };
        let r = compute(&weird).unwrap();
        assert_eq!(r.summary.root_strategy, RootStrategy::Hub);
        assert!((r.nodes.iter().map(|n| n.pagerank).sum::<f64>() - 1.0).abs() < 1e-9);
        assert_eq!(url_path_key("mailto:a@b.c", None), None);
        assert_eq!(url_path_key("%%%", Some("not a base")), None);
        assert_eq!(path_segments("garbage"), None);
    }
}
