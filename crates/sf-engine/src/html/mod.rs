//! HTML analysis: the canonical paragraph splitter (one splitter for chunking AND insertion),
//! link extraction, builder detection, html→text and word counts.
//!
//! Works on rendered HTML and on WordPress raw `post_content` (Gutenberg block comments are
//! comments in the tree and are never touched; classic raw text is split on blank lines like
//! `wpautop`). All functions are pure, deterministic and bounded:
//! input > [`MAX_HTML_BYTES`] is rejected, nesting is capped, outputs are capped.
//!
//! Spec: `docs/research/01-engine-spec.md` §1.3, §1.5, §5; bugs B-5, B-18, B-19.

pub(crate) mod blocks;
pub(crate) mod dom;
pub(crate) mod entities;

use std::collections::{BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use url::Url;

pub use blocks::{MAX_PARAGRAPHS, MIN_PARAGRAPH_CHARS, Paragraph, SplitMode};
use dom::{Dom, Node, NodeKind};

/// Largest HTML document accepted (same as v1 Rust engine).
pub const MAX_HTML_BYTES: usize = 16 * 1024 * 1024;
/// Maximum links returned for one document.
pub const MAX_LINKS: usize = 250_000;
/// Anchor text kept per link (chars).
pub const MAX_LINK_TEXT_CHARS: usize = 255;

/// Errors from HTML analysis.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HtmlError {
    #[error("html exceeds {max} bytes ({len})")]
    TooLarge { len: usize, max: usize },
}

/// Where a link sits on the page (nearest landmark ancestor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    Body,
    Nav,
    Header,
    Footer,
    Aside,
}

impl Placement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::Nav => "nav",
            Self::Header => "header",
            Self::Footer => "footer",
            Self::Aside => "aside",
        }
    }
}

/// One `<a href>` in the document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    /// Trimmed, entity-decoded href as written.
    pub href: String,
    /// Normalized anchor text (first 255 chars).
    pub text: String,
    /// Lowercased rel tokens (split on whitespace or commas).
    pub rel: Vec<String>,
    pub nofollow: bool,
    pub sponsored: bool,
    pub ugc: bool,
    /// Absolute http(s) URL (resolved against `<base href>` / `base_url`), if any.
    pub resolved_url: Option<String>,
    /// Canonical host (lowercase, no `www.`, no trailing dot, no port).
    pub host: Option<String>,
    /// `None` when the href could not be resolved to http(s).
    pub is_external: Option<bool>,
    pub placement: Placement,
    /// Canonical paragraph index containing the link, if any.
    pub paragraph_index: Option<usize>,
    /// Byte offset of the `<a` in the source.
    pub start: usize,
}

/// A heading (`h1`..`h6`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heading {
    pub level: u8,
    pub text: String,
}

/// Page builder that produced the content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Builder {
    Gutenberg,
    Elementor,
    Divi,
    Classic,
}

impl Builder {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gutenberg => "gutenberg",
            Self::Elementor => "elementor",
            Self::Divi => "divi",
            Self::Classic => "classic",
        }
    }
}

/// Full analysis of one document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Analysis {
    pub paragraphs: Vec<Paragraph>,
    pub links: Vec<Link>,
    /// Count of explicit `http://` hrefs (insecure links).
    pub http_link_count: usize,
    /// Sorted distinct external hosts.
    pub outbound_domains: Vec<String>,
    pub headings: Vec<Heading>,
    /// Visible text word count.
    pub word_count: usize,
    pub builder: Builder,
    /// True when an output cap (paragraphs/links) truncated the result.
    pub truncated: bool,
}

/// JavaScript `\s` (spec §18.6): includes U+FEFF and U+00A0, excludes U+0085.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// Collapse JS-whitespace runs to one space and trim (v1 `normalize`).
pub fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending = false;
    for c in s.chars() {
        if is_js_whitespace(c) {
            pending = !out.is_empty();
        } else {
            if pending {
                out.push(' ');
                pending = false;
            }
            out.push(c);
        }
    }
    out
}

/// Length in UTF-16 code units (JS `.length`).
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Lowercase hex SHA-1 of the UTF-8 bytes.
pub fn sha1_hex(text: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

/// Escape text content (`&`, `<`, `>`).
pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// Escape an attribute value for a double-quoted attribute.
pub fn escape_attr(s: &str) -> String {
    escape_text(s).replace('"', "&quot;")
}

/// Canonical host: lowercase, trailing dot and leading `www.` stripped.
pub fn canonical_host(host: &str) -> String {
    let mut h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if h.starts_with("www.") {
        h.drain(..4);
    }
    h
}

/// v1 `normalizeUrl`: `host(no www) + path(no trailing /)`, query/hash dropped; invalid → raw minus trailing `/`.
pub fn normalize_url(raw: &str) -> String {
    match Url::parse(raw.trim()) {
        Ok(u) => {
            let host = u.host_str().map(canonical_host).unwrap_or_default();
            let path = u.path().trim_end_matches('/');
            let path = if path.is_empty() { "/" } else { path };
            format!("{host}{path}")
        }
        Err(_) => raw.trim().trim_end_matches('/').to_string(),
    }
}

/// Analyze with the default [`SplitMode::Prose`] splitter.
pub fn analyze(
    html: &str,
    base_url: Option<&str>,
    site_hosts: &[String],
) -> Result<Analysis, HtmlError> {
    analyze_with(html, base_url, site_hosts, SplitMode::Prose)
}

/// Analyze with an explicit splitter mode.
pub fn analyze_with(
    html: &str,
    base_url: Option<&str>,
    site_hosts: &[String],
    mode: SplitMode,
) -> Result<Analysis, HtmlError> {
    if html.len() > MAX_HTML_BYTES {
        return Err(HtmlError::TooLarge {
            len: html.len(),
            max: MAX_HTML_BYTES,
        });
    }
    let dom = Dom::parse(html);
    let flags = blocks::node_flags(&dom);
    let blocks = blocks::split(&dom, mode, &flags);
    let mut truncated = blocks.len() >= MAX_PARAGRAPHS;
    let paragraphs: Vec<Paragraph> = blocks.into_iter().map(|b| b.para).collect();

    let order = dom.all_in_order();
    let base = resolve_base(&dom, &order, base_url);
    let mut internal: HashSet<String> = site_hosts
        .iter()
        .take(1024)
        .map(|h| canonical_host(h))
        .filter(|h| !h.is_empty())
        .collect();
    if let Some(h) = base_url
        .and_then(|b| Url::parse(b).ok())
        .and_then(|u| u.host_str().map(canonical_host))
    {
        internal.insert(h);
    }

    let mut links = Vec::new();
    let mut http_link_count = 0usize;
    let mut outbound = BTreeSet::new();
    let mut headings = Vec::new();
    for &ix in &order {
        let Some(node) = dom.node(ix) else { continue };
        let Some(name) = node.name() else { continue };
        if let Some(level) = heading_level(name) {
            let text = normalize_ws(&dom.text_content(ix));
            if !text.is_empty() && headings.len() < 10_000 {
                headings.push(Heading { level, text });
            }
            continue;
        }
        if name != "a" {
            continue;
        }
        let Some(href) = node.attr("href").map(str::trim) else {
            continue;
        };
        if href.is_empty() {
            continue;
        }
        if links.len() >= MAX_LINKS {
            truncated = true;
            break;
        }
        if href.len() >= 7
            && href
                .get(..7)
                .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
        {
            http_link_count += 1;
        }
        let resolved = match Url::parse(href) {
            Ok(u) => Some(u),
            Err(_) => base.as_ref().and_then(|b| b.join(href).ok()),
        }
        .filter(|u| matches!(u.scheme(), "http" | "https"));
        let host = resolved
            .as_ref()
            .and_then(Url::host_str)
            .map(canonical_host);
        let is_external = host.as_ref().map(|h| !internal.contains(h));
        if is_external == Some(true)
            && let Some(h) = &host
        {
            outbound.insert(h.clone());
        }
        let rel: Vec<String> = node
            .attr("rel")
            .unwrap_or("")
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|t| !t.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        let text: String = normalize_ws(&dom.text_content(ix))
            .chars()
            .take(MAX_LINK_TEXT_CHARS)
            .collect();
        let paragraph_index = paragraph_at(&paragraphs, node.start);
        links.push(Link {
            href: href.to_string(),
            text,
            nofollow: rel.iter().any(|r| r == "nofollow"),
            sponsored: rel.iter().any(|r| r == "sponsored"),
            ugc: rel.iter().any(|r| r == "ugc"),
            rel,
            resolved_url: resolved.map(|u| u.to_string()),
            host,
            is_external,
            placement: placement_of(&dom, ix),
            paragraph_index,
            start: node.start,
        });
    }

    Ok(Analysis {
        paragraphs,
        links,
        http_link_count,
        outbound_domains: outbound.into_iter().collect(),
        headings,
        word_count: word_count(&text_of(&dom, &order)),
        builder: detect_builder(html, None),
        truncated,
    })
}

/// Canonical paragraphs only (cheaper than [`analyze`] when links are not needed).
pub fn paragraphs(html: &str, mode: SplitMode) -> Result<Vec<Paragraph>, HtmlError> {
    if html.len() > MAX_HTML_BYTES {
        return Err(HtmlError::TooLarge {
            len: html.len(),
            max: MAX_HTML_BYTES,
        });
    }
    let dom = Dom::parse(html);
    let flags = blocks::node_flags(&dom);
    Ok(blocks::split(&dom, mode, &flags)
        .into_iter()
        .map(|b| b.para)
        .collect())
}

fn heading_level(name: &str) -> Option<u8> {
    match name {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

fn paragraph_at(paragraphs: &[Paragraph], pos: usize) -> Option<usize> {
    let i = paragraphs.partition_point(|p| p.start <= pos);
    let p = paragraphs.get(i.checked_sub(1)?)?;
    (pos >= p.start && pos < p.end).then_some(p.index)
}

fn resolve_base(dom: &Dom<'_>, order: &[usize], base_url: Option<&str>) -> Option<Url> {
    let base = base_url.and_then(|b| Url::parse(b).ok());
    let tag_href = order.iter().find_map(|&ix| {
        let n = dom.node(ix)?;
        (n.name() == Some("base")).then(|| n.attr("href").map(str::trim).map(str::to_string))?
    });
    match tag_href {
        Some(h) if !h.is_empty() => match Url::parse(&h) {
            Ok(u) => Some(u),
            Err(_) => base.as_ref().and_then(|b| b.join(&h).ok()).or(base),
        },
        _ => base,
    }
}

fn placement_of(dom: &Dom<'_>, ix: usize) -> Placement {
    for a in dom.ancestors(ix) {
        let Some(n) = dom.node(a) else { continue };
        let role = n.attr("role").map(str::to_ascii_lowercase);
        match (n.name().unwrap_or(""), role.as_deref()) {
            ("nav", _) | (_, Some("navigation")) => return Placement::Nav,
            ("header", _) | (_, Some("banner")) => return Placement::Header,
            ("footer", _) | (_, Some("contentinfo")) => return Placement::Footer,
            ("aside", _) | (_, Some("complementary")) => return Placement::Aside,
            _ => {}
        }
    }
    Placement::Body
}

/// Visible text with separators at block-element boundaries.
fn text_of(dom: &Dom<'_>, order: &[usize]) -> String {
    let flags = blocks::node_flags(dom);
    let mut out = String::new();
    let mut buf = Vec::new();
    // Track element ends to emit separators when leaving block elements.
    for &ix in order {
        let Some(node) = dom.node(ix) else { continue };
        if flags.invisible.get(ix).copied().unwrap_or(false) {
            continue;
        }
        match &node.kind {
            NodeKind::Text { .. } => {
                buf.clear();
                dom.text_chars(ix, 0, usize::MAX, &mut buf);
                out.extend(buf.iter().map(|d| d.ch));
            }
            NodeKind::Element { name, .. } => {
                if !blocks::INLINE.contains(&name.as_str()) || name == "br" {
                    out.push(' ');
                }
            }
            NodeKind::Comment => out.push(' '),
        }
        if out.len() > MAX_HTML_BYTES {
            break;
        }
    }
    normalize_ws(&out)
}

/// HTML → visible text (script/style/noscript/template/svg dropped, whitespace collapsed,
/// block boundaries separate words).
pub fn html_to_text(html: &str) -> String {
    if html.len() > MAX_HTML_BYTES {
        return String::new();
    }
    let dom = Dom::parse(html);
    let order = dom.all_in_order();
    text_of(&dom, &order)
}

/// Whitespace-separated word count (0 for empty text; fixes v1's `"".split().length == 1`).
pub fn word_count(text: &str) -> usize {
    text.split(is_js_whitespace)
        .filter(|w| !w.is_empty())
        .count()
}

/// v1 `detectBuilder(html, raw?)`.
pub fn detect_builder(html: &str, raw: Option<&str>) -> Builder {
    if html.contains("elementor-widget") || html.contains("data-elementor-type") {
        return Builder::Elementor;
    }
    if html.contains("et_pb_") {
        return Builder::Divi;
    }
    if raw.is_some_and(|r| r.contains("<!-- wp:")) {
        return Builder::Gutenberg;
    }
    if html.contains("<!-- wp:") || has_wp_block_class(html) {
        return Builder::Gutenberg;
    }
    Builder::Classic
}

/// `/class="[^"]*wp-block/`
fn has_wp_block_class(html: &str) -> bool {
    let mut rest = html;
    while let Some(p) = rest.find("class=\"") {
        let after = rest.get(p + 7..).unwrap_or("");
        let value = after.split('"').next().unwrap_or("");
        if value.contains("wp-block") {
            return true;
        }
        rest = after;
    }
    false
}

/// Names of top-level elements — handy for diagnostics.
pub fn top_level_tags(html: &str) -> Vec<String> {
    if html.len() > MAX_HTML_BYTES {
        return Vec::new();
    }
    let dom = Dom::parse(html);
    dom.roots
        .iter()
        .filter_map(|&r| dom.node(r).and_then(Node::name).map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests;
