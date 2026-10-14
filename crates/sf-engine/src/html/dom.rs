//! A small, forgiving HTML tokenizer + tree builder that keeps exact source byte spans.
//!
//! Why not html5ever/scraper: they re-serialize and drop source positions, so insertion would
//! have to rewrite the whole document (v1 bug B-1/B-5: block comments lost, wrong paragraph).
//! This parser implements the HTML5 rules that matter for prose content (void elements, raw text
//! elements, implied `</p>`/`</li>`/`</td>` etc., transparent `html/head/body`, nested `<a>`), and
//! never fails: malformed input degrades to text. Depth is bounded by [`MAX_DEPTH`].

use super::entities;

/// Maximum element nesting kept on the open-element stack. Deeper elements are attached flat.
pub(crate) const MAX_DEPTH: usize = 256;

#[derive(Debug, Clone)]
pub(crate) struct Attr {
    /// Lowercased attribute name.
    pub name: String,
    /// Entity-decoded value (empty for boolean attributes).
    pub value: String,
}

#[derive(Debug, Clone)]
pub(crate) enum NodeKind {
    Element {
        /// Lowercased tag name.
        name: String,
        attrs: Vec<Attr>,
        /// Byte offset just past the start tag's `>`.
        start_tag_end: usize,
        /// Byte offset of the explicit end tag's `<`, when present.
        end_tag_start: Option<usize>,
    },
    /// Text; `raw` = content of a raw-text element (script/style), no entity decoding.
    Text {
        raw: bool,
    },
    Comment,
}

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub kind: NodeKind,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    /// Outer span start (the `<` of the start tag, or first text byte).
    pub start: usize,
    /// Outer span end (past the end tag, or past the last content when implicitly closed).
    pub end: usize,
}

impl Node {
    pub fn name(&self) -> Option<&str> {
        match &self.kind {
            NodeKind::Element { name, .. } => Some(name.as_str()),
            _ => None,
        }
    }

    pub fn attr(&self, key: &str) -> Option<&str> {
        match &self.kind {
            NodeKind::Element { attrs, .. } => attrs
                .iter()
                .find(|a| a.name == key)
                .map(|a| a.value.as_str()),
            _ => None,
        }
    }

    /// Byte range of the element content (between the tags).
    pub fn inner(&self) -> (usize, usize) {
        match &self.kind {
            NodeKind::Element {
                start_tag_end,
                end_tag_start,
                ..
            } => {
                let s = *start_tag_end;
                let e = end_tag_start.unwrap_or(self.end).max(s);
                (s, e)
            }
            _ => (self.start, self.end),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Dom<'a> {
    pub src: &'a str,
    pub nodes: Vec<Node>,
    pub roots: Vec<usize>,
}

const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr", "keygen",
];
const RAW_TEXT: &[&str] = &[
    "script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes", "noscript",
];
/// Start tags that implicitly close an open `<p>`.
const CLOSES_P: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "center",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "ul",
    "listing",
    "plaintext",
];
/// Elements that bound the search for an open `<p>` / `<li>` (approximate HTML5 scopes).
const SCOPE_BOUNDARY: &[&str] = &[
    "table", "td", "th", "caption", "marquee", "object", "applet", "template", "button", "html",
];
const HEADINGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];
const TRANSPARENT: &[&str] = &["html", "head", "body"];

pub(crate) fn is_void(name: &str) -> bool {
    VOID.contains(&name)
}

enum Tok {
    Start {
        name: String,
        attrs: Vec<Attr>,
        self_closing: bool,
        start: usize,
        end: usize,
    },
    End {
        name: String,
        start: usize,
        end: usize,
    },
    Text {
        start: usize,
        end: usize,
        raw: bool,
    },
    Comment {
        start: usize,
        end: usize,
    },
}

/// Find `needle` (ASCII, lowercase) case-insensitively in `hay` starting at `from`.
fn find_ci(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let last = hay.len() - needle.len();
    let mut i = from;
    while i <= last {
        if hay
            .get(i..i + needle.len())
            .is_some_and(|w| w.eq_ignore_ascii_case(needle))
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn is_name_byte(b: u8) -> bool {
    !(b.is_ascii_whitespace() || b == b'/' || b == b'>' || b == b'<' || b == b'=')
}

fn tokenize(src: &str) -> Vec<Tok> {
    let b = src.as_bytes();
    let n = b.len();
    let mut toks = Vec::new();
    let mut i = 0usize;
    let mut text_start = 0usize;
    let flush_text = |toks: &mut Vec<Tok>, s: usize, e: usize| {
        if e > s {
            toks.push(Tok::Text {
                start: s,
                end: e,
                raw: false,
            });
        }
    };
    while i < n {
        if b.get(i) != Some(&b'<') {
            // Skip quickly to the next '<'.
            match b.get(i..).and_then(|r| r.iter().position(|&c| c == b'<')) {
                Some(p) => {
                    i += p;
                    continue;
                }
                None => break,
            }
        }
        let next = b.get(i + 1).copied();
        match next {
            Some(b'!') => {
                flush_text(&mut toks, text_start, i);
                let end = if b.get(i + 2..i + 4) == Some(b"--") {
                    match find_ci(b, i + 4, b"-->") {
                        Some(p) => p + 3,
                        None => n,
                    }
                } else {
                    match b.get(i..).and_then(|r| r.iter().position(|&c| c == b'>')) {
                        Some(p) => i + p + 1,
                        None => n,
                    }
                };
                toks.push(Tok::Comment { start: i, end });
                i = end;
                text_start = i;
            }
            Some(b'?') => {
                flush_text(&mut toks, text_start, i);
                let end = match b.get(i..).and_then(|r| r.iter().position(|&c| c == b'>')) {
                    Some(p) => i + p + 1,
                    None => n,
                };
                toks.push(Tok::Comment { start: i, end });
                i = end;
                text_start = i;
            }
            Some(b'/') if b.get(i + 2).is_some_and(u8::is_ascii_alphabetic) => {
                flush_text(&mut toks, text_start, i);
                let name_start = i + 2;
                let mut j = name_start;
                while j < n && b.get(j).copied().is_some_and(is_name_byte) {
                    j += 1;
                }
                let name = src.get(name_start..j).unwrap_or("").to_ascii_lowercase();
                let end = match b.get(j..).and_then(|r| r.iter().position(|&c| c == b'>')) {
                    Some(p) => j + p + 1,
                    None => n,
                };
                toks.push(Tok::End {
                    name,
                    start: i,
                    end,
                });
                i = end;
                text_start = i;
            }
            Some(c) if c.is_ascii_alphabetic() => {
                let Some((tok, end)) = parse_start_tag(src, i) else {
                    // Unterminated tag at EOF: keep as text.
                    break;
                };
                flush_text(&mut toks, text_start, i);
                let raw_name = match &tok {
                    Tok::Start {
                        name, self_closing, ..
                    } if RAW_TEXT.contains(&name.as_str()) && !self_closing => Some(name.clone()),
                    _ => None,
                };
                toks.push(tok);
                i = end;
                text_start = i;
                if let Some(name) = raw_name {
                    // Raw text until `</name` followed by a delimiter.
                    let mut close_needle = Vec::with_capacity(name.len() + 2);
                    close_needle.extend_from_slice(b"</");
                    close_needle.extend_from_slice(name.as_bytes());
                    let mut search = i;
                    let content_end = loop {
                        match find_ci(b, search, &close_needle) {
                            Some(p) => {
                                let after = b.get(p + close_needle.len()).copied();
                                if after.is_none_or(|c| {
                                    c.is_ascii_whitespace() || c == b'/' || c == b'>'
                                }) {
                                    break p;
                                }
                                search = p + 1;
                            }
                            None => break n,
                        }
                    };
                    if content_end > i {
                        toks.push(Tok::Text {
                            start: i,
                            end: content_end,
                            raw: matches!(
                                name.as_str(),
                                "script"
                                    | "style"
                                    | "xmp"
                                    | "iframe"
                                    | "noembed"
                                    | "noframes"
                                    | "noscript"
                            ),
                        });
                    }
                    i = content_end;
                    text_start = i;
                }
            }
            _ => {
                // A lone '<' is text.
                i += 1;
            }
        }
    }
    flush_text(&mut toks, text_start, n.max(text_start));
    toks
}

/// Parse a start tag at `src[i] == '<'`. Returns the token and the offset past `>`.
fn parse_start_tag(src: &str, i: usize) -> Option<(Tok, usize)> {
    let b = src.as_bytes();
    let n = b.len();
    let mut j = i + 1;
    while j < n && b.get(j).copied().is_some_and(is_name_byte) {
        j += 1;
    }
    let name = src.get(i + 1..j)?.to_ascii_lowercase();
    let mut attrs: Vec<Attr> = Vec::new();
    let mut self_closing = false;
    loop {
        while j < n && b.get(j).is_some_and(u8::is_ascii_whitespace) {
            j += 1;
        }
        let c = *b.get(j)?;
        if c == b'>' {
            j += 1;
            break;
        }
        if c == b'/' {
            j += 1;
            if b.get(j) == Some(&b'>') {
                self_closing = true;
                j += 1;
                break;
            }
            continue;
        }
        // Attribute name (a stray '=' or '<' starts a name too, per HTML5 error recovery).
        let an_start = j;
        j += 1;
        while j < n && b.get(j).copied().is_some_and(is_name_byte) {
            j += 1;
        }
        let aname = src.get(an_start..j).unwrap_or("").to_ascii_lowercase();
        while j < n && b.get(j).is_some_and(u8::is_ascii_whitespace) {
            j += 1;
        }
        let mut value = String::new();
        if b.get(j) == Some(&b'=') {
            j += 1;
            while j < n && b.get(j).is_some_and(u8::is_ascii_whitespace) {
                j += 1;
            }
            match b.get(j).copied() {
                Some(q @ (b'"' | b'\'')) => {
                    let vs = j + 1;
                    let ve = b
                        .get(vs..)
                        .and_then(|r| r.iter().position(|&c| c == q))
                        .map(|p| vs + p)?;
                    value = entities::decode_str(src.get(vs..ve).unwrap_or(""));
                    j = ve + 1;
                }
                Some(_) => {
                    let vs = j;
                    while j < n
                        && b.get(j)
                            .is_some_and(|c| !c.is_ascii_whitespace() && *c != b'>')
                    {
                        j += 1;
                    }
                    value = entities::decode_str(src.get(vs..j).unwrap_or(""));
                }
                None => return None,
            }
        }
        if attrs.len() < 256 && !attrs.iter().any(|a| a.name == aname) {
            attrs.push(Attr { name: aname, value });
        }
    }
    Some((
        Tok::Start {
            name,
            attrs,
            self_closing,
            start: i,
            end: j,
        },
        j,
    ))
}

impl<'a> Dom<'a> {
    /// Parse `src` into a tree. Never fails.
    pub fn parse(src: &'a str) -> Self {
        let toks = tokenize(src);
        let mut dom = Dom {
            src,
            nodes: Vec::with_capacity(toks.len()),
            roots: Vec::new(),
        };
        let mut stack: Vec<usize> = Vec::new();
        // Depth of foreign content (svg/math) on the stack: honour self-closing there.
        for tok in toks {
            match tok {
                Tok::Text { start, end, raw } => {
                    dom.append(&stack, NodeKind::Text { raw }, start, end);
                }
                Tok::Comment { start, end } => {
                    dom.append(&stack, NodeKind::Comment, start, end);
                }
                Tok::Start {
                    name,
                    attrs,
                    self_closing,
                    start,
                    end,
                } => {
                    if TRANSPARENT.contains(&name.as_str()) {
                        continue;
                    }
                    dom.implied_closes(&mut stack, &name, start);
                    let in_foreign = stack.iter().any(|&ix| {
                        matches!(dom.nodes.get(ix).and_then(Node::name), Some("svg" | "math"))
                    });
                    let void = is_void(&name)
                        || (self_closing && (in_foreign || name == "svg" || name == "math"));
                    let idx = dom.append(
                        &stack,
                        NodeKind::Element {
                            name,
                            attrs,
                            start_tag_end: end,
                            end_tag_start: None,
                        },
                        start,
                        end,
                    );
                    if !void && stack.len() < MAX_DEPTH {
                        stack.push(idx);
                    }
                }
                Tok::End { name, start, end } => {
                    if TRANSPARENT.contains(&name.as_str()) {
                        continue;
                    }
                    if name == "br" {
                        dom.append(
                            &stack,
                            NodeKind::Element {
                                name,
                                attrs: Vec::new(),
                                start_tag_end: end,
                                end_tag_start: None,
                            },
                            start,
                            end,
                        );
                        continue;
                    }
                    let pos = stack.iter().rposition(|&ix| {
                        dom.nodes.get(ix).and_then(Node::name) == Some(name.as_str())
                    });
                    if let Some(pos) = pos {
                        while stack.len() > pos + 1 {
                            if let Some(ix) = stack.pop() {
                                dom.close_implicit(ix);
                            }
                        }
                        if let Some(ix) = stack.pop()
                            && let Some(node) = dom.nodes.get_mut(ix)
                        {
                            if let NodeKind::Element { end_tag_start, .. } = &mut node.kind {
                                *end_tag_start = Some(start);
                            }
                            node.end = end;
                        }
                    }
                }
            }
        }
        while let Some(ix) = stack.pop() {
            dom.close_implicit(ix);
        }
        dom
    }

    fn append(&mut self, stack: &[usize], kind: NodeKind, start: usize, end: usize) -> usize {
        let idx = self.nodes.len();
        let parent = stack.last().copied();
        self.nodes.push(Node {
            kind,
            parent,
            children: Vec::new(),
            start,
            end,
        });
        match parent.and_then(|p| self.nodes.get_mut(p)) {
            Some(p) => p.children.push(idx),
            None => self.roots.push(idx),
        }
        idx
    }

    /// Close an element without an end tag: its span ends after its last child.
    fn close_implicit(&mut self, ix: usize) {
        let last_end = self
            .nodes
            .get(ix)
            .and_then(|n| n.children.last())
            .and_then(|&c| self.nodes.get(c))
            .map(|c| c.end);
        if let Some(node) = self.nodes.get_mut(ix)
            && let Some(e) = last_end
        {
            node.end = node.end.max(e);
        }
    }

    fn open_name(&self, ix: usize) -> &str {
        self.nodes.get(ix).and_then(Node::name).unwrap_or("")
    }

    /// Pop up to and including the nearest open element named in `targets`, unless a
    /// boundary element is hit first.
    fn close_nearest(&mut self, stack: &mut Vec<usize>, targets: &[&str], boundary: &[&str]) {
        let mut found = None;
        for (pos, &ix) in stack.iter().enumerate().rev() {
            let n = self.open_name(ix);
            if targets.contains(&n) {
                found = Some(pos);
                break;
            }
            if boundary.contains(&n) {
                break;
            }
        }
        if let Some(pos) = found {
            while stack.len() > pos {
                if let Some(ix) = stack.pop() {
                    self.close_implicit(ix);
                }
            }
        }
    }

    fn implied_closes(&mut self, stack: &mut Vec<usize>, name: &str, _at: usize) {
        if CLOSES_P.contains(&name) {
            self.close_nearest(stack, &["p"], SCOPE_BOUNDARY);
        }
        match name {
            "li" => self.close_nearest(
                stack,
                &["li"],
                &["ul", "ol", "menu", "table", "td", "th", "template"],
            ),
            "dt" | "dd" => self.close_nearest(
                stack,
                &["dt", "dd"],
                &["dl", "table", "td", "th", "template"],
            ),
            "option" => {
                if stack
                    .last()
                    .is_some_and(|&ix| self.open_name(ix) == "option")
                {
                    self.close_nearest(stack, &["option"], &[]);
                }
            }
            "optgroup" => {
                self.close_nearest(stack, &["option", "optgroup"], &["select", "datalist"])
            }
            "tr" => self.close_nearest(
                stack,
                &["tr"],
                &["table", "thead", "tbody", "tfoot", "template"],
            ),
            "td" | "th" => self.close_nearest(stack, &["td", "th"], &["tr", "table", "template"]),
            "thead" | "tbody" | "tfoot" => {
                self.close_nearest(stack, &["thead", "tbody", "tfoot"], &["table", "template"]);
            }
            "a" => self.close_nearest(stack, &["a"], &["table", "td", "th", "template"]),
            n if HEADINGS.contains(&n) => {
                if stack
                    .last()
                    .is_some_and(|&ix| HEADINGS.contains(&self.open_name(ix)))
                    && let Some(ix) = stack.pop()
                {
                    self.close_implicit(ix);
                }
            }
            _ => {}
        }
    }

    pub fn node(&self, ix: usize) -> Option<&Node> {
        self.nodes.get(ix)
    }

    /// Pre-order descendants of `ix` (excluding `ix`), iteratively.
    pub fn descendants(&self, ix: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack: Vec<usize> = match self.nodes.get(ix) {
            Some(n) => n.children.iter().rev().copied().collect(),
            None => return out,
        };
        while let Some(c) = stack.pop() {
            out.push(c);
            if let Some(n) = self.nodes.get(c) {
                stack.extend(n.children.iter().rev().copied());
            }
        }
        out
    }

    /// All nodes in document order.
    pub fn all_in_order(&self) -> Vec<usize> {
        let mut out = Vec::with_capacity(self.nodes.len());
        let mut stack: Vec<usize> = self.roots.iter().rev().copied().collect();
        while let Some(c) = stack.pop() {
            out.push(c);
            if let Some(n) = self.nodes.get(c) {
                stack.extend(n.children.iter().rev().copied());
            }
        }
        out
    }

    /// Ancestor element names of `ix`, nearest first (excluding `ix`).
    pub fn ancestors(&self, ix: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut cur = self.nodes.get(ix).and_then(|n| n.parent);
        while let Some(p) = cur {
            out.push(p);
            cur = self.nodes.get(p).and_then(|n| n.parent);
            if out.len() > MAX_DEPTH + 1 {
                break;
            }
        }
        out
    }

    /// Decoded chars (with source offsets) of a text node, restricted to `[from, to)`.
    pub fn text_chars(
        &self,
        ix: usize,
        from: usize,
        to: usize,
        out: &mut Vec<entities::DecodedChar>,
    ) {
        if let Some(n) = self.nodes.get(ix)
            && let NodeKind::Text { raw } = n.kind
        {
            let s = n.start.max(from);
            let e = n.end.min(to);
            entities::decode_range(self.src, s, e, !raw, out);
        }
    }

    /// Concatenated decoded text of `ix` and its descendants (text nodes only).
    pub fn text_content(&self, ix: usize) -> String {
        let mut buf = Vec::new();
        let mut nodes = vec![ix];
        nodes.extend(self.descendants(ix));
        for c in nodes {
            self.text_chars(c, 0, usize::MAX, &mut buf);
        }
        buf.into_iter().map(|d| d.ch).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(dom: &Dom<'_>) -> Vec<String> {
        dom.roots
            .iter()
            .filter_map(|&r| dom.node(r).and_then(Node::name).map(str::to_string))
            .collect()
    }

    #[test]
    fn spans_are_exact() {
        let src = "<p class=\"x\">Hello <b>bold</b> &amp; more</p><!-- wp:x --><div><p>a<p>b</div>";
        let dom = Dom::parse(src);
        assert_eq!(names(&dom), vec!["p", "div"]);
        let p = dom.node(dom.roots[0]).unwrap();
        assert_eq!(
            &src[p.start..p.end],
            "<p class=\"x\">Hello <b>bold</b> &amp; more</p>"
        );
        assert_eq!(p.attr("class"), Some("x"));
        assert_eq!(dom.text_content(dom.roots[0]), "Hello bold & more");
        // implied </p> inside the div
        let div = dom.node(dom.roots[2]).unwrap();
        assert_eq!(div.children.len(), 2);
        let p2 = dom.node(div.children[0]).unwrap();
        assert_eq!(&src[p2.start..p2.end], "<p>a");
    }

    #[test]
    fn raw_text_and_void() {
        let src = "<script>if (a<b) {}</script><br><img src=x><p>x</p>";
        let dom = Dom::parse(src);
        assert_eq!(names(&dom), vec!["script", "br", "img", "p"]);
        assert_eq!(dom.text_content(dom.roots[0]), "if (a<b) {}");
    }

    #[test]
    fn list_items_close_each_other() {
        let dom = Dom::parse("<ul><li>a<li>b</ul>");
        let ul = dom.node(dom.roots[0]).unwrap();
        assert_eq!(ul.children.len(), 2);
    }

    #[test]
    fn transparent_body_and_nested_anchor() {
        let dom = Dom::parse("<html><body><a href=1>x<a href=2>y</a></body></html>");
        assert_eq!(names(&dom), vec!["a", "a"]);
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let src = "<div>".repeat(10_000) + "x" + &"</div>".repeat(10_000);
        let dom = Dom::parse(&src);
        assert!(!dom.nodes.is_empty());
    }
}
