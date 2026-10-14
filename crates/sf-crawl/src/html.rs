//! Minimal HTML helpers (scraper-based): tag stripping, builder detection, main-content
//! extraction, link extraction and live-link lookup. All functions are synchronous and pure;
//! async callers should run full-page parses on `spawn_blocking`.

use std::collections::HashSet;
use std::sync::LazyLock;

use ego_tree::iter::Edge;
use regex::Regex;
use scraper::{ElementRef, Html, Node, Selector};
use serde::{Deserialize, Serialize};
use url::Url;

fn sel(s: &str) -> Option<Selector> {
    Selector::parse(s).ok()
}

/// Collapse runs of whitespace to one space and trim.
pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// First `n` chars (char-boundary safe).
pub fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Text content of an HTML fragment, entities decoded, whitespace collapsed.
pub fn strip_tags(html: &str) -> String {
    if !html.contains('<') && !html.contains('&') {
        return collapse_ws(html);
    }
    let frag = Html::parse_fragment(html);
    let mut out = String::new();
    for t in frag.root_element().text() {
        out.push_str(t);
        out.push(' ');
    }
    collapse_ws(&out)
}

static WP_BLOCK_CLASS: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"class="[^"]*wp-block"#).ok());

/// v1 `detectBuilder`: elementor > divi > gutenberg (raw block comments, wp-block classes) > classic.
pub fn detect_builder(html: &str, raw: Option<&str>) -> &'static str {
    if html.contains("elementor-widget") || html.contains("data-elementor-type") {
        return "elementor";
    }
    if html.contains("et_pb_") || raw.is_some_and(|r| r.contains("[et_pb_")) {
        return "divi";
    }
    if raw.is_some_and(|r| r.contains("<!-- wp:")) {
        return "gutenberg";
    }
    if html.contains("<!-- wp:") || WP_BLOCK_CLASS.as_ref().is_some_and(|re| re.is_match(html)) {
        return "gutenberg";
    }
    "classic"
}

const MAIN_SELECTORS: [&str; 3] = ["article", "main", ".entry-content"];
const CHROME_TAGS: [&str; 8] = [
    "nav", "footer", "header", "aside", "script", "style", "noscript", "template",
];
const VOID_TAGS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// v1 `extractMainContent`: title = `<title>` or first `<h1>`; content = first non-empty of
/// `article`, `main`, `.entry-content`; else `<body>` minus nav/footer/header/aside/script/style.
pub fn extract_main_content(page_html: &str) -> (String, String) {
    let doc = Html::parse_document(page_html);
    let first_text = |s: &str| {
        sel(s)
            .and_then(|q| {
                doc.select(&q)
                    .next()
                    .map(|e| collapse_ws(&e.text().collect::<String>()))
            })
            .filter(|t| !t.is_empty())
    };
    let title = first_text("title")
        .or_else(|| first_text("h1"))
        .unwrap_or_default();
    for s in MAIN_SELECTORS {
        if let Some(q) = sel(s)
            && let Some(el) = doc.select(&q).next()
            && !el.text().all(|t| t.trim().is_empty())
        {
            return (title, el.inner_html());
        }
    }
    let body = sel("body").and_then(|q| doc.select(&q).next());
    let content = match body {
        Some(b) => serialize_without(b, &CHROME_TAGS),
        None => String::new(),
    };
    (title, content)
}

fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

fn escape_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

/// Serialize the children of `root`, skipping elements named in `skip` (iterative, no recursion).
pub fn serialize_without(root: ElementRef<'_>, skip: &[&str]) -> String {
    let mut out = String::new();
    let mut skip_depth: usize = 0;
    let root_id = root.id();
    for edge in root.traverse() {
        match edge {
            Edge::Open(node) => {
                if node.id() == root_id {
                    continue;
                }
                match node.value() {
                    Node::Element(e) => {
                        let name = e.name();
                        if skip_depth > 0 || skip.contains(&name) {
                            skip_depth += 1;
                            continue;
                        }
                        out.push('<');
                        out.push_str(name);
                        for (k, v) in e.attrs() {
                            out.push(' ');
                            out.push_str(k);
                            out.push_str("=\"");
                            escape_attr(v, &mut out);
                            out.push('"');
                        }
                        out.push('>');
                    }
                    Node::Text(t) if skip_depth == 0 => escape_text(t, &mut out),
                    _ => {}
                }
            }
            Edge::Close(node) => {
                if node.id() == root_id {
                    continue;
                }
                if let Node::Element(e) = node.value() {
                    if skip_depth > 0 {
                        skip_depth -= 1;
                        continue;
                    }
                    if !VOID_TAGS.contains(&e.name()) {
                        out.push_str("</");
                        out.push_str(e.name());
                        out.push('>');
                    }
                }
            }
        }
    }
    out
}

/// Link-target identity: http(s) only, fragment dropped, host lowercased, default port removed.
pub fn normalize_target_url(raw: &str) -> Option<String> {
    let mut u = Url::parse(raw.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    u.set_fragment(None);
    Some(u.to_string())
}

/// Scheme-less, port-less, `www.`-stripped lowercase host (v1 `urlHostKey`).
pub fn host_key(raw: &str) -> Option<String> {
    let u = Url::parse(raw.trim()).ok()?;
    let h = u.host_str()?.to_ascii_lowercase();
    let h = h.trim_end_matches('.');
    Some(h.strip_prefix("www.").unwrap_or(h).to_string())
}

/// Build the internal-host set from domains / URLs (bare domains accepted).
pub fn site_host_set<I, S>(values: I) -> HashSet<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut set = HashSet::new();
    for v in values {
        let v = v.as_ref().trim();
        if v.is_empty() {
            continue;
        }
        let k = host_key(v).or_else(|| {
            let bare = v
                .trim_start_matches("https://")
                .trim_start_matches("http://");
            host_key(&format!("https://{bare}"))
        });
        if let Some(k) = k {
            set.insert(k);
        }
    }
    set
}

/// A link found in content (input for [`crate::health::check_links`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedLink {
    /// `href` as authored (may be relative).
    pub href: String,
    /// Absolute URL before redirects.
    pub requested_url: String,
    pub anchor: String,
    /// Sorted, space-joined lowercase rel tokens.
    pub rel: String,
    pub is_nofollow: bool,
    pub is_sponsored: bool,
    pub is_ugc: bool,
    /// `internal` | `external`.
    pub scope: String,
    /// `body` | `nav` | `header` | `footer` | `aside`.
    pub placement: String,
}

fn is_skippable_href(h: &str) -> bool {
    let h = h.trim();
    if h.is_empty() || h.starts_with('#') {
        return true;
    }
    let lower = h.to_ascii_lowercase();
    [
        "mailto:",
        "tel:",
        "sms:",
        "javascript:",
        "data:",
        "file:",
        "ftp:",
        "callto:",
        "whatsapp:",
        "skype:",
    ]
    .iter()
    .any(|p| lower.starts_with(p))
}

fn placement_of(el: ElementRef<'_>) -> &'static str {
    for anc in el.ancestors() {
        if let Some(e) = anc.value().as_element() {
            match e.name() {
                "nav" => return "nav",
                "header" => return "header",
                "footer" => return "footer",
                "aside" => return "aside",
                _ => {}
            }
            match e.attr("role").map(str::to_ascii_lowercase).as_deref() {
                Some("navigation") => return "nav",
                Some("banner") => return "header",
                Some("contentinfo") => return "footer",
                Some("complementary") => return "aside",
                _ => {}
            }
        }
    }
    "body"
}

/// v1 `extractLinks`: every crawlable `<a href>` with anchor, rel tokens, placement and scope.
/// Relative hrefs resolve against `<base href>` when present, else `base_url`.
pub fn extract_links(
    html: &str,
    base_url: &str,
    site_hosts: &HashSet<String>,
) -> Vec<ExtractedLink> {
    let mut out = Vec::new();
    if html.trim().is_empty() {
        return out;
    }
    let Ok(page) = Url::parse(base_url) else {
        return out;
    };
    let frag = Html::parse_fragment(html);
    let base = sel("base[href]")
        .and_then(|q| frag.select(&q).next())
        .and_then(|b| b.value().attr("href"))
        .and_then(|h| page.join(h).ok())
        .unwrap_or(page);
    let Some(a_sel) = sel("a[href]") else {
        return out;
    };
    for a in frag.select(&a_sel) {
        let href = a.value().attr("href").unwrap_or_default();
        if is_skippable_href(href) {
            continue;
        }
        let Ok(abs) = base.join(href.trim()) else {
            continue;
        };
        if !matches!(abs.scheme(), "http" | "https") {
            continue;
        }
        let mut tokens: Vec<String> = a
            .value()
            .attr("rel")
            .unwrap_or_default()
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|t| !t.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        tokens.sort();
        tokens.dedup();
        let has = |t: &str| tokens.iter().any(|x| x == t);
        let internal = host_key(abs.as_str()).is_some_and(|h| site_hosts.contains(&h));
        out.push(ExtractedLink {
            href: href.to_string(),
            requested_url: abs.to_string(),
            anchor: truncate_chars(&collapse_ws(&a.text().collect::<String>()), 255),
            is_nofollow: has("nofollow"),
            is_sponsored: has("sponsored"),
            is_ugc: has("ugc"),
            rel: tokens.join(" "),
            scope: if internal { "internal" } else { "external" }.into(),
            placement: placement_of(a).into(),
        });
    }
    out
}

/// Anchor-text normalisation used by verify (v1): lowercase, curly quotes to straight,
/// en/em dash to `-`, ellipsis to `...`, whitespace collapsed.
pub fn normalize_anchor_text(s: &str) -> String {
    let mapped: String = s
        .chars()
        .flat_map(|c| -> Vec<char> {
            match c {
                '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{2032}' => vec!['\''],
                '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{2033}' => vec!['"'],
                '\u{2013}' | '\u{2014}' => vec!['-'],
                '\u{2026}' => vec!['.', '.', '.'],
                '\u{00A0}' => vec![' '],
                c => c.to_lowercase().collect(),
            }
        })
        .collect();
    collapse_ws(&mapped)
}

fn href_identity(u: &Url) -> String {
    let mut u = u.clone();
    u.set_fragment(None);
    let s = u.to_string();
    s.trim_end_matches('/').to_string()
}

const CONTENT_REGIONS: [&str; 9] = [
    ".entry-content",
    ".post-content",
    ".wp-block-post-content",
    ".elementor-widget-theme-post-content",
    ".td-post-content",
    ".article-content",
    "article",
    "main",
    "body",
];

/// Result of looking for an injected link on a rendered page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkPresence {
    /// Selector of the content region that was searched.
    pub region: String,
    /// `<a>` elements whose href matches.
    pub href_matches: usize,
    /// `<a>` elements whose href AND normalized text match.
    pub anchor_matches: usize,
}

impl LinkPresence {
    /// Exactly the expected link (href + anchor) is present at least once.
    pub fn found(&self) -> bool {
        self.anchor_matches > 0
    }
}

/// Look for `<a href=href>anchor</a>` inside the page's content region (v1 verify regions).
pub fn find_link(page_html: &str, page_url: &str, href: &str, anchor: &str) -> LinkPresence {
    let mut res = LinkPresence::default();
    let Ok(page) = Url::parse(page_url) else {
        return res;
    };
    let Ok(target) = page.join(href.trim()) else {
        return res;
    };
    let want_href = href_identity(&target);
    let want_anchor = normalize_anchor_text(anchor);
    let doc = Html::parse_document(page_html);
    let Some(a_sel) = sel("a[href]") else {
        return res;
    };
    for region in CONTENT_REGIONS {
        let Some(q) = sel(region) else { continue };
        let Some(root) = doc
            .select(&q)
            .find(|e| !e.text().all(|t| t.trim().is_empty()))
        else {
            continue;
        };
        res.region = region.to_string();
        for a in root.select(&a_sel) {
            let Some(h) = a.value().attr("href") else {
                continue;
            };
            let Ok(abs) = page.join(h.trim()) else {
                continue;
            };
            if href_identity(&abs) != want_href {
                continue;
            }
            res.href_matches += 1;
            if normalize_anchor_text(&a.text().collect::<String>()) == want_anchor {
                res.anchor_matches += 1;
            }
        }
        break;
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_and_builder() {
        assert_eq!(
            strip_tags("<b>Hello</b> &amp;   <i>world</i>"),
            "Hello & world"
        );
        assert_eq!(
            detect_builder("<div class=\"elementor-widget\">", None),
            "elementor"
        );
        assert_eq!(
            detect_builder("<p>x</p>", Some("<!-- wp:paragraph -->")),
            "gutenberg"
        );
        assert_eq!(
            detect_builder("<p class=\"wp-block-quote\">", None),
            "gutenberg"
        );
        assert_eq!(detect_builder("<p>x</p>", None), "classic");
    }

    #[test]
    fn main_content_fallbacks() {
        let (t, c) = extract_main_content(
            "<html><head><title> T </title></head><body><nav>menu</nav><article><p>A</p></article></body></html>",
        );
        assert_eq!(t, "T");
        assert_eq!(c, "<p>A</p>");
        let (t, c) = extract_main_content(
            "<html><body><header>h</header><h1>Head</h1><p>Body &amp; text</p><footer>f</footer><script>x</script></body></html>",
        );
        assert_eq!(t, "Head");
        assert_eq!(c, "<h1>Head</h1><p>Body &amp; text</p>");
    }

    #[test]
    fn links_scope_rel_placement() {
        let hosts = site_host_set(["example.com"]);
        let html = r##"<nav><a href="/about">About</a></nav>
            <p><a href="https://WWW.example.com/x#top" rel="NoFollow  sponsored">X</a>
            <a href="https://other.org/">O</a><a href="mailto:a@b">m</a><a href="#s">s</a></p>"##;
        let links = extract_links(html, "https://example.com/post/", &hosts);
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].placement, "nav");
        assert_eq!(links[0].requested_url, "https://example.com/about");
        assert_eq!(links[1].rel, "nofollow sponsored");
        assert!(links[1].is_nofollow && links[1].is_sponsored);
        assert_eq!(links[1].scope, "internal");
        assert_eq!(links[2].scope, "external");
    }

    #[test]
    fn find_link_in_region() {
        let page = r#"<html><body><nav><a href="/t/">Best Bikes</a></nav>
            <div class="entry-content"><p>See <a href="https://ex.com/t">best  bikes</a>.</p></div></body></html>"#;
        let p = find_link(page, "https://ex.com/post/", "/t/", "Best Bikes");
        assert_eq!(p.region, ".entry-content");
        assert_eq!(p.href_matches, 1);
        assert!(p.found());
        let p = find_link(page, "https://ex.com/post/", "/missing/", "x");
        assert!(!p.found());
        assert_eq!(
            normalize_anchor_text("It\u{2019}s \u{2014} done\u{2026}"),
            "it's - done..."
        );
    }

    #[test]
    fn hostile_html_never_panics() {
        let deep = "<div>".repeat(20_000);
        let _ = extract_main_content(&deep);
        let _ = extract_links(&deep, "https://e.com/", &HashSet::new());
        let _ = find_link(&deep, "not a url", "::", "");
        let _ = strip_tags("<<<>>>&#xFFFFFFFF;&bogus;");
        let _ = extract_links(
            "<a href='http://[::1'>x</a>",
            "https://e.com/",
            &HashSet::new(),
        );
        assert_eq!(normalize_target_url("javascript:alert(1)"), None);
        assert_eq!(host_key("https://WWW.Ex.com:8080/a"), Some("ex.com".into()));
    }
}
