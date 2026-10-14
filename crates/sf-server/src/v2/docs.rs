//! Built-in user guide (contract v2.1 §4): `docs/guide/*.md` embedded in the binary.
//!
//! Each page may start with a simple front matter block (`---` / `key: value` lines / `---`)
//! giving `title`, `section` and `order`; without it the title is the first `# ` heading (or the
//! slug), the section "Guide" and the order 1000.

use axum::Json;
use axum::extract::{Path, State};
use rust_embed::RustEmbed;
use serde_json::{Value, json};

use crate::auth::User;
use crate::{ApiError, ApiResult, AppState};

#[derive(RustEmbed)]
#[folder = "../../docs/guide/"]
#[include = "*.md"]
#[allow_missing = true]
struct Guide;

/// One parsed page.
#[derive(Debug, Clone, PartialEq)]
pub struct Doc {
    pub slug: String,
    pub title: String,
    pub section: String,
    pub order: i64,
    pub markdown: String,
}

const MAX_SLUG: usize = 100;

fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SLUG
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Splits `---` front matter (simple `key: value` lines, optional quotes) from the body.
pub fn parse(slug: &str, text: &str) -> Doc {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut title = None;
    let mut section = None;
    let mut order = None;
    let mut body = text;
    let first = text.lines().next().unwrap_or("").trim_end();
    if first == "---" {
        let rest = text.get(first.len()..).unwrap_or("");
        let rest = rest
            .strip_prefix("\r\n")
            .or_else(|| rest.strip_prefix('\n'))
            .unwrap_or(rest);
        let mut consumed = 0usize;
        let mut closed = false;
        for line in rest.split_inclusive('\n') {
            consumed += line.len();
            let l = line.trim();
            if l == "---" {
                closed = true;
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                let v = v.trim();
                let v = v
                    .strip_prefix('"')
                    .and_then(|x| x.strip_suffix('"'))
                    .or_else(|| v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
                    .unwrap_or(v)
                    .to_string();
                match k.trim() {
                    "title" => title = Some(v),
                    "section" => section = Some(v),
                    "order" => order = v.parse::<i64>().ok(),
                    _ => {}
                }
            }
        }
        if closed {
            body = rest.get(consumed..).unwrap_or("");
        } else {
            // no closing line: treat everything as body, ignore what was read
            title = None;
            section = None;
            order = None;
        }
    }
    let body = body.trim_start_matches(['\r', '\n']);
    let title = title.filter(|t| !t.is_empty()).unwrap_or_else(|| {
        body.lines()
            .find_map(|l| l.strip_prefix("# "))
            .map(|t| t.trim().to_string())
            .unwrap_or_else(|| slug.to_string())
    });
    Doc {
        slug: slug.to_string(),
        title,
        section: section
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Guide".into()),
        order: order.unwrap_or(1000),
        markdown: body.to_string(),
    }
}

/// Every embedded page, sorted by section order (first page's order), order, title.
pub fn all() -> Vec<Doc> {
    let mut docs: Vec<Doc> = Guide::iter()
        .filter_map(|name| {
            let slug = name.strip_suffix(".md")?.to_string();
            if !valid_slug(&slug) {
                return None;
            }
            let f = Guide::get(&name)?;
            let text = String::from_utf8_lossy(&f.data).into_owned();
            Some(parse(&slug, &text))
        })
        .collect();
    docs.sort_by(|a, b| {
        (a.section.as_str(), a.order, a.title.as_str()).cmp(&(
            b.section.as_str(),
            b.order,
            b.title.as_str(),
        ))
    });
    docs
}

/// `GET /docs` -> `{docs: [{slug, title, section, order}]}`.
pub async fn list(State(_st): State<AppState>, _u: User) -> ApiResult<Json<Value>> {
    let docs: Vec<Value> = all()
        .into_iter()
        .map(|d| json!({"slug": d.slug, "title": d.title, "section": d.section, "order": d.order}))
        .collect();
    Ok(Json(json!({ "docs": docs })))
}

/// `GET /docs/{slug}` -> `{slug, title, section, order, markdown}`.
pub async fn get_one(
    State(_st): State<AppState>,
    _u: User,
    Path(slug): Path<String>,
) -> ApiResult<Json<Value>> {
    let slug = slug.trim().trim_end_matches(".md").to_ascii_lowercase();
    if !valid_slug(&slug) {
        return Err(ApiError::not_found("doc not found"));
    }
    let d = all()
        .into_iter()
        .find(|d| d.slug == slug)
        .ok_or_else(|| ApiError::not_found("doc not found"))?;
    Ok(Json(json!({
        "slug": d.slug, "title": d.title, "section": d.section, "order": d.order, "markdown": d.markdown,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_parsing() {
        let d = parse(
            "x",
            "---\ntitle: \"Getting started\"\nsection: Basics\norder: 2\nextra: y\n---\n\n# Heading\nbody\n",
        );
        assert_eq!(d.title, "Getting started");
        assert_eq!(d.section, "Basics");
        assert_eq!(d.order, 2);
        assert_eq!(d.markdown, "# Heading\nbody\n");
        let d = parse("y", "# Only heading\ntext");
        assert_eq!(
            (d.title.as_str(), d.section.as_str(), d.order),
            ("Only heading", "Guide", 1000)
        );
        let d = parse("z", "---\ntitle: never closed\n# h");
        assert_eq!(d.title, "h", "falls back to the first heading");
        assert!(d.markdown.starts_with("---"));
        let d = parse("w", "\u{feff}---\r\ntitle: CRLF\r\norder: x\r\n---\r\nbody");
        assert_eq!(d.title, "CRLF");
        assert_eq!(d.order, 1000);
        assert_eq!(d.markdown, "body");
        // hostile input never panics
        for s in ["", "---", "---\n---", "---\n:\n---\n", "\u{0}\u{1}---\n"] {
            let _ = parse("h", s);
        }
    }

    #[test]
    fn embedded_guide_pages_are_listed() {
        let docs = all();
        // docs/guide may be missing in a stripped checkout (allow_missing); when present every
        // page has a slug and a title.
        for d in &docs {
            assert!(valid_slug(&d.slug));
            assert!(!d.title.is_empty());
        }
        if std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/guide")).is_dir() {
            assert!(!docs.is_empty());
        }
        assert!(!valid_slug("../etc"));
        assert!(!valid_slug(""));
    }
}
