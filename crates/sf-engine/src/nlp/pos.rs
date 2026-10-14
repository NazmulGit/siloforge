//! Lightweight part-of-speech heuristic for anchor phrases (no model).
//!
//! Closed-class word lists (function words, adverbs, number words), suffix rules (`-ly`, `-ed`,
//! `-ing`, `-s`, adjective suffixes) and a compact English verb lexicon tag each word token with a
//! coarse class. A few context rules separate noun/verb homographs ("bike lock" vs "you lock",
//! "lights fit most wheels"). Good enough to keep verbs, adverbs and pronouns out of noun-phrase
//! anchors (pilot 2026-10-13: "phone mount withstand daily abuse", "need dependable bike chain").
//!
//! Also: "suspect" tokens (model numbers, unit specs, ALL-CAPS and camelCase brand words) that
//! must not appear in an anchor unless the target itself names them.

use super::tokenize::{Token, TokenKind, norm_word};

/// Coarse word class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    /// Determiner, pronoun, preposition, conjunction, auxiliary, contraction: ends a phrase.
    Func,
    Adv,
    Verb,
    /// Regular `-ed` participle: an adjective at the start of a phrase ("padded shorts"), a verb
    /// right after a noun ("lock impressed reviewers").
    Part,
    /// Adjective (by lexicon/suffix): fine inside a phrase, never its last word.
    Adj,
    /// Number or number word: inside a phrase only.
    Num,
    Noun,
}

impl Tag {
    /// Can be part of a noun-phrase run.
    pub fn in_phrase(self) -> bool {
        matches!(self, Tag::Noun | Tag::Adj | Tag::Num | Tag::Part)
    }
}

/// Verb inflection of a word form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Base,
    S,
    Ed,
    Ing,
}

/// Verbs that are rarely nouns in product/review prose (base forms, sorted).
static VERBS: &[&str] = &[
    "accept",
    "achieve",
    "adapt",
    "add",
    "adjust",
    "afford",
    "aim",
    "align",
    "allow",
    "appear",
    "appreciate",
    "arrive",
    "assemble",
    "assume",
    "attach",
    "avoid",
    "become",
    "begin",
    "believe",
    "belong",
    "blend",
    "boast",
    "boost",
    "breathe",
    "bring",
    "buy",
    "carry",
    "choose",
    "combine",
    "come",
    "compare",
    "compromise",
    "confirm",
    "connect",
    "consider",
    "contain",
    "continue",
    "create",
    "decide",
    "deliver",
    "depend",
    "deploy",
    "describe",
    "deserve",
    "detach",
    "detect",
    "deter",
    "determine",
    "develop",
    "discover",
    "distribute",
    "earn",
    "eliminate",
    "enable",
    "encourage",
    "endure",
    "enhance",
    "enjoy",
    "ensure",
    "enter",
    "equip",
    "evaluate",
    "exceed",
    "excel",
    "expand",
    "expect",
    "explain",
    "explore",
    "extend",
    "fail",
    "fasten",
    "feel",
    "find",
    "follow",
    "forget",
    "fuse",
    "gain",
    "get",
    "give",
    "go",
    "grab",
    "grow",
    "guarantee",
    "happen",
    "hate",
    "have",
    "hear",
    "help",
    "hesitate",
    "hold",
    "hope",
    "identify",
    "ignore",
    "imagine",
    "improve",
    "include",
    "increase",
    "indicate",
    "inspect",
    "install",
    "intend",
    "introduce",
    "invest",
    "involve",
    "join",
    "keep",
    "know",
    "learn",
    "leave",
    "let",
    "live",
    "look",
    "lose",
    "love",
    "maintain",
    "make",
    "manage",
    "maximize",
    "mean",
    "meet",
    "minimize",
    "miss",
    "move",
    "need",
    "notice",
    "obtain",
    "occur",
    "offer",
    "operate",
    "opt",
    "outlast",
    "outperform",
    "overlook",
    "own",
    "pay",
    "perform",
    "permit",
    "prefer",
    "prepare",
    "preserve",
    "prevent",
    "prioritize",
    "produce",
    "promise",
    "protect",
    "prove",
    "provide",
    "pull",
    "purchase",
    "push",
    "put",
    "reach",
    "realize",
    "receive",
    "recharge",
    "recommend",
    "reduce",
    "refer",
    "refuse",
    "relieve",
    "rely",
    "remain",
    "remember",
    "remove",
    "replace",
    "require",
    "resist",
    "respond",
    "retain",
    "reveal",
    "say",
    "see",
    "seek",
    "seem",
    "sell",
    "send",
    "serve",
    "settle",
    "shine",
    "show",
    "shrug",
    "sit",
    "slide",
    "solve",
    "spend",
    "stay",
    "stick",
    "struggle",
    "suffer",
    "suggest",
    "suppose",
    "survive",
    "tackle",
    "take",
    "talk",
    "tell",
    "tend",
    "thank",
    "think",
    "throw",
    "tighten",
    "tolerate",
    "transform",
    "try",
    "understand",
    "unlock",
    "want",
    "warn",
    "wear",
    "weigh",
    "welcome",
    "wish",
    "withstand",
    "wonder",
    "worry",
    "write",
];

/// Noun/verb homographs: nouns by default, verbs in verb contexts (sorted).
static HOMOGRAPHS: &[&str] = &[
    "balance", "block", "brake", "build", "charge", "check", "clamp", "clip", "cover", "cushion",
    "cut", "design", "drive", "feature", "fit", "fold", "grip", "handle", "light", "lock", "match",
    "measure", "mount", "pack", "pair", "park", "pedal", "power", "pump", "rest", "ride", "roll",
    "select", "set", "shift", "stand", "start", "stop", "store", "strike", "support", "test",
    "turn", "work",
];

/// Irregular past / participle forms (always verbs; sorted).
static IRREGULAR_PAST: &[&str] = &[
    "began",
    "begun",
    "bought",
    "brought",
    "built",
    "came",
    "chose",
    "chosen",
    "did",
    "done",
    "felt",
    "found",
    "gave",
    "given",
    "got",
    "gotten",
    "grew",
    "grown",
    "had",
    "held",
    "kept",
    "knew",
    "known",
    "led",
    "left",
    "lost",
    "made",
    "meant",
    "met",
    "paid",
    "ran",
    "said",
    "sat",
    "saw",
    "seen",
    "sent",
    "sold",
    "spent",
    "stood",
    "stuck",
    "taken",
    "taught",
    "thought",
    "threw",
    "thrown",
    "told",
    "took",
    "understood",
    "went",
    "won",
    "wore",
    "worn",
    "written",
    "wrote",
];

/// Function words not covered by the phrase breakers (contractions, quantifiers; sorted).
static EXTRA_FUNC: &[&str] = &[
    "anyway",
    "else",
    "everybody",
    "he's",
    "here's",
    "i'd",
    "i'll",
    "i'm",
    "i've",
    "it'd",
    "it'll",
    "let's",
    "lets",
    "nobody",
    "none",
    "nothing",
    "one",
    "ones",
    "she's",
    "somebody",
    "they'd",
    "they'll",
    "they're",
    "they've",
    "we'd",
    "we'll",
    "we're",
    "we've",
    "what's",
    "where's",
    "who's",
    "you'd",
    "you'll",
    "you're",
    "you've",
];

/// Adverbs without a `-ly` suffix (sorted).
static ADVERBS: &[&str] = &[
    "abroad",
    "afterwards",
    "ahead",
    "alone",
    "anymore",
    "anywhere",
    "apart",
    "aside",
    "barely",
    "everywhere",
    "fast",
    "forth",
    "hardly",
    "later",
    "meanwhile",
    "nonetheless",
    "nowhere",
    "regardless",
    "somewhere",
    "today",
    "tomorrow",
];

/// `-ly` words that are nouns or adjectives, not adverbs (sorted).
static LY_NOT_ADV: &[&str] = &[
    "ally",
    "anomaly",
    "apply",
    "assembly",
    "belly",
    "bubbly",
    "bully",
    "burly",
    "butterfly",
    "chilly",
    "comply",
    "costly",
    "cuddly",
    "curly",
    "daily",
    "dragonfly",
    "early",
    "elderly",
    "family",
    "fly",
    "folly",
    "friendly",
    "gully",
    "hilly",
    "holly",
    "holy",
    "hourly",
    "imply",
    "italy",
    "jelly",
    "jolly",
    "july",
    "likely",
    "lily",
    "lively",
    "lonely",
    "lovely",
    "monopoly",
    "monthly",
    "multiply",
    "nightly",
    "oily",
    "orderly",
    "poly",
    "quarterly",
    "rally",
    "rely",
    "reply",
    "silly",
    "sly",
    "supply",
    "surly",
    "tally",
    "timely",
    "ugly",
    "weekly",
    "wobbly",
    "woolly",
    "wooly",
    "yearly",
];

/// Adjectives that make a bad last word (sorted). Suffix rules cover most others.
static ADJECTIVES: &[&str] = &[
    "able",
    "affordable",
    "available",
    "awesome",
    "big",
    "bright",
    "bulky",
    "cheap",
    "clean",
    "close",
    "comfy",
    "compact",
    "complete",
    "cool",
    "dry",
    "easy",
    "excellent",
    "extra",
    "far",
    "fine",
    "flat",
    "free",
    "full",
    "good",
    "great",
    "handy",
    "heavy",
    "high",
    "huge",
    "ideal",
    "large",
    "lightweight",
    "long",
    "loose",
    "low",
    "narrow",
    "near",
    "new",
    "nice",
    "old",
    "perfect",
    "premium",
    "quick",
    "quiet",
    "ready",
    "real",
    "right",
    "rugged",
    "safe",
    "secure",
    "sharp",
    "short",
    "simple",
    "sleek",
    "small",
    "smart",
    "smooth",
    "snug",
    "soft",
    "solid",
    "stable",
    "steady",
    "stiff",
    "strong",
    "sturdy",
    "sure",
    "tight",
    "tiny",
    "tough",
    "true",
    "versatile",
    "warm",
    "waterproof",
    "wet",
    "wide",
    "worth",
    "wrong",
];

/// Adjectives that follow the noun they describe ("bottles available", "kits perfect for";
/// sorted).
static POSTPOSITIVE: &[&str] = &[
    "available",
    "compatible",
    "ideal",
    "perfect",
    "ready",
    "suitable",
];

/// Salesy lead adjectives: allowed, but an anchor reads better without them (sorted).
static EVALUATIVE: &[&str] = &[
    "amazing",
    "awesome",
    "dependable",
    "excellent",
    "exceptional",
    "fantastic",
    "great",
    "high-quality",
    "ideal",
    "impressive",
    "incredible",
    "outstanding",
    "perfect",
    "premium",
    "quality",
    "reliable",
    "serious",
    "smart",
    "solid",
    "superior",
    "top-notch",
    "top-tier",
    "unbeatable",
    "unmatched",
];

/// Number words (sorted).
static NUMBER_WORDS: &[&str] = &[
    "dozen",
    "eight",
    "eighteen",
    "eleven",
    "fifteen",
    "fifty",
    "five",
    "forty",
    "four",
    "fourteen",
    "hundred",
    "nine",
    "nineteen",
    "ninety",
    "seven",
    "seventeen",
    "six",
    "sixteen",
    "sixty",
    "ten",
    "thirteen",
    "thirty",
    "thousand",
    "three",
    "twelve",
    "twenty",
    "two",
];

/// `-ed` words that are not participles (sorted).
static ED_NOT_PART: &[&str] = &[
    "embed", "hundred", "kindred", "naked", "sacred", "shed", "shred", "sled", "wicked",
];

/// Words after which a homograph is a verb ("you lock", "to match", "will fit"; sorted).
static VERB_TRIGGERS: &[&str] = &[
    "can", "can't", "cannot", "could", "didn't", "doesn't", "don't", "he", "i", "i'll", "may",
    "might", "must", "never", "she", "should", "they", "they'll", "to", "we", "we'll", "will",
    "won't", "would", "you", "you'll",
];

/// Words that start an object after a verb ("fit most wheels", "keeps the chain"; sorted).
static OBJECT_MARKERS: &[&str] = &[
    "a", "all", "an", "any", "both", "each", "every", "her", "him", "his", "it", "its", "many",
    "me", "more", "most", "my", "our", "out", "some", "the", "their", "them", "these", "this",
    "those", "up", "us", "you", "your",
];

/// Determiners / possessives after which a verb form is a noun ("a need", "your hold"; sorted).
static NOUN_DETERMINERS: &[&str] = &[
    "a", "an", "any", "each", "every", "her", "his", "its", "my", "no", "our", "that", "the",
    "their", "this", "your",
];

fn has(list: &[&str], w: &str) -> bool {
    list.binary_search(&w).is_ok()
}

/// Possible verb lemma + form of a word ("keeps" -> keep/S, "chosen" handled as irregular).
fn verb_forms(w: &str) -> Vec<(String, Form)> {
    let mut out = vec![(w.to_string(), Form::Base)];
    let n = w.len();
    if n < 4 {
        return out;
    }
    if let Some(s) = w.strip_suffix("ies") {
        out.push((format!("{s}y"), Form::S));
    } else if let Some(s) = w.strip_suffix("es") {
        out.push((s.to_string(), Form::S));
        out.push((format!("{s}e"), Form::S));
    } else if let Some(s) = w.strip_suffix('s')
        && !w.ends_with("ss")
    {
        out.push((s.to_string(), Form::S));
    }
    if let Some(s) = w.strip_suffix("ied") {
        out.push((format!("{s}y"), Form::Ed));
    } else if let Some(s) = w.strip_suffix("ed") {
        out.push((s.to_string(), Form::Ed));
        out.push((format!("{s}e"), Form::Ed));
        if let Some(d) = undouble(s) {
            out.push((d, Form::Ed));
        }
    }
    if let Some(s) = w.strip_suffix("ing")
        && s.len() >= 2
    {
        out.push((s.to_string(), Form::Ing));
        out.push((format!("{s}e"), Form::Ing));
        if let Some(d) = undouble(s) {
            out.push((d, Form::Ing));
        }
    }
    out
}

/// "stopp" -> "stop" (doubled final consonant before -ed/-ing).
fn undouble(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let n = b.len();
    if n >= 3 && b.get(n - 1) == b.get(n - 2) && b.get(n - 1).is_some_and(|c| !b"aeiou".contains(c))
    {
        s.get(..n - 1).map(str::to_string)
    } else {
        None
    }
}

/// Lexicon lookup of a verb form: (homograph?, form) for the first matching lemma.
fn verb_lookup(w: &str) -> Option<(bool, Form)> {
    for (lemma, form) in verb_forms(w) {
        if has(VERBS, &lemma) {
            return Some((false, form));
        }
        if has(HOMOGRAPHS, &lemma) {
            return Some((true, form));
        }
    }
    None
}

fn is_function_word(w: &str) -> bool {
    super::stopwords::is_phrase_breaker(w) || has(EXTRA_FUNC, w)
}

/// `-ly` adverb heuristic with a noun/adjective exception list.
pub fn is_adverb(w: &str) -> bool {
    if has(ADVERBS, w) {
        return true;
    }
    w.len() > 4
        && w.ends_with("ly")
        && !w.contains('-')
        && !has(LY_NOT_ADV, w)
        && !w.ends_with("ply")
}

/// Adjective by lexicon or suffix (`-able`, `-ible`, `-ous`, `-ful`, `-less`, `-ive`, `-ish`).
pub fn is_adjective(w: &str) -> bool {
    if has(ADJECTIVES, w) || has(EVALUATIVE, w) {
        return true;
    }
    if w.len() < 6 {
        return false;
    }
    const NOT_ADJ: &[&str] = &[
        "adhesive",
        "archive",
        "cable",
        "detective",
        "directive",
        "english",
        "finish",
        "incentive",
        "initiative",
        "locomotive",
        "motive",
        "narrative",
        "objective",
        "olive",
        "perspective",
        "polish",
        "relish",
        "representative",
        "table",
        "timetable",
        "turntable",
        "vegetable",
    ];
    if NOT_ADJ.contains(&w) {
        return false;
    }
    ["able", "ible", "ous", "ful", "less", "ive", "ish"]
        .iter()
        .any(|s| w.ends_with(s))
        && !w.ends_with("fulness")
}

/// Salesy lead adjective ("serious", "reliable", "top-tier").
pub fn is_evaluative(w: &str) -> bool {
    has(EVALUATIVE, w)
}

/// Year token (`1990`-`2099`, possessive allowed).
pub fn is_year(w: &str) -> bool {
    let w = strip_possessive(w);
    w.len() == 4
        && (w.starts_with("19") || w.starts_with("20"))
        && w.chars().all(|c| c.is_ascii_digit())
}

/// Lowercase word without a trailing possessive.
pub fn strip_possessive(w: &str) -> &str {
    ["'s", "\u{2019}s", "'S", "\u{2019}S"]
        .iter()
        .find_map(|s| w.strip_suffix(s))
        .filter(|r| !r.is_empty())
        .unwrap_or(w)
}

/// True when the token is a possessive noun ("shoe's", "Thule’s"), not a contraction.
pub fn is_possessive(raw: &str) -> bool {
    let w = norm_word(raw);
    w.len() > 2 && w.ends_with("'s") && !is_function_word(&w)
}

/// Context-free class of a lowercase word.
fn lexical(w: &str, kind: TokenKind) -> Tag {
    match kind {
        TokenKind::Number => return Tag::Num,
        TokenKind::Punct => return Tag::Func,
        TokenKind::Word => {}
    }
    let w = strip_possessive(w);
    if is_function_word(w) {
        return Tag::Func;
    }
    if has(NUMBER_WORDS, w) {
        return Tag::Num;
    }
    if is_adverb(w) {
        return Tag::Adv;
    }
    if has(IRREGULAR_PAST, w) {
        return Tag::Verb;
    }
    if is_adjective(w) {
        return Tag::Adj;
    }
    if is_number_descriptor(w) {
        return Tag::Adj;
    }
    Tag::Noun
}

/// "16-inch", "3-hole", "4-digit", "2-pack": a number-word compound (a modifier, never a head).
pub fn is_number_descriptor(w: &str) -> bool {
    let parts: Vec<&str> = w.split('-').collect();
    parts.len() >= 2
        && parts
            .iter()
            .any(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit() || c == '.'))
        && parts.iter().all(|p| {
            !p.is_empty()
                && (p.chars().all(|c| c.is_ascii_digit() || c == '.')
                    || p.chars().all(char::is_alphabetic))
        })
}

/// Singular nouns that end in `s` (sorted).
static S_SINGULARS: &[&str] = &[
    "aerodynamics",
    "alias",
    "atlas",
    "axis",
    "bias",
    "canvas",
    "chassis",
    "dynamics",
    "economics",
    "ergonomics",
    "gas",
    "lens",
    "mechanics",
    "news",
    "physics",
    "series",
    "species",
];

fn plural_noun(w: &str) -> bool {
    !has(S_SINGULARS, w)
        && w.len() > 3
        && w.ends_with('s')
        && !w.ends_with("ss")
        && !w.ends_with("us")
        && !w.ends_with("is")
}

/// Tag every token of a tokenized text (same length as `toks`).
pub fn tag_tokens(toks: &[Token<'_>]) -> Vec<Tag> {
    let lower: Vec<String> = toks.iter().map(|t| norm_word(t.text)).collect();
    let mut tags: Vec<Tag> = toks
        .iter()
        .zip(&lower)
        .map(|(t, w)| lexical(w, t.kind))
        .collect();
    for i in 0..toks.len() {
        let Some(t) = toks.get(i) else { continue };
        if t.kind != TokenKind::Word {
            continue;
        }
        let Some(w) = lower.get(i) else { continue };
        let w = strip_possessive(w);
        if is_function_word(w) || has(NUMBER_WORDS, w) || has(IRREGULAR_PAST, w) || is_adverb(w) {
            continue;
        }
        let prev = i
            .checked_sub(1)
            .and_then(|p| toks.get(p).zip(lower.get(p)))
            .filter(|(pt, _)| pt.kind == TokenKind::Word)
            .map(|(_, pw)| pw.as_str());
        let prev_is_sentence_start = i == 0
            || i.checked_sub(1)
                .and_then(|p| toks.get(p))
                .is_some_and(|pt| {
                    pt.kind == TokenKind::Punct && matches!(pt.text, "." | "!" | "?" | ":" | ";")
                });
        let next = toks
            .get(i + 1)
            .zip(lower.get(i + 1))
            .map(|(nt, nw)| (nt.kind, nw.as_str()));
        let next_object = next.is_some_and(|(k, nw)| {
            k == TokenKind::Number || (k == TokenKind::Word && has(OBJECT_MARKERS, nw))
        });
        // "helmet fits right", "lock works well": an adjective/adverb right after
        // ... but not a postpositive adjective after a plural noun ("caps available", "crafts
        // perfect for"): there the -s form is the noun
        let next_modifier = next.is_some_and(|(k, nw)| {
            k == TokenKind::Word
                && (is_adverb(nw) || has(ADJECTIVES, nw) || nw == "well")
                && !has(POSTPOSITIVE, nw)
        });
        let after_determiner = prev.is_some_and(|p| has(NOUN_DETERMINERS, p));
        let after_trigger = prev.is_some_and(|p| has(VERB_TRIGGERS, p));
        let prev_tag = i.checked_sub(1).and_then(|p| tags.get(p)).copied();
        let after_noun = prev_tag.is_some_and(|pt| pt == Tag::Noun);
        let Some((homograph, form)) = verb_lookup(w) else {
            continue;
        };
        let verb = match (homograph, form) {
            (_, Form::Ing) if after_noun && prev.is_some_and(plural_noun) => true,
            // dominant verbs: verbs unless a determiner makes them a noun ("a need", "the hold")
            (false, Form::Base | Form::S) => !after_determiner,
            (false, Form::Ed | Form::Ing) => true,
            // homographs: "-ing"/"-ed" are modifiers ("folding bike", "padded seat")
            (true, Form::Ing | Form::Ed) => false,
            (true, Form::Base) => {
                // "stands support between", "wheels support balance growth": a base form right
                // after a plural noun is a verb (compounds take a singular modifier: "bike rack")
                after_trigger
                    || (prev.is_some_and(plural_noun) && after_noun)
                    || (next_object
                        && (prev_is_sentence_start
                            || prev.is_some_and(|p| !has(NOUN_DETERMINERS, p))))
            }
            (true, Form::S) => {
                prev.is_some_and(|p| {
                    matches!(p, "it" | "he" | "she" | "that" | "which" | "who" | "this")
                }) || ((next_object || next_modifier)
                    && after_noun
                    && prev.is_some_and(|p| !plural_noun(p)))
            }
        };
        if verb {
            if let Some(tag) = tags.get_mut(i) {
                *tag = Tag::Verb;
            }
        } else if !homograph && matches!(form, Form::Base | Form::S) {
            if let Some(tag) = tags.get_mut(i) {
                *tag = Tag::Noun;
            }
        } else if form == Form::Ed
            && let Some(tag) = tags.get_mut(i)
        {
            *tag = Tag::Part;
        }
    }
    // Gerund objects: "Try [crafting] wine bottle lights", "avoid [washing] glass bottles".
    for i in 1..toks.len() {
        let (Some(t), Some(pt)) = (toks.get(i), toks.get(i - 1)) else {
            continue;
        };
        if t.kind != TokenKind::Word || pt.kind != TokenKind::Word {
            continue;
        }
        let (Some(w), Some(pw)) = (lower.get(i), lower.get(i - 1)) else {
            continue;
        };
        let lemma = verb_forms(pw)
            .into_iter()
            .any(|(l, f)| f != Form::Ing && has(GERUND_VERBS, &l));
        if lemma
            && w.len() > 5
            && w.ends_with("ing")
            && !w.contains('-')
            && !has(ING_NOUNS, w)
            && tags
                .get(i)
                .is_some_and(|g| matches!(g, Tag::Noun | Tag::Adj | Tag::Part))
            && let Some(tag) = tags.get_mut(i)
        {
            *tag = Tag::Verb;
        }
    }
    // Regular -ed participles not in the lexicon.
    for (i, t) in toks.iter().enumerate() {
        if t.kind != TokenKind::Word {
            continue;
        }
        let Some(w) = lower.get(i) else { continue };
        if tags.get(i) == Some(&Tag::Noun)
            && w.len() > 4
            && w.ends_with("ed")
            && !w.ends_with("eed")
            && !has(ED_NOT_PART, w)
            && !w.contains('-')
            && let Some(tag) = tags.get_mut(i)
        {
            *tag = Tag::Part;
        }
    }
    tags
}

/// Kinds of suspect tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suspect {
    /// Model number / unit spec: "72V", "60Ah", "Oj9010", "10393".
    Model,
    /// ALL-CAPS word of 5+ letters ("AGPTEK") or camelCase ("iPhone", "monTEK").
    Brand,
}

/// Classify a raw token as a likely model number or brand spelling (lexical rules only).
pub fn suspect_token(raw: &str) -> Option<Suspect> {
    let w = strip_possessive(raw);
    let has_digit = w.chars().any(|c| c.is_ascii_digit());
    let has_alpha = w.chars().any(char::is_alphabetic);
    if has_digit {
        if is_number_descriptor(&w.to_lowercase()) || is_year(w) {
            return None;
        }
        return Some(Suspect::Model);
    }
    if !has_alpha {
        return None;
    }
    let letters: Vec<char> = w.chars().filter(|c| c.is_alphabetic()).collect();
    let upper = letters.iter().filter(|c| c.is_uppercase()).count();
    if upper == letters.len() && letters.len() >= 5 && !w.contains('-') {
        return Some(Suspect::Brand);
    }
    // 4-letter ALL-CAPS: a brand ("AIII", "WORX") unless a known acronym ("MIPS", "HEPA"), the
    // plural of a 3-letter one ("LEDS", "ATVS") or a shouted common word ("BEST", "FREE").
    if upper == letters.len() && letters.len() == 4 && w.chars().count() == 4 {
        let lower = w.to_lowercase();
        let acronym = has(KNOWN_ACRONYMS_4, &lower)
            || lower.ends_with('s')
            || super::stopwords::is_stopword(&lower)
            || super::stopwords::is_listicle(&lower)
            || has(VERBS, &lower)
            || has(HOMOGRAPHS, &lower)
            || is_adjective(&lower)
            || has(BAD_HEADS, &lower)
            || is_unit_word(&lower)
            || has(COMMON_CAPS_WORDS, &lower);
        if !acronym {
            return Some(Suspect::Brand);
        }
    }
    // camelCase: a lowercase letter directly followed by an uppercase one ("iPhone", "monTEK")
    let chars: Vec<char> = w.chars().collect();
    if chars
        .windows(2)
        .any(|p| matches!(p, [a, b] if a.is_lowercase() && b.is_uppercase()))
    {
        return Some(Suspect::Brand);
    }
    None
}

/// Common 4-letter acronyms and initialisms (lowercase, sorted): never brand suspects.
static KNOWN_ACRONYMS_4: &[&str] = &[
    "abec", "ansi", "asap", "astm", "bogo", "cpsc", "dslr", "ebmx", "hdmi", "hdpe", "hepa", "hiit",
    "html", "hvac", "jpeg", "ldpe", "ltpo", "mips", "msrp", "nasa", "ncaa", "nema", "nvme", "oled",
    "pete", "pfas", "pfoa", "ptfe", "qled", "rfid", "sata", "ssid", "tpms", "uhmw", "usps", "wifi",
];

/// Ordinary 4-letter words that show up in ALL CAPS in headings or spec tables (sorted).
static COMMON_CAPS_WORDS: &[&str] = &[
    "also", "body", "bolt", "both", "cage", "deal", "each", "even", "fast", "fork", "from", "gear",
    "here", "into", "kids", "lamp", "lite", "lube", "made", "mini", "mode", "more", "most", "note",
    "only", "over", "pads", "part", "plus", "pump", "rack", "road", "sale", "seat", "size", "tips",
    "tire", "tool", "tube", "very", "what", "when", "with", "your",
];

/// Verbs that take a gerund object ("Try crafting ...", "avoid washing ..."): an -ing word right
/// after one is a verb, not a modifier (sorted).
static GERUND_VERBS: &[&str] = &[
    "avoid",
    "begin",
    "consider",
    "continue",
    "enjoy",
    "finish",
    "imagine",
    "keep",
    "love",
    "practice",
    "prefer",
    "quit",
    "recommend",
    "start",
    "stop",
    "suggest",
    "try",
];

/// -ing words that are nouns even after such a verb ("keep lighting simple"; sorted).
static ING_NOUNS: &[&str] = &[
    "bedding", "ceiling", "clothing", "flooring", "lighting", "seating", "siding", "spring",
    "string", "tubing", "wiring",
];

/// Unit / spec words that are only meaningful after a number ("20 V", "1/2 Inch", "16.9 Oz"):
/// an anchor never starts on one (sorted).
static UNIT_WORDS: &[&str] = &[
    "ah", "cc", "cm", "cu", "db", "feet", "ft", "ft-lbs", "gal", "gallon", "gallons", "gb", "hp",
    "hz", "in-lbs", "inch", "inches", "kg", "khz", "kw", "kwh", "lb", "lbs", "liter", "liters",
    "litre", "litres", "mah", "ml", "mm", "mp", "mph", "nm", "ounce", "ounces", "oz", "pcs", "psi",
    "qt", "qts", "rpm", "tb", "volt", "volts", "watt", "watts", "wh",
];

/// Words that never head an anchor (the last word must be a content noun): trailing
/// evaluations ("kits perfect", "bottle easy") and empty heads ("12V options", "things"; sorted).
static BAD_HEADS: &[&str] = &[
    "aspect",
    "aspects",
    "available",
    "best",
    "better",
    "choice",
    "choices",
    "compatible",
    "easy",
    "good",
    "great",
    "ideal",
    "included",
    "kind",
    "kinds",
    "needed",
    "one",
    "ones",
    "option",
    "options",
    "perfect",
    "popular",
    "ready",
    "stuff",
    "suitable",
    "thing",
    "things",
    "way",
    "ways",
    "worth",
];

/// A unit word ("inch", "oz", "mAh", "V" is a lone letter, see [`is_fragment_lead`]).
pub fn is_unit_word(w_lower: &str) -> bool {
    has(UNIT_WORDS, w_lower)
}

/// A token that cannot start an anchor: a unit word or a lone letter ("V kits", "inch
/// cordless wrenches", "T Rex"): the number before it belongs to the spec.
pub fn is_fragment_lead(w_lower: &str) -> bool {
    let w = strip_possessive(w_lower);
    if is_unit_word(w) {
        return true;
    }
    let mut chars = w.chars();
    matches!((chars.next(), chars.next()), (Some(c), None) if c.is_alphabetic())
}

/// True when the lowercase word cannot be the head (last word) of an anchor: an adjective,
/// evaluation or empty noun, a unit, or a token with digits ("Bike 48V"). A lone letter may end
/// one ("Vitamin C").
pub fn is_bad_head(w_lower: &str) -> bool {
    let w = strip_possessive(w_lower);
    w.is_empty()
        || has(BAD_HEADS, w)
        || has(ADJECTIVES, w)
        || has(EVALUATIVE, w)
        || is_unit_word(w)
        || w.chars().any(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nlp::tokenize::tokenize;

    fn tags(s: &str) -> Vec<(String, Tag)> {
        let t = tokenize(s);
        let g = tag_tokens(&t);
        t.iter().map(|x| x.text.to_string()).zip(g).collect()
    }

    fn tag_of(s: &str, word: &str) -> Tag {
        tags(s)
            .into_iter()
            .find(|(w, _)| w == word)
            .map(|x| x.1)
            .unwrap_or(Tag::Func)
    }

    #[test]
    fn lists_are_sorted() {
        for l in [
            VERBS,
            HOMOGRAPHS,
            IRREGULAR_PAST,
            EXTRA_FUNC,
            ADVERBS,
            LY_NOT_ADV,
            ADJECTIVES,
            EVALUATIVE,
            NUMBER_WORDS,
            ED_NOT_PART,
            VERB_TRIGGERS,
            OBJECT_MARKERS,
            NOUN_DETERMINERS,
            UNIT_WORDS,
            BAD_HEADS,
            POSTPOSITIVE,
            KNOWN_ACRONYMS_4,
            COMMON_CAPS_WORDS,
            GERUND_VERBS,
            ING_NOUNS,
        ] {
            assert!(
                l.windows(2).all(|w| w[0] < w[1]),
                "{:?}",
                l.windows(2).find(|w| w[0] >= w[1])
            );
        }
    }

    #[test]
    fn four_letter_caps_brands_and_gerund_objects() {
        for b in ["AIII", "WORX", "AIII\u{2019}s"] {
            assert_eq!(suspect_token(b), Some(Suspect::Brand), "{b}");
        }
        for a in [
            "MIPS", "HEPA", "LEDS", "ATVS", "BEST", "FREE", "WITH", "LUBE", "USB", "LED",
        ] {
            assert_eq!(suspect_token(a), None, "{a}");
        }
        assert_eq!(
            tag_of("Try crafting wine bottle pendant lighting.", "crafting"),
            Tag::Verb
        );
        assert_eq!(
            tag_of("Kids love riding mountain trails.", "riding"),
            Tag::Verb
        );
        // not after a gerund-taking verb, or an -ing noun
        assert_ne!(
            tag_of("A folding bike fits the trunk.", "folding"),
            Tag::Verb
        );
        assert_eq!(tag_of("Keep lighting simple.", "lighting"), Tag::Noun);
    }

    #[test]
    fn pilot_verbs_are_verbs() {
        assert_eq!(
            tag_of("a phone mount withstand daily abuse", "withstand"),
            Tag::Verb
        );
        assert_eq!(
            tag_of("a phone mount withstand daily abuse", "mount"),
            Tag::Noun
        );
        assert_eq!(
            tag_of("a phone mount withstand daily abuse", "daily"),
            Tag::Noun
        );
        assert_eq!(
            tag_of("You need dependable bike chain tools", "need"),
            Tag::Verb
        );
        assert_eq!(
            tag_of("reliable rear axles to match heavy-duty trailers", "match"),
            Tag::Verb
        );
        assert_eq!(
            tag_of("The cable lock helps secure the bike", "helps"),
            Tag::Verb
        );
        assert_eq!(
            tag_of("The cable lock helps secure the bike", "lock"),
            Tag::Noun
        );
        assert_eq!(
            tag_of("Bike Wheel Lights fit most wheels", "fit"),
            Tag::Verb
        );
        assert_eq!(tag_of("a snug fit for long rides", "fit"), Tag::Noun);
        assert_eq!(tag_of("When choosing cycling shoes", "choosing"), Tag::Verb);
        assert_eq!(tag_of("When choosing cycling shoes", "cycling"), Tag::Noun);
        assert_eq!(
            tag_of("an adult rider seeking comfort", "seeking"),
            Tag::Verb
        );
        assert_eq!(
            tag_of("The lock impressed reviewers", "impressed"),
            Tag::Part
        );
        assert_eq!(tag_of("padded shorts", "padded"), Tag::Part);
        assert_eq!(
            tag_of("The Bell model delivers comfort", "delivers"),
            Tag::Verb
        );
        assert_eq!(tag_of("folding bike", "folding"), Tag::Noun);
        assert_eq!(tag_of("this set keeps the chain clean", "keeps"), Tag::Verb);
        assert_eq!(
            tag_of("Cycling Shoes featuring SPD cleats", "featuring"),
            Tag::Verb
        );
        assert_eq!(
            tag_of("training wheels support balance growth", "support"),
            Tag::Verb
        );
        assert_eq!(tag_of("a lens cover for cameras", "cover"), Tag::Noun);
        assert_eq!(tag_of("make sure the helmet fits right", "fits"), Tag::Verb);
        assert_eq!(tag_of("Pair them with pedals", "Pair"), Tag::Verb);
        assert_eq!(tag_of("the steel bike lock", "lock"), Tag::Noun);
    }

    #[test]
    fn adverbs_adjectives_numbers() {
        assert!(is_adverb("carefully") && is_adverb("seamlessly") && !is_adverb("daily"));
        assert!(!is_adverb("family") && !is_adverb("assembly") && !is_adverb("supply"));
        assert!(is_adjective("comfortable") && is_adjective("secure") && !is_adjective("cable"));
        assert_eq!(tag_of("one lightweight design", "one"), Tag::Func);
        assert_eq!(tag_of("two wheels", "two"), Tag::Num);
        assert!(is_number_descriptor("16-inch") && is_number_descriptor("4-pack"));
        assert!(!is_number_descriptor("e-bike") && !is_number_descriptor("72v"));
    }

    #[test]
    fn suspects() {
        for m in [
            "72V", "60Ah", "Oj9010", "10393", "34L", "2.9L", "12PCS", "PD-EH500",
        ] {
            assert_eq!(suspect_token(m), Some(Suspect::Model), "{m}");
        }
        for b in ["AGPTEK", "KEMIMOTO", "iPhone", "monTEK", "AGPTEK\u{2019}s"] {
            assert_eq!(suspect_token(b), Some(Suspect::Brand), "{b}");
        }
        for ok in [
            "LED", "MIPS", "USB-C", "16-inch", "3-hole", "2026", "bike", "Bike", "E-Bike",
        ] {
            assert_eq!(suspect_token(ok), None, "{ok}");
        }
    }

    #[test]
    fn possessives() {
        assert!(is_possessive("shoe\u{2019}s") && is_possessive("Thule's"));
        assert!(
            !is_possessive("it's") && !is_possessive("that\u{2019}s") && !is_possessive("bikes")
        );
    }
}
