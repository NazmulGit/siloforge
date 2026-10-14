//! Tool output shaping: secret stripping (R2/R14: never hand credentials to the LLM) and size limits.

use serde_json::{Map, Value};

/// Field names that are removed from every API response before it reaches the model
/// (compared case-insensitively). Defensive: the v2 API should not return them anyway.
const SECRET_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "api_key_enc",
    "api_key_hash",
    "wp_app_password",
    "app_password",
    "password",
    "password_hash",
    "secret",
    "client_secret",
    "credentials",
    "credentials_json",
    "private_key",
    "token",
    "token_hash",
    "access_token",
    "refresh_token",
    "jwt",
    "authorization",
];

const MAX_DEPTH: usize = 64;

fn is_secret_key(k: &str) -> bool {
    SECRET_KEYS.iter().any(|s| s.eq_ignore_ascii_case(k))
}

/// True for strings that look like SiloForge secrets even under an innocent key:
/// `sfk_<hex>` API tokens and `sf_<32 hex>` site api keys.
fn looks_like_secret(s: &str) -> bool {
    let hexish =
        |rest: &str, min: usize| rest.len() >= min && rest.bytes().all(|b| b.is_ascii_hexdigit());
    if let Some(rest) = s.strip_prefix("sfk_") {
        return hexish(rest, 16);
    }
    if let Some(rest) = s.strip_prefix("sf_") {
        return rest.len() == 32 && hexish(rest, 32);
    }
    false
}

/// Recursively removes secret fields and redacts secret-looking strings. Depth is bounded.
pub fn strip_secrets(v: &mut Value) {
    strip_inner(v, 0);
}

fn strip_inner(v: &mut Value, depth: usize) {
    if depth > MAX_DEPTH {
        *v = Value::String("[omitted: nesting too deep]".into());
        return;
    }
    match v {
        Value::Object(map) => {
            map.retain(|k, _| !is_secret_key(k));
            for (_, child) in map.iter_mut() {
                strip_inner(child, depth + 1);
            }
        }
        Value::Array(items) => {
            for child in items.iter_mut() {
                strip_inner(child, depth + 1);
            }
        }
        Value::String(s) if looks_like_secret(s) => *s = "[redacted]".into(),
        _ => {}
    }
}

/// Truncates `s` to at most `max` bytes on a char boundary.
pub fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or("")
}

/// Plain text (CSV etc.): cut at a line boundary under `max` chars and say so.
pub fn fit_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let head = truncate_str(text, max);
    let cut = head
        .rfind('\n')
        .map_or(head, |i| head.get(..i).unwrap_or(head));
    let shown = cut.lines().count();
    let total = text.lines().count();
    format!(
        "{cut}\n[truncated: showing {shown} of {total} lines ({} of {} chars). Use the UI or the REST API for the full file.]",
        cut.len(),
        text.len()
    )
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// Names of list fields that may be shortened to fit the output budget.
const LIST_FIELDS: &[&str] = &[
    "edges",
    "rows",
    "posts",
    "items",
    "jobs",
    "lines",
    "logs",
    "notifications",
];

/// Finds the array to shorten: the value itself, or the first known list field of an object.
fn list_len(v: &Value) -> Option<(Option<&'static str>, usize)> {
    match v {
        Value::Array(a) => Some((None, a.len())),
        Value::Object(m) => LIST_FIELDS.iter().find_map(|k| {
            m.get(*k)
                .and_then(Value::as_array)
                .map(|a| (Some(*k), a.len()))
        }),
        _ => None,
    }
}

fn take_items(v: &Value, field: Option<&str>, n: usize) -> Value {
    match (v, field) {
        (Value::Array(a), None) => Value::Array(a.iter().take(n).cloned().collect()),
        (Value::Object(m), Some(f)) => {
            let mut out = Map::new();
            for (k, val) in m {
                if k == f {
                    let items = val
                        .as_array()
                        .map(|a| a.iter().take(n).cloned().collect())
                        .unwrap_or_default();
                    out.insert(k.clone(), Value::Array(items));
                } else {
                    out.insert(k.clone(), val.clone());
                }
            }
            Value::Object(out)
        }
        _ => v.clone(),
    }
}

/// Result of fitting a JSON value into the output budget.
pub struct Fitted {
    pub text: String,
    /// `Some(n)` when only the first `n` list items were kept.
    pub shown_items: Option<usize>,
}

/// Pretty JSON under `max` chars. Lists are shortened item-wise (keeping the JSON valid) with a
/// note; anything else is cut with a note.
pub fn fit_json(v: &Value, max: usize) -> Fitted {
    let full = pretty(v);
    if full.len() <= max {
        return Fitted {
            text: full,
            shown_items: None,
        };
    }
    if let Some((field, len)) = list_len(v) {
        // Binary search the largest item count whose rendering (plus note) fits.
        let budget = max.saturating_sub(300);
        let (mut lo, mut hi) = (0usize, len);
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            if pretty(&take_items(v, field, mid)).len() <= budget {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let text = pretty(&take_items(v, field, lo));
        if text.len() <= budget {
            let what = field.unwrap_or("items");
            return Fitted {
                text: format!(
                    "{text}\n[truncated: showing {lo} of {len} {what} to stay under {max} chars. Page with limit/offset or narrow the filters.]"
                ),
                shown_items: Some(lo),
            };
        }
    }
    Fitted {
        text: format!(
            "{}\n[truncated: output was {} chars, limit {max}. Narrow the query.]",
            truncate_str(&full, max),
            full.len()
        ),
        shown_items: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_nested_secrets() {
        let mut v = json!([{
            "id": "s1", "domain": "a.com", "api_key": "sf_0123456789abcdef0123456789abcdef",
            "settings": {"wp_user": "bob", "wp_app_password": "xxxx", "gsc": {"credentials": "***"}},
            "note": "sfk_0123456789abcdef0123", "API_KEY": "x"
        }]);
        strip_secrets(&mut v);
        let s = v.to_string();
        assert!(!s.contains("api_key") && !s.contains("API_KEY") && !s.contains("wp_app_password"));
        assert!(!s.contains("credentials") && !s.contains("sfk_0123"));
        assert!(s.contains("wp_user") && s.contains("a.com"));
    }

    #[test]
    fn fit_shortens_lists_and_keeps_json_valid() {
        let edges: Vec<Value> = (0..500)
            .map(|i| json!({"id": i, "anchor": "x".repeat(50)}))
            .collect();
        let v = json!({"edges": edges, "total": 500});
        let f = fit_json(&v, 5_000);
        assert!(f.text.len() < 5_000);
        let n = f.shown_items.unwrap_or(0);
        assert!(n > 0 && n < 500);
        let json_part = f.text.split("\n[truncated").next().unwrap_or("");
        let parsed: Value = serde_json::from_str(json_part).unwrap_or(Value::Null);
        assert_eq!(parsed["edges"].as_array().map(Vec::len), Some(n));
    }

    #[test]
    fn fit_text_and_truncate_respect_boundaries() {
        assert_eq!(truncate_str("héllo", 2), "h");
        let csv = (0..1000)
            .map(|i| format!("row{i},é"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = fit_text(&csv, 200);
        assert!(out.contains("[truncated"));
        assert!(out.len() < 400);
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let mut v = json!(1);
        for _ in 0..200 {
            v = json!([v]);
        }
        strip_secrets(&mut v);
        assert!(v.to_string().contains("too deep"));
    }
}
