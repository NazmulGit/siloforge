//! THE canonical paragraph splitter (spec §1.5), shared by chunking and insertion.
//!
//! Output blocks carry their exact source byte span, so insertion is a byte splice and the
//! paragraph `index`/`hash` used at plan time is re-verified at insert time.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::dom::{Dom, Node, NodeKind};
use super::entities::DecodedChar;
use super::{is_js_whitespace, sha1_hex, utf16_len};

/// Minimum canonical text length in UTF-16 code units (v1 `MIN_CHARS`).
pub const MIN_PARAGRAPH_CHARS: usize = 40;
/// Maximum paragraphs returned for one document.
pub const MAX_PARAGRAPHS: usize = 100_000;

/// Splitter flavour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitMode {
    /// v2 default: v1 rules plus WordPress raw-content support (bare text / inline runs split on
    /// blank lines like `wpautop`), non-prose blocks skipped (script, style, pre, form, nav, …,
    /// bug B-19), shortcode tags stripped from text, no duplicated nested `<li>` text.
    #[default]
    Prose,
    /// Byte-for-byte v1 semantics (cheerio/scraper splitter) for hash parity with migrated data.
    V1,
}

/// One canonical paragraph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paragraph {
    /// Canonical index (0-based, document order, after filtering/dedupe) = `paragraphs.idx`.
    pub index: usize,
    /// Whitespace-normalized text.
    pub text: String,
    /// Lowercase hex SHA-1 of the UTF-8 `text`.
    pub hash: String,
    /// Byte offset of the block in the source.
    pub start: usize,
    /// Byte offset past the block in the source.
    pub end: usize,
    /// Exact source bytes of the block (`source[start..end]`).
    pub html: String,
    /// Byte ranges of `text` that come from places an anchor must never go
    /// (existing links, headings, code/pre, buttons, captions, shortcodes, …).
    pub blocked: Vec<(usize, usize)>,
    /// Number of existing `<a href>` elements inside the block.
    pub link_count: usize,
}

#[derive(Debug, Clone)]
pub(crate) enum Piece {
    Node(usize),
    Range {
        node: usize,
        start: usize,
        end: usize,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct Block {
    pub para: Paragraph,
    pub pieces: Vec<Piece>,
}

const HEADINGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];
const CONTAINERS: &[&str] = &["div", "section", "article", "main", "header", "footer"];
/// Inline (phrasing) elements that `wpautop` would wrap into a paragraph with surrounding text.
pub(crate) const INLINE: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "br", "cite", "code", "data", "dfn", "em", "i", "img", "kbd",
    "mark", "q", "s", "samp", "small", "span", "strong", "sub", "sup", "time", "u", "var", "wbr",
    "del", "ins", "font", "strike", "tt", "big", "label", "picture",
];
/// Never prose (Prose mode skips these as blocks).
const NON_PROSE: &[&str] = &[
    "script", "style", "noscript", "template", "iframe", "svg", "math", "form", "button", "select",
    "textarea", "input", "object", "embed", "canvas", "video", "audio", "map", "pre", "nav", "hr",
    "link", "meta", "title", "base", "head",
];
/// Invisible subtrees whose text never counts in Prose mode.
pub(crate) const INVISIBLE: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "math", "head", "title",
];
/// Elements inside which an anchor must never be placed.
pub(crate) const FORBIDDEN: &[&str] = &[
    "a",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "code",
    "pre",
    "kbd",
    "samp",
    "tt",
    "script",
    "style",
    "noscript",
    "template",
    "textarea",
    "button",
    "select",
    "option",
    "label",
    "figcaption",
    "caption",
    "th",
    "svg",
    "math",
    "iframe",
    "object",
    "embed",
    "input",
    "var",
];

/// Per-node flags computed once per document.
pub(crate) struct NodeFlags {
    pub forbidden: Vec<bool>,
    pub invisible: Vec<bool>,
}

pub(crate) fn node_flags(dom: &Dom<'_>) -> NodeFlags {
    let n = dom.nodes.len();
    let mut forbidden = vec![false; n];
    let mut invisible = vec![false; n];
    for ix in dom.all_in_order() {
        let Some(node) = dom.node(ix) else { continue };
        let (pf, pi) = node
            .parent
            .map(|p| {
                (
                    forbidden.get(p).copied().unwrap_or(false),
                    invisible.get(p).copied().unwrap_or(false),
                )
            })
            .unwrap_or((false, false));
        let name = node.name().unwrap_or("");
        if let Some(f) = forbidden.get_mut(ix) {
            *f = pf || FORBIDDEN.contains(&name);
        }
        if let Some(v) = invisible.get_mut(ix) {
            *v = pi || INVISIBLE.contains(&name);
        }
    }
    NodeFlags {
        forbidden,
        invisible,
    }
}

/// A WordPress shortcode tag `[name …]` / `[/name]` found inside a block.
#[derive(Debug, Clone)]
struct Shortcode {
    start: usize,
    end: usize,
    name: String,
    closing: bool,
    attrs: bool,
    self_closing: bool,
}

/// Sorted byte ranges.
pub(crate) type Ranges = Vec<(usize, usize)>;

/// Shortcode analysis over a source range: (tag ranges to strip, content ranges to block).
pub(crate) fn shortcode_ranges(src: &str, start: usize, end: usize) -> (Ranges, Ranges) {
    let b = src.as_bytes();
    let end = end.min(b.len());
    let mut tags: Vec<Shortcode> = Vec::new();
    let mut i = start;
    while i < end && tags.len() < 4096 {
        if b.get(i) != Some(&b'[') {
            i += 1;
            continue;
        }
        // Escaped `[[...]]` is literal text.
        if b.get(i + 1) == Some(&b'[') {
            i += 2;
            continue;
        }
        let mut j = i + 1;
        let closing = b.get(j) == Some(&b'/');
        if closing {
            j += 1;
        }
        let ns = j;
        if !b
            .get(j)
            .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
        {
            i += 1;
            continue;
        }
        while j < end
            && b.get(j)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
        {
            j += 1;
        }
        let name = src.get(ns..j).unwrap_or("").to_ascii_lowercase();
        let after = b.get(j).copied();
        if !matches!(
            after,
            Some(b']') | Some(b' ') | Some(b'/') | Some(b'\t') | Some(b'\n')
        ) {
            i += 1;
            continue;
        }
        // Find the closing ']' on the same tag (no nested '[' or '<').
        let mut k = j;
        let mut attrs = false;
        let mut ok = false;
        while k < end && k - i < 2048 {
            match b.get(k) {
                Some(b']') => {
                    ok = true;
                    break;
                }
                Some(b'[') | Some(b'<') => break,
                Some(b'=') => attrs = true,
                _ => {}
            }
            k += 1;
        }
        if !ok {
            i += 1;
            continue;
        }
        let self_closing = k > 0 && b.get(k - 1) == Some(&b'/');
        tags.push(Shortcode {
            start: i,
            end: k + 1,
            name,
            closing,
            attrs,
            self_closing,
        });
        i = k + 1;
    }
    let mut strip = Vec::new();
    let mut content = Vec::new();
    let mut paired = vec![false; tags.len()];
    for (ci, close) in tags.iter().enumerate() {
        if !close.closing {
            continue;
        }
        // Nearest unpaired opener with the same name before it.
        let opener = tags.iter().enumerate().take(ci).rev().find(|(oi, t)| {
            !t.closing && t.name == close.name && !paired.get(*oi).copied().unwrap_or(true)
        });
        if let Some((oi, open)) = opener {
            if let Some(p) = paired.get_mut(oi) {
                *p = true;
            }
            if let Some(p) = paired.get_mut(ci) {
                *p = true;
            }
            content.push((open.end, close.start));
        }
    }
    for (ti, t) in tags.iter().enumerate() {
        if paired.get(ti).copied().unwrap_or(false) || t.attrs || t.self_closing {
            strip.push((t.start, t.end));
        }
    }
    strip.sort_unstable();
    content.sort_unstable();
    (strip, content)
}

fn in_ranges(ranges: &[(usize, usize)], pos: usize) -> bool {
    // ranges are sorted by start; small lists, linear is fine but use partition for big ones.
    let idx = ranges.partition_point(|r| r.0 <= pos);
    ranges.iter().take(idx).any(|r| pos >= r.0 && pos < r.1)
}

/// Canonical text builder: collapses JS `\s` runs to one space, trims, tracks blocked spans.
pub(crate) struct TextBuilder {
    pub out: String,
    pending: bool,
    /// The pending separator is a hard boundary (between list items): the space is emitted as a
    /// blocked span so no anchor phrase can cross it. The text itself is unchanged.
    hard: bool,
    pub blocked: Vec<(usize, usize)>,
}

impl TextBuilder {
    pub fn new() -> Self {
        Self {
            out: String::new(),
            pending: false,
            hard: false,
            blocked: Vec::new(),
        }
    }

    pub fn sep(&mut self) {
        if !self.out.is_empty() {
            self.pending = true;
        }
    }

    /// Separator that phrases must never span (pilot 2026-10-13: anchors such as
    /// "information Identify common bike" crossed `</li><li>`).
    pub fn hard_sep(&mut self) {
        if !self.out.is_empty() {
            self.pending = true;
            self.hard = true;
        }
    }

    pub fn push(&mut self, ch: char, blocked: bool) {
        if is_js_whitespace(ch) {
            self.sep();
            return;
        }
        if self.pending {
            let s = self.out.len();
            self.out.push(' ');
            self.pending = false;
            if std::mem::take(&mut self.hard) {
                match self.blocked.last_mut() {
                    Some(last) if last.1 >= s => last.1 = s + 1,
                    _ => self.blocked.push((s, s + 1)),
                }
            }
        }
        let s = self.out.len();
        self.out.push(ch);
        let e = self.out.len();
        if blocked {
            match self.blocked.last_mut() {
                Some(last) if last.1 + 1 >= s => last.1 = e,
                _ => self.blocked.push((s, e)),
            }
        }
    }
}

struct Ctx<'d, 'a> {
    dom: &'d Dom<'a>,
    mode: SplitMode,
    flags: &'d NodeFlags,
}

impl Ctx<'_, '_> {
    fn name(&self, ix: usize) -> &str {
        self.dom.node(ix).and_then(Node::name).unwrap_or("")
    }

    fn element_children(&self, ix: usize) -> Vec<usize> {
        self.dom
            .node(ix)
            .map(|n| {
                n.children
                    .iter()
                    .copied()
                    .filter(|&c| {
                        self.dom
                            .node(c)
                            .is_some_and(|c| matches!(c.kind, NodeKind::Element { .. }))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn visit_v1(&self, ix: usize, out: &mut Vec<RawBlock>) {
        let name = self.name(ix);
        if HEADINGS.contains(&name) {
            return;
        }
        if name == "ul" || name == "ol" {
            out.push(RawBlock::list(ix));
            return;
        }
        if matches!(name, "p" | "blockquote" | "pre") {
            out.push(RawBlock::element(ix));
            return;
        }
        if CONTAINERS.contains(&name) {
            let kids = self.element_children(ix);
            if kids.is_empty() {
                out.push(RawBlock::element(ix));
            } else {
                for k in kids {
                    self.visit_v1(k, out);
                }
            }
            return;
        }
        out.push(RawBlock::element(ix));
    }

    fn collect_prose(&self, children: &[usize], out: &mut Vec<RawBlock>) {
        let mut run: Vec<Piece> = Vec::new();
        for &c in children {
            let Some(node) = self.dom.node(c) else {
                continue;
            };
            match &node.kind {
                NodeKind::Text { .. } => {
                    let mut seg_start = node.start;
                    for (bs, be) in blank_line_breaks(self.dom.src, node.start, node.end) {
                        run.push(Piece::Range {
                            node: c,
                            start: seg_start,
                            end: bs,
                        });
                        self.flush_run(&mut run, out);
                        seg_start = be;
                    }
                    run.push(Piece::Range {
                        node: c,
                        start: seg_start,
                        end: node.end,
                    });
                }
                NodeKind::Comment => self.flush_run(&mut run, out),
                NodeKind::Element { name, .. } => {
                    if INLINE.contains(&name.as_str()) {
                        run.push(Piece::Node(c));
                    } else {
                        self.flush_run(&mut run, out);
                        self.visit_prose(c, out);
                    }
                }
            }
        }
        self.flush_run(&mut run, out);
    }

    /// True when the element has a block-level element child (p/div/heading/table/...).
    fn has_block_child(&self, ix: usize) -> bool {
        self.element_children(ix).into_iter().any(|c| {
            let n = self.name(c);
            HEADINGS.contains(&n)
                || CONTAINERS.contains(&n)
                || matches!(
                    n,
                    "p" | "table" | "blockquote" | "figure" | "aside" | "pre" | "dl" | "form"
                )
        })
    }

    fn visit_prose(&self, ix: usize, out: &mut Vec<RawBlock>) {
        let name = self.name(ix);
        if HEADINGS.contains(&name) || NON_PROSE.contains(&name) {
            return;
        }
        if name == "ul" || name == "ol" {
            // A list whose items hold blocks (review lists: `<li><h2/><div/><p/><p/></li>`) is a
            // sequence of sections, not one paragraph (pilot 2026-10-13: whole reviews became one
            // block with glued text such as "Road PedalBest for BeginnersView Latest Price").
            let items = self.element_children(ix);
            if items.iter().any(|&li| self.has_block_child(li)) {
                for li in items {
                    let kids = self
                        .dom
                        .node(li)
                        .map(|n| n.children.clone())
                        .unwrap_or_default();
                    self.collect_prose(&kids, out);
                }
                return;
            }
            out.push(RawBlock::list(ix));
            return;
        }
        if matches!(name, "p" | "blockquote") {
            out.push(RawBlock::element(ix));
            return;
        }
        // Tables are not prose (pilot 2026-10-13): as one block they glued cell text together
        // ("Most Fun FeatureWheel Size") and anchors spanned cells; split per cell they added
        // ~3,500 spec lines ("Material: Reinforced steel cable") per 360 posts. Skip them.
        if name == "table" {
            return;
        }
        if CONTAINERS.contains(&name) || name == "aside" || name == "center" {
            let kids: Vec<usize> = self
                .dom
                .node(ix)
                .map(|n| n.children.clone())
                .unwrap_or_default();
            self.collect_prose(&kids, out);
            return;
        }
        out.push(RawBlock::element(ix));
    }

    /// Turn an inline run into an autop block (trimmed of surrounding whitespace).
    fn flush_run(&self, run: &mut Vec<Piece>, out: &mut Vec<RawBlock>) {
        if run.is_empty() {
            return;
        }
        let pieces = std::mem::take(run);
        let src = self.dom.src;
        let is_blank = |p: &Piece| match p {
            Piece::Range { start, end, .. } => src
                .get(*start..*end)
                .is_none_or(|s| s.trim_matches(is_js_whitespace).is_empty()),
            Piece::Node(_) => false,
        };
        let (Some(first), Some(last)) = (
            pieces.iter().position(|p| !is_blank(p)),
            pieces.iter().rposition(|p| !is_blank(p)),
        ) else {
            return;
        };
        let mut trimmed: Vec<Piece> = pieces
            .get(first..=last)
            .map(<[Piece]>::to_vec)
            .unwrap_or_default();
        if let Some(Piece::Range { start, end, .. }) = trimmed.first_mut()
            && let Some(seg) = src.get(*start..*end)
        {
            *start += seg.len() - seg.trim_start_matches(is_js_whitespace).len();
        }
        if let Some(Piece::Range { start, end, .. }) = trimmed.last_mut()
            && let Some(seg) = src.get(*start..*end)
        {
            *end -= seg.len() - seg.trim_end_matches(is_js_whitespace).len();
        }
        if trimmed.is_empty() {
            return;
        }
        out.push(RawBlock {
            pieces: trimmed,
            list: false,
        });
    }
}

/// Byte ranges `[break_start, break_end)` of blank-line breaks (`\n[ \t\r\f]*\n…`) in a text node.
fn blank_line_breaks(src: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    let b = src.as_bytes();
    let end = end.min(b.len());
    let mut out = Vec::new();
    let mut i = start;
    while i < end {
        if b.get(i) == Some(&b'\n') {
            let mut j = i + 1;
            while j < end && matches!(b.get(j), Some(b' ' | b'\t' | b'\r' | b'\x0c')) {
                j += 1;
            }
            if j < end && b.get(j) == Some(&b'\n') {
                // swallow the whole whitespace run
                let mut k = j + 1;
                while k < end && b.get(k).is_some_and(|c| c.is_ascii_whitespace()) {
                    k += 1;
                }
                out.push((i, k));
                i = k;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[derive(Debug, Clone)]
struct RawBlock {
    pieces: Vec<Piece>,
    list: bool,
}

impl RawBlock {
    fn element(ix: usize) -> Self {
        Self {
            pieces: vec![Piece::Node(ix)],
            list: false,
        }
    }

    fn list(ix: usize) -> Self {
        Self {
            pieces: vec![Piece::Node(ix)],
            list: true,
        }
    }
}

/// Byte span of a piece list.
pub(crate) fn pieces_span(dom: &Dom<'_>, pieces: &[Piece]) -> (usize, usize) {
    let s = match pieces.first() {
        Some(Piece::Node(n)) => dom.node(*n).map(|n| n.start).unwrap_or(0),
        Some(Piece::Range { start, .. }) => *start,
        None => 0,
    };
    let e = match pieces.last() {
        Some(Piece::Node(n)) => dom.node(*n).map(|n| n.end).unwrap_or(s),
        Some(Piece::Range { end, .. }) => *end,
        None => s,
    };
    (s, e.max(s))
}

/// Build canonical text for a raw block.
fn block_text(
    ctx: &Ctx<'_, '_>,
    rb: &RawBlock,
    span: (usize, usize),
) -> (String, Vec<(usize, usize)>, usize) {
    let prose = ctx.mode == SplitMode::Prose;
    let (strip, sc_content) = if prose {
        shortcode_ranges(ctx.dom.src, span.0, span.1)
    } else {
        (Vec::new(), Vec::new())
    };
    let mut tb = TextBuilder::new();
    let mut buf: Vec<DecodedChar> = Vec::new();
    let mut links = 0usize;
    let flags = ctx.flags;

    let emit =
        |tb: &mut TextBuilder, node: usize, from: usize, to: usize, buf: &mut Vec<DecodedChar>| {
            if prose && flags.invisible.get(node).copied().unwrap_or(false) {
                return;
            }
            let forbidden = flags.forbidden.get(node).copied().unwrap_or(false);
            buf.clear();
            ctx.dom.text_chars(node, from, to, buf);
            for d in buf.iter() {
                if prose && in_ranges(&strip, d.start) {
                    tb.sep();
                    continue;
                }
                let blocked = forbidden || (prose && in_ranges(&sc_content, d.start));
                tb.push(d.ch, blocked);
            }
        };

    for piece in &rb.pieces {
        match piece {
            Piece::Range { node, start, end } => emit(&mut tb, *node, *start, *end, &mut buf),
            Piece::Node(root) => {
                let mut all = vec![*root];
                all.extend(ctx.dom.descendants(*root));
                for &n in &all {
                    if ctx.dom.node(n).and_then(Node::name) == Some("a")
                        && ctx.dom.node(n).and_then(|x| x.attr("href")).is_some()
                    {
                        links += 1;
                    }
                }
                if rb.list {
                    let lis: Vec<usize> = all
                        .iter()
                        .copied()
                        .filter(|&n| ctx.name(n) == "li")
                        .collect();
                    if prose {
                        // Walk once; separate at every <li> boundary (no duplicated nested text).
                        for &n in &all {
                            if ctx.name(n) == "li" {
                                tb.hard_sep();
                            }
                            emit(&mut tb, n, 0, usize::MAX, &mut buf);
                        }
                    } else {
                        // v1: join(" ", normalize(li.text) for every descendant li).
                        for li in lis {
                            tb.sep();
                            let mut sub = vec![li];
                            sub.extend(ctx.dom.descendants(li));
                            for n in sub {
                                emit(&mut tb, n, 0, usize::MAX, &mut buf);
                            }
                            tb.sep();
                        }
                    }
                } else {
                    for &n in &all {
                        emit(&mut tb, n, 0, usize::MAX, &mut buf);
                    }
                }
            }
        }
    }
    (tb.out, tb.blocked, links)
}

/// Split a parsed document into canonical blocks.
pub(crate) fn split(dom: &Dom<'_>, mode: SplitMode, flags: &NodeFlags) -> Vec<Block> {
    let ctx = Ctx { dom, mode, flags };
    let mut raw = Vec::new();
    match mode {
        SplitMode::V1 => {
            for &r in &dom.roots {
                if dom
                    .node(r)
                    .is_some_and(|n| matches!(n.kind, NodeKind::Element { .. }))
                {
                    ctx.visit_v1(r, &mut raw);
                }
            }
        }
        SplitMode::Prose => ctx.collect_prose(&dom.roots, &mut raw),
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut blocks = Vec::new();
    for rb in raw {
        let span = pieces_span(dom, &rb.pieces);
        let (text, blocked, link_count) = block_text(&ctx, &rb, span);
        if utf16_len(&text) < MIN_PARAGRAPH_CHARS {
            continue;
        }
        let hash = sha1_hex(&text);
        if !seen.insert(hash.clone()) {
            continue;
        }
        if blocks.len() >= MAX_PARAGRAPHS {
            break;
        }
        let html = dom.src.get(span.0..span.1).unwrap_or("").to_string();
        blocks.push(Block {
            para: Paragraph {
                index: blocks.len(),
                text,
                hash,
                start: span.0,
                end: span.1,
                html,
                blocked,
                link_count,
            },
            pieces: rb.pieces,
        });
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcodes_detected() {
        let s = "a [caption id=\"1\"]<img> Cap[/caption] b [citation needed] [gallery/] c";
        let (strip, content) = shortcode_ranges(s, 0, s.len());
        assert_eq!(strip.len(), 3);
        assert_eq!(content.len(), 1);
        assert_eq!(&s[content[0].0..content[0].1], "<img> Cap");
    }

    #[test]
    fn blank_lines_split() {
        let s = "one\n\ntwo\n \n three\nfour";
        let br = blank_line_breaks(s, 0, s.len());
        assert_eq!(br.len(), 2);
    }
}
