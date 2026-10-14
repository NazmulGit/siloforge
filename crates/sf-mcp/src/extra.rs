//! v2.1 tools (contract §8): everything an admin can do in the UI, reachable over MCP.
//!
//! Most tools are declarative [`RouteTool`]s: one REST route, typed parameters placed in the path,
//! the query string or the JSON body, and an optional response post-processing step. The schema
//! and the request are built from the same parameter list, so they cannot drift apart. A few tools
//! are composite or special ([`CUSTOM`]): `siloforge_capabilities`, `siloforge_doctor`,
//! `siloforge_import_posts_csv` (text body) and the `siloforge_api_request` escape hatch.

use std::sync::LazyLock;

use reqwest::Method;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::client::{Api, ApiBody, ReqBody, ToolError, ToolResult};
use crate::tools::{
    ANCHOR_TYPES, Args, MAX_STR, Out, SITE_DESC, TOPOLOGIES, ToolDef, bad, catalogue_ref, check_id,
    def, query,
};

/// The escape hatch (admin, disabled by default).
pub const API_REQUEST: &str = "siloforge_api_request";
const CAPABILITIES: &str = "siloforge_capabilities";
const DOCTOR: &str = "siloforge_doctor";
const IMPORT_POSTS_CSV: &str = "siloforge_import_posts_csv";
const CUSTOM: &[&str] = &[CAPABILITIES, DOCTOR, IMPORT_POSTS_CSV, API_REQUEST];

const MAX_OFFSET: i64 = u32::MAX as i64;
const MAX_OBJ_BYTES: usize = 256 * 1024;
const MAX_CSV_BYTES: usize = 8 * 1024 * 1024;
const MAX_API_BODY_BYTES: usize = 1024 * 1024;
const MAX_ARRAY: usize = 10_000;
const JOB_KINDS: &[&str] = &["crawl", "plan", "link_health", "audit"];
const LOG_SOURCES: &[&str] = &["ui", "api", "mcp", "job", "system", "auth", "notify"];
const LOG_STATUSES: &[&str] = &["ok", "error", "denied"];
const HTTP_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE"];

// ---------------------------------------------------------------- parameter model

#[derive(Clone, Copy, Debug)]
enum Ty {
    /// String, max bytes.
    Str(usize),
    /// `[A-Za-z0-9_-]{1,128}` (interpolated into paths).
    Id,
    /// Site id or domain, resolved to an id.
    Site,
    Int(i64, i64),
    Num(f64, f64),
    Bool,
    Enum(&'static [&'static str]),
    /// JSON object (max serialized bytes).
    Obj,
    /// Array of strings, max items.
    StrArr(usize),
    /// Array of ids.
    IdArr,
    /// Array of integers.
    IntArr,
    /// Array of objects, max items.
    ObjArr(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Loc {
    Path,
    Query,
    Body,
}

#[derive(Clone, Copy, Debug)]
struct P {
    key: &'static str,
    ty: Ty,
    loc: Loc,
    req: bool,
    nullable: bool,
    desc: &'static str,
    /// Name sent to the API when it differs from the tool argument name.
    wire: Option<&'static str>,
}

impl P {
    fn new(key: &'static str, ty: Ty, loc: Loc, desc: &'static str) -> Self {
        Self {
            key,
            ty,
            loc,
            req: loc == Loc::Path,
            nullable: false,
            desc,
            wire: None,
        }
    }
    fn req(mut self) -> Self {
        self.req = true;
        self
    }
    fn nullable(mut self) -> Self {
        self.nullable = true;
        self
    }
    fn wire(mut self, name: &'static str) -> Self {
        self.wire = Some(name);
        self
    }
    fn wire_name(&self) -> &'static str {
        self.wire.unwrap_or(self.key)
    }
}

fn site() -> P {
    P::new("site", Ty::Site, Loc::Path, SITE_DESC)
}
fn pid(key: &'static str, desc: &'static str) -> P {
    P::new(key, Ty::Id, Loc::Path, desc)
}
fn q(key: &'static str, ty: Ty, desc: &'static str) -> P {
    P::new(key, ty, Loc::Query, desc)
}
fn b(key: &'static str, ty: Ty, desc: &'static str) -> P {
    P::new(key, ty, Loc::Body, desc)
}
fn limit(max: i64) -> P {
    q("limit", Ty::Int(1, max), "page size")
}
fn offset() -> P {
    q(
        "offset",
        Ty::Int(0, MAX_OFFSET),
        "skip this many rows (paging)",
    )
}

fn ty_schema(ty: Ty) -> Map<String, Value> {
    let v = match ty {
        Ty::Str(_) | Ty::Id => json!({"type":"string"}),
        Ty::Site => json!({"type":"string"}),
        Ty::Int(min, max) => json!({"type":"integer","minimum":min,"maximum":max}),
        Ty::Num(min, max) => json!({"type":"number","minimum":min,"maximum":max}),
        Ty::Bool => json!({"type":"boolean"}),
        Ty::Enum(v) => json!({"type":"string","enum":v}),
        Ty::Obj => json!({"type":"object"}),
        Ty::StrArr(max) => json!({"type":"array","items":{"type":"string"},"maxItems":max}),
        Ty::IdArr => json!({"type":"array","items":{"type":"string"},"maxItems":MAX_ARRAY}),
        Ty::IntArr => json!({"type":"array","items":{"type":"integer"},"maxItems":MAX_ARRAY}),
        Ty::ObjArr(max) => json!({"type":"array","items":{"type":"object"},"maxItems":max}),
    };
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn schema_of(params: &[P]) -> Value {
    let mut props = Map::new();
    let mut required = Vec::new();
    for p in params {
        let mut s = ty_schema(p.ty);
        if p.nullable
            && let Some(t) = s.get("type").cloned()
        {
            s.insert("type".into(), json!([t, "null"]));
        }
        if !p.desc.is_empty() {
            s.insert("description".into(), json!(p.desc));
        }
        props.insert(p.key.to_string(), Value::Object(s));
        if p.req {
            required.push(json!(p.key));
        }
    }
    let mut m = Map::new();
    m.insert("type".into(), json!("object"));
    m.insert("properties".into(), Value::Object(props));
    if !required.is_empty() {
        m.insert("required".into(), Value::Array(required));
    }
    Value::Object(m)
}

/// Reads one non-site parameter. `Ok(None)` = absent; `Some(Value::Null)` only for nullable ones.
fn extract(a: &Args<'_>, p: &P) -> ToolResult<Option<Value>> {
    let k = p.key;
    if p.nullable && matches!(a.0.get(k), Some(Value::Null)) {
        return Ok(Some(Value::Null));
    }
    let v = match p.ty {
        Ty::Str(max) => match a.raw(k) {
            None => None,
            Some(Value::String(s)) if s.len() > max => {
                return Err(bad(format!("`{k}` is too long (max {max} bytes)")));
            }
            Some(Value::String(s)) => Some(Value::String(s.clone())),
            Some(_) => return Err(bad(format!("`{k}` must be a string"))),
        },
        Ty::Id => match a.raw(k) {
            None => None,
            Some(_) => Some(Value::String(a.id_req(k)?)),
        },
        Ty::Site => a.str_opt(k)?.map(Value::String),
        Ty::Int(min, max) => a.int_opt(k, min, max)?.map(|n| json!(n)),
        Ty::Num(min, max) => match a.raw(k) {
            None => None,
            Some(v) => {
                let f = v
                    .as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
                    .filter(|f| f.is_finite() && (min..=max).contains(f))
                    .ok_or_else(|| {
                        bad(format!("`{k}` must be a number between {min} and {max}"))
                    })?;
                Some(json!(f))
            }
        },
        Ty::Bool => a.bool_opt(k)?.map(Value::Bool),
        Ty::Enum(allowed) => a.enum_opt(k, allowed)?.map(Value::String),
        Ty::Obj => match a.raw(k) {
            None => None,
            Some(v @ Value::Object(_)) => {
                if v.to_string().len() > MAX_OBJ_BYTES {
                    return Err(bad(format!(
                        "`{k}` is too large (max {MAX_OBJ_BYTES} bytes)"
                    )));
                }
                Some(v.clone())
            }
            Some(_) => return Err(bad(format!("`{k}` must be an object"))),
        },
        Ty::StrArr(max) => match a.raw(k) {
            None => None,
            Some(Value::Array(items)) if items.len() <= max => {
                for it in items {
                    match it.as_str() {
                        Some(s) if s.len() <= MAX_STR => {}
                        _ => return Err(bad(format!("`{k}` must contain only strings"))),
                    }
                }
                Some(Value::Array(items.clone()))
            }
            Some(_) => {
                return Err(bad(format!(
                    "`{k}` must be an array of at most {max} strings"
                )));
            }
        },
        Ty::IdArr => a.ids_opt(k)?.map(|v| json!(v)),
        Ty::IntArr => match a.raw(k) {
            None => None,
            Some(Value::Array(items)) if items.len() <= MAX_ARRAY => {
                if !items.iter().all(|x| x.as_i64().is_some()) {
                    return Err(bad(format!("`{k}` must contain only integers")));
                }
                Some(Value::Array(items.clone()))
            }
            Some(_) => return Err(bad(format!("`{k}` must be an array of integers"))),
        },
        Ty::ObjArr(max) => match a.raw(k) {
            None => None,
            Some(Value::Array(items)) if items.len() <= max => {
                if !items.iter().all(Value::is_object) {
                    return Err(bad(format!("`{k}` must contain only objects")));
                }
                let v = Value::Array(items.clone());
                if v.to_string().len() > MAX_API_BODY_BYTES * 8 {
                    return Err(bad(format!("`{k}` is too large")));
                }
                Some(v)
            }
            Some(_) => {
                return Err(bad(format!(
                    "`{k}` must be an array of at most {max} objects"
                )));
            }
        },
    };
    match v {
        None if p.req => Err(bad(format!("`{k}` is required"))),
        other => Ok(other),
    }
}

fn as_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------- route tools

#[derive(Clone, Copy, Debug)]
enum Kind {
    Read,
    Write,
    Destructive,
}

#[derive(Clone, Copy, Debug)]
enum Post {
    Plain,
    /// Keep only these top-level keys of the JSON answer.
    Pick(&'static [&'static str]),
    /// `{title, markdown}` rendered as Markdown text.
    Markdown,
}

struct RouteTool {
    def: ToolDef,
    method: Method,
    path: &'static str,
    params: Vec<P>,
    body_consts: Vec<(&'static str, Value)>,
    query_consts: Vec<(&'static str, &'static str)>,
    /// Nest the body params under this key (`{settings: {...}}`).
    wrap: Option<&'static str>,
    /// Refuse an empty body (update tools that would otherwise be a no-op).
    need_body: bool,
    post: Post,
}

impl RouteTool {
    fn admin(mut self) -> Self {
        self.def = self.def.admin();
        self
    }
    fn body_const(mut self, k: &'static str, v: Value) -> Self {
        self.body_consts.push((k, v));
        self
    }
    fn query_const(mut self, k: &'static str, v: &'static str) -> Self {
        self.query_consts.push((k, v));
        self
    }
    fn wrap(mut self, k: &'static str) -> Self {
        self.wrap = Some(k);
        self
    }
    fn need_body(mut self) -> Self {
        self.need_body = true;
        self
    }
    fn post(mut self, p: Post) -> Self {
        self.post = p;
        self
    }
}

#[allow(clippy::too_many_arguments)]
fn rt(
    group: &'static str,
    name: &'static str,
    title: &'static str,
    description: &'static str,
    kind: Kind,
    method: Method,
    path: &'static str,
    params: Vec<P>,
) -> RouteTool {
    let (ro, de) = match kind {
        Kind::Read => (true, false),
        Kind::Write => (false, false),
        Kind::Destructive => (false, true),
    };
    RouteTool {
        def: def(name, title, description, schema_of(&params), ro, de).in_category(group),
        method,
        path,
        params,
        body_consts: Vec::new(),
        query_consts: Vec::new(),
        wrap: None,
        need_body: false,
        post: Post::Plain,
    }
}

use Kind::{Destructive as D, Read as R, Write as W};

static ROUTES: LazyLock<Vec<RouteTool>> = LazyLock::new(routes);

#[allow(clippy::too_many_lines)]
fn routes() -> Vec<RouteTool> {
    let get = || Method::GET;
    let post = || Method::POST;
    let patch = || Method::PATCH;
    let put = || Method::PUT;
    let del = || Method::DELETE;
    let s = |max| Ty::Str(max);
    vec![
        // ---------------- sites
        rt("sites", "siloforge_site_get", "Get a site", "One site: id, domain, wp_url, mode, settings (credentials are never returned).", R, get(), "/api/v1/sites/{site}", vec![site()]),
        rt("sites", "siloforge_site_update", "Update site settings", "Change a site's wp_url, mode and/or settings (preset, silo_topology, insert_modules, link limits, ai_anchor_refine, ...; a null value removes a key). WP credentials: use siloforge_site_set_wp_credentials.", W, patch(), "/api/v1/sites/{site}", vec![
            site(),
            b("settings", Ty::Obj, "settings keys to change"),
            b("wp_url", s(2048), "WordPress base URL"),
            b("mode", Ty::Enum(&["wp_rest", "sitemap"]), ""),
        ]).need_body(),
        rt("sites", "siloforge_site_set_wp_credentials", "Set WordPress credentials", "Store the WordPress user + application password used for WP REST injection. Stored encrypted; never returned by the API or MCP.", W, patch(), "/api/v1/sites/{site}", vec![
            site(),
            b("wp_user", s(256), "WordPress username").req(),
            b("wp_app_password", s(512), "WordPress application password").req(),
        ]).wrap("settings"),
        rt("sites", "siloforge_site_rotate_key", "Rotate site plugin key", "Issue a new siloforge-connect plugin api_key; the old key stops working at once. The new key is NOT returned through MCP: copy it from the SiloForge UI into the plugin.", D, post(), "/api/v1/sites/{site}/rotate-key", vec![site()]),
        rt("sites", "siloforge_usage_counts", "Usage counters", "Footprint across the caller's sites: sites, posts, paragraphs, suggested/approved/applied edges, clusters.", R, get(), "/api/v1/usage", vec![]),
        // ---------------- pipeline
        rt("runs", "siloforge_archive_plan", "Plan archive/taxonomy links", "Enqueue an archive/taxonomy silo plan (suggested edges only). Returns a job_run_id.", W, post(), "/api/v1/sites/{site}/archive-plan", vec![
            site(),
            b("topology", Ty::Enum(TOPOLOGIES), ""),
            b("ai_refine", Ty::Bool, "AI anchor refinement for this run"),
        ]),
        rt("runs", "siloforge_audit_site", "Run a site audit", "Enqueue an audit job (crawl + plan, suggestions only; skip_crawl reuses the last crawl). Returns a job_run_id.", W, post(), "/api/v1/schedules/{site}/run-now", vec![
            site(),
            b("topology", Ty::Enum(TOPOLOGIES), ""),
            b("skip_crawl", Ty::Bool, ""),
        ]).body_const("job_kind", json!("audit")),
        rt("runs", "siloforge_link_health_run", "Run link-health check", "Enqueue a link-health check (broken/redirected/nofollow links). Returns a job_run_id; read results with siloforge_link_health_report.", W, post(), "/api/v1/sites/{site}/link-health", vec![
            site(),
            b("internal_only", Ty::Bool, "check internal links only"),
        ]),
        rt("runs", "siloforge_gsc_status", "Search Console status", "Google Search Console connection status for a site.", R, get(), "/api/v1/sites/{site}/gsc", vec![site()]),
        rt("runs", "siloforge_gsc_sync", "Sync Search Console", "Enqueue a Search Console pull (clicks/impressions per page). Returns a job_run_id; 409 when GSC is not connected.", W, post(), "/api/v2/sites/{site}/gsc/sync", vec![site()]),
        rt("runs", "siloforge_gsc_connect", "Connect Search Console", "Connect Google Search Console with a service-account JSON (stored encrypted, never returned).", W, post(), "/api/v1/sites/{site}/gsc", vec![
            site(),
            b("credentials_json", s(256 * 1024), "service-account credentials JSON (as a string)").req(),
        ]),
        rt("runs", "siloforge_gsc_disconnect", "Disconnect Search Console", "Remove the site's Search Console connection and its stored credentials.", D, del(), "/api/v1/sites/{site}/gsc", vec![site()]),
        // ---------------- rules
        rt("rules", "siloforge_list_keyword_rules", "List keyword rules", "Keyword-linker rules of a site (id, keyword, target_url, anchor_type, max_links, active).", R, get(), "/api/v1/rules/keywords/{site}", vec![site()]),
        rt("rules", "siloforge_update_keyword_rule", "Update a keyword rule", "Change a keyword rule: active, max_links, anchor_type.", W, patch(), "/api/v1/rules/keywords/{rule_id}", vec![
            pid("rule_id", "keyword rule id"),
            b("active", Ty::Bool, ""),
            b("max_links", Ty::Int(1, 100), ""),
            b("anchor_type", Ty::Enum(ANCHOR_TYPES), ""),
        ]).need_body(),
        rt("rules", "siloforge_delete_keyword_rule", "Delete a keyword rule", "Delete a keyword-linker rule (edges it already created stay).", D, del(), "/api/v1/rules/keywords/{rule_id}", vec![pid("rule_id", "keyword rule id")]),
        rt("rules", "siloforge_list_ignore_keywords", "List ignore keywords", "The site's anchor stop-list (id, phrase).", R, get(), "/api/v1/rules/ignore/{site}", vec![site()]),
        rt("rules", "siloforge_delete_ignore_keyword", "Delete an ignore keyword", "Remove a phrase from the anchor stop-list.", D, del(), "/api/v1/rules/ignore/{phrase_id}", vec![pid("phrase_id", "ignore-list entry id")]),
        // ---------------- schedules
        rt("schedules", "siloforge_list_schedules", "List schedules", "Recurring jobs of a site (job_kind, cron, tz, payload, active, next_run_at).", R, get(), "/api/v1/schedules/{site}", vec![site()]),
        rt("schedules", "siloforge_upsert_schedule", "Create/update a schedule", "Create or update the site's schedule for one job kind (one schedule per kind).", W, post(), "/api/v1/schedules/{site}", vec![
            site(),
            b("job_kind", Ty::Enum(JOB_KINDS), "default audit"),
            b("cron", s(128), "5-field cron, e.g. `0 3 * * 1`"),
            b("tz", s(64), "IANA timezone, e.g. Europe/Berlin"),
            b("payload", Ty::Obj, "job options (topology, skip_crawl, internal_only, ...)"),
            b("active", Ty::Bool, ""),
        ]),
        rt("schedules", "siloforge_delete_schedule", "Delete a schedule", "Delete a schedule by id.", D, del(), "/api/v1/schedules/{schedule_id}", vec![pid("schedule_id", "schedule id")]),
        rt("schedules", "siloforge_run_schedule_now", "Run a schedule now", "Run a site's schedule immediately (by schedule_id or job_kind; default audit). Returns a job_run_id.", W, post(), "/api/v1/schedules/{site}/run-now", vec![
            site(),
            b("schedule_id", Ty::Id, ""),
            b("job_kind", Ty::Enum(JOB_KINDS), ""),
            b("topology", Ty::Enum(TOPOLOGIES), ""),
            b("skip_crawl", Ty::Bool, ""),
        ]),
        // ---------------- reports
        rt("reports", "siloforge_orphans_report", "Orphan posts", "Posts with no inbound internal links (paged).", R, get(), "/api/v1/posts/{site}", vec![site(), limit(5000), offset()]).query_const("is_orphan", "true"),
        rt("reports", "siloforge_cannibalization_report", "Cannibalization warnings", "Keyword cannibalization warnings (posts competing for the same keyword) plus the health score.", R, get(), "/api/v1/dashboard/{site}", vec![site()]).post(Post::Pick(&["health_score", "cannibalization_warnings"])),
        rt("reports", "siloforge_metrics_trend", "Metrics trend", "Link-metrics snapshots over time (health score, orphans, links, ...).", R, get(), "/api/v1/reports/metrics/{site}", vec![site(), q("trend", Ty::Int(1, 500), "number of snapshots, default 30")]),
        rt("reports", "siloforge_link_health_report", "Link-health report", "Last link-health results: summary counts plus broken/redirect/nofollow lists (paged).", R, get(), "/api/v1/reports/link-health/{site}", vec![site(), limit(1000), offset()]),
        rt("reports", "siloforge_link_graph", "Link graph", "Internal link graph (nodes = posts, edges = links) for visualisation/analysis.", R, get(), "/api/v1/reports/graph/{site}", vec![
            site(),
            q("max_nodes", Ty::Int(1, 750), "default 500"),
            q("max_edges", Ty::Int(0, 2500), "default 1500"),
        ]),
        rt("reports", "siloforge_export_history_csv", "Export link history CSV", "Applied/reverted link history as CSV text.", R, get(), "/api/v1/history/{site}", vec![site()]).query_const("format", "csv"),
        rt("reports", "siloforge_export_link_health_csv", "Export link health CSV", "Link-health results as CSV text.", R, get(), "/api/v1/reports/link-health/{site}", vec![site()]).query_const("format", "csv"),
        // ---------------- silo
        rt("silo", "siloforge_export_clusters", "Export clusters", "Silo clusters as JSON ({clusters:[{label, pillar_url, member_urls}]}), ready for siloforge_import_clusters.", R, get(), "/api/v1/silo/{site}/clusters/export", vec![site()]),
        rt("silo", "siloforge_import_clusters", "Import custom clusters", "REPLACES the site's custom silo clusters (one transaction). Each cluster: {label, pillar_url, member_urls}.", D, post(), "/api/v1/silo/{site}/clusters/import", vec![
            site(),
            b("clusters", Ty::ObjArr(10_000), "[{label, pillar_url, member_urls: [..]}]").req(),
        ]),
        // ---------------- posts
        rt("posts", "siloforge_list_posts", "List/search posts", "Crawled posts of a site with link counts; filter by search text, post_type, cluster_id, is_orphan.", R, get(), "/api/v1/posts/{site}", vec![
            site(),
            q("search", s(200), "title/url substring"),
            q("post_type", s(64), "post, page, ..."),
            q("cluster_id", Ty::Id, "cluster uuid"),
            q("is_orphan", Ty::Bool, ""),
            limit(5000),
            offset(),
        ]),
        // ---------------- AI models (admin)
        rt("ai", "siloforge_ai_status", "AI provider health", "Health of every AI provider/model (last probe, errors, latency).", R, get(), "/api/v1/providers/health", vec![]).admin(),
        rt("ai", "siloforge_list_ai_providers", "List AI providers", "AI providers (keys are never returned).", R, get(), "/api/v1/providers", vec![]).admin(),
        rt("ai", "siloforge_get_ai_provider", "Get an AI provider", "One AI provider.", R, get(), "/api/v1/providers/{provider_id}", vec![pid("provider_id", "provider id")]).admin(),
        rt("ai", "siloforge_create_ai_provider", "Create an AI provider", "Add an OpenAI-compatible (or other kind) provider. api_key is stored encrypted and never returned.", W, post(), "/api/v1/providers", vec![
            b("name", s(200), "").req(),
            b("label", s(200), ""),
            b("kind", s(64), "provider kind, e.g. openai, openrouter, anthropic, ollama").req(),
            b("base_url", s(2048), "").req(),
            b("api_key", s(4096), ""),
            b("headers", Ty::Obj, "extra HTTP headers"),
            b("enabled", Ty::Bool, ""),
        ]).admin(),
        rt("ai", "siloforge_update_ai_provider", "Update an AI provider", "Change provider fields; omit api_key to keep it, \"\" clears it.", W, patch(), "/api/v1/providers/{provider_id}", vec![
            pid("provider_id", "provider id"),
            b("name", s(200), ""),
            b("label", s(200), ""),
            b("kind", s(64), ""),
            b("base_url", s(2048), ""),
            b("api_key", s(4096), ""),
            b("headers", Ty::Obj, ""),
            b("enabled", Ty::Bool, ""),
        ]).admin().need_body(),
        rt("ai", "siloforge_delete_ai_provider", "Delete an AI provider", "Delete a provider (and its models).", D, del(), "/api/v1/providers/{provider_id}", vec![pid("provider_id", "provider id")]).admin(),
        rt("ai", "siloforge_probe_ai_provider", "Probe an AI provider", "Send a tiny test request to a provider and record the result.", W, post(), "/api/v1/providers/{provider_id}/probe", vec![pid("provider_id", "provider id")]).admin(),
        rt("ai", "siloforge_list_ai_models", "List AI models", "Configured models (optionally of one provider).", R, get(), "/api/v1/models", vec![q("provider_id", Ty::Id, "provider uuid")]).admin(),
        rt("ai", "siloforge_create_ai_model", "Create an AI model", "Register a model of a provider for a role (e.g. llm, embed).", W, post(), "/api/v1/models", vec![
            b("provider_id", Ty::Id, "").req(),
            b("model_id", s(256), "provider model name").req(),
            b("role", s(64), "llm | embed | ...").req(),
            b("label", s(200), ""),
            b("context_tokens", Ty::Int(1, 100_000_000), ""),
            b("dim", Ty::Int(1, 100_000), "embedding dimension"),
            b("enabled", Ty::Bool, ""),
            b("price_in", Ty::Num(0.0, 1e9), ""),
            b("price_out", Ty::Num(0.0, 1e9), ""),
            b("notes", s(2000), ""),
        ]).admin(),
        rt("ai", "siloforge_update_ai_model", "Update an AI model", "Change model fields.", W, patch(), "/api/v1/models/{model_row_id}", vec![
            pid("model_row_id", "model row id"),
            b("model_id", s(256), ""),
            b("label", s(200), ""),
            b("context_tokens", Ty::Int(1, 100_000_000), ""),
            b("dim", Ty::Int(1, 100_000), ""),
            b("enabled", Ty::Bool, ""),
            b("price_in", Ty::Num(0.0, 1e9), ""),
            b("price_out", Ty::Num(0.0, 1e9), ""),
            b("notes", s(2000), ""),
        ]).admin().need_body(),
        rt("ai", "siloforge_delete_ai_model", "Delete an AI model", "Delete a model (removes it from chains).", D, del(), "/api/v1/models/{model_row_id}", vec![pid("model_row_id", "model row id")]).admin(),
        rt("ai", "siloforge_probe_ai_model", "Probe an AI model", "Send a tiny test request to one model and record the result.", W, post(), "/api/v1/models/{model_row_id}/probe", vec![pid("model_row_id", "model row id")]).admin(),
        rt("ai", "siloforge_list_ai_chains", "List AI model chains", "Fallback chains (role, site scope, active, rungs).", R, get(), "/api/v1/model-chains", vec![q("site_id", s(64), "site uuid or \"global\"")]).admin(),
        rt("ai", "siloforge_create_ai_chain", "Create an AI model chain", "Create a fallback chain for a role, global or for one site.", W, post(), "/api/v1/model-chains", vec![
            b("name", s(200), "").req(),
            b("role", s(64), "llm | embed | ...").req(),
            P::new("site", Ty::Site, Loc::Body, "optional site (id or domain); omit for a global chain").wire("site_id"),
            b("active", Ty::Bool, ""),
            b("notes", s(2000), ""),
        ]).admin(),
        rt("ai", "siloforge_update_ai_chain", "Update an AI model chain", "Rename a chain or change its notes.", W, patch(), "/api/v1/model-chains/{chain_id}", vec![
            pid("chain_id", "chain id"),
            b("name", s(200), ""),
            b("notes", s(2000), ""),
        ]).admin().need_body(),
        rt("ai", "siloforge_delete_ai_chain", "Delete an AI model chain", "Delete a chain and its rungs.", D, del(), "/api/v1/model-chains/{chain_id}", vec![pid("chain_id", "chain id")]).admin(),
        rt("ai", "siloforge_activate_ai_chain", "Activate an AI model chain", "Make this chain the active one for its role/scope.", W, post(), "/api/v1/model-chains/{chain_id}/activate", vec![pid("chain_id", "chain id")]).admin(),
        rt("ai", "siloforge_deactivate_ai_chain", "Deactivate an AI model chain", "Deactivate a chain.", W, post(), "/api/v1/model-chains/{chain_id}/deactivate", vec![pid("chain_id", "chain id")]).admin(),
        rt("ai", "siloforge_list_ai_rungs", "List chain rungs", "The ordered models (rungs) of a chain.", R, get(), "/api/v1/model-chains/{chain_id}/rungs", vec![pid("chain_id", "chain id")]).admin(),
        rt("ai", "siloforge_add_ai_rung", "Add a chain rung", "Append a model to a chain.", W, post(), "/api/v1/model-chains/{chain_id}/rungs", vec![
            pid("chain_id", "chain id"),
            b("model_row_id", Ty::Id, "model row id").req(),
            b("timeout_ms", Ty::Int(1, 3_600_000), ""),
            b("max_retries", Ty::Int(0, 20), ""),
            b("enabled", Ty::Bool, ""),
        ]).admin(),
        rt("ai", "siloforge_replace_ai_rungs", "Replace all chain rungs", "REPLACES every rung of a chain with the given ordered list [{model_row_id, timeout_ms?, max_retries?, enabled?}].", D, put(), "/api/v1/model-chains/{chain_id}/rungs", vec![
            pid("chain_id", "chain id"),
            b("rungs", Ty::ObjArr(64), "ordered rungs").req(),
        ]).admin(),
        rt("ai", "siloforge_reorder_ai_rungs", "Reorder chain rungs", "Set the rung order (an exact permutation of the chain's rung ids).", W, post(), "/api/v1/model-chains/{chain_id}/rungs/reorder", vec![
            pid("chain_id", "chain id"),
            b("rung_ids", Ty::IdArr, "all rung ids in the new order").req(),
        ]).admin(),
        rt("ai", "siloforge_update_ai_rung", "Update a chain rung", "Change a rung's timeout, retries or enabled flag.", W, patch(), "/api/v1/model-chain-rungs/{rung_id}", vec![
            pid("rung_id", "rung id"),
            b("timeout_ms", Ty::Int(1, 3_600_000), ""),
            b("max_retries", Ty::Int(0, 20), ""),
            b("enabled", Ty::Bool, ""),
        ]).admin().need_body(),
        rt("ai", "siloforge_delete_ai_rung", "Delete a chain rung", "Remove one rung from its chain.", D, del(), "/api/v1/model-chain-rungs/{rung_id}", vec![pid("rung_id", "rung id")]).admin(),
        // ---------------- settings (admin)
        rt("settings", "siloforge_settings_get", "Get settings", "All server settings by section (resources, compute, wordpress, crawl, llm, server, backup, monitor) plus restart_required. Secrets show as \"***\"/\"\".", R, get(), "/api/v2/settings", vec![]).admin(),
        rt("settings", "siloforge_settings_update", "Update settings", "Change any subset of settings, e.g. {resources: {max_concurrent_jobs: 4}, compute: {embed_mode: \"hybrid\"}}. Validated and persisted; returns the new settings and `restart_required` (use siloforge_system_restart for those).", W, put(), "/api/v2/settings", vec![
            b("resources", Ty::Obj, "cpu_threads, max_concurrent_jobs, vector_cache_mb, max_memory_mb, low_priority_jobs, defer_when_cpu_above"),
            b("compute", Ty::Obj, "embed_mode (auto|gpu|cpu|hybrid|remote), gpu_enabled, hybrid_gpu_share, embed_batch_size, embed_chain, remote_model, remote_key (write-only)"),
            b("wordpress", Ty::Obj, "default_insert_modules, verify_after_inject, link limits, request_timeout_ms, user_agent"),
            b("crawl", Ty::Obj, "concurrency, timeout_ms, max_pages, allow_private_hosts"),
            b("llm", Ty::Obj, "fallback_url, fallback_models, timeout_ms"),
            b("server", Ty::Obj, "binds, cors_origins, allow_registration, mcp_http_enabled"),
            b("backup", Ty::Obj, "dir, keep, schedule, passphrase (write-only)"),
            b("monitor", Ty::Obj, "sample_secs, retain_days, alert thresholds"),
        ]).admin().need_body(),
        // ---------------- system
        rt("system", "siloforge_system_stats", "Live system gauges", "CPU, memory, disk, uptime and queue gauges.", R, get(), "/api/v2/system/stats", vec![]).admin(),
        rt("system", "siloforge_system_metrics", "System metrics history", "CPU/RAM/queue samples over a window.", R, get(), "/api/v2/system/metrics", vec![q("window", Ty::Enum(&["1h", "24h", "7d"]), "default 1h")]).admin(),
        rt("system", "siloforge_system_restart", "Restart SiloForge", "Restart the SiloForge process (takes seconds; running jobs are interrupted and resume/retry per their kind). Apply settings listed in restart_required.", D, post(), "/api/v2/system/restart", vec![]).admin(),
        rt("system", "siloforge_list_errors", "List tracked errors", "Grouped server errors (fingerprint, count, last seen, resolved).", R, get(), "/api/v2/errors", vec![
            q("resolved", Ty::Enum(&["all", "true", "false"]), "default all"),
            limit(1000),
        ]).admin(),
        rt("system", "siloforge_resolve_error", "Resolve a tracked error", "Mark an error group as resolved.", W, post(), "/api/v2/errors/{fingerprint}/resolve", vec![pid("fingerprint", "error fingerprint")]).admin(),
        rt("system", "siloforge_audit_log", "Audit log", "Security/admin audit trail, newest first (page with before_id).", R, get(), "/api/v2/audit", vec![limit(1000), q("before_id", Ty::Int(0, i64::MAX), "")]).admin(),
        // ---------------- backups (admin)
        rt("backups", "siloforge_list_backups", "List backups", "Backups (id, kind, status, size, encrypted, created_at) and the backup dir.", R, get(), "/api/v2/backups", vec![limit(1000)]).admin(),
        rt("backups", "siloforge_get_backup", "Get a backup", "One backup row.", R, get(), "/api/v2/backups/{backup_id}", vec![pid("backup_id", "backup id")]).admin(),
        rt("backups", "siloforge_verify_backup", "Verify a backup", "Check a backup archive's integrity (and decryption, with the configured or given passphrase). Returns {ok, manifest?, error?}.", R, post(), "/api/v2/backups/{backup_id}/verify", vec![
            pid("backup_id", "backup id"),
            b("passphrase", s(1024), "only when not the configured one"),
        ]).admin(),
        rt("backups", "siloforge_delete_backup", "Delete a backup", "Delete a backup archive and its row.", D, del(), "/api/v2/backups/{backup_id}", vec![pid("backup_id", "backup id")]).admin(),
        // ---------------- notifications (admin)
        rt("notifications", "siloforge_mark_notifications_read", "Mark notifications read", "Mark notifications as read: {ids: [..]} or {all: true} (nothing = all).", W, post(), "/api/v2/notifications/read", vec![
            b("ids", Ty::IntArr, "notification ids"),
            b("all", Ty::Bool, ""),
        ]).admin(),
        rt("notifications", "siloforge_list_notification_channels", "List notification channels", "Channels (secrets masked) plus the supported kinds and events.", R, get(), "/api/v2/notifications/channels", vec![]).admin(),
        rt("notifications", "siloforge_create_notification_channel", "Create a notification channel", "Add a channel (e.g. webhook, discord, slack, telegram, email) for chosen events.", W, post(), "/api/v2/notifications/channels", vec![
            b("name", s(200), "").req(),
            b("kind", s(64), "channel kind (see siloforge_list_notification_channels)").req(),
            b("config", Ty::Obj, "kind-specific config (url, token, chat_id, ...)"),
            b("events", Ty::StrArr(32), "events, e.g. job.failed, inject.done, backup.failed"),
            b("enabled", Ty::Bool, ""),
        ]).admin(),
        rt("notifications", "siloforge_update_notification_channel", "Update a notification channel", "Change a channel; config is merged key by key and \"***\" keeps a secret.", W, patch(), "/api/v2/notifications/channels/{channel_id}", vec![
            pid("channel_id", "channel id"),
            b("name", s(200), ""),
            b("config", Ty::Obj, ""),
            b("events", Ty::StrArr(32), ""),
            b("enabled", Ty::Bool, ""),
        ]).admin().need_body(),
        rt("notifications", "siloforge_delete_notification_channel", "Delete a notification channel", "Delete a channel.", D, del(), "/api/v2/notifications/channels/{channel_id}", vec![pid("channel_id", "channel id")]).admin(),
        rt("notifications", "siloforge_test_notification_channel", "Test a notification channel", "Send a test message through a channel. Returns {ok, error, attempts}.", W, post(), "/api/v2/notifications/channels/{channel_id}/test", vec![pid("channel_id", "channel id")]).admin(),
        // ---------------- tokens (admin)
        rt("tokens", "siloforge_list_tokens", "List API tokens", "API tokens (prefix, name, scopes, last used, expiry; never the secret).", R, get(), "/api/v2/tokens", vec![
            q("all", Ty::Bool, "admins: every user's tokens"),
            q("include_revoked", Ty::Bool, ""),
        ]).admin(),
        rt("tokens", "siloforge_create_token", "Create an API token", "Create an API token. The secret is NOT returned through MCP (credentials never reach the model): copy it from the SiloForge UI, or create it there.", W, post(), "/api/v2/tokens", vec![
            b("name", s(200), "").req(),
            b("scopes", Ty::StrArr(4), "subset of *, read, write, admin"),
            b("expires_in_days", Ty::Int(1, 3650), ""),
        ]).admin(),
        rt("tokens", "siloforge_update_token", "Update an API token", "Rename a token, change its scopes (never above its parent's), expiry (null = never) or per-token MCP tool policy.", W, patch(), "/api/v2/tokens/{token_id}", vec![
            pid("token_id", "token id"),
            b("name", s(200), ""),
            b("scopes", Ty::StrArr(4), "subset of *, read, write, admin"),
            b("expires_in_days", Ty::Int(1, 3650), "days from now; null removes the expiry").nullable(),
            b("tool_policy", Ty::Obj, "{allow: [tool names]} or null").nullable(),
        ]).admin().need_body(),
        rt("tokens", "siloforge_revoke_token", "Revoke an API token", "Revoke a token immediately (agents using it lose access).", D, del(), "/api/v2/tokens/{token_id}", vec![pid("token_id", "token id")]).admin(),
        // ---------------- MCP
        rt("mcp", "siloforge_mcp_info", "MCP connection info", "MCP status, stdio/HTTP connection details and the tool list with enabled flags.", R, get(), "/api/v2/mcp", vec![]),
        rt("mcp", "siloforge_mcp_policy_get", "Get MCP policy", "The MCP tool policy: http_enabled, per-tool {enabled, min_scope}, per-token allow lists.", R, get(), "/api/v2/mcp/policy", vec![]).admin(),
        rt("mcp", "siloforge_mcp_policy_update", "Update MCP policy", "Change the MCP tool policy (applies to every MCP client within 60 s). Fields as in siloforge_mcp_policy_get.", D, put(), "/api/v2/mcp/policy", vec![
            b("http_enabled", Ty::Bool, "serve MCP over HTTP at /mcp"),
            b("tools", Ty::Obj, "{\"<tool>\": {enabled, min_scope}}"),
            b("per_token", Ty::Obj, "{\"<token id>\": {allow: [..] | null}}"),
        ]).admin().need_body(),
        rt("mcp", "siloforge_mcp_stats", "MCP usage stats", "Per tool: calls and errors in the last 24 h, last call time.", R, get(), "/api/v2/mcp/stats", vec![]).admin(),
        // ---------------- logs (admin)
        rt("logs", "siloforge_logs_query", "Query the unified log", "Search the unified event log (UI/API/MCP/job/system/auth/notify), newest first; page with before_id = next_before_id.", R, get(), "/api/v2/logs", vec![
            q("source", Ty::Enum(LOG_SOURCES), ""),
            q("actor", s(200), "user:<id> | token:<name> | mcp:<token> | system"),
            P::new("site", Ty::Site, Loc::Query, SITE_DESC).wire("site_id"),
            q("status", Ty::Enum(LOG_STATUSES), ""),
            q("q", s(200), "free-text search"),
            q("since", s(64), "RFC 3339 time or relative like 24h"),
            q("until", s(64), "RFC 3339 time"),
            limit(1000),
            q("before_id", Ty::Int(0, i64::MAX), ""),
        ]).admin(),
        rt("logs", "siloforge_logs_stats", "Log statistics", "Event counts by source and status.", R, get(), "/api/v2/logs/stats", vec![q("since", s(64), "RFC 3339 time or relative like 24h")]).admin(),
        // ---------------- docs / downloads
        rt("docs", "siloforge_docs_list", "List user-guide pages", "The built-in SiloForge user guide: slug, title, section.", R, get(), "/api/v2/docs", vec![]),
        rt("docs", "siloforge_docs_get", "Read a user-guide page", "One user-guide page as Markdown.", R, get(), "/api/v2/docs/{slug}", vec![pid("slug", "page slug from siloforge_docs_list")]).post(Post::Markdown),
        rt("downloads", "siloforge_downloads_list", "List downloads", "Downloadable app and WordPress plugin builds (version, size, sha256, latest).", R, get(), "/api/v2/downloads", vec![]),
    ]
}

fn custom_defs() -> Vec<ToolDef> {
    vec![
        def(
            CAPABILITIES,
            "What can SiloForge do?",
            "Self-orientation: the tools you may use, grouped by area with usage notes and the typical workflow (crawl -> plan -> review -> approve -> inject -> verify -> undo). Call this first.",
            json!({"type":"object","properties":{}}),
            true,
            false,
        )
        .in_category("mcp"),
        def(
            DOCTOR,
            "Health check (doctor-lite)",
            "One-call health check: server health, system block (admin), AI provider health (admin), open tracked errors (admin) and the MCP policy source. Parts your token cannot read are reported, not failed.",
            json!({"type":"object","properties":{}}),
            true,
            false,
        )
        .in_category("system"),
        def(
            IMPORT_POSTS_CSV,
            "Import posts from CSV",
            "Upsert posts from CSV text (header `url,content_html[,title]`), for sites without WP REST. One transaction.",
            json!({"type":"object","properties":{
                "site":{"type":"string","description":SITE_DESC},
                "csv":{"type":"string","description":"CSV text with a header row"}
            },"required":["site","csv"]}),
            false,
            false,
        )
        .in_category("posts"),
        def(
            API_REQUEST,
            "Raw SiloForge API request (escape hatch)",
            "Call any SiloForge REST route directly: method + path under /api/v1/ or /api/v2/ (no `..`, `//`, encoded separators, query in the path, or absolute URLs; auth, plugin-sync and token-minting routes are refused). Non-GET requests need `confirm: true`. Admin only and disabled in the default MCP policy; prefer the dedicated tools.",
            json!({"type":"object","properties":{
                "method":{"type":"string","enum":HTTP_METHODS},
                "path":{"type":"string","description":"e.g. /api/v2/system"},
                "query":{"type":"object","description":"query parameters (string/number/boolean values)"},
                "body":{"description":"JSON body for POST/PUT/PATCH"},
                "confirm":{"type":"boolean","description":"required (true) for every non-GET request"}
            },"required":["method","path"]}),
            false,
            true,
        )
        .in_category("api")
        .admin()
        .disabled_by_default(),
    ]
}

/// v2.1 tool definitions, in catalogue order.
pub(crate) fn defs() -> Vec<ToolDef> {
    let mut out: Vec<ToolDef> = ROUTES.iter().map(|r| r.def.clone()).collect();
    out.extend(custom_defs());
    out
}

/// True for the tools dispatched here.
pub(crate) fn handles(name: &str) -> bool {
    CUSTOM.contains(&name) || ROUTES.iter().any(|r| r.def.name == name)
}

pub(crate) async fn call(
    api: &Api,
    name: &str,
    a: &Args<'_>,
    _ct: &CancellationToken,
) -> ToolResult<Out> {
    match name {
        CAPABILITIES => Ok(capabilities(api)),
        DOCTOR => Ok(doctor(api).await),
        IMPORT_POSTS_CSV => import_posts_csv(api, a).await,
        API_REQUEST => api_request(api, a).await,
        _ => match ROUTES.iter().find(|r| r.def.name == name) {
            Some(r) => exec(api, r, a).await,
            None => Err(ToolError::new(format!(
                "unknown tool: {}",
                crate::output::truncate_str(name, 100)
            ))),
        },
    }
}

struct Built {
    path: String,
    query: Vec<(&'static str, Option<String>)>,
    body: Map<String, Value>,
}

fn place(p: &P, v: Option<Value>, out: &mut Built) -> ToolResult<()> {
    let Some(v) = v else {
        return Ok(());
    };
    match p.loc {
        Loc::Path => {
            let text = as_text(&v).ok_or_else(|| bad(format!("`{}` is not a valid id", p.key)))?;
            let text = check_id(p.key, &text)?;
            out.path = out.path.replace(&format!("{{{}}}", p.key), &text);
        }
        Loc::Query => {
            let text = as_text(&v).ok_or_else(|| bad(format!("`{}` must be a scalar", p.key)))?;
            out.query.push((p.wire_name(), Some(text)));
        }
        Loc::Body => {
            out.body.insert(p.wire_name().to_string(), v);
        }
    }
    Ok(())
}

async fn exec(api: &Api, r: &RouteTool, a: &Args<'_>) -> ToolResult<Out> {
    let mut built = Built {
        path: r.path.to_string(),
        query: Vec::new(),
        body: Map::new(),
    };
    // Validate everything that needs no network first, then resolve sites.
    for p in r.params.iter().filter(|p| !matches!(p.ty, Ty::Site)) {
        let v = extract(a, p)?;
        place(p, v, &mut built)?;
    }
    if r.need_body && built.body.is_empty() {
        let keys: Vec<&str> = r
            .params
            .iter()
            .filter(|p| p.loc == Loc::Body)
            .map(|p| p.key)
            .collect();
        return Err(bad(format!(
            "nothing to change: give at least one of {}",
            keys.join(", ")
        )));
    }
    for p in r.params.iter().filter(|p| matches!(p.ty, Ty::Site)) {
        let v = match a.str_opt(p.key)?.filter(|s| !s.trim().is_empty()) {
            Some(s) => {
                let id = api.resolve_site(&s).await?;
                Some(Value::String(check_id(p.key, &id)?))
            }
            None if p.req => {
                return Err(bad(format!("`{}` is required ({SITE_DESC})", p.key)));
            }
            None => None,
        };
        place(p, v, &mut built)?;
    }
    if built.path.contains('{') {
        return Err(ToolError::new("internal: unfilled path parameter"));
    }
    for (k, v) in &r.query_consts {
        built.query.push((k, Some((*v).to_string())));
    }
    for (k, v) in &r.body_consts {
        built.body.insert((*k).to_string(), v.clone());
    }
    let body = match r.wrap {
        Some(w) => json!({ w: Value::Object(built.body) }),
        None => Value::Object(built.body),
    };
    let req_body =
        (r.method != Method::GET && r.method != Method::DELETE).then_some(ReqBody::Json(body));
    let full = format!("{}{}", built.path, query(&built.query));
    let res = api.call_body(r.method.clone(), &full, req_body).await?;
    Ok(match res {
        ApiBody::Text(t) => Out::Text(t),
        ApiBody::Json(v) => match r.post {
            Post::Plain => Out::Json(v),
            Post::Pick(keys) => {
                let mut m = Map::new();
                for k in keys {
                    m.insert((*k).to_string(), v.get(*k).cloned().unwrap_or(Value::Null));
                }
                Out::Json(Value::Object(m))
            }
            Post::Markdown => match v.get("markdown").and_then(Value::as_str) {
                Some(md) => {
                    let title = v.get("title").and_then(Value::as_str).unwrap_or("");
                    Out::Text(if title.is_empty() {
                        md.to_string()
                    } else {
                        format!("# {title}\n\n{md}")
                    })
                }
                None => Out::Json(v),
            },
        },
    })
}

// ---------------------------------------------------------------- custom tools

async fn import_posts_csv(api: &Api, a: &Args<'_>) -> ToolResult<Out> {
    let csv = match a.raw("csv") {
        Some(Value::String(s)) if s.len() > MAX_CSV_BYTES => {
            return Err(bad(format!(
                "`csv` is too large (max {MAX_CSV_BYTES} bytes)"
            )));
        }
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        _ => return Err(bad("`csv` is required (CSV text)".into())),
    };
    let id = crate::tools::site_id(api, a).await?;
    let res = api
        .call_body(
            Method::POST,
            &format!("/api/v1/csv/import/{id}"),
            Some(ReqBody::Text(csv, "text/csv; charset=utf-8")),
        )
        .await?;
    Ok(Out::Json(res.into_json()))
}

/// Paths the escape hatch refuses (credentials, plugin channel, token minting, binary downloads).
fn blocked_api_route(method: &str, path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    if lower.starts_with("/api/v1/auth/") || lower.starts_with("/api/v2/auth/") {
        return Some("authentication routes are not available through MCP");
    }
    if lower == "/api/v1/sync"
        || lower.starts_with("/api/v1/sync/")
        || lower.starts_with("/api/v1/suggestions/")
    {
        return Some("the WordPress plugin channel is not available through MCP");
    }
    if method == "POST" && lower.trim_end_matches('/') == "/api/v2/tokens" {
        return Some(
            "minting API tokens is not available through the escape hatch (use siloforge_create_token)",
        );
    }
    if lower.ends_with("/download") || lower.starts_with("/api/v2/downloads/") {
        return Some("file downloads are not available through MCP");
    }
    None
}

/// Validates an escape-hatch path: `/api/v1/...` or `/api/v2/...`, plain printable ASCII, no
/// traversal or encoded separators, no query/fragment (pass `query` instead).
pub(crate) fn validate_api_path(path: &str) -> Result<(), String> {
    if path.len() > 1024 {
        return Err("path is too long (max 1024)".into());
    }
    if !(path.starts_with("/api/v1/") || path.starts_with("/api/v2/")) {
        return Err("path must start with /api/v1/ or /api/v2/".into());
    }
    if !path.bytes().all(|c| {
        c.is_ascii_graphic()
            && !matches!(
                c,
                b'\\' | b'?' | b'#' | b'"' | b'<' | b'>' | b'`' | b'{' | b'}' | b'|' | b'^'
            )
    }) {
        return Err(
            "path contains characters that are not allowed (put query parameters in `query`)"
                .into(),
        );
    }
    if path.contains("//") || path.split('/').any(|seg| seg == ".." || seg == ".") {
        return Err("path must not contain `//`, `.` or `..` segments".into());
    }
    let lower = path.to_ascii_lowercase();
    if ["%2e", "%2f", "%5c", "%00", "%25"]
        .iter()
        .any(|e| lower.contains(e))
    {
        return Err("path must not contain encoded dots, slashes or percent signs".into());
    }
    if lower.contains("://") || lower.contains('@') {
        return Err("absolute URLs are not allowed".into());
    }
    Ok(())
}

async fn api_request(api: &Api, a: &Args<'_>) -> ToolResult<Out> {
    let method = a.str_req("method")?.trim().to_ascii_uppercase();
    if !HTTP_METHODS.contains(&method.as_str()) {
        return Err(bad(format!(
            "`method` must be one of {}",
            HTTP_METHODS.join("|")
        )));
    }
    let path = a.str_req("path")?;
    let path = path.trim();
    validate_api_path(path).map_err(bad)?;
    if let Some(why) = blocked_api_route(&method, path) {
        return Err(bad(why.into()));
    }
    let mut pairs: Vec<(String, String)> = Vec::new();
    match a.raw("query") {
        None => {}
        Some(Value::Object(m)) if m.len() <= 50 => {
            for (k, v) in m {
                let text = as_text(v).ok_or_else(|| {
                    bad("`query` values must be strings, numbers or booleans".into())
                })?;
                if k.len() > 200 || text.len() > MAX_STR {
                    return Err(bad("`query` entry is too long".into()));
                }
                pairs.push((k.clone(), text));
            }
        }
        Some(_) => {
            return Err(bad(
                "`query` must be an object with at most 50 entries".into()
            ));
        }
    }
    let body = match a.raw("body") {
        None => None,
        Some(v) if v.to_string().len() > MAX_API_BODY_BYTES => {
            return Err(bad(format!(
                "`body` is too large (max {MAX_API_BODY_BYTES} bytes)"
            )));
        }
        Some(v) => Some(v.clone()),
    };
    if method != "GET" && a.bool_opt("confirm")? != Some(true) {
        return Err(ToolError::new(format!(
            "{API_REQUEST} with {method} changes data: call it again with `confirm: true` to proceed (nothing was sent)"
        )));
    }
    if method == "GET" && body.is_some() {
        return Err(bad("GET requests take no `body`".into()));
    }
    let m =
        Method::from_bytes(method.as_bytes()).map_err(|_| bad("`method` is not valid".into()))?;
    let qs = if pairs.is_empty() {
        String::new()
    } else {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &pairs {
            ser.append_pair(k, v);
        }
        format!("?{}", ser.finish())
    };
    let req_body = match (&m, body) {
        (&Method::GET, _) => None,
        (_, Some(b)) => Some(ReqBody::Json(b)),
        (_, None) if m == Method::DELETE => None,
        (_, None) => Some(ReqBody::Json(json!({}))),
    };
    Ok(
        match api.call_body(m, &format!("{path}{qs}"), req_body).await? {
            ApiBody::Text(t) => Out::Text(t),
            ApiBody::Json(v) => Out::Json(v),
        },
    )
}

const CATEGORIES: &[(&str, &str)] = &[
    (
        "sites",
        "Register and configure sites. `site` arguments accept an id or a domain everywhere.",
    ),
    (
        "runs",
        "Background jobs that read sites and produce SUGGESTED edges (crawl, plan, audit, keyword linker, link health, Search Console). Each returns a job_run_id.",
    ),
    (
        "jobs",
        "Follow and control background jobs: wait_for_job blocks until done; job_logs tails progress.",
    ),
    (
        "edges",
        "Review suggestions: list, edit anchors, approve/reject. Approval is the gate before anything is written.",
    ),
    (
        "inject",
        "WRITES TO WORDPRESS: inject approved edges / undo applied ones from snapshots.",
    ),
    ("rules", "Keyword-linker rules and the anchor ignore list."),
    (
        "schedules",
        "Recurring crawl/plan/link_health/audit jobs per site.",
    ),
    (
        "reports",
        "Read-only analysis: dashboard, links, anchors, orphans, cannibalization, trends, link health, CSV exports.",
    ),
    (
        "silo",
        "Topic clusters: list, export, import (replace) custom clusters.",
    ),
    (
        "posts",
        "Crawled posts: list/search, CSV import for non-WordPress sites.",
    ),
    (
        "ai",
        "AI providers, models, fallback chains and rungs (admin).",
    ),
    (
        "settings",
        "Server settings incl. resources and CPU/GPU compute (admin).",
    ),
    (
        "system",
        "Health, metrics, tracked errors, audit log, restart (admin parts).",
    ),
    ("backups", "Create, list, verify, delete backups (admin)."),
    ("notifications", "Notification inbox and channels (admin)."),
    (
        "tokens",
        "API tokens: list, create, change scopes, revoke (admin).",
    ),
    (
        "mcp",
        "Orientation (siloforge_capabilities), MCP connection info, tool policy and usage stats.",
    ),
    ("logs", "Unified event log search and stats (admin)."),
    ("docs", "The built-in user guide."),
    ("downloads", "Downloadable app and WordPress plugin builds."),
    ("api", "Raw REST escape hatch (admin, disabled by default)."),
];

const WORKFLOW: &[&str] = &[
    "1. crawl: siloforge_crawl_site(site) -> siloforge_wait_for_job(job_id)  (or siloforge_run_pipeline for crawl+plan)",
    "2. plan: siloforge_plan_site(site, topology?) -> siloforge_wait_for_job  (suggestions only)",
    "3. review: siloforge_list_edges(site, status=suggested), siloforge_anchor_report, siloforge_edit_anchor",
    "4. approve: siloforge_bulk_decide(edge_ids, decision=approved)  (the approval gate)",
    "5. inject: siloforge_bulk_inject(site, edge_ids?) -> siloforge_wait_for_job  (WRITES to WordPress)",
    "6. verify: siloforge_link_history(site), siloforge_dashboard(site), siloforge_link_health_run",
    "7. undo: siloforge_bulk_undo(site, edge_ids?)  (reverts exactly the inserted links)",
];

fn capabilities(api: &Api) -> Out {
    let policy = api.policy.clone();
    let allowed = |name: &str| policy.as_ref().is_none_or(|p| p.allows(name));
    let mut groups = Vec::new();
    let mut shown = 0usize;
    let mut hidden = 0usize;
    for (g, note) in CATEGORIES {
        let mut tools = Vec::new();
        for t in catalogue_ref().iter().filter(|t| t.category == *g) {
            if !allowed(t.name) {
                hidden += 1;
                continue;
            }
            shown += 1;
            let mut row = json!({
                "name": t.name,
                "title": t.title,
                "min_scope": t.min_scope,
            });
            if t.read_only {
                row["read_only"] = json!(true);
            }
            if t.destructive {
                row["destructive"] = json!(true);
            }
            if crate::tools::needs_confirm(t.name) {
                row["needs_confirm"] = json!(true);
            }
            tools.push(row);
        }
        if !tools.is_empty() {
            groups.push(json!({"category": g, "notes": note, "tools": tools}));
        }
    }
    let (source, scope) = match &policy {
        Some(p) => (p.source.as_str(), p.scope.clone()),
        None => ("unknown", None),
    };
    Out::Summary(json!({
        "server": "SiloForge",
        "version": sf_core::VERSION,
        "policy": {"source": source, "scope": scope, "tools_available": shown, "tools_hidden": hidden},
        "workflow": WORKFLOW,
        "rules": [
            "`site` accepts a site id or a domain.",
            "Nothing is written to WordPress until edges are approved AND siloforge_bulk_inject runs; siloforge_bulk_undo reverts from snapshots.",
            "Destructive v2.1 tools need `confirm: true` (they refuse and send nothing otherwise).",
            "Job tools return job_run_id: chain them with siloforge_wait_for_job.",
            "Credentials (WP passwords, API keys, tokens) are never returned through MCP.",
            "Read the user guide with siloforge_docs_list / siloforge_docs_get.",
        ],
        "categories": groups,
    }))
}

async fn doctor(api: &Api) -> Out {
    let (health, system, ai, errors) = tokio::join!(
        api.get("/health"),
        api.get("/api/v2/system"),
        api.get("/api/v1/providers/health"),
        api.get("/api/v2/errors?resolved=false&limit=20"),
    );
    let part = |r: ToolResult<Value>| match r {
        Ok(v) => json!({"ok": true, "data": v}),
        Err(e) => json!({"ok": false, "error": e.0}),
    };
    let healthy = health
        .as_ref()
        .ok()
        .and_then(|h| h.get("ok"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let open_errors = errors
        .as_ref()
        .ok()
        .and_then(|e| e.get("errors"))
        .and_then(Value::as_array)
        .map(Vec::len);
    let policy = api
        .policy
        .as_ref()
        .map(|p| json!({"source": p.source.as_str(), "tools_allowed": p.allowed_count()}));
    Out::Summary(json!({
        "ok": healthy,
        "open_errors": open_errors,
        "mcp_policy": policy,
        "checks": {
            "health": part(health),
            "system": part(system),
            "ai_providers": part(ai),
            "errors": part(errors),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_paths_are_validated() {
        for ok in [
            "/api/v1/sites",
            "/api/v2/system/metrics",
            "/api/v2/jobs/abc-1/logs",
        ] {
            assert!(validate_api_path(ok).is_ok(), "{ok}");
        }
        for bad_path in [
            "/api/v3/x",
            "http://evil/api/v1/sites",
            "//evil.com/api/v1/x",
            "/api/v1/../v1/auth/login",
            "/api/v1//sites",
            "/api/v1/sites/%2e%2e/x",
            "/api/v1/sites%2Fx",
            "/api/v1/sites?x=1",
            "/api/v1/sites#x",
            "/api/v1/si tes",
            "/api/v1/sites\\x",
            "/api/v1/./sites",
            "api/v1/sites",
            "/api/v1/\u{0}",
            "/api/v1/日本",
        ] {
            assert!(validate_api_path(bad_path).is_err(), "{bad_path}");
        }
        assert!(blocked_api_route("POST", "/api/v1/auth/login").is_some());
        assert!(blocked_api_route("POST", "/api/v2/tokens").is_some());
        assert!(blocked_api_route("GET", "/api/v2/tokens").is_none());
        assert!(blocked_api_route("GET", "/api/v2/backups/x/download").is_some());
        assert!(blocked_api_route("POST", "/api/v1/sync").is_some());
    }

    #[test]
    fn route_tools_are_consistent() {
        let mut seen = std::collections::HashSet::new();
        for r in ROUTES.iter() {
            assert!(seen.insert(r.def.name), "duplicate {}", r.def.name);
            assert!(r.def.name.starts_with("siloforge_"));
            assert!(!r.def.category.is_empty());
            // every `{placeholder}` has a path parameter and vice versa
            for p in r.params.iter().filter(|p| p.loc == Loc::Path) {
                assert!(
                    r.path.contains(&format!("{{{}}}", p.key)),
                    "{}: {}",
                    r.def.name,
                    p.key
                );
            }
            let n = r.path.matches('{').count();
            assert_eq!(
                n,
                r.params.iter().filter(|p| p.loc == Loc::Path).count(),
                "{}",
                r.def.name
            );
            assert!(r.path.starts_with("/api/v1/") || r.path.starts_with("/api/v2/"));
            if r.def.destructive {
                assert!(!r.def.read_only);
            }
        }
        const ALLOWED: &[&str] = &[
            "sites",
            "runs",
            "edges",
            "inject",
            "rules",
            "schedules",
            "reports",
            "silo",
            "posts",
            "ai",
            "settings",
            "system",
            "jobs",
            "backups",
            "notifications",
            "tokens",
            "mcp",
            "logs",
            "docs",
            "downloads",
            "api",
        ];
        for (c, _) in CATEGORIES {
            assert!(ALLOWED.contains(c), "{c}");
        }
        for g in catalogue_ref().iter().map(|t| t.category) {
            assert!(
                CATEGORIES.iter().any(|(n, _)| *n == g),
                "category {g} has no notes"
            );
        }
    }
}
