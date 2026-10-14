//! Link-run options (contract §2) and the Auto-Linker site setting (§5): parsing with defaults,
//! strict validation (unknown keys, ranges, enums) and bounded sizes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MODES: &[&str] = &[
    "opportunity",
    "reverse",
    "serial",
    "priority",
    "circle",
    "bidirectional",
];
pub const CRAWL: &[&str] = &["always", "if_stale", "skip"];
pub const APPROVE: &[&str] = &["selected", "all", "none"];
/// Strategy names (`score_breakdown.strategy` uses the candidate-source ones).
pub const STRATEGIES: &[&str] = &[
    "in_text",
    "contextual",
    "semantic",
    "keyword_rules",
    "silo",
    "orphan_rescue",
    "money_pages",
    "fix_broken",
];
pub const ANCHOR_TYPES: &[&str] = &[
    "exact", "partial", "branded", "generic", "lsi", "naked", "cta",
];
const MAX_LIST: usize = 1000;
const MAX_STR: usize = 2048;

fn t() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Strategies {
    #[serde(default = "t")]
    pub in_text: bool,
    #[serde(default)]
    pub contextual: bool,
    #[serde(default = "t")]
    pub semantic: bool,
    #[serde(default = "t")]
    pub keyword_rules: bool,
    #[serde(default = "t")]
    pub silo: bool,
    #[serde(default = "t")]
    pub orphan_rescue: bool,
    #[serde(default)]
    pub money_pages: bool,
    #[serde(default)]
    pub fix_broken: bool,
}

impl Default for Strategies {
    fn default() -> Self {
        Self {
            in_text: true,
            contextual: false,
            semantic: true,
            keyword_rules: true,
            silo: true,
            orphan_rescue: true,
            money_pages: false,
            fix_broken: false,
        }
    }
}

impl Strategies {
    pub fn enabled(&self, name: &str) -> bool {
        match name {
            "in_text" => self.in_text,
            "contextual" => self.contextual,
            "semantic" => self.semantic,
            "keyword_rules" => self.keyword_rules,
            "silo" => self.silo,
            "orphan_rescue" => self.orphan_rescue,
            "money_pages" => self.money_pages,
            "fix_broken" => self.fix_broken,
            _ => false,
        }
    }
}

fn d_min_score() -> f64 {
    0.55
}
fn d_max_links() -> i64 {
    5
}
fn d_max_new() -> i64 {
    3
}
fn d_max_total() -> i64 {
    500
}
fn d_anchor_types() -> Vec<String> {
    ["exact", "partial", "lsi", "branded"]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}
fn d_exact_share() -> f64 {
    0.35
}
fn d_post_types() -> Vec<String> {
    vec!["post".into(), "page".into()]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Filters {
    #[serde(default = "d_min_score")]
    pub min_score: f64,
    #[serde(default = "d_max_links")]
    pub max_links_per_post: i64,
    #[serde(default = "d_max_new")]
    pub max_new_per_post: i64,
    #[serde(default = "d_max_total")]
    pub max_total: i64,
    #[serde(default = "d_anchor_types")]
    pub anchor_types: Vec<String>,
    #[serde(default = "d_exact_share")]
    pub max_exact_share: f64,
    #[serde(default)]
    pub clusters: Vec<String>,
    #[serde(default)]
    pub include_post_ids: Vec<String>,
    #[serde(default)]
    pub exclude_post_ids: Vec<String>,
    #[serde(default)]
    pub exclude_url_patterns: Vec<String>,
    #[serde(default = "d_post_types")]
    pub post_types: Vec<String>,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            min_score: d_min_score(),
            max_links_per_post: d_max_links(),
            max_new_per_post: d_max_new(),
            max_total: d_max_total(),
            anchor_types: d_anchor_types(),
            max_exact_share: d_exact_share(),
            clusters: Vec::new(),
            include_post_ids: Vec::new(),
            exclude_post_ids: Vec::new(),
            exclude_url_patterns: Vec::new(),
            post_types: d_post_types(),
        }
    }
}

fn d_crawl() -> String {
    "if_stale".into()
}
fn d_mode() -> String {
    "opportunity".into()
}
fn d_approve() -> String {
    "selected".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RunOptions {
    #[serde(default = "d_crawl")]
    pub crawl: String,
    #[serde(default)]
    pub strategies: Strategies,
    #[serde(default = "d_mode")]
    pub mode: String,
    #[serde(default)]
    pub filters: Filters,
    #[serde(default)]
    pub ai_refine: bool,
    /// v2.3: AI work of this run uses the `priority` (high) tier.
    #[serde(default)]
    pub high_priority: bool,
    #[serde(default = "t")]
    pub verify: bool,
    #[serde(default = "d_approve")]
    pub approve: String,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            crawl: d_crawl(),
            strategies: Strategies::default(),
            mode: d_mode(),
            filters: Filters::default(),
            ai_refine: false,
            high_priority: false,
            verify: true,
            approve: d_approve(),
        }
    }
}

impl RunOptions {
    /// The silo topology the plan uses (`None` for `opportunity`).
    pub fn topology(&self) -> Option<&str> {
        (self.mode != "opportunity").then_some(self.mode.as_str())
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

fn one_of(field: &str, v: &str, allowed: &[&str]) -> Result<(), String> {
    if allowed.contains(&v) {
        Ok(())
    } else {
        Err(format!("{field} must be one of {}", allowed.join(", ")))
    }
}

fn list_ok(field: &str, v: &[String]) -> Result<(), String> {
    if v.len() > MAX_LIST {
        return Err(format!("{field}: at most {MAX_LIST} entries"));
    }
    if v.iter().any(|s| s.trim().is_empty() || s.len() > MAX_STR) {
        return Err(format!(
            "{field}: entries must be non-empty strings up to {MAX_STR} bytes"
        ));
    }
    Ok(())
}

/// Parse + validate link-run options (`null`/absent = all defaults).
pub fn parse_options(v: Option<&Value>) -> Result<RunOptions, String> {
    let o: RunOptions = match v {
        None | Some(Value::Null) => RunOptions::default(),
        Some(v @ Value::Object(_)) => {
            if v.to_string().len() > 256 * 1024 {
                return Err("options are too large".into());
            }
            serde_json::from_value(v.clone()).map_err(|e| format!("invalid options: {e}"))?
        }
        Some(_) => return Err("options must be an object".into()),
    };
    validate(&o)?;
    Ok(o)
}

pub fn validate(o: &RunOptions) -> Result<(), String> {
    one_of("crawl", &o.crawl, CRAWL)?;
    one_of("mode", &o.mode, MODES)?;
    one_of("approve", &o.approve, APPROVE)?;
    let s = &o.strategies;
    if !(s.in_text || s.contextual) {
        return Err("strategies: enable in_text and/or contextual (how links are inserted)".into());
    }
    if !(s.semantic || s.keyword_rules || s.silo || s.fix_broken) {
        return Err(
            "strategies: enable at least one of semantic, keyword_rules, silo, fix_broken".into(),
        );
    }
    if s.silo && !s.semantic && !s.keyword_rules && !s.fix_broken && o.mode == "opportunity" {
        return Err(
            "strategies: silo needs a silo linking mode (reverse, serial, priority, circle, bidirectional), not opportunity"
                .into(),
        );
    }
    let f = &o.filters;
    if !(f.min_score.is_finite() && (0.0..=1.0).contains(&f.min_score)) {
        return Err("filters.min_score must be between 0 and 1".into());
    }
    if !(f.max_exact_share.is_finite() && (0.0..=1.0).contains(&f.max_exact_share)) {
        return Err("filters.max_exact_share must be between 0 and 1".into());
    }
    if !(1..=1000).contains(&f.max_links_per_post) {
        return Err("filters.max_links_per_post must be 1..1000".into());
    }
    if !(1..=1000).contains(&f.max_new_per_post) {
        return Err("filters.max_new_per_post must be 1..1000".into());
    }
    if !(1..=50_000).contains(&f.max_total) {
        return Err("filters.max_total must be 1..50000".into());
    }
    if f.anchor_types.is_empty() {
        return Err("filters.anchor_types must not be empty".into());
    }
    for a in &f.anchor_types {
        one_of("filters.anchor_types[]", a, ANCHOR_TYPES)?;
    }
    if f.post_types.is_empty() {
        return Err("filters.post_types must not be empty".into());
    }
    list_ok("filters.anchor_types", &f.anchor_types)?;
    list_ok("filters.clusters", &f.clusters)?;
    list_ok("filters.include_post_ids", &f.include_post_ids)?;
    list_ok("filters.exclude_post_ids", &f.exclude_post_ids)?;
    list_ok("filters.exclude_url_patterns", &f.exclude_url_patterns)?;
    list_ok("filters.post_types", &f.post_types)?;
    Ok(())
}

/// `exclude_url_patterns` entry: `*` wildcards (glob), else a case-insensitive substring.
pub fn url_matches(pattern: &str, url: &str) -> bool {
    let p = pattern.trim().to_ascii_lowercase();
    let u = url.to_ascii_lowercase();
    if p.is_empty() {
        return false;
    }
    if !p.contains('*') {
        return u.contains(&p);
    }
    // glob: pieces must appear in order; anchored at the ends unless the pattern starts/ends with *
    let parts: Vec<&str> = p.split('*').collect();
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let Some(rest) = u.get(pos..) else {
            return false;
        };
        if i == 0 {
            if !rest.starts_with(part) {
                return false;
            }
            pos += part.len();
            continue;
        }
        match rest.find(part) {
            Some(at) => pos += at + part.len(),
            None => return false,
        }
    }
    parts.last().is_none_or(|l| l.is_empty() || u.ends_with(l))
}

// ---------------------------------------------------------------------------------------------
// Auto-Linker (§5)
// ---------------------------------------------------------------------------------------------

fn d_cron() -> String {
    "0 3 * * *".into()
}
fn d_tz() -> String {
    "UTC".into()
}
fn d_per_day() -> i64 {
    50
}
fn d_pause() -> f64 {
    0.2
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AutoLinker {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "d_cron")]
    pub cron: String,
    #[serde(default = "d_tz")]
    pub tz: String,
    #[serde(default)]
    pub options: RunOptions,
    /// false = dry run (stop at `ready` and notify).
    #[serde(default)]
    pub live: bool,
    #[serde(default = "d_per_day")]
    pub max_per_day: i64,
    #[serde(default = "d_pause")]
    pub pause_on_failure_rate: f64,
    /// Set when the Auto-Linker paused itself (failure rate), cleared on the next enable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<String>,
}

impl Default for AutoLinker {
    fn default() -> Self {
        Self {
            enabled: false,
            cron: d_cron(),
            tz: d_tz(),
            options: RunOptions::default(),
            live: false,
            max_per_day: d_per_day(),
            pause_on_failure_rate: d_pause(),
            paused_reason: None,
        }
    }
}

/// Parse + validate the `auto_linker` site setting; returns it with defaults filled in.
pub fn parse_auto_linker(v: &Value) -> Result<AutoLinker, String> {
    if !v.is_object() {
        return Err("settings.auto_linker must be an object".into());
    }
    if v.to_string().len() > 256 * 1024 {
        return Err("settings.auto_linker is too large".into());
    }
    let a: AutoLinker = serde_json::from_value(v.clone())
        .map_err(|e| format!("settings.auto_linker: invalid ({e})"))?;
    validate(&a.options).map_err(|e| format!("settings.auto_linker.options: {e}"))?;
    if a.cron.len() > 200
        || crate::scheduler::next_run(&a.cron, &a.tz, chrono::Utc::now()).is_none()
    {
        return Err("settings.auto_linker.cron is not a valid cron expression".into());
    }
    if a.tz.parse::<chrono_tz::Tz>().is_err() && a.tz != "UTC" {
        return Err("settings.auto_linker.tz is not a known IANA time zone".into());
    }
    if !(1..=10_000).contains(&a.max_per_day) {
        return Err("settings.auto_linker.max_per_day must be 1..10000".into());
    }
    if !(a.pause_on_failure_rate.is_finite() && (0.0..=1.0).contains(&a.pause_on_failure_rate)) {
        return Err("settings.auto_linker.pause_on_failure_rate must be between 0 and 1".into());
    }
    Ok(a)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_and_validation() {
        let o = parse_options(None).unwrap();
        assert_eq!(o.mode, "opportunity");
        assert!(o.strategies.in_text && !o.strategies.contextual);
        assert_eq!(o.filters.max_total, 500);
        let o = parse_options(Some(
            &json!({"mode": "reverse", "filters": {"min_score": 0.1}}),
        ))
        .unwrap();
        assert_eq!(o.topology(), Some("reverse"));
        assert_eq!(o.filters.max_new_per_post, 3);
        for bad in [
            json!({"mode": "x"}),
            json!({"crawl": "never"}),
            json!({"bogus": 1}),
            json!({"strategies": {"in_text": false, "contextual": false}}),
            json!({"strategies": {"semantic": false, "keyword_rules": false, "silo": false, "fix_broken": false}}),
            json!({"strategies": {"semantic": false, "keyword_rules": false}}),
            json!({"filters": {"min_score": 2}}),
            json!({"filters": {"anchor_types": ["weird"]}}),
            json!({"filters": {"max_total": 0}}),
            json!({"filters": {"exclude_url_patterns": [""]}}),
            json!("x"),
            json!({"approve": "everything"}),
        ] {
            assert!(parse_options(Some(&bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn url_patterns() {
        assert!(url_matches("/tag/", "https://a.com/tag/x/"));
        assert!(url_matches("*/category/*", "https://a.com/category/x/"));
        assert!(url_matches("https://a.com/*", "https://a.com/x"));
        assert!(!url_matches("https://b.com/*", "https://a.com/x"));
        assert!(url_matches("*.pdf", "https://a.com/x.PDF"));
        assert!(!url_matches("*.pdf", "https://a.com/x.pdf?y"));
        assert!(!url_matches("", "https://a.com/"));
        assert!(url_matches("a*b*c", "abc"));
        assert!(!url_matches("a*b*c", "acb"));
    }

    #[test]
    fn auto_linker_validation() {
        let a = parse_auto_linker(&json!({"enabled": true})).unwrap();
        assert!(!a.live && a.max_per_day == 50 && a.cron == "0 3 * * *");
        assert!(parse_auto_linker(&json!({"cron": "nope"})).is_err());
        assert!(parse_auto_linker(&json!({"tz": "Mars/Base"})).is_err());
        assert!(parse_auto_linker(&json!({"max_per_day": 0})).is_err());
        assert!(parse_auto_linker(&json!({"pause_on_failure_rate": 1.5})).is_err());
        assert!(parse_auto_linker(&json!({"options": {"mode": "x"}})).is_err());
        assert!(parse_auto_linker(&json!([])).is_err());
        assert!(parse_auto_linker(&json!({"x": 1})).is_err());
        let ok = parse_auto_linker(&json!({"enabled": true, "tz": "Asia/Dhaka", "live": true, "options": {"mode": "reverse"}})).unwrap();
        assert!(ok.live);
    }
}
