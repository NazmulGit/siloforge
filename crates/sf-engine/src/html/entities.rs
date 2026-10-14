//! HTML character-reference decoding with source offsets.
//!
//! Every decoded char carries the byte range of the source it came from, so callers can map a
//! match in decoded text back to an exact byte splice in the original string.

use markup5ever::data::NAMED_ENTITIES;

/// Longest entity name we try to resolve (the longest HTML5 name is 31 chars + `;`).
const MAX_ENTITY_NAME: usize = 40;

/// One decoded char and the source byte range `[start, end)` that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecodedChar {
    pub ch: char,
    pub start: usize,
    pub end: usize,
}

/// Decode `src[start..end]` (clamped to char boundaries) into chars with offsets.
/// When `decode` is false (raw text such as `<script>`), chars are passed through verbatim.
pub(crate) fn decode_range(
    src: &str,
    start: usize,
    end: usize,
    decode: bool,
    out: &mut Vec<DecodedChar>,
) {
    let end = end.min(src.len());
    if start >= end {
        return;
    }
    let Some(slice) = src.get(start..end) else {
        return;
    };
    let bytes = slice.as_bytes();
    let mut i = 0usize;
    while i < slice.len() {
        if decode
            && bytes.get(i) == Some(&b'&')
            && let Some((chars, used)) = decode_entity_at(slice, i)
        {
            for ch in chars.into_iter().flatten() {
                out.push(DecodedChar {
                    ch,
                    start: start + i,
                    end: start + i + used,
                });
            }
            i += used;
            continue;
        }
        let Some(ch) = slice.get(i..).and_then(|s| s.chars().next()) else {
            break;
        };
        let len = ch.len_utf8();
        // HTML parsers replace NUL with U+FFFD in text.
        let ch = if ch == '\0' { '\u{FFFD}' } else { ch };
        out.push(DecodedChar {
            ch,
            start: start + i,
            end: start + i + len,
        });
        i += len;
    }
}

/// Decode a whole string (attribute values, short texts).
pub(crate) fn decode_str(s: &str) -> String {
    if !s.contains('&') {
        return s.replace('\0', "\u{FFFD}");
    }
    let mut buf = Vec::new();
    decode_range(s, 0, s.len(), true, &mut buf);
    buf.into_iter().map(|d| d.ch).collect()
}

/// Try to decode a character reference starting at `s[i] == '&'`.
/// Returns up to two chars and the number of source bytes consumed.
fn decode_entity_at(s: &str, i: usize) -> Option<([Option<char>; 2], usize)> {
    let rest = s.get(i + 1..)?;
    let rb = rest.as_bytes();
    if rb.first() == Some(&b'#') {
        let (radix, digits_start) = match rb.get(1) {
            Some(b'x') | Some(b'X') => (16, 2),
            _ => (10, 1),
        };
        let mut j = digits_start;
        let mut value: u32 = 0;
        let mut any = false;
        while let Some(&b) = rb.get(j) {
            let d = match (radix, b) {
                (_, b'0'..=b'9') => u32::from(b - b'0'),
                (16, b'a'..=b'f') => u32::from(b - b'a' + 10),
                (16, b'A'..=b'F') => u32::from(b - b'A' + 10),
                _ => break,
            };
            value = value.saturating_mul(radix).saturating_add(d);
            any = true;
            j += 1;
        }
        if !any {
            return None;
        }
        if rb.get(j) == Some(&b';') {
            j += 1;
        }
        let ch = numeric_char(value);
        return Some(([Some(ch), None], 1 + j));
    }
    // Named reference: longest alphanumeric run, prefer `name;`, else legacy `name` prefixes.
    let mut j = 0usize;
    while j < rb.len() && j < MAX_ENTITY_NAME && rb.get(j).is_some_and(u8::is_ascii_alphanumeric) {
        j += 1;
    }
    if j == 0 {
        return None;
    }
    if rb.get(j) == Some(&b';') {
        let key = rest.get(..=j)?;
        if let Some(&(a, b)) = NAMED_ENTITIES.get(key)
            && a != 0
        {
            return Some((pair(a, b), 1 + j + 1));
        }
    }
    // Legacy entities without `;` (e.g. `&amp`, `&copy`): longest matching prefix.
    let mut k = j;
    while k > 0 {
        let key = rest.get(..k)?;
        if let Some(&(a, b)) = NAMED_ENTITIES.get(key)
            && a != 0
        {
            return Some((pair(a, b), 1 + k));
        }
        k -= 1;
    }
    None
}

fn pair(a: u32, b: u32) -> [Option<char>; 2] {
    [
        char::from_u32(a),
        if b == 0 { None } else { char::from_u32(b) },
    ]
}

fn numeric_char(value: u32) -> char {
    match value {
        0 => '\u{FFFD}',
        0x80..=0x9F => {
            let idx = (value - 0x80) as usize;
            markup5ever::data::C1_REPLACEMENTS
                .get(idx)
                .copied()
                .flatten()
                .unwrap_or_else(|| char::from_u32(value).unwrap_or('\u{FFFD}'))
        }
        0xD800..=0xDFFF => '\u{FFFD}',
        v if v > 0x10FFFF => '\u{FFFD}',
        v => char::from_u32(v).unwrap_or('\u{FFFD}'),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_named_numeric_and_legacy() {
        assert_eq!(decode_str("a &amp; b"), "a & b");
        assert_eq!(decode_str("it&#8217;s"), "it\u{2019}s");
        assert_eq!(decode_str("&#x2014;"), "\u{2014}");
        assert_eq!(decode_str("&nbsp;x"), "\u{a0}x");
        assert_eq!(decode_str("&copy 2026"), "\u{a9} 2026");
        assert_eq!(decode_str("&bogus; &"), "&bogus; &");
        assert_eq!(
            decode_str("&#0;&#x110000;&#xD800;"),
            "\u{FFFD}\u{FFFD}\u{FFFD}"
        );
        assert_eq!(decode_str("&#150;"), "\u{2013}");
        assert_eq!(decode_str("&#99999999999999999999999;"), "\u{FFFD}");
    }

    #[test]
    fn offsets_cover_entity_source() {
        let mut out = Vec::new();
        decode_range("x&amp;y", 0, 7, true, &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!((out[1].ch, out[1].start, out[1].end), ('&', 1, 6));
        assert_eq!(out[2].start, 6);
    }
}
