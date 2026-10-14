//! sf-mcp: SiloForge MCP server (rmcp), a thin client of the SiloForge REST API so auth,
//! ownership and validation stay in one place.
//!
//! * [`run_stdio`] — `siloforge mcp` (Hermes spawns it; identity from `SILOFORGE_TOKEN`, or the
//!   legacy `SILOFORGE_EMAIL`/`SILOFORGE_PASSWORD` login with one re-login on 401).
//! * [`http_router`] — streamable HTTP MCP at `/mcp` (stateless, JSON responses). The MCP client's
//!   own `Authorization: Bearer` token is forwarded to the REST API, so the caller's permissions
//!   apply; there is no built-in privileged identity.
//!
//! Tools: the 20 v1 names/params/semantics unchanged (see [`tools::catalogue`]) plus v2 job,
//! system, backup and notification tools. Credentials are stripped from every output.

mod client;
mod config;
mod output;
mod server;
pub mod tools;

use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::io::{AsyncRead, AsyncWrite};

pub use config::{DEFAULT_API_URL, DEFAULT_EMAIL, McpConfig};
pub use server::{SiloForgeMcp, tool_list};

use client::Core;
use server::Mode;

/// Names of all tools, in catalogue order (v1 first).
pub fn tool_names() -> Vec<&'static str> {
    tools::catalogue().into_iter().map(|t| t.name).collect()
}

/// Serves MCP over stdin/stdout until the client disconnects.
pub async fn run_stdio(cfg: McpConfig) -> anyhow::Result<()> {
    serve_io(cfg, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Serves MCP (newline-delimited JSON-RPC) over any reader/writer pair.
pub async fn serve_io<R, W>(cfg: McpConfig, reader: R, writer: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    if cfg.token.is_none() && cfg.password.is_some() {
        tracing::warn!(
            "SILOFORGE_PASSWORD is deprecated for MCP: create an API token (`siloforge token create`) and set SILOFORGE_TOKEN instead"
        );
    }
    tracing::info!(api = %cfg.api_url, auth = cfg.auth_mode(), "siloforge mcp starting (stdio)");
    let core = Arc::new(Core::new(cfg)?);
    let running = SiloForgeMcp {
        core,
        mode: Mode::Stdio,
    }
    .serve((reader, writer))
    .await
    .map_err(|e| anyhow::anyhow!("mcp initialize failed: {e}"))?;
    let reason = running.waiting().await?;
    tracing::info!(?reason, "siloforge mcp stopped");
    Ok(())
}

/// Streamable HTTP MCP at `/mcp`. Only `cfg.api_url` (and output/poll settings) is used: the
/// configured token/password are ignored, every request acts as the bearer token it carries.
/// Generic over the outer router state so the binary can merge it into the main app.
pub fn http_router<S>(cfg: McpConfig) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let mut http_cfg = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true);
    if !cfg.http_allowed_hosts.is_empty() {
        let mut hosts = http_cfg.allowed_hosts.clone();
        hosts.extend(cfg.http_allowed_hosts.iter().cloned());
        http_cfg = http_cfg.with_allowed_hosts(hosts);
    }
    let core = match Core::new(cfg) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::error!(error = %e, "mcp http client init failed; /mcp disabled");
            return Router::new().route("/mcp", axum::routing::any(mcp_unavailable));
        }
    };
    let service = StreamableHttpService::new(
        move || {
            Ok(SiloForgeMcp {
                core: core.clone(),
                mode: Mode::Http,
            })
        },
        Arc::new(NeverSessionManager::default()),
        http_cfg,
    );
    Router::new()
        .route_service("/mcp", service)
        .layer(axum::middleware::from_fn(require_bearer))
}

async fn mcp_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({"error": "mcp unavailable"})),
    )
        .into_response()
}

/// Rejects MCP HTTP requests without a bearer token before they reach the protocol layer.
async fn require_bearer(req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().split_once(' '))
        .is_some_and(|(scheme, token)| {
            scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()
        });
    if ok {
        return next.run(req).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        axum::Json(serde_json::json!({"error": "unauthorized"})),
    )
        .into_response()
}
