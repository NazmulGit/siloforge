//! Golden anchor-quality set (`golden_anchors.json`, labels by hand; see
//! docs/testing/anchor-quality.md): 61 site67 pilot cases, 35 unit/spec-fragment cases (v2.0.1)
//! and the v2.0.2 cases (brand capitalisation statistics, generic head nouns, gerund leads,
//! off-topic low-cosine phrases). A case may name a `site` whose statistics (`sites`: docs,
//! word df, capitalisation counts) replace the default site67 counts, and a `semantic` map
//! (lowercase phrase -> cosine to the target post) as the plan's semantic step would give.
//!
//! "Natural" = the anchor is one of the hand-labelled acceptable anchors, or passes the strict
//! noun-phrase check [`is_natural_anchor`]. "before" scores the anchor the previous picker chose
//! for the same paragraph in the pilot (`old_anchor`); "after" runs the current picker.
//! Run with `--nocapture` to see the per-case table.

use std::collections::HashMap;

use serde::Deserialize;

use super::{
    PickOptions, TargetProfile, find_unblocked, is_fragment_anchor, is_natural_anchor, pick_anchor,
};
use crate::nlp::{ParagraphNlp, SiteTerms, stem};

#[derive(Deserialize)]
struct Case {
    id: u32,
    target_title: String,
    keywords: Vec<String>,
    paragraph: String,
    old_anchor: String,
    acceptable: Vec<String>,
    #[serde(default)]
    site: Option<String>,
    #[serde(default)]
    semantic: HashMap<String, f32>,
}

#[derive(Deserialize, Default)]
struct SiteFixture {
    docs: u32,
    /// Lowercase word -> posts containing it (folded to stems, max per stem).
    df: HashMap<String, u32>,
    case_counts: Vec<(String, u32, u32)>,
}

#[derive(Deserialize)]
struct Fixture {
    case_counts: Vec<(String, u32, u32)>,
    cases: Vec<Case>,
    #[serde(default)]
    sites: HashMap<String, SiteFixture>,
}

impl SiteFixture {
    fn terms(&self) -> SiteTerms {
        let mut df: HashMap<String, u32> = HashMap::new();
        for (w, n) in &self.df {
            let e = df.entry(stem(w)).or_insert(0);
            *e = (*e).max(*n);
        }
        SiteTerms::from_counts(self.docs, df, &self.case_counts)
    }
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("golden_anchors.json")).unwrap_or(Fixture {
        case_counts: Vec::new(),
        cases: Vec::new(),
        sites: HashMap::new(),
    })
}

/// Site terms of a case + its target (IDF applied when the case has site statistics).
fn setup<'a>(
    c: &Case,
    default: &'a SiteTerms,
    sites: &'a HashMap<String, SiteTerms>,
) -> (&'a SiteTerms, TargetProfile, PickOptions) {
    let terms = c
        .site
        .as_ref()
        .and_then(|s| sites.get(s))
        .unwrap_or(default);
    let mut t = TargetProfile::new(&c.target_title, &c.keywords, &[]);
    if c.site.is_some() {
        t.apply_idf(terms);
    }
    let mut o = PickOptions::new();
    o.semantic = c.semantic.clone();
    (terms, t, o)
}

struct Judged {
    natural: bool,
    labelled: bool,
    native: bool,
}

fn judge(c: &Case, anchor: &str, t: &TargetProfile, terms: &SiteTerms) -> Judged {
    let labelled = c
        .acceptable
        .iter()
        .any(|a| a.eq_ignore_ascii_case(anchor.trim()));
    Judged {
        natural: labelled || is_natural_anchor(anchor, t, Some(terms)),
        labelled,
        native: find_unblocked(&c.paragraph, anchor, &[]).is_some(),
    }
}

/// Cases after this id are the unit/spec-fragment set (v2.0.1).
const FRAGMENT_CASES_FROM: u32 = 61;
/// Cases after this id are the v2.0.2 set (their `old_anchor` is the known weak pick).
const V202_CASES_FROM: u32 = 96;

fn pct(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        100.0 * n as f64 / d as f64
    }
}

#[test]
fn golden_anchor_naturalness() {
    let f = fixture();
    assert!(f.cases.len() >= 60, "fixture did not load");
    let default_terms = SiteTerms::from_counts(0, Default::default(), &f.case_counts);
    let sites: HashMap<String, SiteTerms> = f
        .sites
        .iter()
        .map(|(k, v)| (k.clone(), v.terms()))
        .collect();
    let (mut nb, mut na, mut lb, mut la, mut vb, mut va, mut labelled_cases) =
        (0, 0, 0, 0, 0, 0, 0);
    let (mut v_n, mut v_a, mut v_l, mut v_ln) = (0, 0, 0, 0);
    let (mut rand_n, mut rand_b, mut rand_a) = (0, 0, 0);
    let (mut frag_n, mut frag_b, mut frag_a, mut frag_l, mut frag_ln) = (0, 0, 0, 0, 0);
    for c in &f.cases {
        let (terms, t, opts) = setup(c, &default_terms, &sites);
        let p = ParagraphNlp::with_terms(&c.paragraph, &[], Some(terms));
        let pick = pick_anchor(&p, &t, &opts);
        let b = judge(c, &c.old_anchor, &t, terms);
        let a = judge(c, &pick.anchor, &t, terms);
        nb += usize::from(b.natural);
        na += usize::from(a.natural);
        vb += usize::from(b.native);
        va += usize::from(a.native && pick.in_paragraph);
        if !c.acceptable.is_empty() {
            labelled_cases += 1;
            lb += usize::from(b.labelled);
            la += usize::from(a.labelled);
        }
        // cases 1-11 are the pilot's known-bad anchors; 12-61 a random sample; 62-96
        // fragments; 97+ the v2.0.2 brand / generic-head / gerund / off-topic cases
        if c.id > V202_CASES_FROM {
            v_n += 1;
            v_a += usize::from(a.natural);
            if !c.acceptable.is_empty() {
                v_ln += 1;
                v_l += usize::from(a.labelled);
            }
            assert!(
                !pick.anchor.eq_ignore_ascii_case(&c.old_anchor),
                "#{} still picks the weak anchor {:?}",
                c.id,
                pick.anchor
            );
        } else if c.id > FRAGMENT_CASES_FROM {
            frag_n += 1;
            frag_b += usize::from(b.natural);
            frag_a += usize::from(a.natural);
            if !c.acceptable.is_empty() {
                frag_ln += 1;
                frag_l += usize::from(a.labelled);
            }
            // a fragment lead or an adjective head never survives
            assert!(
                !is_fragment_anchor(&pick.anchor),
                "#{} picked a fragment {:?}",
                c.id,
                pick.anchor
            );
        } else if c.id > 11 {
            rand_n += 1;
            rand_b += usize::from(b.natural);
            rand_a += usize::from(a.natural);
        }
        eprintln!(
            "#{:>2} {:<5} {:<5} {:<45} -> {:<45} [{}]",
            c.id,
            if b.natural { "ok" } else { "BAD" },
            if a.natural { "ok" } else { "BAD" },
            format!("{:?}", c.old_anchor),
            format!("{:?}", pick.anchor),
            if pick.in_paragraph {
                "native"
            } else {
                "fallback"
            },
        );
    }
    let n = f.cases.len();
    eprintln!(
        "natural: before {:.1}% -> after {:.1}% (random subset {:.1}% -> {:.1}%, n={rand_n}); \
         in hand-labelled set: {:.1}% -> {:.1}% (n={labelled_cases}); native: {:.1}% -> {:.1}%",
        pct(nb, n),
        pct(na, n),
        pct(rand_b, rand_n),
        pct(rand_a, rand_n),
        pct(lb, labelled_cases),
        pct(la, labelled_cases),
        pct(vb, n),
        pct(va, n),
    );
    eprintln!(
        "fragment cases (#{}+): natural {:.1}% -> {:.1}% (n={frag_n}); in hand-labelled set {:.1}%          (n={frag_ln})",
        FRAGMENT_CASES_FROM + 1,
        pct(frag_b, frag_n),
        pct(frag_a, frag_n),
        pct(frag_l, frag_ln),
    );
    eprintln!(
        "v2.0.2 cases (#{}+): natural {:.1}% (n={v_n}); in hand-labelled set {:.1}% (n={v_ln})",
        V202_CASES_FROM + 1,
        pct(v_a, v_n),
        pct(v_l, v_ln),
    );
    assert!(frag_n >= 20, "fragment cases did not load");
    assert!(v_n >= 10, "v2.0.2 cases did not load");
    assert!(
        pct(v_a, v_n) >= 90.0,
        "v2.0.2 natural = {:.1}%",
        pct(v_a, v_n)
    );
    assert!(pct(na, n) >= 90.0, "natural after = {:.1}%", pct(na, n));
    assert!(
        pct(frag_a, frag_n) >= 85.0,
        "fragment natural after = {:.1}%",
        pct(frag_a, frag_n)
    );
    assert!(na >= nb);
}

/// The strict check itself, on hand-picked good/bad anchors from the pilot. Known limit: without
/// sentence context a leading noun/verb homograph passes ("match heavy-duty trailers"); the
/// picker sees the context and rejects it.
#[test]
fn strict_check_examples() {
    let f = fixture();
    let terms = SiteTerms::from_counts(0, Default::default(), &f.case_counts);
    let t = TargetProfile::new(
        "11 Best Bike Mounts for Mobile Phones in 2026",
        &["bike mounts".to_string(), "mobile phones".to_string()],
        &[],
    );
    for good in [
        "phone mount",
        "Bike Mounts for Phones",
        "16-inch bikes",
        "heavy-duty trailers",
        "MIPS bike helmets",
        "Accessories & Gear",
        "HEPA filter bags",
        "LEDS bike lights",
    ] {
        assert!(is_natural_anchor(good, &t, Some(&terms)), "{good}");
    }
    for bad in [
        "phone mount withstand daily abuse",
        "need dependable bike chain",
        "Quad Lock Motorcycle Handlebar Mount",
        "Peloton Cycling Shoes featuring Delta cleats",
        "2026\u{2019}s top bike lights",
        "Folding Electric Bike 48V",
        "shoe\u{2019}s cleat type",
        "Training wheels come",
        "helmet fits right",
        "Best Bike Helmets",
        "training wheels support balance growth",
        "Cycling Shoes featuring SPD cleats",
        "click here",
        "Top Picks",
        "mount",
        "AGPTEK Bike Disc Brake Pads",
        "the phone mount",
        // v2.0.1 fragments
        "V kits perfect",
        "inch cordless Impact Wrenches",
        "Oz Water Bottles",
        "water bottle easy",
        "glass bottles available",
        "bike mounts perfect",
        // v2.0.2 brands (site-wide capitalisation statistics, 4-letter ALL-CAPS)
        "Squirt Chain Lube",
        "squirt lube",
        "AIII Lightweight Electric Dirt Bike",
        "WORX Nitro Cordless Impact Driver",
    ] {
        assert!(!is_natural_anchor(bad, &t, Some(&terms)), "{bad}");
    }
}
