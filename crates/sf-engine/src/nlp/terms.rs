//! Site-wide term statistics (accuracy C.3 + brand detection).
//!
//! * Document frequency per stem → an IDF factor for target-token weights: on a niche site the
//!   niche noun ("bike") is in every post and used to dominate phrase overlap.
//! * Capitalisation per word: a word that is capitalised mid-sentence and (almost) never written
//!   in lowercase on the whole site is a proper noun (brand / product line: "Peloton", "Quad",
//!   "Schwinn"). Anchors that contain one are rejected unless the target names it.
//!
//! Build with [`TermsBuilder`] (one `add_document` per post, mergeable for parallel builds).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::tokenize::{TokenKind, norm_word, stem, tokenize};

/// Sites smaller than this keep uniform weights (IDF is noise on a handful of posts).
pub const MIN_DOCS_FOR_IDF: u32 = 20;
/// Weight floor for a token that is in every post.
pub const IDF_FLOOR: f64 = 0.35;
/// A proper noun is seen capitalised mid-sentence at least this often ...
pub const PROPER_MIN_CAPS: u32 = 2;
/// ... and in lowercase at most this often.
pub const PROPER_MAX_LOWER: u32 = 2;
/// A brand that editors also type in lowercase now and then ("Squirt" 26x vs "squirt" 3x): seen
/// capitalised mid-sentence at least this often ...
pub const PROPER_RATIO_MIN_CAPS: u32 = 8;
/// ... and at least this many times more often capitalised than lowercase.
pub const PROPER_RATIO: u32 = 8;
/// A head noun in at least this share of a site's posts is generic there ("bottles" on a bottle
/// site): an anchor needs a modifier the target names ("wine bottles", not "glass bottles").
pub const GENERIC_HEAD_DF: f64 = 0.5;

/// Per-word capitalisation counts while building.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseCounts {
    /// Capitalised, not at a sentence start.
    pub caps: u32,
    /// Lowercase (anywhere).
    pub lower: u32,
}

/// Accumulates statistics over documents.
#[derive(Debug, Clone, Default)]
pub struct TermsBuilder {
    docs: u32,
    df: HashMap<String, u32>,
    case: HashMap<String, CaseCounts>,
}

impl TermsBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one document (a post's text).
    pub fn add_document(&mut self, text: &str) {
        self.docs = self.docs.saturating_add(1);
        let toks = tokenize(text);
        let mut seen: HashSet<String> = HashSet::new();
        let mut sentence_start = true;
        for t in &toks {
            match t.kind {
                TokenKind::Punct => {
                    if matches!(t.text, "." | "!" | "?" | ":" | ";" | "\u{2022}") {
                        sentence_start = true;
                    }
                    continue;
                }
                TokenKind::Number => {
                    sentence_start = false;
                    continue;
                }
                TokenKind::Word => {}
            }
            let lower = norm_word(t.text);
            let word = super::pos::strip_possessive(&lower).to_string();
            if !sentence_start {
                let first = t.text.chars().next();
                let e = self.case.entry(word.clone()).or_default();
                if first.is_some_and(char::is_uppercase) {
                    e.caps = e.caps.saturating_add(1);
                } else if first.is_some_and(char::is_lowercase) {
                    e.lower = e.lower.saturating_add(1);
                }
            }
            sentence_start = false;
            if word.chars().count() > 2 && seen.len() < 20_000 {
                seen.insert(stem(&word));
            }
        }
        for s in seen {
            let e = self.df.entry(s).or_insert(0);
            *e = e.saturating_add(1);
        }
    }

    /// Merge another builder (parallel reduction).
    pub fn merge(mut self, other: TermsBuilder) -> Self {
        self.docs = self.docs.saturating_add(other.docs);
        for (k, v) in other.df {
            let e = self.df.entry(k).or_insert(0);
            *e = e.saturating_add(v);
        }
        for (k, v) in other.case {
            let e = self.case.entry(k).or_default();
            e.caps = e.caps.saturating_add(v.caps);
            e.lower = e.lower.saturating_add(v.lower);
        }
        self
    }

    pub fn finish(self) -> SiteTerms {
        let proper = self
            .case
            .iter()
            .filter(|(w, c)| is_proper_counts(w, **c))
            .map(|(w, _)| w.clone())
            .collect();
        SiteTerms {
            docs: self.docs,
            df: self.df,
            proper,
        }
    }
}

fn is_proper_counts(word: &str, c: CaseCounts) -> bool {
    let strict = c.caps >= PROPER_MIN_CAPS
        && c.lower <= PROPER_MAX_LOWER
        && c.caps >= c.lower.saturating_mul(10);
    // mostly-capitalised words (site-wide statistics, not per paragraph): the brand is caught
    // even where a paragraph writes it in lowercase
    let ratio = c.caps >= PROPER_RATIO_MIN_CAPS && c.caps >= c.lower.saturating_mul(PROPER_RATIO);
    (strict || ratio)
        && word.chars().any(char::is_alphabetic)
        // (short ALL-CAPS acronyms such as LED/MIPS/USB also land here; the phrase marker skips
        // them by their surface spelling)
        && word.chars().filter(|c| c.is_alphabetic()).count() > 1
}

/// Finished, read-only statistics of a site.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SiteTerms {
    pub docs: u32,
    /// Stem → number of documents containing it.
    pub df: HashMap<String, u32>,
    /// Lowercase words that look like proper nouns site-wide.
    pub proper: HashSet<String>,
}

impl SiteTerms {
    /// From precomputed counts (tests, fixtures): `(word, caps, lower)` triples + stem df.
    pub fn from_counts(docs: u32, df: HashMap<String, u32>, case: &[(String, u32, u32)]) -> Self {
        let proper = case
            .iter()
            .filter(|(w, c, l)| {
                is_proper_counts(
                    w,
                    CaseCounts {
                        caps: *c,
                        lower: *l,
                    },
                )
            })
            .map(|(w, _, _)| w.clone())
            .collect();
        Self { docs, df, proper }
    }

    /// IDF factor in `[IDF_FLOOR, 1]` for a stem (1 when the site is small or the stem unseen).
    pub fn idf_factor(&self, stem: &str) -> f64 {
        if self.docs < MIN_DOCS_FOR_IDF {
            return 1.0;
        }
        let n = f64::from(self.docs);
        let df = f64::from(self.df.get(stem).copied().unwrap_or(0));
        let idf = ((n + 1.0) / (df + 1.0)).ln().max(0.0);
        let max = (n + 1.0).ln();
        if max <= 0.0 {
            return 1.0;
        }
        (IDF_FLOOR + (1.0 - IDF_FLOOR) * idf / max).clamp(IDF_FLOOR, 1.0)
    }

    /// Share of the site's posts that contain this stem; `None` on small sites (fewer than
    /// [`MIN_DOCS_FOR_IDF`] posts), where the share is noise.
    pub fn df_share(&self, stem: &str) -> Option<f64> {
        if self.docs < MIN_DOCS_FOR_IDF {
            return None;
        }
        let df = f64::from(self.df.get(stem).copied().unwrap_or(0));
        Some((df / f64::from(self.docs)).clamp(0.0, 1.0))
    }

    /// True when a head noun with this stem is generic on the site (see [`GENERIC_HEAD_DF`]).
    pub fn is_generic_head(&self, stem: &str) -> bool {
        self.df_share(stem).is_some_and(|s| s >= GENERIC_HEAD_DF)
    }

    /// True when the lowercase word (possessive stripped) is a site-wide proper noun.
    pub fn is_proper(&self, word_lower: &str) -> bool {
        self.proper
            .contains(super::pos::strip_possessive(word_lower))
    }

    /// Rough heap size (for cache budgets).
    pub fn memory_bytes(&self) -> usize {
        self.df
            .keys()
            .map(|k| k.len() + 48)
            .chain(self.proper.iter().map(|k| k.len() + 40))
            .fold(0usize, usize::saturating_add)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proper_nouns_and_idf() {
        let mut b = TermsBuilder::new();
        for i in 0..30 {
            b.add_document(&format!(
                "Our bike pick today is great. You get the Peloton Cycling Shoes and a bike lock. \
                 The cycling shoes fit well {i}."
            ));
        }
        b.add_document("Rare helmet words. The helmet is red.");
        let t = b.finish();
        assert!(t.is_proper("peloton"));
        assert!(
            !t.is_proper("cycling"),
            "cycling is also written in lowercase"
        );
        assert!(!t.is_proper("our"), "sentence starts do not count");
        assert!(t.idf_factor("bike") < 0.5, "{}", t.idf_factor("bike"));
        assert!(t.idf_factor("helmet") > 0.8);
        assert_eq!(t.idf_factor("unseen"), 1.0);
        let small = {
            let mut b = TermsBuilder::new();
            b.add_document("bike bike");
            b.finish()
        };
        assert_eq!(small.idf_factor("bike"), 1.0);
    }

    #[test]
    fn mostly_capitalised_brands_and_generic_heads() {
        let t = SiteTerms::from_counts(
            100,
            [("bottl".to_string(), 70u32), ("wine".to_string(), 7)]
                .into_iter()
                .collect(),
            &[
                ("squirt".into(), 26, 3),
                ("bell".into(), 313, 176),
                ("pro".into(), 275, 125),
                ("rare".into(), 7, 0),
                ("lube".into(), 177, 234),
            ],
        );
        assert!(t.is_proper("squirt"), "26 caps vs 3 lowercase");
        assert!(t.is_proper("rare"));
        for w in ["bell", "pro", "lube"] {
            assert!(!t.is_proper(w), "{w}");
        }
        assert!(t.is_generic_head("bottl"));
        assert!(!t.is_generic_head("wine"));
        assert!(!t.is_generic_head("unseen"));
        let small =
            SiteTerms::from_counts(5, [("bottl".to_string(), 5u32)].into_iter().collect(), &[]);
        assert_eq!(small.df_share("bottl"), None);
        assert!(!small.is_generic_head("bottl"));
    }

    #[test]
    fn merge_equals_sequential() {
        let docs = [
            "The Quad Lock mount. A Quad Lock case.",
            "Another Quad mount here.",
        ];
        let mut a = TermsBuilder::new();
        for d in docs {
            a.add_document(d);
        }
        let mut x = TermsBuilder::new();
        x.add_document(docs[0]);
        let mut y = TermsBuilder::new();
        y.add_document(docs[1]);
        assert_eq!(a.finish(), x.merge(y).finish());
    }

    #[test]
    fn hostile_input_never_panics() {
        let mut b = TermsBuilder::new();
        for s in ["", "\u{0}", &"Aa ".repeat(100_000), "’s ’s 's"] {
            b.add_document(s);
        }
        let t = b.finish();
        let _ = t.idf_factor("");
        let _ = t.is_proper("");
    }
}
