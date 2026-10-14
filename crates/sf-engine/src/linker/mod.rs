//! Keyword-rule linker (v1 `rules/keywordLinker.ts`) as a pure function.
//!
//! For each active rule (in the given order) the target is resolved by normalized URL (v1
//! compared raw strings, B-20), and source posts (in the given order — callers sort by id) get
//! at most one edge each, placed in the first paragraph containing the keyword on a word
//! boundary, case-insensitively, outside existing links/headings/code (paragraph `blocked`
//! spans). Never a self-link, never two new links in one paragraph, never a pair that is
//! already linked, at most `max_links` edges per rule. Ignore phrases match on word boundaries.
//!
//! Spec: `docs/research/01-engine-spec.md` §15; bugs B-18, B-20.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sf_core::types::AnchorType;

use crate::anchor::{find_unblocked, is_ignored};
use crate::html::{normalize_url, normalize_ws, utf16_len};

/// Baseline score of keyword-rule edges (v1).
pub const KEYWORD_EDGE_SCORE: f64 = 0.5;
/// Anchor column cap (UTF-16 units).
pub const MAX_ANCHOR_UNITS: usize = 255;

/// One canonical paragraph of a source post.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkerParagraph {
    pub idx: usize,
    pub text: String,
    /// `html::Paragraph::blocked` spans (empty if unknown).
    #[serde(default)]
    pub blocked: Vec<(usize, usize)>,
}

/// A post as the linker sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkerPost {
    pub id: String,
    pub url: String,
    /// Only published posts are used as sources (v1 `post_status = 'publish'`).
    pub published: bool,
    /// Paragraphs ordered by `idx`.
    pub paragraphs: Vec<LinkerParagraph>,
}

/// A keyword rule (`keyword_rules` row).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeywordRule {
    pub id: String,
    pub keyword: String,
    pub target_url: String,
    pub anchor_type: AnchorType,
    pub max_links: usize,
    pub active: bool,
}

/// A suggested edge produced by a rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeywordEdge {
    pub rule_id: String,
    pub source_post_id: String,
    pub target_post_id: String,
    pub paragraph_idx: usize,
    /// The rule keyword (as v1 stores it), truncated to 255 UTF-16 units.
    pub anchor: String,
    /// The exact matched text in the paragraph (original casing).
    pub surface: String,
    pub anchor_type: AnchorType,
    pub composite_score: f64,
}

/// Extra constraints.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinkerOptions {
    /// (source_post_id, target_post_id) pairs that already have a live/rejected edge.
    pub existing_pairs: HashSet<(String, String)>,
    /// (source_post_id, paragraph_idx) already holding a planned/applied new link.
    pub used_paragraphs: HashSet<(String, usize)>,
    /// Optional cap of keyword edges per source post (None = unlimited).
    pub max_per_source: Option<usize>,
}

/// v1 `wordBoundaryHit`: `(^|[^a-z0-9])kw([^a-z0-9]|$)`, case-insensitive.
pub fn word_boundary_hit(haystack: &str, needle: &str) -> bool {
    let n = normalize_ws(needle).to_lowercase();
    !n.is_empty() && crate::anchor::contains_word(&haystack.to_lowercase(), &n)
}

fn truncate_units(s: &str, max: usize) -> String {
    if utf16_len(s) <= max {
        return s.to_string();
    }
    let mut units = 0;
    s.chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= max
        })
        .collect()
}

/// Run all rules. Deterministic for a given input order.
pub fn run_keyword_linker(
    posts: &[LinkerPost],
    rules: &[KeywordRule],
    ignore: &[String],
    opts: &LinkerOptions,
) -> Vec<KeywordEdge> {
    let mut by_url: HashMap<String, usize> = HashMap::new();
    for (i, p) in posts.iter().enumerate() {
        by_url.entry(normalize_url(&p.url)).or_insert(i);
    }
    let mut pairs = opts.existing_pairs.clone();
    let mut used_paras = opts.used_paragraphs.clone();
    let mut per_source: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::new();

    for rule in rules {
        let kw = normalize_ws(&rule.keyword);
        if !rule.active
            || kw.is_empty()
            || rule.max_links == 0
            || is_ignored(&kw.to_lowercase(), ignore)
        {
            continue;
        }
        let Some(target) = by_url
            .get(&normalize_url(&rule.target_url))
            .and_then(|&i| posts.get(i))
        else {
            continue;
        };
        let mut linked = 0usize;
        for src in posts {
            if linked >= rule.max_links {
                break;
            }
            if !src.published
                || src.id == target.id
                || normalize_url(&src.url) == normalize_url(&target.url)
            {
                continue;
            }
            if pairs.contains(&(src.id.clone(), target.id.clone())) {
                continue;
            }
            if opts
                .max_per_source
                .is_some_and(|m| per_source.get(&src.id).copied().unwrap_or(0) >= m)
            {
                continue;
            }
            let hit = src.paragraphs.iter().find_map(|p| {
                if used_paras.contains(&(src.id.clone(), p.idx)) {
                    return None;
                }
                find_unblocked(&p.text, &kw, &p.blocked).map(|(s, e)| (p, s, e))
            });
            let Some((para, s, e)) = hit else { continue };
            pairs.insert((src.id.clone(), target.id.clone()));
            used_paras.insert((src.id.clone(), para.idx));
            *per_source.entry(src.id.clone()).or_insert(0) += 1;
            linked += 1;
            out.push(KeywordEdge {
                rule_id: rule.id.clone(),
                source_post_id: src.id.clone(),
                target_post_id: target.id.clone(),
                paragraph_idx: para.idx,
                anchor: truncate_units(&kw, MAX_ANCHOR_UNITS),
                surface: para.text.get(s..e).unwrap_or(&kw).to_string(),
                anchor_type: rule.anchor_type.clone(),
                composite_score: KEYWORD_EDGE_SCORE,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(id: &str, url: &str, paras: &[&str]) -> LinkerPost {
        LinkerPost {
            id: id.into(),
            url: url.into(),
            published: true,
            paragraphs: paras
                .iter()
                .enumerate()
                .map(|(i, t)| LinkerParagraph {
                    idx: i,
                    text: (*t).into(),
                    blocked: vec![],
                })
                .collect(),
        }
    }

    fn rule(id: &str, kw: &str, url: &str, max: usize) -> KeywordRule {
        KeywordRule {
            id: id.into(),
            keyword: kw.into(),
            target_url: url.into(),
            anchor_type: AnchorType::Exact,
            max_links: max,
            active: true,
        }
    }

    fn posts() -> Vec<LinkerPost> {
        vec![
            post(
                "t",
                "https://www.ex.com/bike-locks/",
                &["All about the bike lock."],
            ),
            post(
                "a",
                "https://ex.com/a",
                &["Nothing here.", "Buy a Bike Lock today."],
            ),
            post(
                "b",
                "https://ex.com/b",
                &["Bike locksmith services and a cartoon."],
            ),
            post(
                "c",
                "https://ex.com/c",
                &["The bike lock again.", "Another bike lock mention."],
            ),
            post("d", "https://ex.com/d", &["Bike lock in the last post."]),
        ]
    }

    #[test]
    fn links_with_word_boundaries_caps_and_no_self_links() {
        let edges = run_keyword_linker(
            &posts(),
            &[rule("r1", "bike lock", "https://ex.com/bike-locks", 2)],
            &[],
            &LinkerOptions::default(),
        );
        let got: Vec<(&str, usize, &str)> = edges
            .iter()
            .map(|e| {
                (
                    e.source_post_id.as_str(),
                    e.paragraph_idx,
                    e.surface.as_str(),
                )
            })
            .collect();
        assert_eq!(got, vec![("a", 1, "Bike Lock"), ("c", 0, "bike lock")]);
        assert!(edges.iter().all(|e| e.target_post_id == "t"
            && e.anchor == "bike lock"
            && e.composite_score == 0.5));
    }

    #[test]
    fn one_link_per_paragraph_existing_pairs_ignore_and_inactive() {
        let p = posts();
        let rules = vec![
            rule("r1", "bike lock", "https://ex.com/bike-locks", 10),
            rule("r2", "bike lock", "https://ex.com/d", 10),
        ];
        let mut opts = LinkerOptions::default();
        opts.existing_pairs.insert(("a".into(), "t".into()));
        let edges = run_keyword_linker(&p, &rules, &[], &opts);
        // r1: a skipped (existing), c gets paragraph 0, d gets paragraph 0.
        // r2 (target d): a -> para 1, c -> para 1 (para 0 used), t -> para 0; d itself skipped.
        let got: Vec<(&str, &str, usize)> = edges
            .iter()
            .map(|e| {
                (
                    e.rule_id.as_str(),
                    e.source_post_id.as_str(),
                    e.paragraph_idx,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("r1", "c", 0),
                ("r1", "d", 0),
                ("r2", "t", 0),
                ("r2", "a", 1),
                ("r2", "c", 1)
            ]
        );
        // ignore list is word-boundary: "lock" ignores "bike lock" but "loc" does not
        assert!(
            run_keyword_linker(&p, &rules, &["lock".into()], &LinkerOptions::default()).is_empty()
        );
        assert!(
            !run_keyword_linker(&p, &rules, &["loc".into()], &LinkerOptions::default()).is_empty()
        );
        let mut inactive = rules.clone();
        inactive.iter_mut().for_each(|r| r.active = false);
        assert!(run_keyword_linker(&p, &inactive, &[], &LinkerOptions::default()).is_empty());
        // unknown target / zero max
        assert!(
            run_keyword_linker(
                &p,
                &[rule("x", "bike lock", "https://ex.com/zzz", 5)],
                &[],
                &LinkerOptions::default()
            )
            .is_empty()
        );
        assert!(
            run_keyword_linker(
                &p,
                &[rule("x", "bike lock", "https://ex.com/d", 0)],
                &[],
                &LinkerOptions::default()
            )
            .is_empty()
        );
    }

    #[test]
    fn blocked_spans_and_unpublished_respected() {
        let mut p = posts();
        // c's first paragraph has the keyword inside an existing link
        if let Some(c) = p.get_mut(3) {
            c.paragraphs[0].blocked = vec![(4, 13)];
        }
        if let Some(a) = p.get_mut(1) {
            a.published = false;
        }
        let edges = run_keyword_linker(
            &p,
            &[rule("r", "bike lock", "https://ex.com/bike-locks/", 5)],
            &[],
            &LinkerOptions {
                max_per_source: Some(1),
                ..LinkerOptions::default()
            },
        );
        let got: Vec<(&str, usize)> = edges
            .iter()
            .map(|e| (e.source_post_id.as_str(), e.paragraph_idx))
            .collect();
        assert_eq!(got, vec![("c", 1), ("d", 0)]);
        assert!(word_boundary_hit("A Bike-Lock", "bike"));
        assert!(!word_boundary_hit("cartoon", "cart"));
    }

    #[test]
    fn hostile_input_never_panics() {
        let big = "bike lock ".repeat(30_000);
        let p = vec![
            post("x", "not a url", &[&big, ""]),
            post("y", "", &["\u{0}"]),
        ];
        let rules = vec![
            rule("r", &big, "not a url", usize::MAX),
            rule("e", "", "", 1),
            rule("u", "İ", "", 3),
        ];
        let _ = run_keyword_linker(
            &p,
            &rules,
            &[big.clone(), String::new()],
            &LinkerOptions::default(),
        );
        let _ = run_keyword_linker(&[], &[], &[], &LinkerOptions::default());
    }
}
