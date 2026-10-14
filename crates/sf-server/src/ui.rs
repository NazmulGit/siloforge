//! Embedded React UI (`ui/dist`). `/assets/*` get a one-year immutable cache, `index.html` no-cache, and
//! any other GET falls back to `index.html` (SPA routing). If the UI was not built, a small notice page
//! is served instead.

use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../ui/dist/"]
#[allow_missing = true]
struct Assets;

const NOT_BUILT: &str = "<!doctype html><meta charset=utf-8><title>SiloForge</title>\
<body style=\"font-family:system-ui;padding:2rem\"><h1>SiloForge API is running</h1>\
<p>The web UI was not embedded in this build. Run <code>cargo xtask ui</code> and rebuild, or use the API at <code>/api/v1</code>.</p>";

pub async fn serve(method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty() {
        if let Some(file) = Assets::get(path) {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let cache = if path.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            return (
                [
                    (header::CONTENT_TYPE, header_value(mime.as_ref())),
                    (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
                ],
                file.data.into_owned(),
            )
                .into_response();
        }
        if path.starts_with("assets/") {
            return StatusCode::NOT_FOUND.into_response();
        }
    }
    match Assets::get("index.html") {
        Some(index) => (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/html; charset=utf-8"),
                ),
                (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            ],
            index.data.into_owned(),
        )
            .into_response(),
        None => Html(NOT_BUILT).into_response(),
    }
}

/// L-12: anti-framing / sniffing / referrer headers on every response (UI and API). The CSP is
/// limited to directives that cannot break the bundled UI (`frame-ancestors`, `object-src`,
/// `base-uri`); a handler that already set one of these headers keeps its own value.
pub async fn security_headers(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    for (name, value) in [
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (
            header::CONTENT_SECURITY_POLICY,
            "frame-ancestors 'none'; object-src 'none'; base-uri 'self'",
        ),
    ] {
        if !h.contains_key(&name) {
            h.insert(name, HeaderValue::from_static(value));
        }
    }
    res
}

fn header_value(s: &str) -> HeaderValue {
    HeaderValue::from_str(s)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}
