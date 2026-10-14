//! Prompt builders for the rephrase/context modules (v1 `pipeline/llm.ts`) and strict
//! validation of LLM output before anything is published (bug B-4/B-28).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::splice::urls_match;
use crate::html::dom::{Dom, NodeKind};
use crate::html::{escape_attr, escape_text, html_to_text, normalize_ws};

/// v1 request parameters.
pub const LLM_MAX_TOKENS: u32 = 2000;
pub const LLM_TEMPERATURE: f32 = 0.3;
/// Largest reply we even look at.
pub const MAX_REPLY_BYTES: usize = 32 * 1024;

/// A provider-neutral chat message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

fn msg(role: &str, content: String) -> ChatMessage {
    ChatMessage {
        role: role.into(),
        content,
    }
}

/// Anchor text made safe to quote inside a prompt (no quotes/angle brackets, one line).
fn prompt_safe(s: &str) -> String {
    normalize_ws(s)
        .chars()
        .filter(|c| !matches!(c, '"' | '<' | '>' | '`'))
        .take(200)
        .collect()
}

fn link_example(anchor: &str, url: &str) -> String {
    format!(
        "<a href=\"{}\">{}</a>",
        escape_attr(url.trim()),
        escape_text(&prompt_safe(anchor))
    )
}

/// Rephrase prompt (v1 text; anchor/url/title escaped against prompt injection).
pub fn rephrase_prompt(
    paragraph_html: &str,
    anchor: &str,
    target_url: &str,
    target_title: &str,
) -> Vec<ChatMessage> {
    let a = prompt_safe(anchor);
    vec![
        msg(
            "system",
            "You are an expert content editor. You rewrite HTML paragraphs to naturally include an internal link. \
             You PRESERVE all existing HTML tags and attributes exactly as they are. \
             You return ONLY the rewritten paragraph HTML — no explanations, no markdown fences."
                .into(),
        ),
        msg(
            "user",
            format!(
                "Rewrite the following paragraph so that it naturally includes the anchor text \"{a}\" linked as {}. \
                 The link points to an article titled \"{}\". Keep the paragraph's meaning, tone and length. \
                 Preserve every existing HTML tag and attribute exactly. Return only the paragraph HTML.\n\nParagraph:\n{}",
                link_example(anchor, target_url),
                prompt_safe(target_title),
                paragraph_html.chars().take(16_000).collect::<String>()
            ),
        ),
    ]
}

/// Context-sentence prompt (v1 text, escaped).
pub fn context_prompt(
    paragraph_text: &str,
    anchor: &str,
    target_url: &str,
    target_title: &str,
) -> Vec<ChatMessage> {
    let a = prompt_safe(anchor);
    vec![
        msg(
            "system",
            "You write single connective sentences for articles. \
             You return ONLY one sentence of HTML — no explanations, no markdown fences."
                .into(),
        ),
        msg(
            "user",
            format!(
                "Write exactly one natural transition sentence to append to the paragraph below. \
                 The sentence must contain the anchor text \"{a}\" wrapped as {} \
                 and should reference the linked article \"{}\" in a way that flows from the paragraph. \
                 Return only the sentence HTML.\n\nParagraph:\n{}",
                link_example(anchor, target_url),
                prompt_safe(target_title),
                paragraph_text.chars().take(16_000).collect::<String>()
            ),
        ),
    ]
}

/// Strip markdown code fences the model may wrap HTML in (v1 `cleanHtmlReply`).
pub fn strip_code_fences(reply: &str) -> String {
    let mut s = reply.trim();
    if let Some(rest) = s.strip_prefix("```") {
        let rest = rest
            .strip_prefix("html")
            .or_else(|| rest.strip_prefix("HTML"))
            .unwrap_or(rest);
        s = rest.trim_start();
    }
    if let Some(rest) = s.strip_suffix("```") {
        s = rest.trim_end();
    }
    s.trim().to_string()
}

/// Which module produced the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmKind {
    /// Whole rewritten paragraph.
    Rephrase,
    /// One sentence appended to the paragraph.
    Context,
}

/// Validation thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LlmLimits {
    /// Rephrase: output/original visible-text length ratio bounds.
    pub min_length_ratio: f64,
    pub max_length_ratio: f64,
    /// Rephrase: share of the original's 3-word shingles kept (spec B-4: 0.8).
    pub min_shingle_coverage: f64,
    /// Context: max visible chars of the sentence.
    pub max_sentence_chars: usize,
}

impl Default for LlmLimits {
    fn default() -> Self {
        Self {
            min_length_ratio: 0.7,
            max_length_ratio: 1.6,
            min_shingle_coverage: 0.8,
            max_sentence_chars: 400,
        }
    }
}

/// Why an LLM reply was rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum LlmRejection {
    #[error("empty reply")]
    Empty,
    #[error("reply too large")]
    TooLarge,
    #[error("refusal or explanation instead of HTML")]
    Refusal,
    #[error("forbidden element <{tag}>")]
    ForbiddenTag { tag: String },
    #[error("event handler attribute {attr}")]
    EventHandler { attr: String },
    #[error("unsafe url or style in {attr}")]
    UnsafeUrl { attr: String },
    #[error("the new link is missing")]
    MissingLink,
    #[error("the new link points to {href}")]
    WrongHref { href: String },
    #[error("the anchor text is missing from the link")]
    MissingAnchor,
    #[error("{count} new links (exactly one allowed)")]
    ExtraLinks { count: usize },
    #[error("an existing <{tag}> was removed")]
    TagLoss { tag: String },
    #[error("length ratio {ratio:.2} out of bounds")]
    LengthRatio { ratio: f64 },
    #[error("only {coverage:.2} of the original text kept")]
    LowCoverage { coverage: f64 },
    #[error("reply is not a single block")]
    MultipleBlocks,
    #[error("block element <{tag}> in a context sentence")]
    BlockTagInSentence { tag: String },
    #[error("sentence too long or too short ({chars} chars)")]
    SentenceLength { chars: usize },
    /// The reply is not fully parseable, canonical HTML (unterminated tag, unclosed quote, stray
    /// `<`, unbalanced tags, too many attributes). Fail-closed (security audit H-01).
    #[error("malformed HTML: {detail}")]
    Malformed { detail: String },
}

const ALLOWED_TAGS: &[&str] = &[
    "p",
    "a",
    "strong",
    "b",
    "em",
    "i",
    "u",
    "span",
    "br",
    "code",
    "mark",
    "small",
    "sub",
    "sup",
    "s",
    "del",
    "ins",
    "abbr",
    "cite",
    "q",
    "ul",
    "ol",
    "li",
    "blockquote",
    "time",
];
const SENTENCE_BLOCK_TAGS: &[&str] = &["p", "ul", "ol", "li", "blockquote", "br"];
const REFUSAL_LEADS: &[&str] = &[
    "i'm sorry",
    "i am sorry",
    "sorry",
    "i cannot",
    "i can't",
    "i can not",
    "i won't",
    "i will not",
    "i'm unable",
    "i am unable",
    "as an ai",
    "unfortunately",
    "here is",
    "here's",
    "sure",
    "certainly",
    "okay",
    "ok,",
    "note:",
    "rewritten",
];

fn is_refusal(s: &str) -> bool {
    let t = s.trim_start().to_lowercase().replace('\u{2019}', "'");
    REFUSAL_LEADS.iter().any(|l| t.starts_with(l))
        || t.contains("as an ai language model")
        || t.contains("i cannot help")
        || t.contains("i can't help")
}

/// Max attributes on one tag in an LLM reply (H-01: the DOM tokenizer ignored attributes past
/// its own cap, so a handler hidden after them was never inspected).
pub const MAX_REPLY_ATTRS: usize = 16;
/// Max nesting depth of a reply.
const MAX_REPLY_DEPTH: usize = 32;
const VOID_TAGS: &[&str] = &["br"];

fn malformed(detail: impl Into<String>) -> LlmRejection {
    LlmRejection::Malformed {
        detail: detail.into(),
    }
}

/// Attributes kept on a re-serialized tag (everything else is dropped; `on*` is rejected).
fn attr_allowed(tag: &str, name: &str) -> bool {
    matches!(name, "class" | "id" | "title" | "lang" | "dir" | "style")
        || name.starts_with("data-")
        || matches!(
            (tag, name),
            ("a", "href" | "rel" | "target" | "hreflang")
                | ("time", "datetime")
                | ("q" | "blockquote" | "del" | "ins", "cite")
        )
}

/// `&name;`, `&#123;` or `&#x1F;` at the start of `s`: the entity's byte length.
fn entity_len(s: &str) -> Option<usize> {
    if !s.starts_with('&') {
        return None;
    }
    let semi = s.get(1..)?.find(';')? + 1;
    if !(2..=32).contains(&semi) {
        return None;
    }
    let body = s.get(1..semi)?;
    let ok = if let Some(n) = body.strip_prefix('#') {
        match n.strip_prefix(['x', 'X']) {
            Some(h) => !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()),
            None => !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()),
        }
    } else {
        body.chars().all(|c| c.is_ascii_alphanumeric())
    };
    ok.then_some(semi + 1)
}

/// Escape for canonical output: `<`, `>`, `"` always; `&` unless it starts a valid entity.
fn escape_canonical(s: &str, out: &mut String) {
    let mut i = 0;
    while let Some(c) = s.get(i..).and_then(|r| r.chars().next()) {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '&' => match s.get(i..).and_then(entity_len) {
                Some(n) => {
                    out.push_str(s.get(i..i + n).unwrap_or(""));
                    i += n;
                    continue;
                }
                None => out.push_str("&amp;"),
            },
            c => out.push(c),
        }
        i += c.len_utf8();
    }
}

fn squash_lower(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .collect::<String>()
        .to_lowercase()
}

/// URL attribute check: relative URLs and http(s) only; no entity-obfuscated schemes.
fn url_ok(value: &str) -> bool {
    let v = squash_lower(value);
    if v.starts_with("http://") || v.starts_with("https://") {
        return true;
    }
    if v.contains("&#") || v.contains("&colon") || v.contains("&tab") || v.contains("&newline") {
        return false;
    }
    // any scheme other than http(s) (a ':' before the first / ? #) is rejected
    let head = v.split(['/', '?', '#']).next().unwrap_or("");
    !head.contains(':') && !v.starts_with("//")
}

fn style_ok(value: &str) -> bool {
    let v = squash_lower(value);
    !(v.contains("expression(")
        || v.contains("url(")
        || v.contains("javascript:")
        || v.contains('\\')
        || v.contains("&#"))
}

/// Parse an LLM reply strictly and re-serialize it from its tokens through the tag/attribute
/// allowlist (security audit H-01). Fails closed on anything not fully parsed: a `<` that does
/// not open a complete allowed tag, an unterminated tag, an unclosed attribute quote, more than
/// [`MAX_REPLY_ATTRS`] attributes, unbalanced tags, comments, unsafe URLs or event handlers.
/// Text is escaped; the returned string is the ONLY thing that may be published.
pub fn canonicalize_reply(html: &str) -> Result<String, LlmRejection> {
    let mut out = String::with_capacity(html.len() + 16);
    let mut stack: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < html.len() {
        let rest = html.get(i..).unwrap_or("");
        let Some(lt) = rest.find('<') else {
            escape_canonical(rest, &mut out);
            break;
        };
        escape_canonical(rest.get(..lt).unwrap_or(""), &mut out);
        i += lt;
        let tag_src = html.get(i..).unwrap_or("");
        if tag_src.starts_with("<!") || tag_src.starts_with("<?") {
            return Err(LlmRejection::ForbiddenTag {
                tag: "comment".into(),
            });
        }
        let b = tag_src.as_bytes();
        let closing = b.get(1) == Some(&b'/');
        let mut j = if closing { 2 } else { 1 };
        let name_start = j;
        while b.get(j).is_some_and(u8::is_ascii_alphanumeric) {
            j += 1;
        }
        if j == name_start || !b.get(name_start).is_some_and(u8::is_ascii_alphabetic) {
            return Err(malformed("stray '<'"));
        }
        let name = tag_src
            .get(name_start..j)
            .unwrap_or("")
            .to_ascii_lowercase();
        if !ALLOWED_TAGS.contains(&name.as_str()) {
            return Err(LlmRejection::ForbiddenTag { tag: name });
        }
        if closing {
            while b.get(j).is_some_and(u8::is_ascii_whitespace) {
                j += 1;
            }
            if b.get(j) != Some(&b'>') {
                return Err(malformed(format!("unterminated </{name}")));
            }
            if VOID_TAGS.contains(&name.as_str()) || stack.last() != Some(&name) {
                return Err(malformed(format!("unbalanced </{name}>")));
            }
            stack.pop();
            out.push_str("</");
            out.push_str(&name);
            out.push('>');
            i += j + 1;
            continue;
        }
        let mut attrs: Vec<(String, Option<String>)> = Vec::new();
        let self_closing;
        loop {
            while b.get(j).is_some_and(u8::is_ascii_whitespace) {
                j += 1;
            }
            match b.get(j) {
                None => return Err(malformed(format!("unterminated <{name}"))),
                Some(b'>') => {
                    self_closing = false;
                    j += 1;
                    break;
                }
                Some(b'/') if b.get(j + 1) == Some(&b'>') => {
                    self_closing = true;
                    j += 2;
                    break;
                }
                _ => {}
            }
            let an_start = j;
            while b.get(j).is_some_and(|c| {
                c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b':' | b'.')
            }) {
                j += 1;
            }
            if j == an_start {
                return Err(malformed(format!("bad attribute in <{name}")));
            }
            let aname = tag_src.get(an_start..j).unwrap_or("").to_ascii_lowercase();
            while b.get(j).is_some_and(u8::is_ascii_whitespace) {
                j += 1;
            }
            let mut value = None;
            if b.get(j) == Some(&b'=') {
                j += 1;
                while b.get(j).is_some_and(u8::is_ascii_whitespace) {
                    j += 1;
                }
                match b.get(j).copied() {
                    Some(q) if q == b'"' || q == b'\'' => {
                        let vs = j + 1;
                        let Some(len) = tag_src.get(vs..).and_then(|r| r.find(char::from(q)))
                        else {
                            return Err(malformed(format!("unclosed quote in <{name}")));
                        };
                        let v = tag_src.get(vs..vs + len).unwrap_or("");
                        if v.contains('<') {
                            return Err(malformed(format!("'<' in an attribute of <{name}")));
                        }
                        value = Some(v.to_string());
                        j = vs + len + 1;
                    }
                    Some(_) => {
                        let vs = j;
                        while b.get(j).is_some_and(|c| {
                            !c.is_ascii_whitespace()
                                && !matches!(c, b'>' | b'"' | b'\'' | b'<' | b'=' | b'`')
                        }) {
                            j += 1;
                        }
                        if j == vs || matches!(b.get(j), Some(b'"' | b'\'' | b'<' | b'=' | b'`')) {
                            return Err(malformed(format!("bad unquoted value in <{name}")));
                        }
                        value = Some(tag_src.get(vs..j).unwrap_or("").to_string());
                    }
                    None => return Err(malformed(format!("unterminated <{name}"))),
                }
            }
            if aname.starts_with("on") {
                return Err(LlmRejection::EventHandler { attr: aname });
            }
            if attrs.len() >= MAX_REPLY_ATTRS {
                return Err(malformed(format!("too many attributes on <{name}")));
            }
            attrs.push((aname, value));
        }
        out.push('<');
        out.push_str(&name);
        for (an, av) in &attrs {
            let v = av.as_deref().unwrap_or("");
            if matches!(an.as_str(), "href" | "cite") && !url_ok(v) {
                return Err(LlmRejection::UnsafeUrl { attr: an.clone() });
            }
            if an == "style" && !style_ok(v) {
                return Err(LlmRejection::UnsafeUrl { attr: an.clone() });
            }
            if !attr_allowed(&name, an) {
                continue;
            }
            out.push(' ');
            out.push_str(an);
            if let Some(v) = av {
                out.push_str("=\"");
                escape_canonical(v, &mut out);
                out.push('"');
            }
        }
        out.push('>');
        if !VOID_TAGS.contains(&name.as_str()) {
            if self_closing {
                return Err(malformed(format!("self-closing <{name}/>")));
            }
            if stack.len() >= MAX_REPLY_DEPTH {
                return Err(malformed("nesting too deep"));
            }
            stack.push(name);
        }
        i += j;
    }
    if let Some(open) = stack.last() {
        return Err(malformed(format!("unclosed <{open}>")));
    }
    Ok(out)
}

struct Profile {
    tags: HashMap<String, usize>,
    links: Vec<(String, String)>,
    roots: Vec<String>,
    root_text: bool,
}

fn profile(html: &str) -> Result<Profile, LlmRejection> {
    let dom = Dom::parse(html);
    let mut tags: HashMap<String, usize> = HashMap::new();
    let mut links = Vec::new();
    for ix in dom.all_in_order() {
        let Some(n) = dom.node(ix) else { continue };
        match &n.kind {
            NodeKind::Comment => {
                return Err(LlmRejection::ForbiddenTag {
                    tag: "comment".into(),
                });
            }
            NodeKind::Text { .. } => {}
            NodeKind::Element { name, attrs, .. } => {
                if !ALLOWED_TAGS.contains(&name.as_str()) {
                    return Err(LlmRejection::ForbiddenTag { tag: name.clone() });
                }
                for a in attrs {
                    if a.name.starts_with("on") {
                        return Err(LlmRejection::EventHandler {
                            attr: a.name.clone(),
                        });
                    }
                    let v: String = a
                        .value
                        .chars()
                        .filter(|c| !c.is_whitespace() && !c.is_control())
                        .collect();
                    let v = v.to_lowercase();
                    let url_attr = matches!(
                        a.name.as_str(),
                        "href" | "src" | "xlink:href" | "formaction" | "action" | "srcdoc"
                    );
                    if (url_attr
                        && (v.starts_with("javascript:")
                            || v.starts_with("data:")
                            || v.starts_with("vbscript:")))
                        || a.name == "srcdoc"
                        || (a.name == "style"
                            && (v.contains("expression(")
                                || v.contains("url(")
                                || v.contains("javascript:")))
                    {
                        return Err(LlmRejection::UnsafeUrl {
                            attr: a.name.clone(),
                        });
                    }
                }
                *tags.entry(name.clone()).or_insert(0) += 1;
                if name == "a" {
                    links.push((
                        n.attr("href").unwrap_or("").trim().to_string(),
                        normalize_ws(&dom.text_content(ix)).to_lowercase(),
                    ));
                }
            }
        }
    }
    let mut roots = Vec::new();
    let mut root_text = false;
    for &r in &dom.roots {
        let Some(n) = dom.node(r) else { continue };
        match &n.kind {
            NodeKind::Element { name, .. } => roots.push(name.clone()),
            NodeKind::Text { .. } => {
                if !dom.text_content(r).trim().is_empty() {
                    root_text = true;
                }
            }
            NodeKind::Comment => {}
        }
    }
    Ok(Profile {
        tags,
        links,
        roots,
        root_text,
    })
}

fn shingles(text: &str) -> Vec<String> {
    let words: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    words.windows(3).map(|w| w.join(" ")).collect()
}

/// Validate an LLM reply. Returns the canonical HTML ([`canonicalize_reply`] of the
/// fence-stripped reply: allowlisted tags and attributes, escaped text), never the raw reply.
///
/// Rules: no refusals/explanations; only inline/paragraph tags (no script, iframe, style,
/// form, svg, comments…); no `on*` attributes, no `javascript:`/`data:` URLs; exactly ONE new
/// link, pointing at `target_url`, whose text contains `anchor`; every original link and tag
/// kept. Rephrase: single block of the same root element, length ratio and 3-word shingle
/// coverage within [`LlmLimits`]. Context: one inline sentence, bounded length.
pub fn validate_llm_output(
    kind: LlmKind,
    original_html: &str,
    reply: &str,
    anchor: &str,
    target_url: &str,
    limits: &LlmLimits,
) -> Result<String, LlmRejection> {
    if reply.len() > MAX_REPLY_BYTES {
        return Err(LlmRejection::TooLarge);
    }
    let out = strip_code_fences(reply);
    if out.is_empty() {
        return Err(LlmRejection::Empty);
    }
    if is_refusal(&out) {
        return Err(LlmRejection::Refusal);
    }
    // H-01: never publish the raw reply; everything below works on the canonical form.
    let out = canonicalize_reply(&out)?;
    let got = profile(&out)?;
    let orig = match kind {
        LlmKind::Rephrase => profile(original_html).unwrap_or(Profile {
            tags: HashMap::new(),
            links: Vec::new(),
            roots: Vec::new(),
            root_text: false,
        }),
        LlmKind::Context => Profile {
            tags: HashMap::new(),
            links: Vec::new(),
            roots: Vec::new(),
            root_text: false,
        },
    };

    // Links: original multiset must survive; exactly one new link.
    let mut remaining: Vec<(String, String)> = got.links.clone();
    for ol in &orig.links {
        match remaining.iter().position(|l| l.0 == ol.0) {
            Some(p) => {
                remaining.remove(p);
            }
            None => return Err(LlmRejection::TagLoss { tag: "a".into() }),
        }
    }
    match remaining.len() {
        0 => return Err(LlmRejection::MissingLink),
        1 => {}
        n => return Err(LlmRejection::ExtraLinks { count: n }),
    }
    let (href, text) = remaining.into_iter().next().unwrap_or_default();
    if !urls_match(&href, target_url) {
        return Err(LlmRejection::WrongHref { href });
    }
    let want = normalize_ws(anchor).to_lowercase();
    if want.is_empty() || !text.contains(&want) {
        return Err(LlmRejection::MissingAnchor);
    }

    match kind {
        LlmKind::Rephrase => {
            for (tag, n) in &orig.tags {
                if tag != "a" && got.tags.get(tag).copied().unwrap_or(0) < *n {
                    return Err(LlmRejection::TagLoss { tag: tag.clone() });
                }
            }
            if !orig.roots.is_empty() && (got.roots != orig.roots || got.root_text) {
                return Err(LlmRejection::MultipleBlocks);
            }
            let ot = html_to_text(original_html);
            let nt = html_to_text(&out);
            let (ol, nl) = (ot.chars().count(), nt.chars().count());
            if ol > 0 {
                let ratio = nl as f64 / ol as f64;
                if ratio < limits.min_length_ratio || ratio > limits.max_length_ratio {
                    return Err(LlmRejection::LengthRatio { ratio });
                }
            }
            let os = shingles(&ot);
            if !os.is_empty() {
                let ns: std::collections::HashSet<String> = shingles(&nt).into_iter().collect();
                let kept = os.iter().filter(|s| ns.contains(*s)).count();
                let coverage = kept as f64 / os.len() as f64;
                if coverage < limits.min_shingle_coverage {
                    return Err(LlmRejection::LowCoverage { coverage });
                }
            }
        }
        LlmKind::Context => {
            if let Some(tag) = got
                .tags
                .keys()
                .find(|t| SENTENCE_BLOCK_TAGS.contains(&t.as_str()))
            {
                return Err(LlmRejection::BlockTagInSentence { tag: tag.clone() });
            }
            let text = html_to_text(&out);
            let chars = text.chars().count();
            if chars > limits.max_sentence_chars || text.split_whitespace().count() < 3 {
                return Err(LlmRejection::SentenceLength { chars });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://example.com/target";
    const ORIG: &str =
        "<p>We tested many bike locks over the winter and the results surprised us a lot.</p>";

    fn rephrase(reply: &str) -> Result<String, LlmRejection> {
        validate_llm_output(
            LlmKind::Rephrase,
            ORIG,
            reply,
            "steel bike lock",
            URL,
            &LlmLimits::default(),
        )
    }

    #[test]
    fn accepts_good_rephrase() {
        let r = "```html\n<p>We tested many bike locks over the winter, including a <a href=\"https://example.com/target\">steel bike lock</a>, and the results surprised us a lot.</p>\n```";
        assert!(rephrase(r).is_ok(), "{:?}", rephrase(r));
    }

    #[test]
    fn rejects_bad_rephrases() {
        let ok_body =
            "We tested many bike locks over the winter and the results surprised us a lot.";
        let cases: Vec<(String, &str)> = vec![
            ("I'm sorry, I can't do that.".into(), "refusal"),
            (
                format!(
                    "Here is the paragraph: <p>{ok_body} <a href=\"{URL}\">steel bike lock</a></p>"
                ),
                "refusal",
            ),
            (
                format!(
                    "<p>{ok_body} <a href=\"{URL}\">steel bike lock</a><script>x()</script></p>"
                ),
                "forbidden_tag",
            ),
            (
                format!("<p onclick=\"x()\">{ok_body} <a href=\"{URL}\">steel bike lock</a></p>"),
                "event_handler",
            ),
            (
                format!("<p>{ok_body} <a href=\"javascript:alert(1)\">steel bike lock</a></p>"),
                "unsafe_url",
            ),
            (format!("<p>{ok_body}</p>"), "missing_link"),
            (
                format!("<p>{ok_body} <a href=\"https://evil.com/\">steel bike lock</a></p>"),
                "wrong_href",
            ),
            (
                format!("<p>{ok_body} <a href=\"{URL}\">click here</a></p>"),
                "missing_anchor",
            ),
            (
                format!(
                    "<p>{ok_body} <a href=\"{URL}\">steel bike lock</a> <a href=\"/x\">x</a></p>"
                ),
                "extra_links",
            ),
            (
                format!("<p>Short. <a href=\"{URL}\">steel bike lock</a></p>"),
                "length_ratio",
            ),
            (
                format!(
                    "<p>Completely different words are used everywhere in this new text here now ok <a href=\"{URL}\">steel bike lock</a></p>"
                ),
                "low_coverage",
            ),
            (
                format!("<p>{ok_body}</p><p><a href=\"{URL}\">steel bike lock</a></p>"),
                "multiple_blocks",
            ),
            (
                format!(
                    "<p>{ok_body} <a href=\"{URL}\">steel bike lock</a></p><iframe src=x></iframe>"
                ),
                "forbidden_tag",
            ),
            ("<!-- hi --><p>x</p>".into(), "forbidden_tag"),
            (String::new(), "empty"),
        ];
        for (reply, want) in cases {
            let err = rephrase(&reply).expect_err(&reply);
            let got = serde_json::to_value(&err)
                .ok()
                .and_then(|v| v.get("reason").and_then(|r| r.as_str().map(str::to_string)));
            assert_eq!(got.as_deref(), Some(want), "{reply} -> {err:?}");
        }
    }

    #[test]
    fn keeps_existing_links_and_tags() {
        let orig = "<p>Read <strong>our</strong> <a href=\"/a\">older post</a> about locks and chains for bikes today.</p>";
        let lost = format!(
            "<p>Read our older post about locks and chains for bikes today, see <a href=\"{URL}\">steel bike lock</a>.</p>"
        );
        let e = validate_llm_output(
            LlmKind::Rephrase,
            orig,
            &lost,
            "steel bike lock",
            URL,
            &LlmLimits::default(),
        );
        assert!(matches!(e, Err(LlmRejection::TagLoss { .. })), "{e:?}");
    }

    #[test]
    fn context_sentence_rules() {
        let l = LlmLimits::default();
        let good = format!("For more detail, see our <a href=\"{URL}\">steel bike lock</a> guide.");
        assert!(
            validate_llm_output(LlmKind::Context, "", &good, "steel bike lock", URL, &l).is_ok()
        );
        let block = format!("<p>See our <a href=\"{URL}\">steel bike lock</a> guide.</p>");
        assert!(matches!(
            validate_llm_output(LlmKind::Context, "", &block, "steel bike lock", URL, &l),
            Err(LlmRejection::BlockTagInSentence { .. })
        ));
        // v1 appended a link to anything (even refusals); v2 rejects a reply without the link.
        assert!(matches!(
            validate_llm_output(
                LlmKind::Context,
                "",
                "A nice sentence without a link.",
                "steel bike lock",
                URL,
                &l
            ),
            Err(LlmRejection::MissingLink)
        ));
    }

    #[test]
    fn prompts_escape_injection() {
        let m = rephrase_prompt(
            "<p>x</p>",
            "a\"><script>",
            "https://e.com/?a=1&b=\"2\"",
            "T\"itle",
        );
        assert_eq!(m.len(), 2);
        assert!(!m[1].content.contains("<script>"));
        assert!(m[1].content.contains("&amp;b=&quot;2&quot;"));
        let c = context_prompt("text", "steel bike lock", URL, "Title");
        assert!(
            c[1].content
                .contains(&format!("<a href=\"{URL}\">steel bike lock</a>"))
        );
        assert_eq!(strip_code_fences("```html\n<p>x</p>\n```"), "<p>x</p>");
    }

    /// Security audit H-01 (PoC D:/tmp/sf-sec/poc-llm): the three bypass forms are rejected and
    /// accepted output is canonical.
    #[test]
    fn h01_bypass_forms_are_rejected() {
        let l = LlmLimits::default();
        let base =
            format!("For more detail, see our <a href=\"{URL}\">steel bike lock</a> guide today.");
        let many: String = (0..256).map(|i| format!(" a{i}")).collect();
        let cases = [
            format!("{base} <img src=x onerror=alert(1) "),
            format!("{base} <img src=x onerror=alert(1) alt=\""),
            format!("{base} <span{many} onmouseover=alert(1)>x</span>"),
            format!("{base} <script>alert(1)</script>"),
            format!("{base} <span title=\"x\" onclick=y>z</span>"),
            format!("{base} <span title=\"unclosed>z</span>"),
            format!("{base} <a href=\"/x\">x"),
            format!("{base} 1 < 2"),
            format!("{base} <SPAN>x</span"),
            "For more, see our <a href=\"jav&#x61;script:alert(1)\">steel bike lock</a> guide today.".to_string(),
            format!("For more, see our <a href=\"{URL}\" style=\"background:url(x)\">steel bike lock</a> guide."),
            "For more, see our <a href=\"mailto:x@y\">steel bike lock</a> guide today now.".to_string(),
        ];
        for r in cases {
            let res = validate_llm_output(LlmKind::Context, "", &r, "steel bike lock", URL, &l);
            assert!(res.is_err(), "accepted: {r}");
        }
        // canonical output: attributes outside the allowlist dropped, text escaped
        let ok = format!(
            "For more &amp; less, see our <A HREF='{URL}' data-x=1 tabindex=3>steel bike lock</A> guide > all."
        );
        let got =
            validate_llm_output(LlmKind::Context, "", &ok, "steel bike lock", URL, &l).unwrap();
        assert_eq!(
            got,
            format!(
                "For more &amp; less, see our <a href=\"{URL}\" data-x=\"1\">steel bike lock</a> guide &gt; all."
            )
        );
        assert!(canonicalize_reply("<p>a<br>b<br/>c &nbsp; &#8212; & d</p>").is_ok());
        assert_eq!(
            canonicalize_reply("a & b &bogus c").unwrap(),
            "a &amp; b &amp;bogus c"
        );
        for hostile in [
            "<",
            "<<<<",
            "</",
            "<a",
            "<a href=",
            "<a href='",
            "<p></p></p>",
            "<br></br>",
        ] {
            assert!(canonicalize_reply(hostile).is_err(), "{hostile}");
        }
        let deep = "<span>".repeat(100);
        assert!(canonicalize_reply(&deep).is_err());
    }
}
