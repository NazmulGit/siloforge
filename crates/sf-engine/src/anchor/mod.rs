//! Anchor generation: phrase scoring, picking with guards, classification, site-wide /
//! per-target guards and the cannibalization report.
//!
//! Typical job flow (per site, deterministic):
//! 1. For every source paragraph: `ParagraphNlp::from_paragraph` (memoize by `cache_key`).
//! 2. For every target: `TargetProfile::new(title, keywords_by_score, gsc_striking)`.
//! 3. Per edge: `pick_anchor_across(top_paragraphs, &target, &opts)`; classify with
//!    [`classify_anchor`] against `target.top_keyword` and [`brand_from_domain`].
//! 4. Run [`apply_over_optimization_guard`] then [`apply_diversity_guard`] over committed + new.
//!
//! Spec: `docs/research/01-engine-spec.md` §10; accuracy C.2–C.7; bugs B-9, B-20, B-29, B-30.

pub mod classify;
pub mod guards;
pub mod pick;
pub mod refine;

#[cfg(test)]
mod golden;

pub use classify::{
    brand_from_domain, classify_anchor, contains_word, is_cta_anchor, is_generic_anchor,
    is_naked_anchor,
};
pub use guards::{
    AnchorAlt, AnchorDistribution, AnchorUse, CannibalizationWarning, DIVERSITY_MAX_SHARE,
    DIVERSITY_MIN_POOL, EXACT_MAX_SHARE, GENERIC_MAX_SHARE, GuardItem, anchor_distribution,
    apply_diversity_guard, apply_over_optimization_guard, cannibalization_report,
    reserved_exact_anchors, type_share,
};
pub use pick::{
    AnchorPick, PhraseScore, PickOptions, PickSource, SEMANTIC_BASE, SEMANTIC_WEIGHT,
    STRIKING_WEIGHT, TargetProfile, clean_title, find_unblocked, is_fragment_anchor, is_ignored,
    is_natural_anchor, pick_anchor, pick_anchor_across, score_phrase, top_phrases,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;
    use crate::nlp::ParagraphNlp;

    #[test]
    fn hostile_inputs_never_panic() {
        let cases = [
            String::new(),
            "\u{0}".into(),
            "a".repeat(300_000),
            "steel bike lock ".repeat(20_000),
            "İ".repeat(1000),
            "[[]]<<>>&&".into(),
        ];
        for c in &cases {
            let p = ParagraphNlp::new(c, &[(0, 3), (usize::MAX - 1, usize::MAX)]);
            let t = TargetProfile::new(
                c,
                std::slice::from_ref(c),
                &[(c.clone(), f64::NAN), (c.clone(), -1.0)],
            );
            let mut o = PickOptions::new();
            o.min_words = 9;
            o.max_words = 1;
            o.ignore.push(c.clone());
            let _ = pick_anchor(&p, &t, &o);
            let _ = pick_anchor_across(&[(0, &p)], &t, &PickOptions::default());
            let _ = classify_anchor(c, Some(c), c);
            let _ = clean_title(c, 0);
            let _ = find_unblocked(c, c, &[]);
            let _ = brand_from_domain(c);
            let _ = is_naked_anchor(c);
            let _ = is_cta_anchor(c);
        }
    }

    /// Bench-ish: analyze + pick over 2,000 synthetic paragraphs < 2 s (debug, opt-level 1).
    #[test]
    fn bench_analyze_and_pick_2000_paragraphs() {
        let words = [
            "steel",
            "bike",
            "lock",
            "waterproof",
            "trunk",
            "bag",
            "commuter",
            "city",
            "chain",
            "durable",
            "helmet",
            "light",
            "rack",
            "pannier",
            "frame",
        ];
        let mut html_doc = String::new();
        for i in 0..2000usize {
            let w = |k: usize| words[(i * 7 + k * 3) % words.len()];
            html_doc.push_str(&format!(
                "<!-- wp:paragraph --><p>Paragraph {i}: the {} {} {} is great for a {} {} and we tested the {} {} {} in <a href=\"/p{i}\">another post</a> today.</p><!-- /wp:paragraph -->\n",
                w(0), w(1), w(2), w(3), w(4), w(5), w(6), w(7)
            ));
        }
        let targets: Vec<TargetProfile> = (0..20)
            .map(|k| {
                let kw = format!(
                    "{} {} {}",
                    words[k % 15],
                    words[(k + 1) % 15],
                    words[(k + 2) % 15]
                );
                TargetProfile::new(&format!("Best {kw} Guide"), &[kw], &[])
            })
            .collect();
        let start = std::time::Instant::now();
        let a = html::analyze(&html_doc, Some("https://example.com/"), &[]).unwrap();
        assert_eq!(a.paragraphs.len(), 2000);
        let opts = PickOptions::new();
        let mut in_para = 0;
        for (i, p) in a.paragraphs.iter().enumerate() {
            let nlp = ParagraphNlp::from_paragraph(p);
            let pick = pick_anchor(&nlp, &targets[i % targets.len()], &opts);
            if pick.in_paragraph {
                in_para += 1;
                assert!(!pick.anchor.contains("another post"));
            }
        }
        let el = start.elapsed();
        assert!(in_para > 1000, "in_para={in_para}");
        assert!(el.as_millis() < 2000, "took {el:?}");
    }
}
