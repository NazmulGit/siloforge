//! rmcp `ServerHandler`: tools/list + tools/call over the shared tool catalogue.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::RoleServer;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use serde_json::Value;

use crate::client::{Api, Auth, Core, ToolError};
use crate::output::{fit_json, fit_text};
use crate::tools::{self, Out};

const INSTRUCTIONS: &str = "SiloForge internal-linking tools. `site` accepts a site id or a domain. \
Typical flow: siloforge_crawl_site -> siloforge_wait_for_job -> siloforge_plan_site -> siloforge_wait_for_job \
(or siloforge_run_pipeline), review with siloforge_list_edges, approve with siloforge_decide_edge / \
siloforge_bulk_decide, then siloforge_bulk_inject. Nothing is written to WordPress unless edges are approved \
and siloforge_bulk_inject is called; siloforge_bulk_undo reverts from snapshots.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Process-wide identity from the config (token or legacy login).
    Stdio,
    /// The caller's own bearer token from the HTTP request, forwarded to the API.
    Http,
}

/// The MCP server object (one per stdio connection / per HTTP request in stateless mode).
#[derive(Clone)]
pub struct SiloForgeMcp {
    pub(crate) core: Arc<Core>,
    pub(crate) mode: Mode,
}

/// MCP tool list built from the catalogue.
pub fn tool_list() -> Vec<Tool> {
    tools::catalogue()
        .into_iter()
        .map(|t| {
            let schema = match t.schema {
                Value::Object(m) => m,
                _ => serde_json::Map::new(),
            };
            let ann = ToolAnnotations::with_title(t.title)
                .read_only(t.read_only)
                .destructive(t.destructive)
                .open_world(false);
            Tool::new(t.name, t.description, Arc::new(schema))
                .with_title(t.title)
                .with_annotations(ann)
        })
        .collect()
}

fn bearer_from_parts(parts: Option<&http::request::Parts>) -> Option<String> {
    let value = parts?
        .headers
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, token) = value.trim().split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() && token.len() <= 4096)
        .then(|| token.to_string())
}

fn error_result(e: &ToolError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!("Error: {}", e.0))])
}

pub(crate) fn render(out: Out, max: usize) -> CallToolResult {
    match out {
        Out::Json(v) => CallToolResult::success(vec![ContentBlock::text(match v {
            Value::String(s) => fit_text(&s, max),
            other => fit_json(&other, max).text,
        })]),
        Out::Text(t) => CallToolResult::success(vec![ContentBlock::text(fit_text(&t, max))]),
        Out::Summary(v) => {
            let fitted = fit_json(&v, max);
            let truncated = fitted.text.contains("\n[truncated");
            let mut res = CallToolResult::success(vec![ContentBlock::text(fitted.text)]);
            if v.is_object() && !truncated {
                res.structured_content = Some(v);
            }
            res
        }
    }
}

impl SiloForgeMcp {
    async fn run_tool(
        &self,
        req: CallToolRequestParams,
        ctx: &RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let name = req.name.to_string();
        if !tools::is_known(&name) {
            return Err(McpError::invalid_params(
                format!("unknown tool: {}", crate::output::truncate_str(&name, 100)),
                None,
            ));
        }
        let auth = match self.mode {
            Mode::Stdio => self.core.stdio_auth.clone(),
            Mode::Http => match bearer_from_parts(ctx.extensions.get::<http::request::Parts>()) {
                Some(t) => Auth::Forwarded(t),
                None => {
                    return Ok(error_result(&ToolError::new(
                        "unauthorized: send `Authorization: Bearer <SiloForge API token>` with the MCP request",
                    )));
                }
            },
        };
        let api = Api {
            core: self.core.clone(),
            auth,
        };
        let args = req.arguments.unwrap_or_default();
        tracing::debug!(tool = %name, "mcp tool call");
        let max = self.core.cfg.max_output_chars;
        Ok(match tools::call(&api, &name, &args, &ctx.ct).await {
            Ok(out) => render(out, max),
            Err(e) => {
                tracing::info!(tool = %name, error = %e, "mcp tool failed");
                error_result(&e)
            }
        })
    }
}

impl ServerHandler for SiloForgeMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("siloforge", sf_core::VERSION).with_title("SiloForge"),
            )
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tool_list()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.run_tool(request, &context)
            .await
            .map(CallToolResponse::from)
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_list().into_iter().find(|t| t.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_parsing() {
        let req = http::Request::builder()
            .header("authorization", "Bearer sfk_abc")
            .body(())
            .unwrap_or_default();
        let (parts, ()) = req.into_parts();
        assert_eq!(bearer_from_parts(Some(&parts)).as_deref(), Some("sfk_abc"));
        let req = http::Request::builder()
            .header("authorization", "Basic x")
            .body(())
            .unwrap_or_default();
        let (parts, ()) = req.into_parts();
        assert_eq!(bearer_from_parts(Some(&parts)), None);
        assert_eq!(bearer_from_parts(None), None);
    }

    #[test]
    fn tool_list_is_valid() {
        let tools = tool_list();
        assert_eq!(tools.len(), 28);
        for t in &tools {
            assert_eq!(
                t.input_schema.get("type").and_then(Value::as_str),
                Some("object")
            );
            assert!(t.name.starts_with("siloforge_"));
        }
    }
}
