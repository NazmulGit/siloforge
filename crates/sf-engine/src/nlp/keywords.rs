//! Layered keyword extraction with 1–3-grams (replaces v1's unigram-only extractor, bug B-9).
//!
//! Layers and base weights follow v1 (`keywords/extract.ts`): title 1.0, htag 0.8, meta 0.7,
//! body 0.4 (top 15 by frequency). Candidate n-grams come from stopword-bounded chunks using the
//! vendored v1 SMART stop list, so "best bike locks for 2026" yields `bike locks`, `bike`, `locks`.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use super::stopwords::is_stopword;
use super::tokenize::{TokenKind, norm_word, stem, tokenize};
use crate::html::utf16_len;

/// Base layer weights (v1 `LAYER_WEIGHTS`).
pub const TITLE_WEIGHT: f64 = 1.0;
pub const HTAG_WEIGHT: f64 = 0.8;
pub const META_WEIGHT: f64 = 0.7;
pub const BODY_WEIGHT: f64 = 0.4;
/// Body keywords kept per post (v1 `BODY_TOP_N`).
pub const BODY_TOP_N: usize = 15;
/// Largest n-gram.
pub const MAX_NGRAM: usize = 3;
/// Keyword length cap in UTF-16 units (DB column, v1).
pub const MAX_KEYWORD_UNITS: usize = 255;

/// Keyword source layer (DB `keywords.source` values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeywordSource {
    Title,
    Htag,
    Meta,
    Body,
}

impl KeywordSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Htag => "htag",
            Self::Meta => "meta",
            Self::Body => "body",
        }
    }
}

/// One extracted keyword.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Keyword {
    pub keyword: String,
    pub source: KeywordSource,
    /// Score in (0, 1].
    pub score: f64,
}

/// Input for [`extract_keywords`].
#[derive(Debug, Clone, Copy, Default)]
pub struct KeywordInput<'a> {
    pub title: &'a str,
    /// h2/h3 texts (v1 `meta.h_tags`).
    pub headings: &'a [String],
    pub meta_description: &'a str,
    /// Visible body text.
    pub body: &'a str,
}

/// v1 `extractTerms` parity: lowercase unigrams, stopwords and digit-only words removed, len > 2.
pub fn extract_terms(text: &str) -> Vec<String> {
    chunks(text)
        .into_iter()
        .flatten()
        .filter(|w| w.chars().count() > 2)
        .collect()
}

/// Stopword/punctuation-bounded word chunks (lowercase).
fn chunks(text: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    for t in tokenize(text) {
        match t.kind {
            TokenKind::Word => {
                let w = norm_word(t.text);
                if is_stopword(&w) || w.chars().count() < 2 {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                } else {
                    cur.push(w);
                }
            }
            // v1 removes digit-only words; they also end a chunk.
            TokenKind::Number | TokenKind::Punct => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// All 1..=`max_n` grams of stopword-bounded chunks, in document order (with repeats).
/// Unigrams must be longer than 2 chars.
pub fn ngrams(text: &str, max_n: usize) -> Vec<String> {
    let max_n = max_n.clamp(1, MAX_NGRAM);
    let mut out = Vec::new();
    for ch in chunks(text) {
        for s in 0..ch.len() {
            for n in 1..=max_n {
                let Some(words) = ch.get(s..s + n) else { break };
                if n == 1 && words.first().is_some_and(|w| w.chars().count() <= 2) {
                    continue;
                }
                out.push(words.join(" "));
            }
        }
    }
    out
}

fn ngram_factor(gram: &str) -> f64 {
    match gram.split(' ').count() {
        1 => 0.8,
        2 => 0.9,
        _ => 1.0,
    }
}

fn truncate_units(s: &str) -> String {
    if utf16_len(s) <= MAX_KEYWORD_UNITS {
        return s.to_string();
    }
    let mut units = 0;
    s.chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= MAX_KEYWORD_UNITS
        })
        .collect()
}

/// Stem key used to merge inflections ("bike lock" / "bike locks").
pub fn stem_key(gram: &str) -> String {
    gram.split(' ').map(stem).collect::<Vec<_>>().join(" ")
}

/// Layered 1–3-gram keywords. Output order: title, htag, meta, body; within a layer score
/// desc, then more words first, then lexical — fully deterministic.
pub fn extract_keywords(input: &KeywordInput<'_>) -> Vec<Keyword> {
    let mut out = Vec::new();
    let heading_text = input
        .headings
        .iter()
        .take(200)
        .cloned()
        .collect::<Vec<_>>()
        .join(" . ");
    for (src, text, weight) in [
        (KeywordSource::Title, input.title, TITLE_WEIGHT),
        (KeywordSource::Htag, heading_text.as_str(), HTAG_WEIGHT),
        (KeywordSource::Meta, input.meta_description, META_WEIGHT),
    ] {
        let mut layer: BTreeMap<String, f64> = BTreeMap::new();
        for g in ngrams(text, MAX_NGRAM) {
            let g = truncate_units(&g);
            let s = weight * ngram_factor(&g);
            let e = layer.entry(g).or_insert(0.0);
            if s > *e {
                *e = s;
            }
        }
        push_sorted(&mut out, src, layer.into_iter().collect());
    }

    // Body: frequency over grams, multi-word grams need >= 2 hits, inflections merged by stem.
    let mut counts: HashMap<String, (usize, BTreeMap<String, usize>)> = HashMap::new();
    for g in ngrams(input.body, MAX_NGRAM) {
        let key = stem_key(&g);
        let e = counts.entry(key).or_insert_with(|| (0, BTreeMap::new()));
        e.0 += 1;
        *e.1.entry(g).or_insert(0) += 1;
    }
    let mut scored: Vec<(String, f64)> = counts
        .into_values()
        .filter_map(|(count, surfaces)| {
            // Most frequent surface form; ties → lexical smallest (BTreeMap order).
            let (surface, _) =
                surfaces
                    .iter()
                    .fold(None::<(&String, usize)>, |best, (s, &c)| match best {
                        Some((_, bc)) if bc >= c => best,
                        _ => Some((s, c)),
                    })?;
            let n = surface.split(' ').count();
            if n > 1 && count < 2 {
                return None;
            }
            let boost = match n {
                1 => 1.0,
                2 => 1.6,
                _ => 2.0,
            };
            Some((truncate_units(surface), count as f64 * boost))
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.truncate(BODY_TOP_N);
    let max = scored
        .first()
        .map(|s| s.1)
        .unwrap_or(1.0)
        .max(f64::MIN_POSITIVE);
    let body: Vec<(String, f64)> = scored
        .into_iter()
        .map(|(k, v)| (k, round4(BODY_WEIGHT * v / max)))
        .collect();
    push_sorted(&mut out, KeywordSource::Body, body);
    out
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

fn push_sorted(out: &mut Vec<Keyword>, source: KeywordSource, mut items: Vec<(String, f64)>) {
    items.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| b.0.split(' ').count().cmp(&a.0.split(' ').count()))
            .then_with(|| a.0.cmp(&b.0))
    });
    out.extend(items.into_iter().map(|(keyword, score)| Keyword {
        keyword,
        source,
        score: round4(score),
    }));
}

/// Deterministic top keyword (spec C.2): score desc, then word count desc, then lexical.
pub fn top_keyword<'a>(keywords: impl IntoIterator<Item = (&'a str, f64)>) -> Option<&'a str> {
    keywords
        .into_iter()
        .filter(|(k, _)| !k.trim().is_empty())
        .fold(None::<(&str, f64)>, |best, cur| match best {
            None => Some(cur),
            Some(b) => {
                let ord = cur
                    .1
                    .total_cmp(&b.1)
                    .then_with(|| {
                        cur.0
                            .split_whitespace()
                            .count()
                            .cmp(&b.0.split_whitespace().count())
                    })
                    .then_with(|| b.0.cmp(cur.0));
                if ord == std::cmp::Ordering::Greater {
                    Some(cur)
                } else {
                    Some(b)
                }
            }
        })
        .map(|(k, _)| k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unigram_terms_like_v1() {
        assert_eq!(
            extract_terms("The 11 Best Bike Trunk Bags for 2026"),
            vec!["bike", "trunk", "bags"]
        );
    }

    #[test]
    fn ngrams_bounded_by_stopwords() {
        let g = ngrams("best bike trunk bags for commuters", 3);
        assert!(g.contains(&"bike trunk bags".to_string()));
        assert!(g.contains(&"commuters".to_string()));
        assert!(!g.iter().any(|x| x.contains("for")));
    }

    #[test]
    fn layered_extraction() {
        let h = vec!["Choosing a Steel Bike Lock".to_string()];
        let body = "A steel bike lock is heavy. Every steel bike lock we tested resisted bolt cutters. \
                    Bolt cutters are the main threat. Steel bike locks win.";
        let kws = extract_keywords(&KeywordInput {
            title: "Best Steel Bike Locks for 2026",
            headings: &h,
            meta_description: "Tested steel bike locks.",
            body,
        });
        let first = kws.first().map(|k| (k.keyword.as_str(), k.source));
        assert_eq!(first, Some(("steel bike locks", KeywordSource::Title)));
        let body_kws: Vec<&Keyword> = kws
            .iter()
            .filter(|k| k.source == KeywordSource::Body)
            .collect();
        assert!(body_kws.len() <= BODY_TOP_N);
        assert_eq!(
            body_kws.first().map(|k| k.keyword.as_str()),
            Some("steel bike lock")
        );
        assert!((body_kws.first().map(|k| k.score).unwrap_or(0.0) - 0.4).abs() < 1e-9);
        // deterministic
        assert_eq!(
            kws,
            extract_keywords(&KeywordInput {
                title: "Best Steel Bike Locks for 2026",
                headings: &h,
                meta_description: "Tested steel bike locks.",
                body,
            })
        );
    }

    #[test]
    fn top_keyword_tie_breaks() {
        let kws = [
            ("bike", 1.0),
            ("bike locks", 1.0),
            ("steel", 0.9),
            ("abc locks", 1.0),
        ];
        assert_eq!(
            top_keyword(kws.iter().map(|(k, s)| (*k, *s))),
            Some("abc locks")
        );
        assert_eq!(top_keyword(std::iter::empty()), None);
    }
}
