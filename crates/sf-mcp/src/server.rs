//! rmcp `ServerHandler`: tools/list + tools/call over the shared tool catalogue.

use std::sync::Arc;
use std::time::{Duration, Instant};

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
use crate::output::{fit_json, fit_text, truncate_str};
use crate::policy::{Effective, USAGE_PATH};
use crate::tools::{self, Out, ToolDef};

const INSTRUCTIONS: &str = "SiloForge internal-linking tools. Call siloforge_capabilities first for the tools you may use, \
grouped by area. `site` accepts a site id or a domain. Typical flow: siloforge_crawl_site -> siloforge_wait_for_job -> siloforge_plan_site -> siloforge_wait_for_job \
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

fn to_tool(t: &ToolDef) -> Tool {
    let schema = match &t.schema {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    let ann = ToolAnnotations::with_title(t.title)
        .read_only(t.read_only)
        .destructive(t.destructive)
        .open_world(false);
    Tool::new(t.name, t.description, Arc::new(schema))
        .with_title(t.title)
        .with_annotations(ann)
}

/// MCP tool list built from the whole catalogue (before any policy filtering).
pub fn tool_list() -> Vec<Tool> {
    tools::catalogue_ref().iter().map(to_tool).collect()
}

/// The tools one caller may see.
fn tool_list_for(eff: &Effective) -> Vec<Tool> {
    tools::catalogue_ref()
        .iter()
        .filter(|t| eff.allows(t.name))
        .map(to_tool)
        .collect()
}

const USAGE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_USAGE_ERROR: usize = 300;

/// Reports one tool call to `POST /api/v2/mcp/usage` without blocking or failing the tool.
/// At most `MAX_USAGE_IN_FLIGHT` reports run at once; extra ones are dropped.
fn report_usage(api: &Api, tool: &'static str, ok: bool, duration: Duration, error: Option<&str>) {
    let Ok(permit) = api.core.usage_slots.clone().try_acquire_owned() else {
        tracing::debug!(tool, "mcp usage report dropped (too many in flight)");
        return;
    };
    let api = api.clone();
    let body = serde_json::json!({
        "tool": tool,
        "ok": ok,
        "duration_ms": u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        "error": error.map(|e| truncate_str(e, MAX_USAGE_ERROR).to_string()),
    });
    tokio::spawn(async move {
        let _permit = permit;
        match api.post_quiet(USAGE_PATH, body, USAGE_TIMEOUT).await {
            Ok(status) if (200..300).contains(&status) => {}
            Ok(status) => tracing::debug!(tool, status, "mcp usage report not accepted"),
            Err(e) => tracing::debug!(tool, error = %e, "mcp usage report failed"),
        }
    });
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
    /// The identity a request acts as (`None`: HTTP request without a bearer token).
    fn auth_for(&self, ctx: &RequestContext<RoleServer>) -> Option<Auth> {
        match self.mode {
            Mode::Stdio => Some(self.core.stdio_auth.clone()),
            Mode::Http => {
                bearer_from_parts(ctx.extensions.get::<http::request::Parts>()).map(Auth::Forwarded)
            }
        }
    }

    async fn effective(&self, auth: &Auth) -> Arc<Effective> {
        let api = Api {
            core: self.core.clone(),
            auth: auth.clone(),
            tool: None,
            policy: None,
        };
        self.core
            .policy
            .effective(&api, self.mode == Mode::Http)
            .await
    }

    async fn run_tool(
        &self,
        req: CallToolRequestParams,
        ctx: &RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let Some(def) = tools::find(&req.name) else {
            return Err(McpError::invalid_params(
                format!("unknown tool: {}", truncate_str(&req.name, 100)),
                None,
            ));
        };
        let name = def.name;
        let Some(auth) = self.auth_for(ctx) else {
            return Ok(error_result(&ToolError::new(
                "unauthorized: send `Authorization: Bearer <SiloForge API token>` with the MCP request",
            )));
        };
        let eff = self.effective(&auth).await;
        if !eff.allows(name) {
            tracing::info!(tool = name, "mcp tool refused by policy");
            return Err(McpError::invalid_request(
                format!(
                    "tool {name} is not allowed for this credential by the SiloForge MCP policy (disabled, or needs the `{}` scope); see siloforge_capabilities",
                    def.min_scope
                ),
                None,
            ));
        }
        let api = Api {
            core: self.core.clone(),
            auth,
            tool: Some(name),
            policy: Some(eff.clone()),
        };
        let args = req.arguments.unwrap_or_default();
        tracing::debug!(tool = name, "mcp tool call");
        let max = self.core.cfg.max_output_chars;
        let started = Instant::now();
        let result = tools::call(&api, name, &args, &ctx.ct).await;
        if eff.reports_usage() {
            let err = result.as_ref().err().map(|e| e.0.as_str());
            report_usage(&api, name, result.is_ok(), started.elapsed(), err);
        }
        Ok(match result {
            Ok(out) => render(out, max),
            Err(e) => {
                tracing::info!(tool = name, error = %e, "mcp tool failed");
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
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let eff = match self.auth_for(&context) {
            Some(auth) => self.effective(&auth).await,
            None => Arc::new(Effective::fail_open(crate::policy::Source::Unavailable)),
        };
        Ok(ListToolsResult::with_all_items(tool_list_for(&eff)))
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

    /// 28 v1/v2.0 tools + the v2.1 additions.
    const EXPECTED_TOOLS: usize = 118;

    #[test]
    fn scopes_and_defaults() {
        for t in crate::tools::catalogue_ref() {
            assert!(
                matches!(t.min_scope, "read" | "write" | "admin"),
                "{}",
                t.name
            );
            if !t.read_only {
                assert_ne!(t.min_scope, "read", "{} writes but has read scope", t.name);
            }
            assert_eq!(
                t.default_enabled,
                t.name != "siloforge_api_request",
                "{}",
                t.name
            );
            if crate::tools::needs_confirm(t.name) {
                let req = t.schema.get("required").and_then(Value::as_array);
                assert!(
                    req.is_some_and(|r| r.iter().any(|x| x == "confirm")),
                    "{} needs confirm in its schema",
                    t.name
                );
            }
        }
        let api = crate::tools::find("siloforge_api_request");
        assert_eq!(api.map(|t| t.min_scope), Some("admin"));
    }

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
        assert_eq!(tools.len(), crate::tools::catalogue_ref().len());
        assert_eq!(tools.len(), EXPECTED_TOOLS);
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), EXPECTED_TOOLS, "duplicate tool names");
        for t in &tools {
            assert_eq!(
                t.input_schema.get("type").and_then(Value::as_str),
                Some("object")
            );
            assert!(t.name.starts_with("siloforge_"));
        }
    }
}
