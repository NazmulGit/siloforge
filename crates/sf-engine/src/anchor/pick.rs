//! Anchor picking (v1 `anchor/nlp.ts::pickAnchor`, `scorePhrase`, `cleanTitle`) with v2 guards.
//!
//! Hot-path design (B-29/B-30): a source paragraph is parsed once into [`ParagraphNlp`]
//! (memoizable by hash) and a target once into [`TargetProfile`]; `pick_anchor` is then pure
//! arithmetic over precomputed stems — no re-tokenizing per edge.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use super::classify::is_generic_anchor;
use crate::html::normalize_ws;
use crate::nlp::pos::{self, Tag};
use crate::nlp::{
    LISTICLE_WORDS, NounPhrase, ParagraphNlp, SiteTerms, TokenKind, content_tokens, norm_word,
    stem, tokenize,
};

/// Phrase word bounds (v1 MIN_WORDS/MAX_WORDS).
pub const MIN_ANCHOR_WORDS: usize = 2;
pub const MAX_ANCHOR_WORDS: usize = 6;
/// Words kept by the title fallback when cleaning yields nothing (v1).
pub const TITLE_FALLBACK_WORDS: usize = 8;
/// Weight of GSC striking-distance keywords (v1 1.5).
pub const STRIKING_WEIGHT: f64 = 1.5;
/// Max alternates returned per pick (v1: 4 phrases + cleaned title).
pub const MAX_ALTERNATES: usize = 4;
/// Semantic phrase scoring (C.1): `score += SEMANTIC_WEIGHT * (cos(phrase, target) - SEMANTIC_BASE)`.
pub const SEMANTIC_WEIGHT: f64 = 30.0;
pub const SEMANTIC_BASE: f64 = 0.35;
/// Cost per rank step of the paragraph (cosine order) a phrase comes from.
pub const PARAGRAPH_RANK_PENALTY: f64 = 3.0;
/// Cost of placing a link in a sentence that already holds a link / blocked span (C.8).
pub const BLOCKED_SENTENCE_PENALTY: f64 = 6.0;
/// Cost per sentence index (earlier sentences preferred, capped at 4 sentences; C.8).
pub const SENTENCE_POSITION_PENALTY: f64 = 0.5;
/// Cost of a salesy lead adjective ("serious stopping power").
pub const EVALUATIVE_LEAD_PENALTY: f64 = 4.0;
/// Cost of a site-wide generic head noun whose modifiers the target does not name
/// ("Starbucks glass bottles" -> a bottle-crafts post on a bottle site).
pub const GENERIC_HEAD_PENALTY: f64 = 12.0;
/// Minimum cosine(phrase, target post) for an in-paragraph phrase pick (v2.0.2). Below it a
/// phrase that the target does not fully name is off-topic ("open road" -> phone mounts, cos
/// 0.01 on the pilot), and the pick falls back to a target keyword / title found in the
/// paragraph, else the cleaned title (not in paragraph, so the insert step writes a sentence).
/// The pilot's 5th percentile was 0.28. Site setting `anchor_semantic_min` overrides it.
pub const MIN_PHRASE_SEMANTIC: f32 = 0.15;
/// Inner function words a natural anchor may contain ("Bike Mounts for Phones").
const INNER_FUNCTION_WORDS: &[&str] = &["a", "an", "and", "for", "in", "of", "on", "the", "with"];

const CLAUSE_BREAKERS: &[&str] = &[
    "that", "which", "who", "whom", "where", "when", "because", "so",
];
const TRAILING_STOP: &[&str] = &[
    "for", "the", "a", "an", "of", "to", "and", "or", "with", "in", "on", "at", "by", "your",
    "our", "my", "their",
];

fn listicle_stems() -> &'static HashSet<String> {
    static S: OnceLock<HashSet<String>> = OnceLock::new();
    S.get_or_init(|| LISTICLE_WORDS.iter().map(|w| stem(w)).collect())
}

fn is_listicle_token(tok: &str, st: &str) -> bool {
    LISTICLE_WORDS.contains(&tok) || listicle_stems().contains(st)
}

/// Precomputed target descriptor: stem → weight (max-merged), plus raw title/keywords.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetProfile {
    pub title: String,
    /// Target keywords, best first (callers pass `ORDER BY score DESC, keyword`).
    pub keywords: Vec<String>,
    /// Deterministic top keyword (for exact/partial classification).
    pub top_keyword: Option<String>,
    pub weights: HashMap<String, f64>,
    /// Lowercase words of the title: a model number / brand in an anchor is only allowed when it
    /// is one of these.
    #[serde(default)]
    pub names: HashSet<String>,
}

impl TargetProfile {
    /// `weighted` = extra (text, weight) pairs, e.g. GSC striking-distance queries at
    /// [`STRIKING_WEIGHT`]. Title and keywords get weight 1 (v1 `weightedTokenMap`).
    pub fn new(title: &str, keywords: &[String], weighted: &[(String, f64)]) -> Self {
        let mut weights: HashMap<String, f64> = HashMap::new();
        let mut add = |text: &str, w: f64| {
            if !w.is_finite() || w <= 0.0 {
                return;
            }
            for t in content_tokens(text) {
                let e = weights.entry(stem(&t)).or_insert(0.0);
                if w > *e {
                    *e = w;
                }
            }
        };
        add(title, 1.0);
        for k in keywords.iter().take(64) {
            add(k, 1.0);
        }
        for (t, w) in weighted.iter().take(64) {
            add(t, *w);
        }
        let keywords: Vec<String> = keywords.iter().take(64).map(|k| normalize_ws(k)).collect();
        let mut names: HashSet<String> = HashSet::new();
        // title only: keywords on affiliate sites are full of product names
        for text in [title] {
            for t in tokenize(text) {
                if t.kind == TokenKind::Punct {
                    continue;
                }
                let w = norm_word(t.text);
                let w = pos::strip_possessive(&w).to_string();
                for part in w.split('-').filter(|p| !p.is_empty()) {
                    names.insert(part.to_string());
                }
                names.insert(w);
            }
        }
        Self {
            title: normalize_ws(title),
            top_keyword: keywords.first().cloned().filter(|k| !k.is_empty()),
            keywords,
            weights,
            names,
        }
    }

    /// Scale token weights by site-wide IDF (C.3): a token in every post ("bike" on a bike
    /// site) counts about a third of a rare, specific one.
    pub fn apply_idf(&mut self, terms: &SiteTerms) {
        for (st, w) in self.weights.iter_mut() {
            *w *= terms.idf_factor(st);
        }
    }

    /// True when the target title names this lowercase token.
    pub fn names_token(&self, token_lower: &str) -> bool {
        self.names.contains(token_lower)
            || token_lower
                .split('-')
                .all(|p| p.is_empty() || self.names.contains(p))
    }

    fn weight(&self, st: &str) -> f64 {
        self.weights.get(st).copied().unwrap_or(0.0)
    }
}

/// Quality score of a phrase against a target (v1 `PhraseScore`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PhraseScore {
    pub score: f64,
    pub overlap_desc: f64,
    pub descriptive_count: usize,
    pub word_count: usize,
}

/// v1 `scorePhrase` over stems, plus the v2 edge rule: each phrase edge word that carries no
/// target weight costs 5 (anchors should start and end on meaningful words, e.g.
/// `steel bike lock` beats `steel bike lock stood`). Returns `None` below the quality floor.
pub fn score_phrase(phrase: &NounPhrase, target: &TargetProfile) -> Option<PhraseScore> {
    // brand / model-number fragments only when the target names them; never mid product name
    if phrase.after_brand
        || phrase.split_modifier
        || phrase.suspects.iter().any(|t| !target.names_token(t))
    {
        return None;
    }
    if has_repeated_word(&phrase.text) {
        return None;
    }
    let mut seen: HashSet<&str> = HashSet::new();
    let (mut content, mut descriptive) = (0usize, 0usize);
    let (mut overlap_all, mut overlap_desc) = (0.0f64, 0.0f64);
    for (tok, st) in phrase.tokens.iter().zip(&phrase.stems) {
        if !seen.insert(st.as_str()) {
            continue;
        }
        content += 1;
        let w = target.weight(st);
        overlap_all += w;
        if !is_listicle_token(tok, st) {
            descriptive += 1;
            overlap_desc += w;
        }
    }
    if content == 0 || overlap_desc == 0.0 {
        return None;
    }
    let word_count = phrase.words;
    if descriptive < 2 && word_count < 3 {
        return None;
    }
    // prefer 2-5 words (pilot: 6-word clauses read badly)
    let length_score = match word_count {
        3 | 4 => 6.0,
        5 => 4.0,
        2 => 2.0,
        6 => -3.0,
        _ => -5.0,
    };
    let lead_penalty = if phrase.lead_evaluative {
        EVALUATIVE_LEAD_PENALTY
    } else {
        0.0
    };
    let generic_penalty = 5.0 * (content - descriptive) as f64;
    // "Starbucks glass bottles" on a bottle site: the head is in most posts, so the phrase is
    // only specific when a modifier is a target word
    let head = phrase.edge_stems.get(1).map(String::as_str).unwrap_or("");
    let head_penalty = if phrase.generic_head
        && !phrase
            .stems
            .iter()
            .any(|st| st.as_str() != head && target.weight(st) > 0.0)
    {
        GENERIC_HEAD_PENALTY
    } else {
        0.0
    };
    let edge_penalty: f64 = phrase
        .edge_stems
        .iter()
        .map(|s| {
            if s.is_empty() || target.weight(s) == 0.0 {
                5.0
            } else {
                0.0
            }
        })
        .sum();
    let score = 20.0 * overlap_desc + 3.0 * overlap_all + 2.0 * descriptive as f64 + length_score
        - generic_penalty
        - edge_penalty
        - lead_penalty
        - head_penalty;
    Some(PhraseScore {
        score,
        overlap_desc,
        descriptive_count: descriptive,
        word_count,
    })
}

/// v1 `cleanTitle`, hardened by the 2026-10-13 pilot: strip list noise and a leading `[..]`/`(..)`
/// aside, cut at the first clause word, year, bracketed aside or separator, cap the words, then
/// drop dangling stop words and edge punctuation. v1 removed years after the clause cut and
/// trimmed stop words before the cap, so "12 Best Bike Locks With Alarms for 2026 (Loud & Secure
/// Choices)" became the anchor "Bike Locks With Alarms for (Loud".
pub fn clean_title(title: &str, max_words: usize) -> String {
    let norm = |w: &str| -> String {
        w.to_lowercase()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect()
    };
    let mut words: Vec<&str> = title.split_whitespace().take(256).collect();
    let is_year = |w: &str| {
        let n = norm(w);
        n.len() == 4
            && (n.starts_with("19") || n.starts_with("20"))
            && n.chars().all(|c| c.is_ascii_digit())
    };
    let leading_noise = |w: &str| {
        let n = norm(w);
        n.is_empty()
            || n.chars().all(|c| c.is_ascii_digit())
            || LISTICLE_WORDS.contains(&n.as_str())
            || matches!(n.as_str(), "the" | "a" | "an")
    };
    let opens = |w: &str| w.starts_with('(') || w.starts_with('[');
    let closes = |w: &str| w.ends_with(')') || w.ends_with(']');
    let is_separator = |w: &str| matches!(w, "-" | "\u{2013}" | "\u{2014}" | "|" | ":");
    let is_num = |w: &str| {
        let n = norm(w);
        !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())
    };
    // "10 Best 1/2 Inch Cordless Impact Wrenches": a number run followed by a unit is a spec,
    // kept whole ("1/2 Inch ..."), never cut to "Inch ...".
    let spec_at = |ws: &[&str], i: usize| {
        let mut j = i;
        while ws.get(j).is_some_and(|w| is_num(w)) {
            j += 1;
        }
        j > i && ws.get(j).is_some_and(|w| pos::is_unit_word(&norm(w)))
    };
    for _ in 0..4 {
        let skip = (0..words.len())
            .take_while(|&i| {
                words
                    .get(i)
                    .is_some_and(|w| leading_noise(w) && !spec_at(&words, i))
            })
            .count();
        words.drain(..skip);
        // A leading aside ("[with Brackets] Bike Locks") is dropped as a whole.
        if words.first().is_some_and(|w| opens(w)) {
            let end = words.iter().position(|w| closes(w)).unwrap_or(0);
            words.drain(..=end);
        } else {
            break;
        }
    }
    // Cut at the first clause word, year, bracketed aside or separator (never at position 0).
    if let Some(brk) = words.iter().enumerate().position(|(i, w)| {
        i > 0
            && (CLAUSE_BREAKERS.contains(&norm(w).as_str())
                || is_year(w)
                || opens(w)
                || is_separator(w))
    }) {
        words.truncate(brk);
    }
    // "Bikes:" / "Guide," end the phrase but keep the word itself.
    if let Some(p) = words
        .iter()
        .position(|w| w.ends_with(':') || w.ends_with(',') || w.ends_with(';'))
    {
        words.truncate(p + 1);
    }
    words.retain(|w| !is_year(w));
    words.truncate(max_words);
    // Dangling stop words / punctuation are dropped AFTER the cap ("... Alarms for" -> "... Alarms").
    let edge_junk = |w: &str| norm(w).is_empty() || TRAILING_STOP.contains(&norm(w).as_str());
    while words.last().is_some_and(|w| edge_junk(w)) {
        words.pop();
    }
    let lead = words.iter().take_while(|w| norm(w).is_empty()).count();
    words.drain(..lead);
    let trim = |w: &str| -> String {
        w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '\u{2019}')
            .to_string()
    };
    let n = words.len();
    words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            if i == 0 || i + 1 == n {
                trim(w)
            } else {
                (*w).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Generation-time constraints and preferences.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PickOptions {
    /// Minimum anchor words (default 2: one-word anchors are banned, C.5).
    pub min_words: usize,
    /// Maximum anchor words (default 6).
    pub max_words: usize,
    /// Ignore list (lowercase phrases); matched on word boundaries (fixes B-20 substring match).
    pub ignore: Vec<String>,
    /// Anchors (lowercase) already used in the same source post — never reused.
    pub used_in_source: HashSet<String>,
    /// The source post's own primary keyword (lowercase) — never used (self-cannibalization).
    pub source_keyword: Option<String>,
    /// Exact anchors (lowercase) already pointing at a DIFFERENT target — never used (C.5).
    pub reserved_exact: HashSet<String>,
    /// Lowercase anchor → times already used for this target; each use costs 8 (diversity, C.6).
    pub target_anchor_counts: HashMap<String, usize>,
    /// When true (site exact share is at its cap), the exact top keyword costs 30 (C.2/guard).
    pub avoid_exact: bool,
    /// Lowercase phrase → cosine(phrase embedding, target post embedding) (C.1). Phrases not in
    /// the map get no semantic adjustment.
    #[serde(default)]
    pub semantic: HashMap<String, f32>,
    /// In-paragraph phrases with a known cosine below this (and not fully named by the target)
    /// are skipped; 0 disables. [`PickOptions::new`] sets [`MIN_PHRASE_SEMANTIC`].
    #[serde(default)]
    pub min_semantic: f32,
}

impl PickOptions {
    pub fn new() -> Self {
        Self {
            min_words: MIN_ANCHOR_WORDS,
            max_words: MAX_ANCHOR_WORDS,
            min_semantic: MIN_PHRASE_SEMANTIC,
            ..Self::default()
        }
    }

    fn bounds(&self) -> (usize, usize) {
        let min = if self.min_words == 0 {
            MIN_ANCHOR_WORDS
        } else {
            self.min_words
        };
        let max = if self.max_words == 0 {
            MAX_ANCHOR_WORDS
        } else {
            self.max_words
        };
        (min.min(max), max.max(min))
    }

    /// True when `anchor_lower` hits the ignore list on a word boundary.
    pub fn is_ignored(&self, anchor_lower: &str) -> bool {
        is_ignored(anchor_lower, &self.ignore)
    }

    fn allowed(&self, anchor_lower: &str) -> bool {
        !(self.is_ignored(anchor_lower)
            || self.used_in_source.contains(anchor_lower)
            || self.source_keyword.as_deref() == Some(anchor_lower)
            || self.reserved_exact.contains(anchor_lower))
    }
}

/// Word-boundary ignore check (lowercase inputs).
pub fn is_ignored(text_lower: &str, phrases: &[String]) -> bool {
    phrases.iter().any(|p| {
        let p = p.trim().to_lowercase();
        !p.is_empty() && super::classify::contains_word(text_lower, &p)
    })
}

/// How a pick was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickSource {
    Phrase,
    Keyword,
    Title,
    CleanTitle,
}

/// Result of [`pick_anchor`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnchorPick {
    pub anchor: String,
    /// True when `anchor` occurs (unblocked) in the paragraph and can be wrapped natively.
    pub in_paragraph: bool,
    /// Byte span of the anchor in the paragraph text (when `in_paragraph`).
    pub span: Option<(usize, usize)>,
    pub score: Option<f64>,
    pub source: PickSource,
    /// Paragraph idx the anchor was found in (multi-paragraph search).
    pub paragraph_idx: Option<usize>,
    /// Up to 4 other good phrases (score order) + the cleaned title, for the guards.
    pub alternates: Vec<String>,
}

/// Find `needle` (any case) in `text` on word boundaries outside blocked spans.
/// Returns the byte span of the first valid occurrence.
pub fn find_unblocked(
    text: &str,
    needle: &str,
    blocked: &[(usize, usize)],
) -> Option<(usize, usize)> {
    let needle = normalize_ws(needle).to_lowercase();
    if needle.is_empty() || needle.len() > text.len() {
        return None;
    }
    // Lowercasing can change byte lengths for some scripts; map via char indices.
    let mut lower = String::with_capacity(text.len());
    let mut map: Vec<usize> = Vec::with_capacity(text.len() + 1);
    for (i, c) in text.char_indices() {
        for lc in c.to_lowercase() {
            for _ in 0..lc.len_utf8() {
                map.push(i);
            }
            lower.push(lc);
        }
    }
    map.push(text.len());
    let mut from = 0;
    while let Some(p) = lower.get(from..).and_then(|s| s.find(&needle)) {
        let s = from + p;
        let e = s + needle.len();
        let before = lower.get(..s).and_then(|x| x.chars().next_back());
        let after = lower.get(e..).and_then(|x| x.chars().next());
        let boundary = before.is_none_or(|c| !c.is_alphanumeric())
            && after.is_none_or(|c| !c.is_alphanumeric());
        let (os, oe) = (
            map.get(s).copied().unwrap_or(0),
            map.get(e).copied().unwrap_or(text.len()),
        );
        if boundary && !crate::nlp::overlaps(blocked, os, oe) {
            return Some((os, oe));
        }
        from = s + needle.chars().next().map(char::len_utf8).unwrap_or(1);
    }
    None
}

fn title_fallback(target: &TargetProfile) -> String {
    let cleaned = clean_title(&target.title, MAX_ANCHOR_WORDS);
    if !cleaned.is_empty() {
        return cleaned;
    }
    let first: Vec<&str> = target
        .title
        .split_whitespace()
        .take(TITLE_FALLBACK_WORDS)
        .collect();
    if first.is_empty() {
        target.title.clone()
    } else {
        first.join(" ")
    }
}

/// Pick the anchor for one (paragraph, target) pair.
///
/// Order: best-scoring unblocked noun phrase (v1 scoring + edge rule + generic reject +
/// diversity/exact penalties + constraints) → first target keyword (>= `min_words`) found on a
/// word boundary → title found → cleaned title (`in_paragraph = false`).
pub fn pick_anchor(para: &ParagraphNlp, target: &TargetProfile, opts: &PickOptions) -> AnchorPick {
    let (min_w, max_w) = opts.bounds();
    let top = target.top_keyword.as_deref().map(str::to_lowercase);
    let mut scored: Vec<(f64, &NounPhrase)> = Vec::new();
    for ph in &para.phrases {
        if ph.words < min_w || ph.words > max_w {
            continue;
        }
        let lower = ph.text.to_lowercase();
        if !opts.allowed(&lower) || is_generic_anchor(&lower) || listicle_lead(&lower) {
            continue;
        }
        let Some(s) = score_phrase(ph, target) else {
            continue;
        };
        let mut score = s.score + context_adjustment(ph, para);
        if opts.avoid_exact && top.as_deref() == Some(lower.as_str()) {
            score -= 30.0;
        }
        if let Some(&n) = opts.target_anchor_counts.get(&lower) {
            score -= 8.0 * n.min(3) as f64;
        }
        if let Some(&c) = opts.semantic.get(&lower)
            && c.is_finite()
        {
            if opts.min_semantic > 0.0 && c < opts.min_semantic && !named_by_target(ph, target) {
                continue;
            }
            score += SEMANTIC_WEIGHT * (f64::from(c) - SEMANTIC_BASE);
        }
        scored.push((score, ph));
    }
    // Best by score, first (document order) wins ties.
    let best = scored
        .iter()
        .enumerate()
        .fold(None::<(usize, f64)>, |acc, (i, (s, _))| match acc {
            Some((_, bs)) if bs >= *s => acc,
            _ => Some((i, *s)),
        });
    let clean = title_fallback(target);
    let alternates_for = |skip: Option<usize>| -> Vec<String> {
        let mut idx: Vec<usize> = (0..scored.len()).filter(|i| Some(*i) != skip).collect();
        idx.sort_by(|&a, &b| {
            let (sa, sb) = (
                scored.get(a).map(|x| x.0).unwrap_or(0.0),
                scored.get(b).map(|x| x.0).unwrap_or(0.0),
            );
            sb.total_cmp(&sa).then(a.cmp(&b))
        });
        let mut out: Vec<String> = Vec::new();
        for i in idx {
            if let Some((_, ph)) = scored.get(i)
                && !out.iter().any(|o| o.eq_ignore_ascii_case(&ph.text))
            {
                out.push(ph.text.clone());
            }
            if out.len() >= MAX_ALTERNATES {
                break;
            }
        }
        if !clean.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(&clean)) {
            out.push(clean.clone());
        }
        out
    };
    if let Some((i, score)) = best
        && let Some((_, ph)) = scored.get(i)
    {
        let alts = alternates_for(Some(i));
        return AnchorPick {
            anchor: ph.text.clone(),
            in_paragraph: true,
            span: Some((ph.start, ph.end)),
            score: Some(score),
            source: PickSource::Phrase,
            paragraph_idx: None,
            alternates: alts
                .into_iter()
                .filter(|a| !a.eq_ignore_ascii_case(&ph.text))
                .collect(),
        };
    }
    // Keyword fallback: multi-word only (v1 emitted one-word anchors here, B-9/C.5).
    for kw in &target.keywords {
        let words = kw.split_whitespace().count();
        let lower = kw.to_lowercase();
        if words < min_w
            || words > max_w
            || kw.chars().count() <= 3
            || !opts.allowed(&lower)
            || lower
                .split_whitespace()
                .next()
                .is_some_and(pos::is_fragment_lead)
        {
            continue;
        }
        if let Some((s, e)) = find_unblocked(&para.text, kw, &para.blocked)
            && !text_mid_spec(&para.text, s)
        {
            let surface = para.text.get(s..e).unwrap_or(kw).to_string();
            return AnchorPick {
                anchor: surface,
                in_paragraph: true,
                span: Some((s, e)),
                score: None,
                source: PickSource::Keyword,
                paragraph_idx: None,
                alternates: alternates_for(None),
            };
        }
    }
    let title_words = target.title.split_whitespace().count();
    if title_words >= min_w
        && title_words <= max_w
        && opts.allowed(&target.title.to_lowercase())
        && let Some((s, e)) = find_unblocked(&para.text, &target.title, &para.blocked)
    {
        return AnchorPick {
            anchor: para.text.get(s..e).unwrap_or(&target.title).to_string(),
            in_paragraph: true,
            span: Some((s, e)),
            score: None,
            source: PickSource::Title,
            paragraph_idx: None,
            alternates: alternates_for(None),
        };
    }
    AnchorPick {
        anchor: clean.clone(),
        in_paragraph: false,
        span: None,
        score: None,
        source: PickSource::CleanTitle,
        paragraph_idx: None,
        alternates: alternates_for(None)
            .into_iter()
            .filter(|a| *a != clean)
            .collect(),
    }
}

/// Every descriptive word of the phrase is a target word ("Commuter Bike Helmets" for a
/// commuter-helmet post): the semantic floor never drops such a phrase (short phrases embed
/// poorly against a whole post: the pilot's exact titles scored 0.17-0.2).
fn named_by_target(ph: &NounPhrase, target: &TargetProfile) -> bool {
    let mut any = false;
    for (tok, st) in ph.tokens.iter().zip(&ph.stems) {
        if is_listicle_token(tok, st) {
            continue;
        }
        if target.weight(st) <= 0.0 {
            return false;
        }
        any = true;
    }
    any
}

/// "Best Drill Kits", "Top Picks": a listicle lead reads as a heading (same rule as
/// [`is_natural_anchor`]).
fn listicle_lead(lower: &str) -> bool {
    lower
        .split_whitespace()
        .next()
        .is_some_and(|w| LISTICLE_WORDS.contains(&w))
}

/// The match at byte `start` continues a number spec: "3/[8-inch ...]", "4.[5 Inch ...]".
fn text_mid_spec(text: &str, start: usize) -> bool {
    let mut before = text.get(..start).unwrap_or("").chars().rev();
    matches!(
        (before.next(), before.next()),
        (Some('/' | '.' | '-' | '\u{d7}' | '\u{2044}'), Some(d)) if d.is_ascii_digit()
    )
}

/// "Lock Bike Lock Cable", "MIPS feature MIPS": a repeated word is never a natural anchor.
fn has_repeated_word(text: &str) -> bool {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|w| pos::strip_possessive(&norm_word(w)).to_string())
        .collect();
    words
        .iter()
        .enumerate()
        .any(|(i, w)| words.iter().skip(i + 1).any(|o| o == w))
}

/// Sentence-level placement (C.8): earlier sentences preferred, sentences that already hold a
/// link (blocked span) avoided.
fn context_adjustment(ph: &NounPhrase, para: &ParagraphNlp) -> f64 {
    let mut adj = -SENTENCE_POSITION_PENALTY * f64::from(ph.sentence.min(4));
    if para.sentence_blocked(ph.sentence) {
        adj -= BLOCKED_SENTENCE_PENALTY;
    }
    adj
}

/// Pick across the candidate paragraphs (cosine-ranked, best first). The best phrase pick wins,
/// each rank step costing [`PARAGRAPH_RANK_PENALTY`] (v1 took the first paragraph with any
/// pick); without a phrase pick, the first keyword/title found in a paragraph; else the title
/// fallback.
pub fn pick_anchor_across(
    paragraphs: &[(usize, &ParagraphNlp)],
    target: &TargetProfile,
    opts: &PickOptions,
) -> AnchorPick {
    let mut best: Option<(f64, AnchorPick)> = None;
    let mut first_in_para: Option<AnchorPick> = None;
    let mut fallback: Option<AnchorPick> = None;
    for (rank, (idx, p)) in paragraphs.iter().enumerate() {
        let mut pick = pick_anchor(p, target, opts);
        if pick.in_paragraph {
            pick.paragraph_idx = Some(*idx);
            if pick.source == PickSource::Phrase {
                let s = pick.score.unwrap_or(0.0) - PARAGRAPH_RANK_PENALTY * rank as f64;
                if best.as_ref().is_none_or(|(b, _)| s > *b) {
                    best = Some((s, pick));
                }
            } else if first_in_para.is_none() {
                first_in_para = Some(pick);
            }
        } else if fallback.is_none() {
            fallback = Some(pick);
        }
    }
    best.map(|(_, p)| p)
        .or(first_in_para)
        .or(fallback)
        .unwrap_or_else(|| pick_anchor(&ParagraphNlp::new("", &[]), target, opts))
}

/// The `k` best phrase candidates (lowercase surface text, best first) across the paragraphs by
/// lexical score, for semantic re-scoring: embed only these, then pick with
/// [`PickOptions::semantic`].
pub fn top_phrases(
    paragraphs: &[(usize, &ParagraphNlp)],
    target: &TargetProfile,
    opts: &PickOptions,
    k: usize,
) -> Vec<String> {
    let (min_w, max_w) = opts.bounds();
    let mut all: Vec<(f64, String)> = Vec::new();
    for (rank, (_, para)) in paragraphs.iter().enumerate() {
        for ph in &para.phrases {
            if ph.words < min_w || ph.words > max_w {
                continue;
            }
            let lower = ph.text.to_lowercase();
            if !opts.allowed(&lower) || is_generic_anchor(&lower) || listicle_lead(&lower) {
                continue;
            }
            if let Some(s) = score_phrase(ph, target) {
                let score =
                    s.score + context_adjustment(ph, para) - PARAGRAPH_RANK_PENALTY * rank as f64;
                all.push((score, lower));
            }
        }
    }
    all.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut out: Vec<String> = Vec::new();
    for (_, t) in all {
        if !out.contains(&t) {
            out.push(t);
        }
        if out.len() >= k {
            break;
        }
    }
    out
}

/// Context-free spec-fragment check (v2.0.1): the first word is a unit or a lone letter ("V kits
/// perfect", "inch cordless Impact Wrenches"), or the last word cannot be a head (an adjective such
/// as "perfect", a unit, a token with digits). The plan uses it to re-pick anchors of pending
/// suggestions made by older pickers (v1 split "20V" into "20" + "V").
pub fn is_fragment_anchor(anchor: &str) -> bool {
    let lower = crate::nlp::norm_word(anchor);
    let first = lower.split_whitespace().next().unwrap_or("");
    let last = lower.split_whitespace().last().unwrap_or("");
    crate::nlp::pos::is_fragment_lead(first) || crate::nlp::pos::is_bad_head(last)
}

/// Strict "natural anchor" check: 2-6 words, a noun phrase (starts on a noun/adjective/
/// participle, ends on a noun, no verbs, adverbs, pronouns or possessives; inner `for/of/with/
/// and/in/on/a/the` allowed), no model number or brand word the target does not name, not
/// generic, not only listicle words. `terms` adds site-wide proper-noun detection.
pub fn is_natural_anchor(anchor: &str, target: &TargetProfile, terms: Option<&SiteTerms>) -> bool {
    let anchor = normalize_ws(anchor);
    let lower = anchor.to_lowercase();
    if anchor.is_empty() || anchor.len() > 120 || is_generic_anchor(&lower) {
        return false;
    }
    if has_repeated_word(&anchor) {
        return false;
    }
    let toks = tokenize(&anchor);
    if toks
        .iter()
        .any(|t| t.kind == TokenKind::Punct && !matches!(t.text, "&" | "/"))
    {
        return false;
    }
    let words: Vec<usize> = (0..toks.len())
        .filter(|&i| toks.get(i).is_some_and(|t| t.kind != TokenKind::Punct))
        .collect();
    if words.len() < MIN_ANCHOR_WORDS || words.len() > MAX_ANCHOR_WORDS {
        return false;
    }
    let tags = pos::tag_tokens(&toks);
    let tag = |i: usize| tags.get(i).copied().unwrap_or(Tag::Func);
    let word_at = |k: usize| words.get(k).and_then(|&i| toks.get(i));
    // A leading number spec is fine only as a whole, unit included ("1/2 Inch Impact Wrenches");
    // a unit or lone letter without its number never starts an anchor ("V kits", "inch drills").
    let mut lead = 0usize;
    while word_at(lead).is_some_and(|t| t.kind == TokenKind::Number) {
        lead += 1;
    }
    if lead > 0 {
        if !word_at(lead).is_some_and(|t| pos::is_unit_word(&norm_word(t.text))) {
            return false;
        }
        lead += 1;
    }
    let (Some(&first), Some(&last)) = (words.get(lead), words.last()) else {
        return false;
    };
    if lead == 0
        && toks
            .get(first)
            .is_some_and(|t| pos::is_fragment_lead(&norm_word(t.text)))
    {
        return false;
    }
    if !matches!(tag(first), Tag::Noun | Tag::Adj | Tag::Part) || tag(last) != Tag::Noun {
        return false;
    }
    // the head must be a content noun ("kits perfect", "12V options", "water bottle easy")
    if toks
        .get(last)
        .is_some_and(|t| pos::is_bad_head(&norm_word(t.text)))
    {
        return false;
    }
    // a listicle lead ("Best Bike Helmets", "Top Picks") reads as a heading, not an anchor
    if toks
        .get(first)
        .is_some_and(|t| LISTICLE_WORDS.contains(&norm_word(t.text).as_str()))
    {
        return false;
    }
    let mut descriptive = 0usize;
    for &i in words.get(lead..).unwrap_or(&[]) {
        let Some(t) = toks.get(i) else { return false };
        let w = norm_word(t.text);
        match tag(i) {
            Tag::Verb | Tag::Adv => return false,
            Tag::Func if !INNER_FUNCTION_WORDS.contains(&w.as_str()) => return false,
            Tag::Part if i != first => return false,
            _ => {}
        }
        if t.kind == TokenKind::Word && pos::is_possessive(t.text) {
            return false;
        }
        let bare = pos::strip_possessive(&w).to_string();
        let suspect = pos::suspect_token(t.text).is_some()
            || (terms.is_some_and(|ts| ts.is_proper(&bare))
                && !t
                    .text
                    .chars()
                    .filter(|c| c.is_alphabetic())
                    .all(char::is_uppercase));
        if suspect && !target.names_token(&bare) {
            return false;
        }
        if tag(i) != Tag::Func && !LISTICLE_WORDS.contains(&bare.as_str()) {
            descriptive += 1;
        }
    }
    descriptive >= 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kws(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn pick(para: &str, title: &str, k: &[&str]) -> AnchorPick {
        let p = ParagraphNlp::new(para, &[]);
        pick_anchor(
            &p,
            &TargetProfile::new(title, &kws(k), &[]),
            &PickOptions::new(),
        )
    }

    fn phrase(text: &str) -> NounPhrase {
        let tokens = content_tokens(text);
        let stems = tokens.iter().map(|t| stem(t)).collect();
        let words: Vec<&str> = text.split_whitespace().collect();
        let es = |w: Option<&&str>| {
            w.map(|w| content_tokens(w))
                .and_then(|t| t.into_iter().next())
                .map(|t| stem(&t))
                .unwrap_or_default()
        };
        NounPhrase {
            text: text.into(),
            start: 0,
            end: text.len(),
            words: words.len(),
            tokens,
            stems,
            edge_stems: [es(words.first()), es(words.last())],
            suspects: Vec::new(),
            after_brand: false,
            split_modifier: false,
            lead_evaluative: false,
            sentence: 0,
            generic_head: false,
        }
    }

    #[test]
    fn golden_score_phrase() {
        let t = TargetProfile::new("waterproof bike trunk bag lock steel", &[], &[]);
        assert!(score_phrase(&phrase("Top Bike"), &t).is_none());
        assert!(score_phrase(&phrase("Best Picks"), &t).is_none());
        let long = score_phrase(&phrase("waterproof bike trunk bag"), &t).unwrap();
        let short = score_phrase(&phrase("bike bag"), &t).unwrap();
        assert!(long.score > short.score);
    }

    #[test]
    fn golden_weighted_scoring() {
        let low = TargetProfile::new("", &[], &[("waterproof gear".into(), 1.0)]);
        let high = TargetProfile::new("", &[], &[("waterproof gear".into(), 1.5)]);
        let p = phrase("waterproof hiking gear");
        assert!(score_phrase(&p, &high).unwrap().score > score_phrase(&p, &low).unwrap().score);
        let striking = TargetProfile::new(
            "Bike Security Guide",
            &kws(&["bike security"]),
            &[("steel bike lock".into(), STRIKING_WEIGHT)],
        );
        assert_eq!(striking.weights.get("bike").copied(), Some(1.5));
        assert_eq!(striking.weights.get("lock").copied(), Some(1.5));
    }

    #[test]
    fn golden_clean_title() {
        assert_eq!(
            clean_title("11 Best Bike Trunk Bags for 2026 That Commuters Trust", 6),
            "Bike Trunk Bags"
        );
        assert_eq!(
            clean_title("Top 10 Waterproof Backpacks Reviewed", 6),
            "Waterproof Backpacks Reviewed"
        );
        assert!(
            clean_title("Durable Steel Bike Locks With Loud Alarm Systems", 6)
                .split(' ')
                .count()
                <= 6
        );
        assert_eq!(clean_title("", 6), "");
    }

    /// Pilot 2026-10-13 (site65 titles): no year holes, parentheses or dangling stop words.
    #[test]
    fn clean_title_pilot_regressions() {
        let cases = [
            (
                "12 Best Bike Locks With Alarms for 2026 (Loud & Secure Choices)",
                "Bike Locks With Alarms",
            ),
            (
                "6 Best Light Mounts for Bikes in 2026 (Secure & Easy to Install)",
                "Light Mounts for Bikes",
            ),
            (
                "9 Best Comfort Saddles for 2026 That Make Every Ride Easier",
                "Comfort Saddles",
            ),
            (
                "12 Best [with Brackets] Bike Locks for 2026 (Secure & Easy Install)",
                "Bike Locks",
            ),
            (
                "7 Best Bike Storage Solutions for 2026 Garages (Space-Saving & Smart)",
                "Bike Storage Solutions",
            ),
            (
                "8 Best Bike Pedal Clips and Shoes for 2026 (Ride Smarter)",
                "Bike Pedal Clips and Shoes",
            ),
            ("Bike Lights: The Complete Guide", "Bike Lights"),
            ("HTML SITEMAP", "HTML SITEMAP"),
            ("Accessories & Gear", "Accessories & Gear"),
        ];
        for (title, want) in cases {
            assert_eq!(clean_title(title, MAX_ANCHOR_WORDS), want, "{title}");
        }
        for t in [
            "(((",
            "[a] [b] [c] [d] [e]",
            "2026 2026",
            "- - -",
            "Best (x",
        ] {
            let c = clean_title(t, 6);
            assert!(!c.contains('(') && !c.contains('['), "{t} -> {c}");
        }
    }

    #[test]
    fn golden_pick_a_prefers_descriptive_phrase() {
        let p = pick(
            "We reviewed our Top Bike lock picks. The waterproof bike trunk bag stood out as the most durable option for daily commuters.",
            "Best Bike Trunk Bags",
            &["waterproof bike trunk bag", "bike trunk bag"],
        );
        assert!(p.in_paragraph);
        assert_eq!(p.anchor.to_lowercase(), "waterproof bike trunk bag");
    }

    #[test]
    fn golden_pick_b_rejects_generic() {
        let p = pick(
            "Our Best Picks include the Top Bike model, but the durable steel bike lock impressed reviewers most.",
            "Bike Lock Reviews",
            &["steel bike lock", "bike lock"],
        );
        assert!(p.in_paragraph);
        let a = p.anchor.to_lowercase();
        assert_ne!(a, "top bike");
        assert_ne!(a, "best picks");
        assert!(a.contains("steel bike"), "{a}");
        assert!(p.anchor.split_whitespace().count() >= 3);
    }

    #[test]
    fn golden_pick_b2_title_fallback() {
        let p = pick(
            "Our Best Picks. The Top Bike. Best choice overall.",
            "11 Best Bike Trunk Bags for 2026 That Commuters Trust",
            &["bike trunk bag"],
        );
        assert!(!p.in_paragraph);
        assert_eq!(p.anchor, "Bike Trunk Bags");
        assert_eq!(p.source, PickSource::CleanTitle);
    }

    #[test]
    fn golden_across_paragraphs() {
        let a = ParagraphNlp::new("Our Best Picks. The Top Bike. Best choice overall.", &[]);
        let b = ParagraphNlp::new(
            "The durable steel bike lock impressed reviewers most this year.",
            &[],
        );
        let t = TargetProfile::new(
            "Bike Lock Reviews",
            &kws(&["steel bike lock", "bike lock"]),
            &[],
        );
        let p = pick_anchor_across(&[(0, &a), (1, &b)], &t, &PickOptions::new());
        assert!(p.in_paragraph);
        assert_eq!(p.paragraph_idx, Some(1));
        assert!(p.anchor.to_lowercase().contains("steel bike"));

        let c = ParagraphNlp::new(
            "The waterproof bike trunk bag is our top recommendation for commuters.",
            &[],
        );
        let d = ParagraphNlp::new("A steel bike lock also performed well in testing.", &[]);
        let t2 = TargetProfile::new(
            "Best Bike Trunk Bags",
            &kws(&["waterproof bike trunk bag"]),
            &[],
        );
        assert_eq!(
            pick_anchor_across(&[(3, &c), (7, &d)], &t2, &PickOptions::new()).paragraph_idx,
            Some(3)
        );

        let t3 = TargetProfile::new(
            "11 Best Bike Trunk Bags for 2026 That Commuters Trust",
            &kws(&["bike trunk bag"]),
            &[],
        );
        let e = ParagraphNlp::new("Our Best Picks. The Top Bike.", &[]);
        let f = ParagraphNlp::new("Best choice overall. Great value.", &[]);
        let p3 = pick_anchor_across(&[(0, &e), (1, &f)], &t3, &PickOptions::new());
        assert!(!p3.in_paragraph);
        assert_eq!(p3.anchor, "Bike Trunk Bags");
        let p4 = pick_anchor_across(
            &[],
            &TargetProfile::new(
                "11 Best Bike Trunk Bags for 2026",
                &kws(&["bike trunk bag"]),
                &[],
            ),
            &PickOptions::new(),
        );
        assert!(!p4.in_paragraph && !p4.anchor.is_empty());
    }

    #[test]
    fn guards_blocked_used_ignored_reserved() {
        let text = "Read about the steel bike lock and the best waterproof bike trunk bag today.";
        let t = TargetProfile::new(
            "Bike Gear",
            &kws(&["steel bike lock", "waterproof bike trunk bag"]),
            &[],
        );
        let p = ParagraphNlp::new(text, &[]);
        let base = pick_anchor(&p, &t, &PickOptions::new());
        assert!(base.in_paragraph);
        // blocked span over the winner forces another choice
        let (s, e) = base.span.unwrap();
        let p2 = ParagraphNlp::new(text, &[(s, e)]);
        let other = pick_anchor(&p2, &t, &PickOptions::new());
        assert_ne!(other.anchor, base.anchor);
        // used in source / reserved exact / ignored all exclude the phrase
        for mutate in 0..3 {
            let mut o = PickOptions::new();
            let lower = base.anchor.to_lowercase();
            match mutate {
                0 => {
                    o.used_in_source.insert(lower);
                }
                1 => {
                    o.reserved_exact.insert(lower);
                }
                _ => o.ignore.push(lower),
            }
            assert_ne!(
                pick_anchor(&p, &t, &o).anchor.to_lowercase(),
                base.anchor.to_lowercase()
            );
        }
        // ignore list is word-boundary: "car" must not block "carpet cleaning machine"
        assert!(!is_ignored("carpet cleaning machine", &["car".into()]));
        assert!(is_ignored("a car wash", &["car".into()]));
    }

    #[test]
    fn keyword_fallback_is_multiword_and_word_bounded() {
        // Only a one-word keyword occurs -> not used (C.5); title fallback instead.
        let p = pick(
            "Locks are everywhere, really.",
            "Steel Bike Locks",
            &["locks"],
        );
        assert!(!p.in_paragraph);
        // word boundary: "bike locks" must not match inside "ebike lockstep"
        assert!(find_unblocked("ebike lockstep", "bike lock", &[]).is_none());
        assert_eq!(
            find_unblocked("Big Bike Lock here", "bike lock", &[]),
            Some((4, 13))
        );
        assert_eq!(
            find_unblocked("İstanbul bike lock", "bike lock", &[]).map(|x| x.1),
            Some("İstanbul bike lock".len())
        );
    }

    #[test]
    fn diversity_and_exact_penalties_shift_choice() {
        let text = "Compare the steel bike lock with a heavy steel chain lock for city use.";
        let t = TargetProfile::new("Steel Bike Lock", &kws(&["steel bike lock"]), &[]);
        let p = ParagraphNlp::new(text, &[]);
        let base = pick_anchor(&p, &t, &PickOptions::new());
        assert_eq!(base.anchor, "steel bike lock");
        let mut o = PickOptions::new();
        o.avoid_exact = true;
        o.target_anchor_counts.insert("steel bike lock".into(), 3);
        let alt = pick_anchor(&p, &t, &o);
        assert_ne!(alt.anchor, "steel bike lock");
    }

    /// v2.0.1: unit / spec fragments and trailing adjectives (Hermes acceptance, site46.example).
    #[test]
    fn fragments_are_never_natural_or_picked() {
        let t = TargetProfile::new(
            "12 Best Drill Driver Kits Under $200 in 2026",
            &kws(&["drill driver kits", "drill kits", "20v"]),
            &[],
        );
        for bad in [
            "V kits perfect",
            "V kits",
            "inch cordless Impact Wrenches",
            "Oz Water Bottles",
            "drill kits perfect",
            "12V options",
            "water bottle easy",
            "kits available",
            "Drill Kits 20V",
            "mm drill bits",
        ] {
            assert!(!is_natural_anchor(bad, &t, None), "{bad}");
        }
        for good in ["drill driver kits", "cordless drill kits", "Vitamin C"] {
            assert!(is_natural_anchor(good, &t, None), "{good}");
        }
        // a whole spec is fine when the target names it
        let w = TargetProfile::new(
            "10 Best 1/2 Inch Cordless Impact Wrenches for 2026",
            &[],
            &[],
        );
        assert!(is_natural_anchor(
            "1/2 Inch Cordless Impact Wrenches",
            &w,
            None
        ));
        assert!(!is_natural_anchor(
            "Inch Cordless Impact Wrenches",
            &w,
            None
        ));
        assert!(!is_natural_anchor("1/2 Cordless Impact Wrenches", &w, None));
        let p = pick(
            "You'll find powerful 20V kits perfect for heavy-duty tasks, while 8V to 12V options \
             keep things light. Look for lithium-ion batteries with quick USB-C charging.",
            "12 Best Drill Driver Kits Under $200 in 2026",
            &["drill driver kits", "kits", "batteries"],
        );
        assert!(
            !p.anchor.starts_with("V ") && !p.anchor.ends_with("perfect"),
            "{}",
            p.anchor
        );
        let p = pick(
            "You get serious power in 2026's top 3/8-inch cordless Impact Wrenches, with brushless \
             motors.",
            "10 Best 3/8 Inch Cordless Impact Wrenches for 2026",
            &["cordless impact wrenches", "inch cordless impact wrenches"],
        );
        assert_eq!(p.anchor, "cordless Impact Wrenches");
    }

    #[test]
    fn keyword_fallback_skips_spec_fragments() {
        // only the keyword matches; "inch ..." would cut "1/2-inch" in two
        let p = pick(
            "Grab a 1/2-inch impact wrench today.",
            "Impact Gear",
            &["inch impact wrench"],
        );
        assert!(!p.anchor.to_lowercase().starts_with("inch"), "{}", p.anchor);
        assert!(text_mid_spec("3/8-inch", 2));
        assert!(text_mid_spec("4.5 Inch", 2));
        assert!(!text_mid_spec("a 20 V", 5));
        assert!(!text_mid_spec("", 0));
        assert!(!text_mid_spec("\u{d7}", 99));
    }

    #[test]
    fn clean_title_keeps_whole_specs() {
        assert_eq!(
            clean_title("10 Best 1/2 Inch Cordless Impact Wrenches for 2026", 6),
            "1/2 Inch Cordless Impact Wrenches"
        );
        assert_eq!(
            clean_title("12 Best 4 1/2 Inch Angle Grinders for 2026 (Top Picks)", 6),
            "4 1/2 Inch Angle Grinders"
        );
        assert_eq!(
            clean_title("9 Best 32 Oz Water Bottles With Straw for 2026", 6),
            "32 Oz Water Bottles With Straw"
        );
    }

    #[test]
    fn semantic_floor_falls_back_to_keyword_then_title() {
        let t = TargetProfile::new(
            "11 Best Phone Mounts for Bike in 2026",
            &kws(&["road bike phone mount", "phone mounts"]),
            &[],
        );
        let p = ParagraphNlp::new("You will dominate the open road with these shades.", &[]);
        let mut o = PickOptions::new();
        assert_eq!(pick_anchor(&p, &t, &o).anchor, "open road", "lexical pick");
        o.semantic.insert("open road".into(), 0.01);
        // off-topic and no keyword in the paragraph -> cleaned title, not native
        let fb = pick_anchor(&p, &t, &o);
        assert_eq!(fb.source, PickSource::CleanTitle, "{fb:?}");
        assert!(!fb.in_paragraph);
        assert_eq!(fb.anchor, "Phone Mounts for Bike");
        // floor 0 disables it; a cosine at the floor passes
        let mut off = o.clone();
        off.min_semantic = 0.0;
        assert_eq!(pick_anchor(&p, &t, &off).anchor, "open road");
        let mut at = o.clone();
        at.semantic.insert("open road".into(), MIN_PHRASE_SEMANTIC);
        assert_eq!(pick_anchor(&p, &t, &at).anchor, "open road");
        // a keyword in the paragraph is the fallback
        let p2 = ParagraphNlp::new(
            "You will dominate the open road with these shades and phone mounts.",
            &[],
        );
        let mut o2 = PickOptions::new();
        for ph in &p2.phrases {
            o2.semantic.insert(ph.text.to_lowercase(), 0.01);
        }
        let k = pick_anchor(&p2, &t, &o2);
        assert!(k.in_paragraph, "{k:?}");
        assert_eq!(k.anchor, "phone mounts");
        // a phrase the target fully names is never dropped by the floor
        let t3 = TargetProfile::new("Road Shades Guide", &kws(&["road shades"]), &[]);
        let p3 = ParagraphNlp::new("The open road calls, so grab road shades now.", &[]);
        let mut o3 = PickOptions::new();
        for ph in &p3.phrases {
            o3.semantic.insert(ph.text.to_lowercase(), 0.01);
        }
        let k3 = pick_anchor(&p3, &t3, &o3);
        assert_eq!(k3.anchor, "road shades", "{k3:?}");
        assert_eq!(k3.source, PickSource::Phrase);
    }

    #[test]
    fn generic_head_needs_a_named_modifier() {
        let terms = SiteTerms::from_counts(
            100,
            [("bottl".to_string(), 80u32)].into_iter().collect(),
            &[],
        );
        let t = TargetProfile::new(
            "How to Paint Wine Bottles With Acrylic Paint",
            &kws(&["wine bottles", "acrylic paint"]),
            &[],
        );
        let p = ParagraphNlp::with_terms(
            "Clear glass bottles take paint evenly. Always rinse the wine bottles.",
            &[],
            Some(&terms),
        );
        let get = |s: &str| p.phrases.iter().find(|x| x.text == s);
        let glass = get("Clear glass bottles").map(|x| x.generic_head);
        assert_eq!(glass, Some(true));
        let (Some(g), Some(w)) = (get("Clear glass bottles"), get("wine bottles")) else {
            panic!("phrases {:?}", p.phrases);
        };
        let (Some(gs), Some(ws)) = (score_phrase(g, &t), score_phrase(w, &t)) else {
            panic!("unscored");
        };
        assert!(
            ws.score > gs.score + GENERIC_HEAD_PENALTY / 2.0,
            "{ws:?} {gs:?}"
        );
        assert_eq!(
            pick_anchor(&p, &t, &PickOptions::new()).anchor,
            "wine bottles"
        );
    }
}
