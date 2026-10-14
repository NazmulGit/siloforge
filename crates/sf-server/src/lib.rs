//! SiloForge HTTP server.
//!
//! * `/api/v1/*`: the v1 contract (WP S, `v1/`), so the UI, MCP and WP plugin work unchanged.
//! * `/api/v2/*`: new features (WP O, `v2/`): system, logs, notifications, backups, tokens.
//! * `/health`: unauthenticated liveness + component status.
//! * everything else: the embedded React UI with SPA fallback (`ui.rs`).

pub mod auth;
pub mod error;
pub mod eventlog;
pub mod mail;
pub mod platform;
pub mod sessions;
pub mod state;
pub mod tenancy;
pub mod ui;
pub mod v1;
pub mod v2;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use tower_http::compression::CompressionLayer;

pub use error::{ApiError, ApiResult};
pub use state::AppState;

/// Builds the full application router. `extra` lets the binary mount more routes (e.g. `/mcp`).
pub fn router(state: AppState, extra: Option<Router<AppState>>) -> Router {
    // L-02 / L-07: hash legacy site keys and seal provider headers once per start (background).
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        let st = state.clone();
        rt.spawn(async move {
            match v1::common::hash_legacy_site_keys(&st.db).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(sites = n, "hashed legacy plaintext site api keys"),
                Err(e) => tracing::warn!(error = %e, "could not hash legacy site api keys"),
            }
            // L-07: same for provider custom header values.
            match v1::providers::seal_legacy_provider_headers(&st).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(providers = n, "sealed plaintext provider header values"),
                Err(e) => tracing::warn!(error = %e, "could not seal provider header values"),
            }
        });
    }
    let body_limit = state.cfg.server.body_limit_mb.saturating_mul(1024 * 1024);
    let mut app = Router::new()
        .route("/health", get(health))
        .nest("/api/v1", v1::router())
        .nest("/api/v2", v2::router())
        .route("/api/{*rest}", axum::routing::any(api_not_found));
    if let Some(extra) = extra {
        // v2.1 §3: `/mcp` answers 404 while the MCP policy has `http_enabled: false`.
        app = app.merge(extra.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            v2::mcp::http_gate,
        )));
    }
    let app = app
        .fallback(ui::serve)
        .layer(DefaultBodyLimit::max(body_limit));
    // G2-4: `server.cors_origins` (empty = same-origin only, no CORS headers at all).
    let app = match cors_layer(&state.cfg.server.cors_origins) {
        Some(cors) => app.layer(cors),
        None => app,
    };
    app.layer(CompressionLayer::new())
        .layer(axum::middleware::from_fn(ui::security_headers))
        .layer(axum::middleware::from_fn(error::request_log))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            error::track_errors,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            eventlog::middleware,
        ))
        // v2.4: request context (method, workspace / view-as headers) for the tenancy checks
        .layer(axum::middleware::from_fn(tenancy::middleware))
        .with_state(state)
}

/// CORS for the configured origins. `"*"` allows any origin (no credentials either way: the API
/// uses bearer tokens, not cookies). Unparsable entries are skipped with a warning.
pub fn cors_layer(origins: &[String]) -> Option<tower_http::cors::CorsLayer> {
    use axum::http::{HeaderName, HeaderValue, Method, header};
    use tower_http::cors::{AllowOrigin, CorsLayer};
    let list: Vec<&str> = origins
        .iter()
        .map(|o| o.trim().trim_end_matches('/'))
        .filter(|o| !o.is_empty())
        .collect();
    if list.is_empty() {
        return None;
    }
    let allow = if list.contains(&"*") {
        AllowOrigin::any()
    } else {
        let vals: Vec<HeaderValue> = list
            .iter()
            .filter_map(|o| match HeaderValue::from_str(o) {
                Ok(v) => Some(v),
                Err(_) => {
                    tracing::warn!(origin = %o, "ignoring invalid server.cors_origins entry");
                    None
                }
            })
            .collect();
        if vals.is_empty() {
            return None;
        }
        AllowOrigin::list(vals)
    };
    Some(
        CorsLayer::new()
            .allow_origin(allow)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([
                header::AUTHORIZATION,
                header::CONTENT_TYPE,
                header::ACCEPT,
                HeaderName::from_static("x-api-key"),
                HeaderName::from_static("x-site-key"),
                HeaderName::from_static(tenancy::WORKSPACE_HEADER),
                HeaderName::from_static(tenancy::VIEW_AS_HEADER),
            ])
            .max_age(std::time::Duration::from_secs(600)),
    )
}

async fn api_not_found() -> ApiError {
    ApiError::not_found("route not found")
}

/// v1 shape: `{ ok, services: { db, redis, qdrant } }` (+ v2 fields). Redis/Qdrant no longer exist; they
/// report the components that replaced them (queue and vector index) so old probes stay green.
async fn health(
    axum::extract::State(st): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    let db_ok = st
        .db
        .read(|c| {
            c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
                .map_err(sf_store::err)
        })
        .await
        .is_ok();
    let (running, queued) = st.jobs.counts().await.unwrap_or((0, 0));
    axum::Json(serde_json::json!({
        "ok": db_ok,
        "version": sf_core::VERSION,
        "uptime_secs": st.started.elapsed().as_secs(),
        "services": { "db": db_ok, "redis": db_ok, "qdrant": db_ok, "queue": db_ok },
        "jobs": { "running": running, "queued": queued },
        // R8: supervised background loops (restarts, last error)
        "background": sf_ops::supervise::services(),
        "background_ok": sf_ops::supervise::all_running(),
    }))
}

#[cfg(test)]
mod cors_tests {
    use super::*;

    /// G2-4: cors_origins is applied (empty = no layer).
    #[test]
    fn cors_layer_follows_config() {
        assert!(cors_layer(&[]).is_none());
        assert!(cors_layer(&["  ".into()]).is_none());
        assert!(cors_layer(&["https://ui.example.com".into()]).is_some());
        assert!(cors_layer(&["*".into()]).is_some());
    }
}
