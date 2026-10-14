//! Noun-phrase chunker on top of the [`super::pos`] heuristic tagger (no POS model).
//!
//! v1 used wink-nlp UD tags (maximal ADJ/NOUN/PROPN runs). Here a run is a maximal sequence of
//! tokens tagged noun / adjective / number / leading participle; verbs, adverbs, function words
//! and punctuation end a run, a possessive noun ends its run ("shoe’s cleat type" gives "cleat
//! type"; "Bag’s strap" gives "... Bag"). Candidate enumeration then follows v1: per run, lengths
//! `min(run, 6)` down to 2, starts left to right, deduped case-insensitively, boilerplate leads
//! dropped; v2 adds that a candidate must END on a noun (not an adjective, number or participle)
//! and never START on a number.
//!
//! Each candidate carries its "suspect" tokens (model numbers, unit specs, ALL-CAPS / camelCase
//! brand spellings, site-wide proper nouns via [`SiteTerms`]): the picker rejects a candidate with
//! a suspect the target does not name.

use serde::{Deserialize, Serialize};

use super::pos::{self, Tag};
use super::terms::SiteTerms;
use super::tokenize::{TokenKind, content_tokens, norm_word, stem, tokenize};

pub const NP_MIN_WORDS: usize = 2;
pub const NP_MAX_WORDS: usize = 6;
/// Total candidate cap per text (v1: 40; raised because heuristic runs are longer).
pub const MAX_PHRASES: usize = 120;
/// Candidate cap per run so one long run cannot starve later runs.
pub const MAX_PHRASES_PER_RUN: usize = 40;

/// v1 `BOILERPLATE_LEAD`: page-furniture first words.
const BOILERPLATE_LEAD: &[&str] = &[
    "analysis",
    "terms",
    "disclaimer",
    "disclosure",
    "affiliate",
    "table",
    "contents",
];

/// Second halves of compound modifiers ("heavy duty", "water resistant", "full size"): a phrase
/// never starts on one when the first half precedes it.
const COMPOUND_TAILS: &[&str] = &[
    "capacity",
    "duty",
    "end",
    "grade",
    "performance",
    "proof",
    "quality",
    "range",
    "resistant",
    "sided",
    "size",
    "speed",
    "weight",
];

/// A candidate phrase with its byte span in the source text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NounPhrase {
    /// Surface text exactly as in the source.
    pub text: String,
    pub start: usize,
    pub end: usize,
    /// Whitespace word count.
    pub words: usize,
    /// v1 `tokenSet` content tokens (lowercase, len > 2, deduped).
    pub tokens: Vec<String>,
    /// Porter2 stems of `tokens` (same order).
    pub stems: Vec<String>,
    /// Stems of the first and last word's content token ("" when the edge word has none).
    pub edge_stems: [String; 2],
    /// Lowercase tokens that look like a model number or brand (allowed only when the target
    /// names them).
    #[serde(default)]
    pub suspects: Vec<String>,
    /// Starts in the middle of a product name: right after a capitalised site-wide proper noun
    /// ("Quad [Lock Mount]").
    #[serde(default)]
    pub after_brand: bool,
    /// Starts on an adjective that a preceding noun modifies ("Water [Resistant Saddle Bags]").
    #[serde(default)]
    pub split_modifier: bool,
    /// First word is a salesy adjective ("serious stopping power").
    #[serde(default)]
    pub lead_evaluative: bool,
    /// Sentence index within the text (0-based).
    #[serde(default)]
    pub sentence: u32,
    /// The head (last word) is a site-wide generic noun ([`SiteTerms::is_generic_head`]:
    /// "bottles" on a bottle site), so only a modifier the target names makes it specific.
    #[serde(default)]
    pub generic_head: bool,
}

fn edge_stem(word: Option<&str>) -> String {
    word.map(content_tokens)
        .and_then(|t| t.into_iter().next())
        .map(|t| stem(&t))
        .unwrap_or_default()
}

/// ALL-CAPS acronym of at most 4 letters (LED, MIPS, USB-C): never a brand suspect.
fn is_acronym(raw: &str) -> bool {
    let letters: Vec<char> = raw.chars().filter(|c| c.is_alphabetic()).collect();
    !letters.is_empty() && letters.len() <= 4 && letters.iter().all(|c| c.is_uppercase())
}

/// Is this raw token a suspect (model number, brand spelling, site proper noun)?
fn suspect(raw: &str, terms: Option<&SiteTerms>) -> bool {
    if pos::suspect_token(raw).is_some() {
        return true;
    }
    if is_acronym(pos::strip_possessive(raw)) {
        return false;
    }
    terms.is_some_and(|t| t.is_proper(&norm_word(raw)))
}

/// Separators inside a number spec: "1/2", "3-8", "2×4".
const SPEC_JOINERS: &[&str] = &["/", ".", "-", "\u{d7}", "\u{2044}"];

/// Token `i` is glued (no whitespace) to a number before it, directly or through a spec joiner
/// ("3/[8-inch sockets]", "1/[2-inch drive]"): a phrase starting there would cut a spec in two.
/// The tokenizer keeps "20V" and "4.0Ah" whole.
pub(crate) fn starts_mid_spec(toks: &[super::tokenize::Token<'_>], i: usize) -> bool {
    let Some(cur) = toks.get(i) else { return false };
    let Some(prev) = i.checked_sub(1).and_then(|p| toks.get(p)) else {
        return false;
    };
    if prev.end != cur.start {
        return false;
    }
    if prev.kind == TokenKind::Number {
        return true;
    }
    prev.kind == TokenKind::Punct
        && SPEC_JOINERS.contains(&prev.text)
        && i.checked_sub(2)
            .and_then(|p| toks.get(p))
            .is_some_and(|pp| pp.kind == TokenKind::Number && pp.end == prev.start)
}

/// Extract candidate noun phrases in document order (no site statistics).
pub fn noun_phrases(text: &str) -> Vec<NounPhrase> {
    noun_phrases_with(text, None)
}

/// Extract candidate noun phrases; `terms` adds site-wide proper-noun detection.
pub fn noun_phrases_with(text: &str, terms: Option<&SiteTerms>) -> Vec<NounPhrase> {
    let text = super::bounded(text);
    let toks = tokenize(text);
    let tags = pos::tag_tokens(&toks);
    // sentence index per token
    let mut sent = Vec::with_capacity(toks.len());
    let mut s_idx = 0u32;
    for t in &toks {
        sent.push(s_idx);
        if t.kind == TokenKind::Punct && matches!(t.text, "." | "!" | "?") {
            s_idx = s_idx.saturating_add(1);
        }
    }
    let mut runs: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let tag = tags.get(i).copied().unwrap_or(Tag::Func);
        let keep = match tag {
            Tag::Noun | Tag::Adj => true,
            Tag::Num => !cur.is_empty(),
            Tag::Part => cur.is_empty(),
            Tag::Func | Tag::Adv | Tag::Verb => false,
        };
        if keep {
            cur.push(i);
            if t.kind == TokenKind::Word && pos::is_possessive(t.text) {
                runs.push(std::mem::take(&mut cur));
            }
        } else if !cur.is_empty() {
            runs.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        runs.push(cur);
    }

    // Title-case proper nouns (multi-word brand names continue into the next capitalised word);
    // ALL-CAPS brands and model numbers are usually one token and do not count here.
    let is_proper: Vec<bool> = toks
        .iter()
        .map(|t| {
            t.kind == TokenKind::Word
                && pos::suspect_token(t.text).is_none()
                && !is_acronym(pos::strip_possessive(t.text))
                && terms.is_some_and(|ts| ts.is_proper(&norm_word(t.text)))
        })
        .collect();
    let is_suspect: Vec<bool> = toks
        .iter()
        .map(|t| t.kind != TokenKind::Punct && suspect(t.text, terms))
        .collect();
    let mut out: Vec<NounPhrase> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    'runs: for run in runs {
        if run.len() < NP_MIN_WORDS {
            continue;
        }
        let mut per_run = 0usize;
        let max_len = run.len().min(NP_MAX_WORDS);
        for len in (NP_MIN_WORDS..=max_len).rev() {
            for s in 0..=(run.len() - len) {
                let (Some(&a), Some(&b)) = (run.get(s), run.get(s + len - 1)) else {
                    continue;
                };
                let (Some(ta), Some(tb)) = (toks.get(a), toks.get(b)) else {
                    continue;
                };
                let first_tag = tags.get(a).copied().unwrap_or(Tag::Func);
                let last_tag = tags.get(b).copied().unwrap_or(Tag::Func);
                // never start on a number, always end on a noun
                if ta.kind == TokenKind::Number || first_tag == Tag::Num || last_tag != Tag::Noun {
                    continue;
                }
                if pos::is_year(&norm_word(tb.text)) {
                    continue;
                }
                // never start mid-spec ("20 [V kits]", "1/2 [Inch Cordless ...]", "3/[8-inch
                // sockets]") and always end on a content noun ("kits perfect", "12V options")
                if pos::is_fragment_lead(&norm_word(ta.text))
                    || starts_mid_spec(&toks, a)
                    || pos::is_bad_head(&norm_word(tb.text))
                {
                    continue;
                }
                let Some(full) = text.get(ta.start..tb.end) else {
                    continue;
                };
                // A possessive never ends an anchor ("Bike Handlebar Bag’s" -> "... Bag").
                let surface = ["\u{2019}s", "'s", "\u{2019}S", "'S"]
                    .iter()
                    .find_map(|suf| full.strip_suffix(suf))
                    .unwrap_or(full);
                if surface.trim().is_empty() {
                    continue;
                }
                let end = ta.start + surface.len();
                let key = norm_word(surface);
                let first = key.split_whitespace().next().unwrap_or("");
                if BOILERPLATE_LEAD.contains(&first) {
                    continue;
                }
                if !seen.insert(key) {
                    continue;
                }
                let tokens = content_tokens(surface);
                let stems = tokens.iter().map(|t| stem(t)).collect();
                let edge_stems = [
                    edge_stem(surface.split_whitespace().next()),
                    edge_stem(surface.split_whitespace().last()),
                ];
                let mut suspects: Vec<String> = Vec::new();
                for &ti in run.get(s..s + len).unwrap_or(&[]) {
                    if is_suspect.get(ti).copied().unwrap_or(false)
                        && let Some(t) = toks.get(ti)
                    {
                        let w = pos::strip_possessive(&norm_word(t.text)).to_string();
                        if !suspects.contains(&w) {
                            suspects.push(w);
                        }
                    }
                }
                let prev = s.checked_sub(1).and_then(|i| run.get(i)).copied();
                let after_brand = ta.text.chars().next().is_some_and(char::is_uppercase)
                    && prev.is_some_and(|p| is_proper.get(p).copied().unwrap_or(false));
                // a compound modifier cut in two: "Water [Resistant Saddle Bags]",
                // "Heavy [Duty Bike U-Lock]"
                let split_modifier = prev.is_some_and(|p| {
                    (first_tag == Tag::Adj
                        && tags.get(p) == Some(&Tag::Noun)
                        && !is_suspect.get(p).copied().unwrap_or(false))
                        || COMPOUND_TAILS.contains(&norm_word(ta.text).as_str())
                });
                let generic_head = terms.is_some_and(|ts| {
                    edge_stems
                        .get(1)
                        .is_some_and(|h| !h.is_empty() && ts.is_generic_head(h))
                });
                out.push(NounPhrase {
                    text: surface.to_string(),
                    start: ta.start,
                    end,
                    words: surface.split_whitespace().count(),
                    tokens,
                    stems,
                    edge_stems,
                    suspects,
                    after_brand,
                    split_modifier,
                    lead_evaluative: pos::is_evaluative(&norm_word(ta.text)),
                    sentence: sent.get(a).copied().unwrap_or(0),
                    generic_head,
                });
                per_run += 1;
                if out.len() >= MAX_PHRASES {
                    break 'runs;
                }
                if per_run >= MAX_PHRASES_PER_RUN {
                    continue 'runs;
                }
            }
        }
    }
    out
}

/// Byte spans of the sentences of `text` (split after `.`, `!`, `?`), in order. Sentence `i`
/// matches [`NounPhrase::sentence`] `== i`.
pub fn sentence_spans(text: &str) -> Vec<(usize, usize)> {
    let text = super::bounded(text);
    let mut out = Vec::new();
    let mut start = 0usize;
    for t in tokenize(text) {
        if t.kind == TokenKind::Punct && matches!(t.text, "." | "!" | "?") {
            out.push((start, t.end));
            start = t.end;
        }
    }
    if start < text.len() || out.is_empty() {
        out.push((start, text.len()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(s: &str) -> Vec<String> {
        noun_phrases(s).into_iter().map(|p| p.text).collect()
    }

    #[test]
    fn runs_break_on_function_words_and_verbs() {
        let p = texts("The durable steel bike lock impressed reviewers most this year.");
        assert!(p.contains(&"durable steel bike lock".to_string()));
        assert!(p.contains(&"steel bike lock".to_string()));
        assert!(!p.iter().any(|x| x.contains("impressed")));
    }

    /// Pilot 2026-10-13: "Bike Handlebar Bag’s" was picked as an anchor.
    #[test]
    fn possessive_tail_is_stripped() {
        let src = "Every Bike Handlebar Bag\u{2019}s strap matters. A rider's bike handlebar bag's zipper.";
        let ps = noun_phrases(src);
        for p in &ps {
            assert!(
                !p.text.ends_with("\u{2019}s") && !p.text.ends_with("'s"),
                "{}",
                p.text
            );
            assert_eq!(src.get(p.start..p.end), Some(p.text.as_str()));
        }
        assert!(ps.iter().any(|p| p.text == "Bike Handlebar Bag"), "{ps:?}");
    }

    #[test]
    fn possessives_never_inside() {
        let p = texts("the shoe\u{2019}s cleat type and your child\u{2019}s endless energy");
        assert!(p.contains(&"cleat type".to_string()), "{p:?}");
        assert!(!p.iter().any(|x| x.contains('\u{2019}')), "{p:?}");
    }

    #[test]
    fn enumeration_order_longest_first() {
        let p = texts("waterproof bike trunk bag");
        assert_eq!(
            p.first().map(String::as_str),
            Some("waterproof bike trunk bag")
        );
        // ends on a noun: "waterproof bike trunk", "bike trunk bag", "waterproof bike",
        // "bike trunk", "trunk bag" (+1 for the full phrase)
        assert_eq!(p.len(), 6, "{p:?}");
    }

    #[test]
    fn boilerplate_and_numbers() {
        let p = texts("Table of Contents. Affiliate disclosure text. iPhone 15 Pro cases 2026");
        assert!(!p.iter().any(|x| x.to_lowercase().starts_with("affiliate")));
        assert!(p.contains(&"iPhone 15 Pro cases".to_string()));
        assert!(!p.iter().any(|x| x.ends_with("2026")));
        let ip = noun_phrases("iPhone 15 Pro cases")
            .into_iter()
            .find(|x| x.text == "iPhone 15 Pro cases")
            .unwrap();
        assert_eq!(ip.suspects, vec!["iphone".to_string(), "15".to_string()]);
    }

    #[test]
    fn spans_point_into_source() {
        let s = "We love the waterproof bike trunk bag.";
        for p in noun_phrases(s) {
            assert_eq!(&s[p.start..p.end], p.text);
        }
    }

    #[test]
    fn pilot_anchors_lose_their_verbs() {
        for (src, bad, good) in [
            (
                "How does a phone mount withstand daily abuse on rough roads?",
                "phone mount withstand daily abuse",
                "phone mount",
            ),
            (
                "You need dependable bike chain tools that handle repairs fast.",
                "need dependable bike chain",
                "bike chain tools",
            ),
            (
                "reliable rear axles to match heavy-duty trailers like the VEVOR",
                "match heavy-duty trailers",
                "heavy-duty trailers",
            ),
            (
                "Rechargeable Bike Wheel Lights 4-Pack fit most wheels.",
                "Rechargeable Bike Wheel Lights 4-Pack fit",
                "Rechargeable Bike Wheel Lights",
            ),
            (
                "The KRIDDO Toddler Balance Bike stands out as one of the top picks.",
                "KRIDDO Toddler Balance Bike stands",
                "Toddler Balance Bike",
            ),
            (
                "Choose helmets with MIPS. one lightweight design wins.",
                "Choose helmets",
                "lightweight design",
            ),
        ] {
            let p = texts(src);
            assert!(!p.iter().any(|x| x == bad), "{bad}: {p:?}");
            assert!(p.iter().any(|x| x == good), "{good}: {p:?}");
        }
    }

    #[test]
    fn suspects_and_brand_fragments() {
        let terms = SiteTerms::from_counts(
            100,
            Default::default(),
            &[("quad".into(), 50, 0), ("lock".into(), 10, 300)],
        );
        let ps = noun_phrases_with(
            "You lock it with the Quad Lock Motorcycle Handlebar Mount PRO and a 72V battery pack.",
            Some(&terms),
        );
        let get = |t: &str| ps.iter().find(|p| p.text == t);
        assert_eq!(
            get("Quad Lock Motorcycle Handlebar Mount").map(|p| p.suspects.clone()),
            Some(vec!["quad".to_string()])
        );
        assert_eq!(
            get("Lock Motorcycle Handlebar Mount").map(|p| p.after_brand),
            Some(true)
        );
        assert_eq!(
            get("Motorcycle Handlebar Mount").map(|p| (p.after_brand, p.suspects.is_empty())),
            Some((false, true))
        );
        assert_eq!(
            get("72V battery pack").map(|p| p.suspects.clone()),
            Some(vec!["72v".to_string()])
        );
        // short acronyms are not suspects
        let ps = noun_phrases_with("Many MIPS bike helmets ship with LED lights.", Some(&terms));
        assert!(ps.iter().all(|p| p.suspects.is_empty()), "{ps:?}");
    }

    #[test]
    fn sentences_are_indexed() {
        let s = "Bike locks matter. The steel bike lock wins! Cable locks too?";
        let ps = noun_phrases(s);
        let sp = sentence_spans(s);
        assert_eq!(sp.len(), 3);
        let p = ps.iter().find(|p| p.text == "steel bike lock").unwrap();
        assert_eq!(p.sentence, 1);
        let (a, b) = sp[1];
        assert!(a <= p.start && p.end <= b);
    }

    #[test]
    fn compound_modifiers_are_not_split() {
        let w = noun_phrases("The 34L Water Resistant Saddle Bags hold a lot.");
        assert!(w.iter().any(|p| p.text == "Water Resistant Saddle Bags"
            && !p.after_brand
            && !p.split_modifier));
        assert!(
            w.iter()
                .any(|p| p.text == "Resistant Saddle Bags" && p.split_modifier)
        );
        let h = noun_phrases("the Sportneer Heavy Duty Bike U-Lock today");
        assert!(
            h.iter()
                .any(|p| p.text == "Duty Bike U-Lock" && p.split_modifier)
        );
        assert!(
            h.iter()
                .any(|p| p.text == "Bike U-Lock" && !p.split_modifier)
        );
    }

    /// Hermes acceptance 2026-10-14 (site39.example): "V kits perfect", "inch cordless Impact
    /// Wrenches". A unit or lone letter never starts a phrase, a phrase never starts inside a
    /// glued spec, and the last word is a content noun.
    #[test]
    fn unit_fragments_and_trailing_adjectives() {
        let p = texts(
            "You'll find powerful 20V kits perfect for heavy-duty tasks, while 8V to 12V options \
             keep things light. Get the 20 V kits perfect for decks.",
        );
        assert!(p.contains(&"20V kits".to_string()), "{p:?}");
        assert!(!p.iter().any(|x| x.starts_with("V ")), "{p:?}");
        assert!(!p.iter().any(|x| x.ends_with("perfect")), "{p:?}");
        assert!(!p.iter().any(|x| x.ends_with("options")), "{p:?}");
        let p =
            texts("top 3/8-inch cordless Impact Wrenches and 1/2 Inch Cordless Impact Wrenches");
        assert!(p.contains(&"cordless Impact Wrenches".to_string()), "{p:?}");
        assert!(
            !p.iter()
                .any(|x| x.to_lowercase().starts_with("inch")
                    || x.to_lowercase().starts_with("8-inch")),
            "{p:?}"
        );
        for (src, bad, good) in [
            (
                "How Many 16.9 Oz Water Bottles fit",
                "Oz Water Bottles",
                "Water Bottles",
            ),
            (
                "the ECOMAX 6 Gal Air Compressor runs",
                "Gal Air Compressor",
                "Air Compressor",
            ),
            (
                "uncrimped 26 mm beer bottle caps work",
                "mm beer bottle caps",
                "beer bottle caps",
            ),
            (
                "with 4.0Ah batteries and dual 4.0Ah packs",
                "dual 4.0Ah",
                "4.0Ah batteries",
            ),
            (
                "glass bottles available today",
                "glass bottles available",
                "glass bottles",
            ),
            (
                "a hot water bottle easy to pack",
                "hot water bottle easy",
                "hot water bottle",
            ),
        ] {
            let p = texts(src);
            assert!(!p.iter().any(|x| x == bad), "{bad}: {p:?}");
            assert!(p.iter().any(|x| x == good), "{good}: {p:?}");
        }
        // a spec glued by a joiner: "3/[8-inch sockets]"
        let p = texts("a set of 3/8-inch drive sockets");
        assert!(!p.iter().any(|x| x.starts_with("8-inch")), "{p:?}");
        assert!(p.contains(&"drive sockets".to_string()), "{p:?}");
        // a number descriptor written with a space before it is a whole spec
        assert!(texts("its 2-inch spindle travel").contains(&"2-inch spindle travel".to_string()));
    }

    #[test]
    fn spec_fragments_hostile_input_never_panics() {
        for s in [
            "/",
            "1/",
            "/2",
            "1/2",
            "1/2\u{2033}",
            "V",
            "V V V V",
            "\u{d7}\u{d7}",
            "1\u{d7}",
            "3/8-",
            "-8-inch",
            "20V/",
            "inch inch inch",
            "1 / 2 Inch",
            "\u{2044}9 mm",
            "...V kits",
            "4.0.0.0Ah ah ah",
            "\u{2033}\u{2033} kits perfect",
        ] {
            for p in noun_phrases(s) {
                assert_eq!(s.get(p.start..p.end), Some(p.text.as_str()));
            }
            let toks = tokenize(s);
            for i in 0..toks.len() + 2 {
                let _ = starts_mid_spec(&toks, i);
            }
        }
    }
}
