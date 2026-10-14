//! Search intent: rule-based query intent classification and the v1 query→page matcher
//! (`intent/match.ts::bestPostForQuery`, never called in v1; bug B-25 fixed: the volume factor
//! is reported but cannot change the argmax, and no `Math.max(...spread)` stack overflow).

use serde::{Deserialize, Serialize};

use super::tokenize::{TokenKind, norm_word, tokenize};

/// Coarse search intent of a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchIntent {
    Informational,
    Commercial,
    Transactional,
    Navigational,
}

impl SearchIntent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Informational => "informational",
            Self::Commercial => "commercial",
            Self::Transactional => "transactional",
            Self::Navigational => "navigational",
        }
    }
}

const TRANSACTIONAL: &[&str] = &[
    "buy",
    "order",
    "purchase",
    "price",
    "prices",
    "pricing",
    "cheap",
    "cheapest",
    "deal",
    "deals",
    "discount",
    "coupon",
    "coupons",
    "sale",
    "shop",
    "shipping",
    "subscribe",
    "download",
    "booking",
    "book",
    "hire",
    "quote",
    "cost",
    "costs",
    "near",
];
const COMMERCIAL: &[&str] = &[
    "best",
    "top",
    "review",
    "reviews",
    "vs",
    "versus",
    "compare",
    "comparison",
    "alternative",
    "alternatives",
    "rated",
    "ranking",
    "rankings",
    "recommended",
    "worth",
];
const INFORMATIONAL: &[&str] = &[
    "how",
    "what",
    "why",
    "when",
    "where",
    "who",
    "which",
    "guide",
    "tutorial",
    "tips",
    "ideas",
    "examples",
    "meaning",
    "definition",
    "learn",
    "steps",
    "does",
    "can",
    "is",
    "are",
    "should",
];
const NAVIGATIONAL: &[&str] = &[
    "login", "log", "sign", "signin", "account", "contact", "official", "website", "app",
];

/// Classify a search query. Precedence: navigational (brand/login words) → transactional →
/// commercial → informational (default).
pub fn classify_intent(query: &str, brand: Option<&str>) -> SearchIntent {
    let words: Vec<String> = tokenize(query)
        .into_iter()
        .filter(|t| t.kind == TokenKind::Word)
        .take(64)
        .map(|t| norm_word(t.text))
        .collect();
    let has = |list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    let brand_hit = brand
        .map(|b| b.trim().to_lowercase())
        .filter(|b| !b.is_empty())
        .is_some_and(|b| words.contains(&b));
    if has(NAVIGATIONAL) || (brand_hit && words.len() <= 2) {
        return SearchIntent::Navigational;
    }
    if has(TRANSACTIONAL) {
        return SearchIntent::Transactional;
    }
    if has(COMMERCIAL) {
        return SearchIntent::Commercial;
    }
    if has(INFORMATIONAL) {
        return SearchIntent::Informational;
    }
    SearchIntent::Informational
}

/// Result of [`best_post_for_query`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntentMatch {
    pub post_id: String,
    /// Raw cosine similarity.
    pub cosine: f64,
    /// `cosine * sqrt(volume / max_volume)` (v1 score).
    pub score: f64,
}

fn cosine(a: &[f32], b: &[f32]) -> Option<f64> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let d = na.sqrt() * nb.sqrt();
    if d == 0.0 || !d.is_finite() {
        return None;
    }
    Some(dot / d)
}

/// Best post for a query vector (argmax cosine; first wins ties). Posts whose vector has a
/// different dimension are skipped. `None` when no post or `max_volume <= 0`.
pub fn best_post_for_query(
    query: &[f32],
    volume: f64,
    max_volume: f64,
    posts: &[(&str, &[f32])],
) -> Option<IntentMatch> {
    if posts.is_empty() || max_volume <= 0.0 || !max_volume.is_finite() {
        return None;
    }
    let factor = (volume.max(0.0) / max_volume).sqrt();
    let mut best: Option<IntentMatch> = None;
    for (id, v) in posts {
        let Some(c) = cosine(query, v) else { continue };
        if best.as_ref().is_none_or(|b| c > b.cosine) {
            best = Some(IntentMatch {
                post_id: (*id).to_string(),
                cosine: c,
                score: c * factor,
            });
        }
    }
    best
}

/// Max volume without spreading into a call (v1 overflowed on huge keyword sets).
pub fn max_volume(volumes: impl IntoIterator<Item = f64>) -> f64 {
    volumes
        .into_iter()
        .filter(|v| v.is_finite())
        .fold(0.0, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies() {
        assert_eq!(
            classify_intent("how to fit a bike lock", None),
            SearchIntent::Informational
        );
        assert_eq!(
            classify_intent("best bike locks 2026", None),
            SearchIntent::Commercial
        );
        assert_eq!(
            classify_intent("buy kryptonite lock", None),
            SearchIntent::Transactional
        );
        assert_eq!(
            classify_intent("example login", None),
            SearchIntent::Navigational
        );
        assert_eq!(
            classify_intent("example", Some("Example")),
            SearchIntent::Navigational
        );
        assert_eq!(classify_intent("", None), SearchIntent::Informational);
    }

    #[test]
    fn best_post() {
        let q = [1.0f32, 0.0];
        let a = [0.0f32, 1.0];
        let b = [0.9f32, 0.1];
        let bad = [1.0f32];
        let m =
            best_post_for_query(&q, 50.0, 200.0, &[("a", &a), ("bad", &bad), ("b", &b)]).unwrap();
        assert_eq!(m.post_id, "b");
        assert!((m.score - m.cosine * 0.5).abs() < 1e-12);
        assert!(best_post_for_query(&q, 1.0, 0.0, &[("a", &a)]).is_none());
        assert_eq!(max_volume([1.0, f64::NAN, 7.0]), 7.0);
    }
}
