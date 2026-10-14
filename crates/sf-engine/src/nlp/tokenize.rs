//! Word tokenizer with byte offsets, plus stemming helpers.

use std::sync::OnceLock;

use rust_stemmers::{Algorithm, Stemmer};

/// Token class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Letters (optionally with digits, internal `'`/`’`/`-`).
    Word,
    /// Digits only (optionally with internal `.`/`,`).
    Number,
    /// Any other non-whitespace char.
    Punct,
}

/// A token borrowed from the input, with its byte span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    pub text: &'a str,
    pub start: usize,
    pub end: usize,
    pub kind: TokenKind,
}

/// Upper bound on tokens produced for one input.
pub const MAX_TOKENS: usize = 100_000;

fn is_joiner(c: char) -> bool {
    matches!(c, '\'' | '\u{2019}' | '-')
}

/// Split text into word / number / punctuation tokens (whitespace dropped). Bounded by [`MAX_TOKENS`].
pub fn tokenize(text: &str) -> Vec<Token<'_>> {
    let text = super::bounded(text);
    let mut out = Vec::new();
    let mut it = text.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if out.len() >= MAX_TOKENS {
            break;
        }
        if c.is_whitespace() || crate::html::is_js_whitespace(c) {
            continue;
        }
        if c.is_alphanumeric() {
            let mut end = i + c.len_utf8();
            let mut digits_only = c.is_ascii_digit();
            while let Some(&(j, d)) = it.peek() {
                if d.is_alphanumeric() {
                    digits_only &= d.is_ascii_digit();
                    end = j + d.len_utf8();
                    it.next();
                    continue;
                }
                let joins = is_joiner(d) || (digits_only && matches!(d, '.' | ','));
                if joins {
                    // Only join when followed by an alphanumeric char.
                    let next = text.get(j + d.len_utf8()..).and_then(|s| s.chars().next());
                    if next.is_some_and(char::is_alphanumeric) {
                        it.next();
                        end = j + d.len_utf8();
                        continue;
                    }
                }
                break;
            }
            let tok = text.get(i..end).unwrap_or("");
            out.push(Token {
                text: tok,
                start: i,
                end,
                kind: if digits_only {
                    TokenKind::Number
                } else {
                    TokenKind::Word
                },
            });
        } else {
            out.push(Token {
                text: text.get(i..i + c.len_utf8()).unwrap_or(""),
                start: i,
                end: i + c.len_utf8(),
                kind: TokenKind::Punct,
            });
        }
    }
    out
}

/// Lowercase + curly apostrophe → straight.
pub fn norm_word(w: &str) -> String {
    w.chars()
        .flat_map(char::to_lowercase)
        .map(|c| if c == '\u{2019}' { '\'' } else { c })
        .collect()
}

fn stemmer() -> &'static Stemmer {
    static S: OnceLock<Stemmer> = OnceLock::new();
    S.get_or_init(|| Stemmer::create(Algorithm::English))
}

/// English (Porter2) stem of a lowercase word; possessive `'s` stripped first.
pub fn stem(word_lower: &str) -> String {
    let w = word_lower.strip_suffix("'s").unwrap_or(word_lower);
    let w = w.trim_matches('\'');
    if w.len() > 64 {
        return w.to_string();
    }
    stemmer().stem(w).into_owned()
}

/// v1 `tokenSet` semantics, Unicode-aware: lowercase, split on anything that is not
/// alphanumeric or an apostrophe, keep tokens longer than 2 chars, dedupe (first occurrence order).
pub fn content_tokens(text: &str) -> Vec<String> {
    let text = super::bounded(text);
    let lower = norm_word(text);
    let mut out: Vec<String> = Vec::new();
    for t in lower.split(|c: char| !(c.is_alphanumeric() || c == '\'')) {
        if t.chars().count() > 2 && !out.iter().any(|o| o == t) {
            out.push(t.to_string());
            if out.len() >= 4096 {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_words_numbers_punct() {
        let t = tokenize("The e-bike's 2,500 W motor, isn't it?");
        let words: Vec<(&str, TokenKind)> = t.iter().map(|t| (t.text, t.kind)).collect();
        assert_eq!(
            words,
            vec![
                ("The", TokenKind::Word),
                ("e-bike's", TokenKind::Word),
                ("2,500", TokenKind::Number),
                ("W", TokenKind::Word),
                ("motor", TokenKind::Word),
                (",", TokenKind::Punct),
                ("isn't", TokenKind::Word),
                ("it", TokenKind::Word),
                ("?", TokenKind::Punct),
            ]
        );
        assert_eq!(&"The e-bike's"[t[1].start..t[1].end], "e-bike's");
    }

    #[test]
    fn stems() {
        assert_eq!(stem("locks"), "lock");
        assert_eq!(stem("reviewers"), "review");
        assert_eq!(stem("bike's"), "bike");
    }

    #[test]
    fn content_tokens_like_v1() {
        assert_eq!(
            content_tokens("Best Bike-Trunk bags, an ox"),
            vec!["best", "bike", "trunk", "bags"]
        );
    }
}
