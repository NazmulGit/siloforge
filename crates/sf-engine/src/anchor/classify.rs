//! Anchor classification (v1 `anchor/classify.ts`), precedence
//! naked → branded → cta → exact → partial → generic → lsi.

use std::sync::OnceLock;

use regex::Regex;
use sf_core::types::AnchorType;

/// Whole-anchor generic phrases.
pub const GENERIC_ANCHORS: &[&str] = &[
    "click here",
    "read more",
    "this article",
    "learn more",
    "here",
];
/// Anchors of <= 2 words containing one of these are generic (LOOM `is_generic_anchor`).
pub const GENERIC_WORDS: &[&str] = &["here", "click", "read", "more", "learn", "check"];
/// CTA verbs a CTA anchor must lead with.
pub const CTA_VERBS: &[&str] = &[
    "read", "see", "check", "learn", "discover", "view", "shop", "compare", "find", "get",
];
/// TLDs accepted for scheme-less, `www`-less naked anchors.
pub const NAKED_BARE_TLDS: &[&str] = &[
    "com", "net", "org", "io", "co", "ai", "app", "dev", "info", "biz", "edu", "gov", "shop",
    "store", "blog", "news", "me", "tv", "us", "uk", "ca", "au", "de", "fr", "es", "it", "nl",
    "se", "no", "eu", "in", "jp",
];

/// v1 CTA_PATTERNS, verbatim.
const CTA_PATTERN_SRC: &[&str] = &[
    r"^read\s+(?:the\s+|our\s+|this\s+|a\s+|my\s+|their\s+)?(?:full\s+|complete\s+|detailed\s+|in-?depth\s+)?(?:guide|review|article|post|breakdown|comparison|story|report|case\s+study)\b",
    r"^see\s+(?:our|the|my|their|these|this|full|how|why|what|more)\b\s*\S*",
    r"^check\s+(?:out\s+)?(?:the\s+|our\s+|current\s+|today'?s\s+)?(?:latest\s+|live\s+|best\s+)?(?:price|prices|pricing|deal|deals|availability|reviews?|specs|stock|it\s+out)\b",
    r"^learn\s+more\s+(?:here|about|on|at|in|from)\b",
    r"^learn\s+(?:how|why|what|which)\b",
    r"^discover\s+(?:more|how|why|the|our|these)\b",
    r"^view\s+(?:the\s+|our\s+|full\s+|all\s+)?(?:comparison|chart|table|guide|details|pricing|specs|options|results|list)\b",
    r"^shop\s+(?:now|here|today|the|our|all)\b",
    r"^compare\s+(?:options|plans|prices|pricing|models|features|deals|providers|tools|the|all|these)\b",
    r"^find\s+out\s+(?:more|how|why|what|which|the)\b",
    r"^get\s+(?:started|the|your|our|a|more|it)\b\s*\S*",
];

fn cta_patterns() -> &'static [Regex] {
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    P.get_or_init(|| {
        CTA_PATTERN_SRC
            .iter()
            .filter_map(|s| Regex::new(s).ok())
            .collect()
    })
}

fn naked_re() -> Option<&'static Regex> {
    static R: OnceLock<Option<Regex>> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^(?:https?://)?((?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+[a-z]{2,})(?::\d{1,5})?(?:[/?#]\S*)?$").ok()
    })
    .as_ref()
}

/// v1 `isGenericAnchor`.
pub fn is_generic_anchor(anchor: &str) -> bool {
    let a = anchor.trim().to_lowercase();
    if GENERIC_ANCHORS.contains(&a.as_str()) {
        return true;
    }
    let words: Vec<&str> = a.split_whitespace().collect();
    words.len() <= 2 && words.iter().any(|w| GENERIC_WORDS.contains(w))
}

/// v1 `isNakedAnchor`.
pub fn is_naked_anchor(anchor: &str) -> bool {
    let a = anchor.trim().to_lowercase();
    if a.is_empty() || a.len() > 2048 || a.chars().any(char::is_whitespace) {
        return false;
    }
    let Some(re) = naked_re() else { return false };
    let Some(caps) = re.captures(&a) else {
        return false;
    };
    let host = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    if a.starts_with("http://") || a.starts_with("https://") || host.starts_with("www.") {
        return true;
    }
    let tld = host.rsplit('.').next().unwrap_or("");
    NAKED_BARE_TLDS.contains(&tld)
}

/// v1 `isCtaAnchor`.
pub fn is_cta_anchor(anchor: &str) -> bool {
    let a = crate::html::normalize_ws(&anchor.to_lowercase());
    if a.is_empty() || a.len() > 2048 || is_generic_anchor(&a) {
        return false;
    }
    let lead: String = a
        .split(' ')
        .next()
        .unwrap_or("")
        .chars()
        .filter(char::is_ascii_lowercase)
        .collect();
    if !CTA_VERBS.contains(&lead.as_str()) {
        return false;
    }
    cta_patterns().iter().any(|re| re.is_match(&a))
}

/// Whole-word containment: `(^|[^a-z0-9])needle([^a-z0-9]|$)` on lowercase text.
pub fn contains_word(hay_lower: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return false;
    }
    let hb = hay_lower.as_bytes();
    let mut from = 0;
    while let Some(p) = hay_lower.get(from..).and_then(|s| s.find(needle_lower)) {
        let s = from + p;
        let e = s + needle_lower.len();
        let before_ok = s == 0 || hb.get(s - 1).is_none_or(|c| !c.is_ascii_alphanumeric());
        let after_ok = hb.get(e).is_none_or(|c| !c.is_ascii_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        from = s + needle_lower.chars().next().map(char::len_utf8).unwrap_or(1);
    }
    false
}

/// Classify an anchor against the target's top keyword and the site brand.
pub fn classify_anchor(anchor: &str, top_keyword: Option<&str>, brand: &str) -> AnchorType {
    let a = anchor.trim().to_lowercase();
    let kw = top_keyword.unwrap_or("").trim().to_lowercase();
    let brand = brand.trim().to_lowercase();
    if is_naked_anchor(&a) {
        return AnchorType::Naked;
    }
    if !brand.is_empty() && contains_word(&a, &brand) {
        return AnchorType::Branded;
    }
    if is_cta_anchor(&a) {
        return AnchorType::Cta;
    }
    if !kw.is_empty() && a == kw {
        return AnchorType::Exact;
    }
    if !kw.is_empty() && (a.contains(&kw) || kw.contains(&a)) {
        return AnchorType::Partial;
    }
    if is_generic_anchor(&a) {
        return AnchorType::Generic;
    }
    AnchorType::Lsi
}

/// Site brand from domain: `www.my-site.co.uk` → `my-site`.
pub fn brand_from_domain(domain: &str) -> String {
    let d = domain.trim().to_lowercase();
    let d = d.strip_prefix("www.").unwrap_or(&d);
    d.split('.').next().unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KW: Option<&str> = Some("best seo tools");
    const BRAND: &str = "example";

    #[test]
    fn golden_classify() {
        assert_eq!(
            classify_anchor("Best SEO Tools", KW, BRAND),
            AnchorType::Exact
        );
        assert_eq!(
            classify_anchor("the best seo tools guide", KW, BRAND),
            AnchorType::Partial
        );
        assert_eq!(classify_anchor("seo tools", KW, BRAND), AnchorType::Partial);
        assert_eq!(
            classify_anchor("example review", Some("other keyword"), BRAND),
            AnchorType::Branded
        );
        for g in ["click here", "read more", "here"] {
            assert_eq!(classify_anchor(g, None, BRAND), AnchorType::Generic);
        }
        assert!(is_generic_anchor("more info"));
        assert!(!is_generic_anchor("a detailed reading guide"));
        assert_eq!(
            classify_anchor("quantum computing basics", KW, BRAND),
            AnchorType::Lsi
        );
    }

    #[test]
    fn golden_naked() {
        for y in [
            "https://example.com/page",
            "http://www.example.com/blog/post/",
            "https://example.com",
            "HTTPS://Example.com/Page",
            "https://example.com:8443/page?a=1#top",
            "example.com/page",
            "www.example.com",
            "example.com/",
            "example.co.uk/guides/bike-locks",
            "  example.com  ",
        ] {
            assert!(is_naked_anchor(y), "{y}");
        }
        for n in [
            "read example.com for more",
            "best seo tools",
            "node.js",
            "e.g.",
            "3.5",
            "example",
            "",
        ] {
            assert!(!is_naked_anchor(n), "{n}");
        }
    }

    #[test]
    fn golden_cta() {
        for c in [
            "read the full guide",
            "see our review",
            "check the latest price",
            "learn more here",
            "find out more",
            "shop now",
            "compare options",
            "view the comparison",
            "get started",
            "read our in-depth review",
        ] {
            assert!(is_cta_anchor(c), "{c}");
        }
        for n in [
            "a detailed reading guide",
            "checking account rates",
            "compared to other tools",
            "getting started with seo",
            "best seo tools guide",
            "the guide we read",
            "",
            "read more",
            "learn more",
            "click here",
            "see more",
        ] {
            assert!(!is_cta_anchor(n), "{n}");
        }
        assert_eq!(cta_patterns().len(), CTA_PATTERN_SRC.len());
    }

    #[test]
    fn golden_precedence() {
        assert_eq!(
            classify_anchor("https://example.com/page", None, BRAND),
            AnchorType::Naked
        );
        assert_eq!(
            classify_anchor("www.example.com", None, BRAND),
            AnchorType::Naked
        );
        assert_eq!(
            classify_anchor("example.co.uk/guides", None, ""),
            AnchorType::Naked
        );
        assert_eq!(
            classify_anchor("https://example.com/best-seo-tools", KW, BRAND),
            AnchorType::Naked
        );
        assert_eq!(
            classify_anchor("example.com", Some("example.com"), BRAND),
            AnchorType::Naked
        );
        assert_eq!(
            classify_anchor("read the full guide", KW, BRAND),
            AnchorType::Cta
        );
        assert_eq!(
            classify_anchor("check the latest price", None, BRAND),
            AnchorType::Cta
        );
        assert_eq!(
            classify_anchor("read the full guide to best seo tools", KW, BRAND),
            AnchorType::Cta
        );
        assert_eq!(
            classify_anchor("see the example review", KW, BRAND),
            AnchorType::Branded
        );
        assert_eq!(
            classify_anchor("example", Some("example"), BRAND),
            AnchorType::Branded
        );
        assert_eq!(
            classify_anchor("examples of link building", KW, BRAND),
            AnchorType::Lsi
        );
        assert_eq!(
            classify_anchor("my-site link guide", KW, "my-site"),
            AnchorType::Branded
        );
        assert_eq!(classify_anchor("read more", KW, BRAND), AnchorType::Generic);
        assert_eq!(classify_anchor("more info", KW, BRAND), AnchorType::Generic);
    }

    #[test]
    fn brand_from_domain_golden() {
        assert_eq!(brand_from_domain("www.my-site.co.uk"), "my-site");
        assert_eq!(brand_from_domain("example.com"), "example");
    }
}
