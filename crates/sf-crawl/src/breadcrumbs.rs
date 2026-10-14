//! Breadcrumb detection (v1 `health/breadcrumbs.ts`, checklist E5): a pure, bounded function
//! over a LIVE-fetched page body.
//!
//! Neither signal lives in `posts.content_html`: Yoast and Rank Math print `BreadcrumbList`
//! JSON-LD into `wp_head`, and the visible trail is rendered by the theme template. Callers must
//! hand this the body of a real GET of the page URL.
//!
//! The report serializes (camelCase) to the `posts.breadcrumb_report` column, matching the v1
//! `BreadcrumbReport` shape (`crumbs`, `hasSchema`, `hasVisible`, `visibleSelector`, `mismatch`,
//! `issues`) plus v2 extras (`schemaSource`, `count`, `truncated`).

use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bytes of HTML parsed at most (the rest is ignored; `truncated` is set).
pub const MAX_BREADCRUMB_HTML_BYTES: usize = 2 * 1024 * 1024;
/// Tags parsed at most: the HTML parser is O(depth) per start tag, so a hostile page of 400k
/// nested `<div>`s would be quadratic. Real pages carry far fewer; breadcrumbs sit near the top.
const MAX_TAGS: usize = 25_000;
/// JSON-LD nests arbitrarily (@graph, WebPage.breadcrumb, arrays of arrays); cap the walk.
const MAX_WALK_DEPTH: usize = 8;
/// Total JSON nodes visited per page (wide documents).
const MAX_WALK_NODES: usize = 20_000;
/// JSON-LD blocks examined, and bytes per block.
const MAX_SCRIPTS: usize = 64;
const MAX_SCRIPT_BYTES: usize = 512 * 1024;
const MAX_CRUMBS: usize = 100;
const MAX_NAME_CHARS: usize = 300;
const MAX_URL_CHARS: usize = 2048;

/// Visible-trail selectors, all keyword-scoped (v1 list + RDFa). A bare `nav[aria-label]` is
/// deliberately absent: it matches "Main menu" navs, which would flag every theme.
const VISIBLE_SELECTORS: [&str; 12] = [
    r#"[itemtype*="BreadcrumbList" i]"#,
    r#"[typeof*="BreadcrumbList" i]"#,
    r#"nav[aria-label*="breadcrumb" i]"#,
    r#"[aria-label*="breadcrumb" i]"#,
    r#"[class*="breadcrumb" i]"#,
    r#"[id*="breadcrumb" i]"#,
    r#"[class*="brdcrmb" i]"#,
    ".rank-math-breadcrumb",
    ".yoast-breadcrumb",
    "#crumbs",
    ".trail-items",
    ".woocommerce-breadcrumb",
];

/// Microdata / RDFa `BreadcrumbList` containers (count as schema).
const MICRODATA_SELECTOR: &str = r#"[itemtype*="BreadcrumbList" i], [typeof*="BreadcrumbList" i]"#;

/// Cheap substring gate: without any of these the page cannot match, so skip the DOM parse.
const KEYWORDS: [&str; 4] = ["breadcrumb", "brdcrmb", "crumbs", "trail-items"];

/// One crumb. `url` is `None` for the current page (the last ListItem may omit `item`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreadcrumbCrumb {
    pub position: i64,
    pub name: String,
    pub url: Option<String>,
}

/// Full breadcrumb report for one page body (the `posts.breadcrumb_report` JSON).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BreadcrumbReport {
    pub crumbs: Vec<BreadcrumbCrumb>,
    /// `BreadcrumbList` structured data (JSON-LD, or microdata/RDFa) is present.
    pub has_schema: bool,
    /// A visible trail with at least one link is present.
    pub has_visible: bool,
    /// The selector that matched the visible trail (debugging a false negative).
    pub visible_selector: Option<String>,
    /// Schema without a visible trail, or the reverse: the actionable finding.
    pub mismatch: bool,
    pub issues: Vec<String>,
    /// `"json-ld"`, `"microdata"` or `None`.
    pub schema_source: Option<String>,
    /// `crumbs.len()`.
    pub count: usize,
    /// The body was larger than the parse cap and only its head was inspected.
    pub truncated: bool,
}

impl BreadcrumbReport {
    /// `posts.has_breadcrumbs`.
    pub fn has_breadcrumbs(&self) -> bool {
        self.has_visible
    }
    /// `posts.has_breadcrumb_schema`.
    pub fn has_breadcrumb_schema(&self) -> bool {
        self.has_schema
    }
    /// `posts.breadcrumb_report` column value.
    pub fn to_json_string(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

/// Longest prefix of `s` that is at most `max` bytes and ends on a char boundary.
fn prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    s.get(..i).unwrap_or_default()
}

/// Cut `s` before its `max_tags`-th `<`.
fn cap_tags(s: &str, max_tags: usize) -> (&str, bool) {
    match s.match_indices('<').nth(max_tags) {
        Some((i, _)) => (s.get(..i).unwrap_or_default(), true),
        None => (s, false),
    }
}

fn clip(s: &str, n: usize) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(n)
        .collect()
}

fn sel(s: &str) -> Option<Selector> {
    Selector::parse(s).ok()
}

/// Detect breadcrumbs in a page body. Never panics; input beyond the caps is ignored.
pub fn detect_breadcrumbs(html: &str) -> BreadcrumbReport {
    let byte_capped = prefix(html, MAX_BREADCRUMB_HTML_BYTES);
    let (body, tag_capped) = cap_tags(byte_capped, MAX_TAGS);
    let mut report = BreadcrumbReport {
        truncated: byte_capped.len() < html.len() || tag_capped,
        ..Default::default()
    };
    let lower = body.to_ascii_lowercase();
    if !KEYWORDS.iter().any(|k| lower.contains(k)) {
        return report;
    }
    drop(lower);
    let doc = Html::parse_document(body);

    // 1. JSON-LD
    let mut schema_issues = Vec::new();
    let mut crumbs = Vec::new();
    if let Some(list) = find_json_ld_list(&doc) {
        report.has_schema = true;
        report.schema_source = Some("json-ld".into());
        crumbs = crumbs_from_json(&list, &mut schema_issues);
    }

    // 2. microdata / RDFa
    if !report.has_schema
        && let Some(s) = sel(MICRODATA_SELECTOR)
        && let Some(container) = doc.select(&s).next()
    {
        report.has_schema = true;
        report.schema_source = Some("microdata".into());
        crumbs = crumbs_from_microdata(container, &mut schema_issues);
    }
    finish_crumbs(&mut crumbs, &mut schema_issues, report.has_schema);
    report.crumbs = crumbs;
    report.count = report.crumbs.len();

    // 3. visible trail
    report.visible_selector = find_visible(&doc).map(str::to_string);
    report.has_visible = report.visible_selector.is_some();

    let mut issues = schema_issues;
    if report.has_schema && !report.has_visible {
        issues.push("BreadcrumbList schema present but no visible breadcrumb trail found".into());
    }
    if !report.has_schema && report.has_visible {
        issues.push("Visible breadcrumb trail present but no BreadcrumbList schema".into());
    }
    report.mismatch = report.has_schema != report.has_visible;
    report.issues = issues;
    report
}

/// First visible-trail selector with a matching element that contains a link.
fn find_visible(doc: &Html) -> Option<&'static str> {
    let a = sel("a[href]")?;
    for s in VISIBLE_SELECTORS {
        let Some(selector) = sel(s) else { continue };
        let hit = doc.select(&selector).take(200).any(|el| {
            // `<body class="has-breadcrumbs">` would otherwise match any page with a link.
            !matches!(
                el.value().name(),
                "html" | "body" | "head" | "script" | "style"
            ) && el.select(&a).next().is_some()
        });
        if hit {
            return Some(s);
        }
    }
    None
}

/// Strip a CDATA wrapper (`//<![CDATA[ ... //]]>`) some caching/SEO plugins emit.
fn strip_cdata(raw: &str) -> &str {
    let mut t = raw.trim();
    for p in ["//<![CDATA[", "<![CDATA["] {
        if let Some(r) = t.strip_prefix(p) {
            t = r.trim();
            break;
        }
    }
    for p in ["//]]>", "]]>"] {
        if let Some(r) = t.strip_suffix(p) {
            t = r.trim();
            break;
        }
    }
    t
}

fn find_json_ld_list(doc: &Html) -> Option<Value> {
    let s = sel("script")?;
    let mut seen = 0usize;
    for el in doc.select(&s) {
        let ty = el.value().attr("type").unwrap_or_default();
        if !ty.to_ascii_lowercase().contains("ld+json") {
            continue;
        }
        seen += 1;
        if seen > MAX_SCRIPTS {
            break;
        }
        let text: String = el.text().collect();
        if text.len() > MAX_SCRIPT_BYTES {
            continue;
        }
        let raw = strip_cdata(&text);
        if raw.is_empty() {
            continue;
        }
        // serde_json caps nesting (128) itself; invalid JSON-LD is a schema problem, not ours.
        let Ok(v) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        let mut budget = MAX_WALK_NODES;
        if let Some(list) = find_breadcrumb_list(&v, 0, &mut budget) {
            return Some(list.clone());
        }
    }
    None
}

/// `@type` is a string in most output but an array in some plugins/themes.
fn type_matches(v: Option<&Value>, want: &str) -> bool {
    match v {
        Some(Value::String(s)) => s.to_ascii_lowercase().ends_with(want),
        Some(Value::Array(a)) => a.iter().any(|x| type_matches(Some(x), want)),
        _ => false,
    }
}

/// Recursively locate the first `BreadcrumbList` node (depth- and node-bounded).
fn find_breadcrumb_list<'a>(v: &'a Value, depth: usize, budget: &mut usize) -> Option<&'a Value> {
    if depth > MAX_WALK_DEPTH || *budget == 0 {
        return None;
    }
    *budget -= 1;
    match v {
        Value::Array(items) => items
            .iter()
            .find_map(|x| find_breadcrumb_list(x, depth + 1, budget)),
        Value::Object(map) => {
            if type_matches(map.get("@type"), "breadcrumblist")
                && matches!(
                    map.get("itemListElement"),
                    Some(Value::Array(_) | Value::Object(_))
                )
            {
                return Some(v);
            }
            map.values()
                .filter(|x| x.is_object() || x.is_array())
                .find_map(|x| find_breadcrumb_list(x, depth + 1, budget))
        }
        _ => None,
    }
}

fn json_position(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn crumbs_from_json(list: &Value, issues: &mut Vec<String>) -> Vec<BreadcrumbCrumb> {
    let entries: Vec<&serde_json::Map<String, Value>> = match list.get("itemListElement") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_object).collect(),
        Some(Value::Object(o)) => vec![o],
        _ => Vec::new(),
    };
    let n = entries.len();
    let mut out = Vec::new();
    for (idx, entry) in entries.into_iter().take(MAX_CRUMBS).enumerate() {
        let item = entry.get("item").filter(|v| !v.is_null());
        let mut name = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let url = match item {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Object(o)) => {
                if name.is_empty()
                    && let Some(n) = o.get("name").and_then(Value::as_str)
                {
                    name = n.to_string();
                }
                o.get("@id")
                    .and_then(Value::as_str)
                    .or_else(|| o.get("url").and_then(Value::as_str))
                    .map(str::to_string)
            }
            _ => None,
        };
        let pos = json_position(entry.get("position"));
        push_crumb(
            &mut out,
            issues,
            idx,
            idx + 1 == n,
            pos,
            name,
            url,
            item.is_some(),
        );
    }
    out
}

fn microdata_value(el: ElementRef<'_>) -> String {
    let v = el.value();
    v.attr("content")
        .map(str::to_string)
        .unwrap_or_else(|| el.text().collect::<String>())
}

fn crumbs_from_microdata(
    container: ElementRef<'_>,
    issues: &mut Vec<String>,
) -> Vec<BreadcrumbCrumb> {
    let (Some(li), Some(name_s), Some(item_s), Some(pos_s)) = (
        sel(r#"[itemprop~="itemListElement"]"#),
        sel(r#"[itemprop~="name"]"#),
        sel(r#"[itemprop~="item"]"#),
        sel(r#"[itemprop~="position"]"#),
    ) else {
        return Vec::new();
    };
    let entries: Vec<ElementRef<'_>> = container.select(&li).take(MAX_CRUMBS).collect();
    let n = entries.len();
    let mut out = Vec::new();
    for (idx, e) in entries.into_iter().enumerate() {
        let name = e
            .select(&name_s)
            .next()
            .map(microdata_value)
            .unwrap_or_default();
        let item = e.select(&item_s).next();
        let url = item.and_then(|i| {
            let v = i.value();
            v.attr("href")
                .or_else(|| v.attr("itemid"))
                .or_else(|| v.attr("content"))
                .or_else(|| v.attr("resource"))
                .map(str::to_string)
        });
        let pos = e
            .select(&pos_s)
            .next()
            .and_then(|p| microdata_value(p).trim().parse::<i64>().ok());
        push_crumb(
            &mut out,
            issues,
            idx,
            idx + 1 == n,
            pos,
            name,
            url,
            item.is_some(),
        );
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn push_crumb(
    out: &mut Vec<BreadcrumbCrumb>,
    issues: &mut Vec<String>,
    idx: usize,
    is_last: bool,
    pos: Option<i64>,
    name: String,
    url: Option<String>,
    has_item: bool,
) {
    let human = idx + 1;
    if pos.is_none() {
        issues.push(format!("ListItem {human} has a non-numeric position"));
    }
    let name = clip(&name, MAX_NAME_CHARS);
    if name.is_empty() {
        issues.push(format!("ListItem {human} has no name"));
    }
    // The LAST item is the current page and may omit `item`.
    if !has_item && !is_last {
        issues.push(format!("ListItem {human} is missing its item URL"));
    }
    out.push(BreadcrumbCrumb {
        position: pos.unwrap_or(i64::try_from(human).unwrap_or(i64::MAX)),
        name,
        url: url.map(|u| u.trim().chars().take(MAX_URL_CHARS).collect()),
    });
}

fn finish_crumbs(crumbs: &mut [BreadcrumbCrumb], issues: &mut Vec<String>, found: bool) {
    if !found {
        return;
    }
    crumbs.sort_by_key(|c| c.position);
    if crumbs.first().is_some_and(|c| c.position != 1) {
        issues.push("breadcrumb positions do not start at 1".into());
    }
    if crumbs.len() == 1 {
        issues.push("breadcrumb trail has only one level".into());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const YOAST: &str = r#"<html><head>
<script type="application/ld+json; charset=UTF-8" class="yoast-schema-graph">{"@context":"https://schema.org","@graph":[
 {"@type":"WebPage","@id":"https://a.example/p/","breadcrumb":{"@id":"https://a.example/p/#breadcrumb"}},
 {"@type":"BreadcrumbList","@id":"https://a.example/p/#breadcrumb","itemListElement":[
   {"@type":"ListItem","position":1,"name":"Home","item":"https://a.example/"},
   {"@type":"ListItem","position":"2","name":"Bikes","item":"https://a.example/bikes/"},
   {"@type":"ListItem","position":3,"name":"Locks"}]}]}</script>
</head><body class="post-template">
<nav class="yoast-breadcrumbs"><span><a href="/">Home</a> » <a href="/bikes/">Bikes</a> » Locks</span></nav>
<p>Body</p></body></html>"#;

    #[test]
    fn yoast_graph_and_visible_trail() {
        let r = detect_breadcrumbs(YOAST);
        assert!(r.has_schema && r.has_visible && !r.mismatch, "{r:?}");
        assert_eq!(r.schema_source.as_deref(), Some("json-ld"));
        assert_eq!(r.count, 3);
        assert_eq!(r.crumbs[1].name, "Bikes");
        assert_eq!(r.crumbs[1].position, 2);
        assert_eq!(r.crumbs[2].url, None);
        assert!(r.issues.is_empty(), "{:?}", r.issues);
        assert_eq!(
            r.visible_selector.as_deref(),
            Some(r#"[class*="breadcrumb" i]"#)
        );
        let j: Value = serde_json::from_str(&r.to_json_string()).unwrap();
        for k in [
            "crumbs",
            "hasSchema",
            "hasVisible",
            "visibleSelector",
            "mismatch",
            "issues",
        ] {
            assert!(j.get(k).is_some(), "missing {k}: {j}");
        }
    }

    #[test]
    fn rank_math_item_objects_and_top_level_array() {
        let html = r#"<script type="application/ld+json">[{"@type":"Article"},[{"@type":["Thing","BreadcrumbList"],
          "itemListElement":[{"position":1,"item":{"@id":"https://a.example/","name":"Home"}},
                             {"position":2,"item":{"@id":"https://a.example/x/","name":"X"}}]}]]</script>
          <nav aria-label="Breadcrumbs" class="rank-math-breadcrumb"><p><a href="/">Home</a></p></nav>"#;
        let r = detect_breadcrumbs(html);
        assert!(r.has_schema && r.has_visible);
        assert_eq!(r.crumbs[0].name, "Home");
        assert_eq!(r.crumbs[1].url.as_deref(), Some("https://a.example/x/"));
    }

    #[test]
    fn cdata_wrapper_is_stripped() {
        let html = "<script type=\"application/ld+json\">//<![CDATA[\n{\"@type\":\"BreadcrumbList\",\"itemListElement\":[{\"position\":1,\"name\":\"Home\",\"item\":\"/\"},{\"position\":2,\"name\":\"P\"}]}\n//]]></script>";
        let r = detect_breadcrumbs(html);
        assert!(r.has_schema && !r.has_visible && r.mismatch);
        assert!(r.issues.iter().any(|i| i.contains("no visible")));
    }

    #[test]
    fn microdata_counts_as_schema_and_visible() {
        let html = r#"<ol itemscope itemtype="https://schema.org/BreadcrumbList">
          <li itemprop="itemListElement" itemscope itemtype="https://schema.org/ListItem">
            <a itemprop="item" href="https://a.example/"><span itemprop="name">Home</span></a>
            <meta itemprop="position" content="1"></li>
          <li itemprop="itemListElement" itemscope itemtype="https://schema.org/ListItem">
            <span itemprop="name">Here</span><meta itemprop="position" content="2"></li></ol>"#;
        let r = detect_breadcrumbs(html);
        assert!(r.has_schema && r.has_visible && !r.mismatch, "{r:?}");
        assert_eq!(r.schema_source.as_deref(), Some("microdata"));
        assert_eq!(r.count, 2);
        assert_eq!(r.crumbs[0].url.as_deref(), Some("https://a.example/"));
        assert_eq!(r.crumbs[1].name, "Here");
    }

    #[test]
    fn woocommerce_and_id_trails_without_schema() {
        for html in [
            r#"<nav class="woocommerce-breadcrumb"><a href="/">Home</a> / Shop</nav>"#,
            r#"<div id="breadcrumbs"><a href="/">Home</a></div>"#,
        ] {
            let r = detect_breadcrumbs(html);
            assert!(r.has_visible && !r.has_schema && r.mismatch, "{html}");
        }
    }

    #[test]
    fn empty_wrappers_body_classes_and_menus_do_not_count() {
        let html = r#"<body class="has-breadcrumbs"><div class="breadcrumbs"></div>
          <nav aria-label="Main menu"><a href="/">Home</a></nav></body>"#;
        let r = detect_breadcrumbs(html);
        assert!(!r.has_visible && !r.has_schema && !r.mismatch, "{r:?}");
        assert_eq!(detect_breadcrumbs(""), BreadcrumbReport::default());
    }

    #[test]
    fn json_ld_issues_are_reported() {
        let html = r#"<script type="application/ld+json">{"@type":"BreadcrumbList","itemListElement":[
          {"position":"two","name":""},{"position":3,"name":"C"}]}</script>"#;
        let r = detect_breadcrumbs(html);
        assert!(r.issues.iter().any(|i| i.contains("non-numeric")));
        assert!(r.issues.iter().any(|i| i.contains("no name")));
        assert!(r.issues.iter().any(|i| i.contains("missing its item URL")));
        let html = r#"<script type="application/ld+json">{"@type":"BreadcrumbList","itemListElement":[
          {"position":2,"name":"B","item":"/b"}]}</script>"#;
        let r = detect_breadcrumbs(html);
        assert!(r.issues.iter().any(|i| i.contains("do not start at 1")));
        assert!(r.issues.iter().any(|i| i.contains("only one level")));
    }

    #[test]
    fn hostile_input_never_panics() {
        let deep_json = format!(
            "<script type=\"application/ld+json\">{}{}</script><p>breadcrumb</p>",
            "[".repeat(100_000),
            "]".repeat(100_000)
        );
        let deep_obj = format!(
            "<script type=\"application/ld+json\">{}\"@type\":\"BreadcrumbList\"{}</script>",
            "{\"a\":".repeat(100),
            "}".repeat(100)
        );
        let deep_html = format!("breadcrumb{}", "<div class=breadcrumb>".repeat(400_000));
        let huge = format!(
            "<div class=breadcrumb><a href=/>x</a></div>{}",
            "é".repeat(3 * 1024 * 1024)
        );
        let inputs = [
            String::from("\u{0}\u{fffd}<<<>>>breadcrumb<script type=application/ld+json>{{{"),
            String::from(
                "<script type=\"application/ld+json\">{\"@type\":\"BreadcrumbList\",\"itemListElement\":5}</script>",
            ),
            String::from(
                "<script type=\"application/ld+json\">{\"@type\":\"BreadcrumbList\",\"itemListElement\":[1,null,\"x\",{\"position\":1e308,\"item\":[]}]}</script>",
            ),
            String::from("<script type=\"application/ld+json\">]]></script><a class=breadcrumb"),
            deep_json,
            deep_obj,
            deep_html,
            huge,
        ];
        for h in &inputs {
            let started = std::time::Instant::now();
            let r = detect_breadcrumbs(h);
            assert_eq!(r.count, r.crumbs.len());
            assert!(
                started.elapsed() < std::time::Duration::from_secs(20),
                "slow: {:?}",
                started.elapsed()
            );
        }
        let r = detect_breadcrumbs(&inputs[7]);
        assert!(r.truncated && r.has_visible);
        assert!(prefix("éé", 3).len() == 2);
    }
}
