//! §9 unified event log, HTTP side: the request middleware and a `record` helper for handlers.
//!
//! The binary registers an [`EventLog`] in `AppState.services`; without one everything here is a
//! no-op. Logging never fails or slows a request: events go to a bounded queue.
//!
//! Per `/api/*` request (never `/health`, `/mcp` or the static UI):
//! * `source` = `mcp` when the request carries `X-SiloForge-Via: mcp` (the tool name from
//!   `X-SiloForge-Tool` goes into `detail.tool`), else `api` for `sfk_` API tokens and site tokens,
//!   else `ui` for browser sessions. Anonymous calls are `ui` when a browser sent them
//!   (`Sec-Fetch-*` headers), else `api`.
//! * `actor` = `user:<id>` | `token:<name>` | `mcp:<token name>` | `site:<id>` | `anonymous`.
//! * `action` = `"<METHOD> <route template>"` (e.g. `PATCH /api/v1/sites/{id}`), never raw ids.
//! * `status` = `denied` (401/403), `error` (any other >= 400), else `ok`.
//! * Recorded: every non-GET, every MCP request, every error; successful GETs only when
//!   `monitor.log_reads` is on (read live per request).

use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderMap, Method};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::{Map, Value, json};
pub use sf_ops::eventlog::{Event, EventLog, source, status};

use crate::AppState;
use crate::auth::Principal;

/// Header set by sf-mcp on every API call it makes on behalf of a tool.
pub const VIA_HEADER: &str = "x-siloforge-via";
/// Tool name header set by sf-mcp.
pub const TOOL_HEADER: &str = "x-siloforge-tool";

/// The registered event log (disabled handle when none is registered).
pub fn log(st: &AppState) -> EventLog {
    st.services
        .get::<EventLog>()
        .map(|l| (*l).clone())
        .unwrap_or_default()
}

/// Records one event (no-op without a registered event log).
pub fn record(st: &AppState, ev: Event) {
    if let Some(l) = st.services.get::<EventLog>() {
        l.record(ev);
    }
}

/// `source=system` event (settings changed, restart, backup, ...).
pub fn record_system(st: &AppState, action: &str, ok: bool, detail: Value) {
    record(st, Event::system(action).ok(ok).detail(detail));
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::trim)
}

fn bearer(h: &HeaderMap) -> Option<String> {
    let v = header(h, "authorization")?;
    let (scheme, value) = v.split_once(' ')?;
    let value = value.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !value.is_empty() && value.len() <= 4096)
        .then(|| value.to_string())
}

fn clean(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect::<String>()
}

/// Path parameters by name, from the matched template and the real path (segment-wise).
pub fn path_params(template: &str, path: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (t, p) in template.split('/').zip(path.split('/')) {
        if let Some(name) = t.strip_prefix('{').and_then(|n| n.strip_suffix('}'))
            && !name.starts_with('*')
            && !p.is_empty()
        {
            out.push((name.to_string(), clean(p, 200)));
        }
        if out.len() >= 8 {
            break;
        }
    }
    out
}

/// `(target, site_id)` from the path params: `site_id` = `{site_id}` or the `{id}` right after
/// `/sites/`; `target` = the last param.
fn target_and_site(
    template: &str,
    params: &[(String, String)],
) -> (Option<String>, Option<String>) {
    let mut site = params
        .iter()
        .find(|(k, _)| k == "site_id" || k == "siteId")
        .map(|(_, v)| v.clone());
    if site.is_none() {
        let segs: Vec<&str> = template.split('/').collect();
        for w in segs.windows(2) {
            if let [a, b] = w
                && *a == "sites"
                && b.starts_with('{')
            {
                let name = b.trim_start_matches('{').trim_end_matches('}');
                site = params
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone());
                break;
            }
        }
    }
    (params.last().map(|(_, v)| v.clone()), site)
}

/// `(source, actor, user_id, site_id)` for a request.
async fn who(
    st: &AppState,
    cred: Option<&str>,
    via_mcp: bool,
    browser: bool,
) -> (&'static str, String, Option<String>, Option<String>) {
    let p = match cred {
        Some(c) => crate::auth::resolve(st, c).await.ok(),
        None => None,
    };
    match p {
        Some(Principal::User {
            user_id,
            token: Some(name),
            ..
        }) => {
            if via_mcp {
                (source::MCP, format!("mcp:{name}"), Some(user_id), None)
            } else {
                (source::API, format!("token:{name}"), Some(user_id), None)
            }
        }
        Some(Principal::User {
            user_id,
            token: None,
            ..
        }) => {
            let src = if via_mcp { source::MCP } else { source::UI };
            (src, format!("user:{user_id}"), Some(user_id), None)
        }
        Some(Principal::Site { site_id }) => {
            let src = if via_mcp { source::MCP } else { source::API };
            (src, format!("site:{site_id}"), None, Some(site_id))
        }
        None => {
            let src = if via_mcp {
                source::MCP
            } else if browser {
                source::UI
            } else {
                source::API
            };
            (src, "anonymous".to_string(), None, None)
        }
    }
}

/// `denied` for 401/403, `error` for any other status >= 400, else `ok`.
pub fn status_for(code: u16) -> &'static str {
    match code {
        401 | 403 => status::DENIED,
        c if c >= 400 => status::ERROR,
        _ => status::OK,
    }
}

/// The request middleware (see the module docs). Layer it with `Router::layer` so the matched
/// route template is known.
pub async fn middleware(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if !path.starts_with("/api/") {
        return next.run(req).await;
    }
    let Some(log) = st.services.get::<EventLog>() else {
        return next.run(req).await;
    };
    let method = req.method().clone();
    let path = path.to_string();
    let template = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string());
    let h = req.headers();
    let via_mcp = header(h, VIA_HEADER).is_some_and(|v| v.eq_ignore_ascii_case("mcp"));
    let tool = header(h, TOOL_HEADER)
        .map(|t| clean(t, 100))
        .filter(|t| !t.is_empty());
    let cred = bearer(h);
    let browser = h.contains_key("sec-fetch-mode") || h.contains_key("sec-fetch-site");
    let t0 = Instant::now();
    let res = next.run(req).await;
    let took = t0.elapsed();
    let code = res.status().as_u16();
    let read_only = matches!(method, Method::GET | Method::HEAD | Method::OPTIONS);
    if read_only && !via_mcp && code < 400 && !st.live.get().monitor.log_reads {
        return res;
    }
    // The handler already resolved (and cached) the credential, so this is a cache hit.
    let (src, actor, user_id, principal_site) = who(&st, cred.as_deref(), via_mcp, browser).await;
    let route = template.unwrap_or_else(|| clean(&path, 200));
    let params = path_params(&route, &path);
    let (target, site) = target_and_site(&route, &params);
    let mut detail = Map::new();
    if let Some(t) = &tool {
        detail.insert("tool".into(), json!(t));
    }
    let ev = Event::new(src, &actor, &format!("{method} {route}"))
        .status(status_for(code))
        .http(code)
        .duration(took)
        .target(target.as_deref())
        .site(site.or(principal_site).as_deref())
        .user(user_id.as_deref())
        .detail(if detail.is_empty() {
            Value::Null
        } else {
            Value::Object(detail)
        });
    log.record(ev);
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_and_site_from_template() {
        let p = path_params(
            "/api/v1/sites/{id}/edges/{edge_id}",
            "/api/v1/sites/s1/edges/e9",
        );
        assert_eq!(
            p,
            vec![("id".into(), "s1".into()), ("edge_id".into(), "e9".into())]
        );
        let (t, s) = target_and_site("/api/v1/sites/{id}/edges/{edge_id}", &p);
        assert_eq!((t.as_deref(), s.as_deref()), (Some("e9"), Some("s1")));
        let p = path_params("/api/v2/jobs/{id}", "/api/v2/jobs/j1");
        assert_eq!(
            target_and_site("/api/v2/jobs/{id}", &p),
            (Some("j1".into()), None)
        );
        assert!(path_params("/api/{*rest}", "/api/nope/x").is_empty());
        // hostile paths never panic
        for path in [
            "",
            "/",
            "//////",
            "/api/v1/sites/\u{0}\u{7f}",
            &"/a".repeat(5000),
        ] {
            let p = path_params("/api/v1/sites/{id}", path);
            let _ = target_and_site("/api/v1/sites/{id}", &p);
        }
        assert_eq!(status_for(401), "denied");
        assert_eq!(status_for(403), "denied");
        assert_eq!(status_for(404), "error");
        assert_eq!(status_for(500), "error");
        assert_eq!(status_for(201), "ok");
    }
}
