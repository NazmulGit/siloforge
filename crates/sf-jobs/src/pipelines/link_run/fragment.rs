//! `open_url`: the live post URL plus a URL Fragment Text Directive (`#:~:text=`) that scrolls
//! to and highlights the link in Chromium browsers (WICG scroll-to-text-fragment spec).
//! Terms are percent-encoded; `-`, `,` and `&` are always encoded because they are directive
//! syntax. Context from the paragraph is always added when it is safe (see [`text_directive`]):
//! `prefix-,text,-suffix` with up to 3 words before and after the first occurrence.

/// Characters left as-is inside a text directive term (unreserved + a few safe sub-delims);
/// everything else (incl. `-`, `,`, `&`, `#`, `%`, space, non-ASCII) is percent-encoded as UTF-8.
fn keep(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'!' | b'*' | b'\'' | b'(' | b')')
}

/// Percent-encode one text directive term.
pub fn encode_term(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if keep(b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric()
}

/// Char-index start positions of whole-word, case-insensitive occurrences of `needle` in `hay`.
fn occurrences(hay: &[char], needle: &[char]) -> Vec<usize> {
    let m = needle.len();
    if m == 0 || hay.len() < m {
        return Vec::new();
    }
    let mut out = Vec::new();
    for s in 0..=(hay.len() - m) {
        let hit = needle
            .iter()
            .enumerate()
            .all(|(k, n)| hay.get(s + k).is_some_and(|h| h == n));
        if !hit {
            continue;
        }
        let before_ok = s == 0 || hay.get(s - 1).is_none_or(|c| !is_word(*c));
        let after_ok = hay.get(s + m).is_none_or(|c| !is_word(*c));
        if before_ok && after_ok {
            out.push(s);
        }
    }
    out
}

/// A context word that survives WordPress rendering unchanged (no quotes or dashes that
/// `wptexturize` rewrites, no punctuation that would sit between the context and the match).
fn clean_word(w: &str) -> bool {
    !w.is_empty()
        && w.chars().all(|c| c.is_alphanumeric() || c == '-')
        && !w.starts_with('-')
        && !w.ends_with('-')
        && !w.contains("--")
}

/// Punctuation that WordPress renders unchanged and that may sit between context and anchor.
fn safe_punct(c: char) -> bool {
    matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']')
}

/// Characters allowed in text glued to the anchor (no whitespace in between): letters, digits,
/// safe punctuation, brackets and typographic quotes. Straight quotes and dashes are left out
/// because `wptexturize` rewrites them in the rendered page.
fn glue_char(c: char) -> bool {
    c.is_alphanumeric()
        || safe_punct(c)
        || matches!(
            c,
            '(' | '[' | '\u{2019}' | '\u{2018}' | '\u{201C}' | '\u{201D}'
        )
}

/// Context directly before (`before`) or after the anchor: the text glued to it (for example
/// "\u{2019}s" or "."), then up to 3 clean words. The text-fragment spec allows only whitespace
/// between a context term and the match, so glued text must be part of the term, and anything
/// WordPress may rewrite (straight quotes, dashes) ends the context.
fn context_words(text: &str, before: bool) -> Option<String> {
    // the glued token next to the anchor
    let (glued, rest) = if before {
        let cut = text
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
        (&text[cut..], &text[..cut])
    } else {
        let cut = text
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map_or(text.len(), |(i, _)| i);
        (&text[..cut], &text[cut..])
    };
    if !glued.chars().all(glue_char) {
        return None;
    }
    let words: Vec<&str> = rest.split_whitespace().collect();
    let mut picked: Vec<&str> = Vec::new();
    let iter: Box<dyn Iterator<Item = &&str>> = if before {
        Box::new(words.iter().rev())
    } else {
        Box::new(words.iter())
    };
    for (n, w) in iter.take(3).enumerate() {
        if clean_word(w) {
            picked.push(w);
            continue;
        }
        // the word next to the anchor may end in punctuation that touches the whitespace
        // before the anchor ("pick: Bike Helmet")
        if before && n == 0 && glued.is_empty() {
            let stem = w.trim_end_matches(safe_punct);
            if stem.len() < w.len() && clean_word(stem) {
                picked.push(w);
                continue;
            }
        }
        // punctuation on the far side of a word is fine ("... the lock, -> the lock"): keep
        // the word and stop there
        let trimmed = if before {
            w.trim_start_matches(|c: char| !c.is_alphanumeric())
        } else {
            w.trim_end_matches(|c: char| !c.is_alphanumeric())
        };
        if trimmed.len() < w.len() && clean_word(trimmed) {
            picked.push(trimmed);
        }
        break;
    }
    if before {
        picked.reverse();
    }
    let words = picked.join(" ");
    let out = match (before, glued.is_empty(), words.is_empty()) {
        (_, true, true) => return None,
        (_, true, false) => words,
        (_, false, true) => glued.to_string(),
        (true, false, false) => format!("{words} {glued}"),
        (false, false, false) => format!("{glued} {words}"),
    };
    Some(out)
}

/// The text directive for `anchor` inside `paragraph` (plain text):
/// `text=[<prefix>-,]<anchor>[,-<suffix>]` with up to 3 words before and after the first
/// occurrence (the one the splicer links), each only when it is safe (see [`context_words`]).
///
/// Context is added even when the anchor is unique in the paragraph: a text fragment matches
/// the first occurrence on the whole page, and anchors usually occur earlier in the title,
/// headings, a table of contents or a product table (E2E 2.2: 4 of 5 Open links landed above
/// the link; with a prefix only, "Giro Caden LED MIPS-,Urban Bike Helmet" still hit the
/// product table, the suffix rules that out).
pub fn text_directive(paragraph: Option<&str>, anchor: &str) -> String {
    let anchor = collapse(anchor);
    if anchor.is_empty() {
        return String::new();
    }
    let mut prefix: Option<String> = None;
    let mut suffix: Option<String> = None;
    if let Some(p) = paragraph {
        let para: Vec<char> = collapse(p).chars().flat_map(char::to_lowercase).collect();
        let original: Vec<char> = collapse(p).chars().collect();
        let needle: Vec<char> = anchor.chars().flat_map(char::to_lowercase).collect();
        // lowercase may change lengths for some scripts; only use positions when they line up
        let occ = if para.len() == original.len() {
            occurrences(&para, &needle)
        } else {
            Vec::new()
        };
        if let Some(&first) = occ.first() {
            let before: String = original.iter().take(first).collect();
            let after: String = original.iter().skip(first + needle.len()).collect();
            prefix = context_words(&before, true);
            suffix = context_words(&after, false);
        }
    }
    let mut d = String::from("text=");
    if let Some(p) = prefix {
        d.push_str(&encode_term(&p));
        d.push_str("-,");
    }
    d.push_str(&encode_term(&anchor));
    if let Some(s) = suffix {
        d.push_str(",-");
        d.push_str(&encode_term(&s));
    }
    d
}

/// `<post url>#:~:text=...` (any existing fragment of `url` is dropped). Empty anchor = plain URL.
pub fn open_url(url: &str, paragraph: Option<&str>, anchor: &str) -> String {
    let base = url.split('#').next().unwrap_or("");
    let d = text_directive(paragraph, anchor);
    if d.is_empty() || base.is_empty() {
        return base.to_string();
    }
    format!("{base}#:~:{d}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_syntax_characters() {
        assert_eq!(encode_term("bike locks"), "bike%20locks");
        assert_eq!(encode_term("u-lock, 2&3"), "u%2Dlock%2C%202%263");
        assert_eq!(encode_term("café"), "caf%C3%A9");
        assert_eq!(encode_term("50%#"), "50%25%23");
    }

    #[test]
    fn plain_and_prefix_forms() {
        // unique in the paragraph: still gets context (the page may repeat the anchor)
        assert_eq!(
            open_url(
                "https://a.com/p/#old",
                Some("Use bike locks daily."),
                "bike locks"
            ),
            "https://a.com/p/#:~:text=Use-,bike%20locks,-daily"
        );
        let para = "Good bike locks matter. Cheap bike locks fail.";
        assert_eq!(
            open_url("https://a.com/p/", Some(para), "Bike  Locks"),
            "https://a.com/p/#:~:text=Good-,Bike%20Locks,-matter"
        );
        let para = "We tested many u-lock models. The best u-lock, the Kryptonite u-lock wins.";
        assert_eq!(
            text_directive(Some(para), "u-lock"),
            "text=We%20tested%20many-,u%2Dlock,-models"
        );
        // occurrence at the very start: no words before, suffix form
        assert_eq!(
            text_directive(Some("locks and locks"), "locks"),
            "text=locks,-and%20locks"
        );
        // whole words only ("lockstep" is not "locks")
        assert_eq!(
            text_directive(Some("lockstep locks"), "locks"),
            "text=lockstep-,locks"
        );
        assert_eq!(text_directive(Some("locks"), "locks"), "text=locks");
        assert_eq!(text_directive(None, "locks"), "text=locks");
        assert_eq!(open_url("https://a.com/", None, "  "), "https://a.com/");
        assert_eq!(open_url("", None, "x"), "");
    }

    #[test]
    fn context_skips_texturized_or_punctuated_words() {
        // the word right before ends in ':' -> kept in the prefix
        assert_eq!(
            text_directive(Some("Best pick: Bike Helmet for kids"), "Bike Helmet"),
            "text=Best%20pick%3A-,Bike%20Helmet,-for%20kids"
        );
        // punctuation glued to the anchor starts the suffix (was: no suffix, wrong spot)
        assert_eq!(
            text_directive(
                Some("Try the Ultra Bright USB Light Set. It lasts long"),
                "USB Light Set"
            ),
            "text=the%20Ultra%20Bright-,USB%20Light%20Set,-.%20It%20lasts%20long"
        );
        assert_eq!(
            text_directive(Some("We like the USB Light Set."), "USB Light Set"),
            "text=We%20like%20the-,USB%20Light%20Set,-."
        );
        // quotes are rewritten by wptexturize: stop at them
        assert_eq!(
            text_directive(Some("It's the \"best\" safe Bike Helmet."), "Bike Helmet"),
            "text=safe-,Bike%20Helmet,-."
        );
        // anchor followed by a comma: the comma (encoded) starts the suffix
        assert_eq!(
            text_directive(Some("Bike Helmet, and more"), "Bike Helmet"),
            "text=Bike%20Helmet,-%2C%20and%20more"
        );
        // a typographic possessive glued to the anchor is part of the suffix (E2E 2.2:
        // "... Light Set’s got you covered" matched an earlier product table)
        assert_eq!(
            text_directive(
                Some("The Ultra Bright Light Set’s got you covered."),
                "Light Set"
            ),
            "text=The%20Ultra%20Bright-,Light%20Set,-%E2%80%99s%20got%20you%20covered"
        );
        // straight quotes / dashes are rewritten by wptexturize: no suffix
        assert_eq!(
            text_directive(Some("Bike Helmet's strap"), "Bike Helmet"),
            "text=Bike%20Helmet"
        );
        assert_eq!(
            text_directive(Some("Bike Helmet--strap"), "Bike Helmet"),
            "text=Bike%20Helmet"
        );
        // an opening bracket glued before the anchor is part of the prefix
        assert_eq!(
            text_directive(Some("the best (Bike Helmet) here"), "Bike Helmet"),
            "text=the%20best%20(-,Bike%20Helmet,-)%20here"
        );
        // at most 3 words of prefix
        assert_eq!(
            text_directive(Some("one two three four Bike Helmet"), "bike helmet"),
            "text=two%20three%20four-,bike%20helmet"
        );
    }
}
