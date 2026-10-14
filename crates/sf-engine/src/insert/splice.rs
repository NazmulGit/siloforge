//! Block-aware byte-splice insertion into raw post content, undo, paragraph replacement and
//! post-write verification.

use serde::{Deserialize, Serialize};

use super::InsertError;
use crate::html::blocks::{self, Block, NodeFlags, Piece, SplitMode};
use crate::html::dom::{Dom, NodeKind};
use crate::html::{
    MAX_HTML_BYTES, Paragraph, escape_attr, is_js_whitespace, normalize_url, normalize_ws,
};

/// Inline formatting elements a new link may wrap entirely (`<a><strong>x</strong></a>`).
const ATOMIC_INLINE: &[&str] = &[
    "b", "strong", "em", "i", "u", "span", "mark", "small", "abbr", "cite", "q", "s", "del", "ins",
    "sub", "sup", "time", "data", "dfn", "bdi", "bdo", "font", "big", "strike",
];

/// A native link insertion request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InsertRequest {
    /// Canonical paragraph index (`paragraphs.idx`).
    pub paragraph_index: usize,
    /// Expected canonical hash (`paragraphs.hash`) — the paragraph must be unchanged.
    pub expected_hash: String,
    pub anchor: String,
    pub target_url: String,
    /// Splitter used when the paragraph rows were produced.
    #[serde(default)]
    pub mode: SplitMode,
}

/// Result of a successful insertion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InsertOutcome {
    /// New full content.
    pub content: String,
    /// Byte offset of the inserted `<a` in `content`.
    pub link_start: usize,
    /// Byte offset past the inserted `</a>` in `content`.
    pub link_end: usize,
    /// The exact source text that was wrapped (original casing/markup).
    pub wrapped: String,
}

/// Only http(s) absolute URLs or site-relative paths may be inserted.
pub fn validate_target_url(url: &str) -> Result<&str, InsertError> {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    let ok_scheme = lower.starts_with("https://") || lower.starts_with("http://");
    let ok_relative = u.starts_with('/') && !u.starts_with("//");
    if u.is_empty()
        || u.len() > 2048
        || u.chars().any(|c| c.is_control() || c.is_whitespace())
        || !(ok_scheme || ok_relative)
    {
        return Err(InsertError::InvalidUrl);
    }
    Ok(u)
}

/// Loose URL identity for "is this the same target": exact, or same host+path (v1
/// `normalizeUrl`), or a relative path equal to the absolute URL's path.
pub fn urls_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let pa = url::Url::parse(a).ok();
    let pb = url::Url::parse(b).ok();
    match (pa, pb) {
        (Some(_), Some(_)) => normalize_url(a) == normalize_url(b),
        (Some(u), None) | (None, Some(u)) => {
            let rel = if url::Url::parse(a).is_ok() { b } else { a };
            if !rel.starts_with('/') || rel.starts_with("//") {
                return false;
            }
            let rel_path = rel
                .split(['?', '#'])
                .next()
                .unwrap_or("")
                .trim_end_matches('/');
            rel_path == u.path().trim_end_matches('/')
        }
        (None, None) => a.trim_end_matches('/') == b.trim_end_matches('/'),
    }
}

struct Parsed<'a> {
    dom: Dom<'a>,
    flags: NodeFlags,
    blocks: Vec<Block>,
}

fn parse(raw: &str, mode: SplitMode) -> Result<Parsed<'_>, InsertError> {
    if raw.len() > MAX_HTML_BYTES {
        return Err(InsertError::TooLarge);
    }
    let dom = Dom::parse(raw);
    let flags = blocks::node_flags(&dom);
    let blocks = blocks::split(&dom, mode, &flags);
    Ok(Parsed { dom, flags, blocks })
}

fn locate<'b>(
    blocks: &'b [Block],
    index: usize,
    expected_hash: &str,
) -> Result<&'b Block, InsertError> {
    let Some(b) = blocks.get(index) else {
        return Err(InsertError::ParagraphNotFound {
            index,
            count: blocks.len(),
        });
    };
    if !b.para.hash.eq_ignore_ascii_case(expected_hash.trim()) {
        return Err(InsertError::ContentChanged {
            index,
            found_hash: b.para.hash.clone(),
            found_at: blocks
                .iter()
                .position(|x| x.para.hash.eq_ignore_ascii_case(expected_hash.trim())),
        });
    }
    Ok(b)
}

/// Re-split `raw` and return the paragraph at `index` if its hash still matches.
pub fn locate_paragraph(
    raw: &str,
    index: usize,
    expected_hash: &str,
    mode: SplitMode,
) -> Result<Paragraph, InsertError> {
    let p = parse(raw, mode)?;
    locate(&p.blocks, index, expected_hash).map(|b| b.para.clone())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
    Text,
    Atomic(usize),
    Barrier,
}

#[derive(Debug, Clone, Copy)]
struct HayChar {
    ch: char,
    start: usize,
    end: usize,
    region: Region,
}

struct Search<'p, 'a> {
    p: &'p Parsed<'a>,
    /// Shortcode tag + paired content ranges inside the block: never link there.
    no_go: Vec<(usize, usize)>,
}

impl Search<'_, '_> {
    fn forbidden(&self, ix: usize) -> bool {
        self.p.flags.forbidden.get(ix).copied().unwrap_or(true)
            || self.p.flags.invisible.get(ix).copied().unwrap_or(true)
    }

    fn no_go(&self, pos: usize) -> bool {
        self.no_go.iter().any(|&(s, e)| pos >= s && pos < e)
    }

    fn push_char(hay: &mut Vec<HayChar>, ch: char, start: usize, end: usize, region: Region) {
        if region != Region::Barrier && is_js_whitespace(ch) {
            if hay
                .last()
                .is_some_and(|h| h.ch == ' ' && h.region != Region::Barrier)
            {
                return;
            }
            hay.push(HayChar {
                ch: ' ',
                start,
                end,
                region,
            });
            return;
        }
        for lc in ch.to_lowercase() {
            hay.push(HayChar {
                ch: lc,
                start,
                end,
                region,
            });
        }
    }

    fn barrier(hay: &mut Vec<HayChar>, at: usize) {
        if hay.last().is_some_and(|h| h.region == Region::Barrier) {
            return;
        }
        hay.push(HayChar {
            ch: '\0',
            start: at,
            end: at,
            region: Region::Barrier,
        });
    }

    fn push_text(
        &self,
        hay: &mut Vec<HayChar>,
        node: usize,
        from: usize,
        to: usize,
        region: Region,
    ) {
        let mut buf = Vec::new();
        self.p.dom.text_chars(node, from, to, &mut buf);
        for d in buf {
            if self.no_go(d.start) {
                Self::barrier(hay, d.start);
            } else {
                Self::push_char(hay, d.ch, d.start, d.end, region);
            }
        }
    }

    /// An inline element can be wrapped whole when it contains no link/forbidden/void-ish node.
    fn atomic_ok(&self, ix: usize) -> bool {
        let dom = &self.p.dom;
        let Some(name) = dom.node(ix).and_then(|n| n.name()) else {
            return false;
        };
        if !ATOMIC_INLINE.contains(&name) || self.forbidden(ix) {
            return false;
        }
        dom.descendants(ix)
            .into_iter()
            .all(|d| match dom.node(d).map(|n| &n.kind) {
                Some(NodeKind::Element { name, .. }) => {
                    ATOMIC_INLINE.contains(&name.as_str()) && !self.forbidden(d)
                }
                Some(NodeKind::Text { .. }) => !self.forbidden(d),
                _ => false,
            })
    }

    fn build_hay(&self, children: &[Piece]) -> Vec<HayChar> {
        let dom = &self.p.dom;
        let mut hay = Vec::new();
        for piece in children {
            match piece {
                Piece::Range { node, start, end } => {
                    if self.forbidden(*node) {
                        Self::barrier(&mut hay, *start);
                    } else {
                        self.push_text(&mut hay, *node, *start, *end, Region::Text);
                    }
                }
                Piece::Node(ix) => {
                    let Some(n) = dom.node(*ix) else { continue };
                    match &n.kind {
                        NodeKind::Text { .. } => {
                            if self.forbidden(*ix) {
                                Self::barrier(&mut hay, n.start);
                            } else {
                                self.push_text(&mut hay, *ix, n.start, n.end, Region::Text);
                            }
                        }
                        NodeKind::Comment => Self::barrier(&mut hay, n.start),
                        NodeKind::Element { .. } => {
                            if self.atomic_ok(*ix) && !self.no_go(n.start) {
                                let mut all = vec![*ix];
                                all.extend(dom.descendants(*ix));
                                for d in all {
                                    self.push_text(&mut hay, d, 0, usize::MAX, Region::Atomic(*ix));
                                }
                            } else {
                                Self::barrier(&mut hay, n.start);
                            }
                        }
                    }
                }
            }
        }
        hay
    }

    /// First valid match of `needle` in a child list → source byte span to wrap.
    fn find(&self, children: &[Piece], needle: &[char]) -> Option<(usize, usize)> {
        let hay = self.build_hay(children);
        let m = needle.len();
        if m == 0 || hay.len() < m {
            return None;
        }
        let dom = &self.p.dom;
        'outer: for s in 0..=(hay.len() - m) {
            for (k, nc) in needle.iter().enumerate() {
                match hay.get(s + k) {
                    Some(h) if h.ch == *nc && h.region != Region::Barrier => {}
                    _ => continue 'outer,
                }
            }
            let e = s + m;
            let before = s.checked_sub(1).and_then(|i| hay.get(i));
            let after = hay.get(e);
            if before.is_some_and(|h| h.ch.is_alphanumeric())
                || after.is_some_and(|h| h.ch.is_alphanumeric())
            {
                continue;
            }
            let (Some(first), Some(last)) = (hay.get(s), hay.get(e - 1)) else {
                continue;
            };
            // Atomic regions at the edges must be fully covered.
            if let Region::Atomic(x) = first.region
                && before.is_some_and(|h| h.region == Region::Atomic(x))
            {
                continue;
            }
            if let Region::Atomic(y) = last.region
                && after.is_some_and(|h| h.region == Region::Atomic(y))
            {
                continue;
            }
            let start = match first.region {
                Region::Atomic(x) => dom.node(x).map(|n| n.start)?,
                _ => first.start,
            };
            let end = match last.region {
                Region::Atomic(y) => dom.node(y).map(|n| n.end)?,
                _ => last.end,
            };
            if end > start {
                return Some((start, end));
            }
        }
        None
    }
}

fn needle_chars(anchor: &str) -> Vec<char> {
    normalize_ws(anchor)
        .chars()
        .flat_map(char::to_lowercase)
        .collect()
}

/// Insert exactly one `<a href="target_url">…</a>` around the first valid occurrence of
/// `anchor` inside the addressed paragraph. Never inside an existing link, heading, code/pre,
/// script/style, button, caption or shortcode; whole words only; case-insensitive; may wrap
/// whole inline formatting elements (`best <b>bike</b> locks`). The rest of the document is
/// preserved byte-for-byte (Gutenberg block comments included).
pub fn insert_link(raw: &str, req: &InsertRequest) -> Result<InsertOutcome, InsertError> {
    let url = validate_target_url(&req.target_url)?;
    let needle = needle_chars(&req.anchor);
    if needle.is_empty() || needle.len() > 512 {
        return Err(InsertError::EmptyAnchor);
    }
    let parsed = parse(raw, req.mode)?;
    let block = locate(&parsed.blocks, req.paragraph_index, &req.expected_hash)?;
    let dom = &parsed.dom;

    // Gather element nodes of the block (pre-order) for the parent-by-parent search.
    let mut elements: Vec<usize> = Vec::new();
    for piece in &block.pieces {
        if let Piece::Node(ix) = piece {
            elements.push(*ix);
            elements.extend(dom.descendants(*ix));
        }
    }
    // Refuse when the paragraph already links to the target.
    for &ix in &elements {
        if let Some(n) = dom.node(ix)
            && n.name() == Some("a")
            && n.attr("href").is_some_and(|h| urls_match(h, url))
        {
            return Err(InsertError::AlreadyLinked);
        }
    }
    let (strip, content) = blocks::shortcode_ranges(raw, block.para.start, block.para.end);
    let mut no_go = strip;
    no_go.extend(content);
    let search = Search { p: &parsed, no_go };

    let mut best: Option<(usize, usize)> = None;
    let mut consider = |cand: Option<(usize, usize)>| {
        if let Some(c) = cand
            && best.is_none_or(|b| c.0 < b.0)
        {
            best = Some(c);
        }
    };
    // Virtual parent: the block's own piece list (autop runs, top-level inline elements).
    consider(search.find(&block.pieces, &needle));
    for &ix in &elements {
        let Some(n) = dom.node(ix) else { continue };
        if !matches!(n.kind, NodeKind::Element { .. })
            || search.forbidden(ix)
            || n.children.is_empty()
        {
            continue;
        }
        let kids: Vec<Piece> = n.children.iter().map(|&c| Piece::Node(c)).collect();
        consider(search.find(&kids, &needle));
    }
    let Some((s, e)) = best else {
        return Err(InsertError::AnchorNotFound);
    };
    let wrapped = raw
        .get(s..e)
        .ok_or(InsertError::AnchorNotFound)?
        .to_string();
    let open = format!("<a href=\"{}\">", escape_attr(url));
    let mut content = String::with_capacity(raw.len() + open.len() + 4);
    content.push_str(raw.get(..s).unwrap_or(""));
    content.push_str(&open);
    content.push_str(&wrapped);
    content.push_str("</a>");
    content.push_str(raw.get(e..).unwrap_or(""));
    Ok(InsertOutcome {
        link_start: s,
        link_end: s + open.len() + wrapped.len() + 4,
        content,
        wrapped,
    })
}

/// All `<a>` elements whose href matches `target_url` and whose normalized text equals
/// `anchor` (case-insensitive): (outer start, outer end, inner start, inner end).
fn matching_links(
    dom: &Dom<'_>,
    target_url: &str,
    anchor: &str,
) -> Vec<(usize, usize, usize, usize)> {
    let want = normalize_ws(anchor).to_lowercase();
    dom.all_in_order()
        .into_iter()
        .filter_map(|ix| {
            let n = dom.node(ix)?;
            if n.name() != Some("a") || !n.attr("href").is_some_and(|h| urls_match(h, target_url)) {
                return None;
            }
            if normalize_ws(&dom.text_content(ix)).to_lowercase() != want {
                return None;
            }
            let (is, ie) = n.inner();
            Some((n.start, n.end, is, ie))
        })
        .collect()
}

/// Undo: unwrap one link to `target_url` with text `anchor`, restoring its inner bytes.
/// `near` = the `link_start` returned by [`insert_link`] (closest match wins; first on ties);
/// `None` = first match. Insert-then-remove restores the original byte-for-byte.
pub fn remove_link(
    raw: &str,
    target_url: &str,
    anchor: &str,
    near: Option<usize>,
) -> Result<String, InsertError> {
    if raw.len() > MAX_HTML_BYTES {
        return Err(InsertError::TooLarge);
    }
    let dom = Dom::parse(raw);
    let links = matching_links(&dom, target_url, anchor);
    let chosen = match near {
        Some(p) => links.iter().min_by_key(|l| l.0.abs_diff(p)),
        None => links.first(),
    };
    let Some(&(s, e, is, ie)) = chosen else {
        return Err(InsertError::LinkNotFound);
    };
    let mut out = String::with_capacity(raw.len());
    out.push_str(raw.get(..s).unwrap_or(""));
    out.push_str(raw.get(is..ie).unwrap_or(""));
    out.push_str(raw.get(e..).unwrap_or(""));
    Ok(out)
}

/// Number of links to `target_url` whose text equals `anchor` (case/whitespace-insensitive,
/// curly quotes/dashes normalized like v1 `verify.ts`).
pub fn count_links(html: &str, target_url: &str, anchor: &str) -> usize {
    if html.len() > MAX_HTML_BYTES {
        return 0;
    }
    let dom = Dom::parse(html);
    let want = verify_norm(anchor);
    dom.all_in_order()
        .into_iter()
        .filter(|&ix| {
            dom.node(ix).is_some_and(|n| {
                n.name() == Some("a")
                    && n.attr("href").is_some_and(|h| urls_match(h, target_url))
                    && verify_norm(&dom.text_content(ix)) == want
            })
        })
        .count()
}

fn verify_norm(s: &str) -> String {
    normalize_ws(s)
        .to_lowercase()
        .replace(['\u{2018}', '\u{2019}'], "'")
        .replace(['\u{201C}', '\u{201D}'], "\"")
        .replace(['\u{2013}', '\u{2014}'], "-")
        .replace('\u{2026}', "...")
}

/// Post-write check: exactly one link to `target_url` with text `anchor` exists.
pub fn verify_link(html_after: &str, target_url: &str, anchor: &str) -> bool {
    count_links(html_after, target_url, anchor) == 1
}

/// Replace the addressed paragraph's bytes with `new_html` (rephrase module). Hash-checked.
pub fn replace_paragraph(
    raw: &str,
    index: usize,
    expected_hash: &str,
    new_html: &str,
    mode: SplitMode,
) -> Result<String, InsertError> {
    let parsed = parse(raw, mode)?;
    let b = locate(&parsed.blocks, index, expected_hash)?;
    let mut out = String::with_capacity(raw.len() + new_html.len());
    out.push_str(raw.get(..b.para.start).unwrap_or(""));
    out.push_str(new_html);
    out.push_str(raw.get(b.para.end..).unwrap_or(""));
    Ok(out)
}

/// Append `" " + sentence_html` at the end of the addressed paragraph, inside its block
/// element when it has an explicit end tag (context module). Hash-checked.
pub fn append_to_paragraph(
    raw: &str,
    index: usize,
    expected_hash: &str,
    sentence_html: &str,
    mode: SplitMode,
) -> Result<String, InsertError> {
    let parsed = parse(raw, mode)?;
    let b = locate(&parsed.blocks, index, expected_hash)?;
    let at = match b.pieces.as_slice() {
        [Piece::Node(ix)] => parsed
            .dom
            .node(*ix)
            .and_then(|n| match &n.kind {
                NodeKind::Element {
                    end_tag_start: Some(e),
                    ..
                } => Some(*e),
                _ => None,
            })
            .unwrap_or(b.para.end),
        _ => b.para.end,
    };
    let mut out = String::with_capacity(raw.len() + sentence_html.len() + 1);
    out.push_str(raw.get(..at).unwrap_or(""));
    out.push(' ');
    out.push_str(sentence_html);
    out.push_str(raw.get(at..).unwrap_or(""));
    Ok(out)
}
