//! Text NLP: tokenizer, stopwords, Porter2 stemming, 1–3-gram layered keywords, heuristic
//! noun phrases, search intent, and a per-paragraph analysis that jobs can memoize by hash.
//!
//! All inputs are truncated to [`MAX_NLP_BYTES`] (at a char boundary) so cost is bounded.
//! Spec: `docs/research/01-engine-spec.md` §3, §10.1; accuracy C.2, C.4; bugs B-9, B-25, B-29.

pub mod intent;
pub mod keywords;
pub mod phrases;
pub mod pos;
pub mod stopwords;
pub mod terms;
pub mod tokenize;

use serde::{Deserialize, Serialize};

pub use intent::{IntentMatch, SearchIntent, best_post_for_query, classify_intent, max_volume};
pub use keywords::{
    Keyword, KeywordInput, KeywordSource, extract_keywords, extract_terms, ngrams, top_keyword,
};
pub use phrases::{NounPhrase, noun_phrases, noun_phrases_with, sentence_spans};
pub use stopwords::{LISTICLE_WORDS, is_listicle, is_stopword};
pub use terms::{SiteTerms, TermsBuilder};
pub use tokenize::{Token, TokenKind, content_tokens, norm_word, stem, tokenize};

/// Max bytes of text any NLP function looks at.
pub const MAX_NLP_BYTES: usize = 256 * 1024;
/// Bump when phrase extraction / scoring inputs change so cached [`ParagraphNlp`] rows are rebuilt.
pub const NLP_VERSION: u32 = 2;

/// Truncate to [`MAX_NLP_BYTES`] at a char boundary.
pub(crate) fn bounded(text: &str) -> &str {
    if text.len() <= MAX_NLP_BYTES {
        return text;
    }
    let mut end = MAX_NLP_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or("")
}

/// Cache key for a paragraph's NLP analysis: derived from the canonical paragraph hash
/// (`paragraphs.hash`) and [`NLP_VERSION`].
pub fn cache_key_for_hash(paragraph_hash: &str) -> String {
    format!("nlp{NLP_VERSION}:{paragraph_hash}")
}

/// Cache key computed from text directly (SHA-1 of the text, same as the paragraph hash).
pub fn cache_key(text: &str) -> String {
    cache_key_for_hash(&crate::html::sha1_hex(text))
}

/// Parsed-once paragraph view used by the anchor picker. Serializable so jobs can memoize it
/// by [`ParagraphNlp::cache_key`] instead of re-parsing a source paragraph for every target (B-29).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParagraphNlp {
    pub text: String,
    /// Noun-phrase candidates that do not overlap a blocked span (existing link, code, …).
    pub phrases: Vec<NounPhrase>,
    /// Byte ranges of `text` where anchors must not go (from `html::Paragraph::blocked`).
    pub blocked: Vec<(usize, usize)>,
    pub cache_key: String,
    /// Sentence byte spans (index = [`NounPhrase::sentence`]).
    #[serde(default)]
    pub sentences: Vec<(usize, usize)>,
}

impl ParagraphNlp {
    /// Analyze a paragraph once. `blocked` = `html::Paragraph::blocked` (or empty).
    pub fn new(text: &str, blocked: &[(usize, usize)]) -> Self {
        Self::with_terms(text, blocked, None)
    }

    /// Like [`ParagraphNlp::new`], with site statistics for proper-noun (brand) detection.
    pub fn with_terms(text: &str, blocked: &[(usize, usize)], terms: Option<&SiteTerms>) -> Self {
        let text = bounded(text);
        let phrases = noun_phrases_with(text, terms)
            .into_iter()
            .filter(|p| !overlaps(blocked, p.start, p.end))
            .collect();
        Self {
            text: text.to_string(),
            phrases,
            blocked: blocked.to_vec(),
            cache_key: cache_key(text),
            sentences: sentence_spans(text),
        }
    }

    /// From an analyzed HTML paragraph (reuses its hash for the cache key).
    pub fn from_paragraph(p: &crate::html::Paragraph) -> Self {
        Self::from_paragraph_with(p, None)
    }

    /// From an analyzed HTML paragraph, with site statistics.
    pub fn from_paragraph_with(p: &crate::html::Paragraph, terms: Option<&SiteTerms>) -> Self {
        let mut me = Self::with_terms(&p.text, &p.blocked, terms);
        if p.text.len() <= MAX_NLP_BYTES {
            me.cache_key = cache_key_for_hash(&p.hash);
        }
        me
    }

    /// True when sentence `i` contains a blocked span (an existing link, code, ...).
    pub fn sentence_blocked(&self, i: u32) -> bool {
        usize::try_from(i)
            .ok()
            .and_then(|i| self.sentences.get(i))
            .is_some_and(|&(s, e)| overlaps(&self.blocked, s, e))
    }

    /// True when `[start, end)` overlaps a blocked span.
    pub fn is_blocked(&self, start: usize, end: usize) -> bool {
        overlaps(&self.blocked, start, end)
    }
}

pub(crate) fn overlaps(ranges: &[(usize, usize)], start: usize, end: usize) -> bool {
    ranges.iter().any(|&(s, e)| start < e && s < end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraph_nlp_filters_blocked_phrases() {
        let text = "See internal linking basics and the waterproof bike trunk bag.";
        let blocked = [(4usize, 20usize)]; // "internal linking"
        let p = ParagraphNlp::new(text, &blocked);
        assert!(p.phrases.iter().all(|ph| !ph.text.contains("internal")));
        assert!(
            p.phrases
                .iter()
                .any(|ph| ph.text == "waterproof bike trunk bag")
        );
        assert_eq!(p.cache_key, cache_key(text));
        assert!(p.cache_key.starts_with("nlp2:"));
    }

    #[test]
    fn hostile_inputs_never_panic() {
        let big = "word ".repeat(200_000);
        let cases = [
            String::new(),
            " ".into(),
            "\u{0}\u{FEFF}\u{10FFFF}".into(),
            "'''---'''".into(),
            "é".repeat(200_000),
            big,
            "a-".repeat(50_000),
            "1,2,3,4,".repeat(10_000),
        ];
        for c in &cases {
            let _ = tokenize(c);
            let _ = noun_phrases(c);
            let _ = content_tokens(c);
            let _ = extract_terms(c);
            let _ = ngrams(c, 9);
            let _ = classify_intent(c, Some(c));
            let _ = ParagraphNlp::new(c, &[(0, usize::MAX)]);
            let hs = vec![c.clone()];
            let _ = extract_keywords(&KeywordInput {
                title: c,
                headings: &hs,
                meta_description: c,
                body: c,
            });
            let _ = stem(c);
            let _ = cache_key(c);
        }
        assert!(bounded(&"é".repeat(200_000)).len() <= MAX_NLP_BYTES);
    }
}
