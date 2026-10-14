//! Tool catalogue and dispatch. The 20 v1 tool names, parameter names and semantics are kept
//! exactly (Hermes kanban cards and skills reference them, R17); v2 adds job/system tools.

use std::time::{Duration, Instant};

use reqwest::Method;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::client::{Api, ApiBody, ToolError, ToolResult};
use crate::output::fit_json;

/// What a tool produced, before rendering.
pub enum Out {
    /// JSON rendered as pretty text (v1 shape).
    Json(Value),
    /// Raw text (CSV).
    Text(String),
    /// JSON rendered as text and also returned as structured content.
    Summary(Value),
}

/// Static description of one tool.
pub struct ToolDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub schema: Value,
    pub read_only: bool,
    pub destructive: bool,
}

const SITE_DESC: &str = "site id or domain (e.g. example.com)";
const EDGE_STATUSES: &str = "suggested|approved|applied|reverted|rejected|failed";
const ANCHOR_TYPES: &[&str] = &[
    "exact", "partial", "branded", "generic", "lsi", "naked", "cta",
];
/// All five server topologies (v2 accepts `bidirectional`, fixing v1 B3; pilot 2026-10-13 found MCP still refused it).
const TOPOLOGIES: &[&str] = &["reverse", "serial", "priority", "circle", "bidirectional"];
const DECISIONS: &[&str] = &["approved", "rejected"];
const MAX_STR: usize = 4096;
const MAX_IDS: usize = 10_000;
const WAIT_MAX_SECS: u64 = 900;
const PIPELINE_MAX_SECS: u64 = 1800;
/// L-11: long-held waits (wait_for_job / run_pipeline) allowed at once per process.
pub const MAX_LONG_WAITS: usize = 8;
static LONG_WAITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_LONG_WAITS);

fn long_wait_permit() -> ToolResult<tokio::sync::SemaphorePermit<'static>> {
    LONG_WAITS.try_acquire().map_err(|_| {
        ToolError::new(format!(
            "too many concurrent long waits (max {MAX_LONG_WAITS}); poll siloforge_job_status instead or retry shortly"
        ))
    })
}
const APPROVAL_GATE: &str = "Nothing is written to WordPress until edges are approved (siloforge_decide_edge / siloforge_bulk_decide) and siloforge_bulk_inject is called.";

fn site_only() -> Value {
    json!({"type":"object","properties":{"site":{"type":"string","description":SITE_DESC}},"required":["site"]})
}

fn job_only(extra: &str) -> Value {
    json!({"type":"object","properties":{"job_id":{"type":"string","description":format!("job run id (job_run_id){extra}")}},"required":["job_id"]})
}

fn def(
    name: &'static str,
    title: &'static str,
    description: &'static str,
    schema: Value,
    read_only: bool,
    destructive: bool,
) -> ToolDef {
    ToolDef {
        name,
        title,
        description,
        schema,
        read_only,
        destructive,
    }
}

/// All tools, v1 first (in v1 order), then v2.
pub fn catalogue() -> Vec<ToolDef> {
    vec![
        // ---------------- v1 read ----------------
        def(
            "siloforge_list_sites",
            "List sites",
            "List all SILOFORGE sites (id, domain, mode, post/edge counts) the caller owns. Credentials (api_key, WP app passwords) are never included.",
            json!({"type":"object","properties":{}}),
            true,
            false,
        ),
        def(
            "siloforge_dashboard",
            "Site dashboard",
            "Link-health dashboard for a site: health score, orphans, dead-ends, opportunities, http-links, outbound-domains, link-duplication, cannibalization, recent jobs.",
            site_only(),
            true,
            false,
        ),
        def(
            "siloforge_list_edges",
            "List candidate/link edges",
            "List link edges for a site, optionally filtered by status (suggested|approved|applied|reverted|rejected|failed) and origin tool (plan|keyword_linker|archive). Sorted by score. Returns `total`; page with `offset` (next_offset is returned when more edges exist) so big sites are not silently cut at the limit.",
            json!({"type":"object","properties":{
                "site":{"type":"string","description":SITE_DESC},
                "status":{"type":"string","description":format!("filter by edge status ({EDGE_STATUSES})")},
                "tool":{"type":"string","description":"filter by origin (plan, keyword_linker, archive)"},
                "limit":{"type":"integer","minimum":1,"maximum":1000,"description":"page size, default 200"},
                "offset":{"type":"integer","minimum":0,"description":"skip this many edges (paging), default 0"}
            },"required":["site"]}),
            true,
            false,
        ),
        def(
            "siloforge_links_report",
            "Links report",
            "Per-post link metrics: inbound, outbound, pagerank, click-depth, Link Flow Score, duplicate count.",
            site_only(),
            true,
            false,
        ),
        def(
            "siloforge_anchor_report",
            "Anchor distribution",
            "Anchor-type distribution + top anchors + over-optimization warnings (exact>40% / generic>30%).",
            site_only(),
            true,
            false,
        ),
        def(
            "siloforge_link_history",
            "Link history",
            "Every applied/reverted internal link (source, target, anchor, tool, status, applied time). Clean DB audit trail — no tracking params. Optional limit (1-100, default 50) and offset for paging.",
            json!({"type":"object","properties":{
                "site":{"type":"string","description":SITE_DESC},
                "limit":{"type":"integer","minimum":1,"maximum":100},
                "offset":{"type":"integer","minimum":0}
            },"required":["site"]}),
            true,
            false,
        ),
        def(
            "siloforge_list_jobs",
            "List jobs",
            "Recent background jobs (crawl/plan/inject/undo/keyword_link/link_health/audit/backup) with status + stats. With only `site`: the site's latest 25 runs (v1 behaviour). Without `site`, or with status/kind/offset filters: all jobs the caller can see, newest first.",
            json!({"type":"object","properties":{
                "site":{"type":"string","description":SITE_DESC},
                "status":{"type":"string","enum":["queued","running","done","failed","cancelled"]},
                "kind":{"type":"string","description":"job kind, e.g. crawl, plan, inject"},
                "limit":{"type":"integer","minimum":1,"maximum":200},
                "offset":{"type":"integer","minimum":0}
            }}),
            true,
            false,
        ),
        def(
            "siloforge_list_clusters",
            "List silo clusters",
            "Semantic silo clusters for a site (label, source auto|custom, member count, pillar).",
            site_only(),
            true,
            false,
        ),
        // ---------------- v1 build ----------------
        def(
            "siloforge_add_site",
            "Add a site",
            "Register a WordPress/other site. Returns the new site (id, domain, mode). The site api_key for the siloforge-connect plugin is NOT returned through MCP; copy it from the SiloForge UI (Sites) if needed.",
            json!({"type":"object","properties":{
                "domain":{"type":"string","description":"bare domain, e.g. example.com"},
                "wp_url":{"type":"string","description":"WordPress base URL if different from https://domain"},
                "mode":{"type":"string","enum":["wp_rest","sitemap"]}
            },"required":["domain"]}),
            false,
            false,
        ),
        def(
            "siloforge_crawl_site",
            "Crawl a site",
            "Enqueue a crawl (pull posts/pages, chunk, hash). Read-only towards WordPress. Returns a job_run_id — follow it with siloforge_wait_for_job (or poll siloforge_job_status / siloforge_list_jobs).",
            site_only(),
            false,
            false,
        ),
        def(
            "siloforge_plan_site",
            "Plan internal links",
            "Run the full NLP plan (embed → keywords → graph → cluster → silo → 11-dim score → anchors) to generate SUGGESTED link edges. Optionally scope to a silo topology and/or include archive/taxonomy pages. Returns a job_run_id. Suggestions only: nothing is written to WordPress until edges are approved and siloforge_bulk_inject is called.",
            json!({"type":"object","properties":{
                "site":{"type":"string","description":SITE_DESC},
                "topology":{"type":"string","enum":TOPOLOGIES},
                "archive":{"type":"boolean","description":"also build the archive/taxonomy silo"},
                "ai_refine":{"type":"boolean","description":"AI anchor refinement for this run (overrides the site setting ai_anchor_refine; validated, falls back to heuristic anchors)"}
            },"required":["site"]}),
            false,
            false,
        ),
        // ---------------- v1 decide / apply ----------------
        def(
            "siloforge_decide_edge",
            "Approve/reject an edge",
            "Approve or reject a single suggested (or reverted) link edge. Approval is the gate: nothing is injected until approved AND siloforge_bulk_inject runs.",
            json!({"type":"object","properties":{
                "edge_id":{"type":"string"},
                "decision":{"type":"string","enum":DECISIONS}
            },"required":["edge_id","decision"]}),
            false,
            false,
        ),
        def(
            "siloforge_bulk_decide",
            "Bulk approve/reject",
            "Approve or reject many edges at once (only edges of sites you own are affected). Approving does not touch WordPress; siloforge_bulk_inject does.",
            json!({"type":"object","properties":{
                "edge_ids":{"type":"array","items":{"type":"string"},"minItems":1},
                "decision":{"type":"string","enum":DECISIONS}
            },"required":["edge_ids","decision"]}),
            false,
            false,
        ),
        def(
            "siloforge_bulk_inject",
            "Bulk inject approved links",
            "WRITES TO THE LIVE SITE. Inject approved edges into WordPress (via WP REST if credentials are set, else marked for plugin pull — 'marked' is NOT live). Snapshots are taken before every change and siloforge_bulk_undo reverts. Only approved edges are injected. Returns a job_run_id; use siloforge_wait_for_job to get applied/marked/failed counts.",
            json!({"type":"object","properties":{
                "site":{"type":"string","description":SITE_DESC},
                "edge_ids":{"type":"array","items":{"type":"string"},"description":"default: all approved"}
            },"required":["site"]}),
            false,
            true,
        ),
        def(
            "siloforge_bulk_undo",
            "Bulk undo applied links",
            "WRITES TO THE LIVE SITE. Revert applied edges from their snapshots. Returns a job_run_id.",
            json!({"type":"object","properties":{
                "site":{"type":"string","description":SITE_DESC},
                "edge_ids":{"type":"array","items":{"type":"string"},"description":"default: all applied"}
            },"required":["site"]}),
            false,
            true,
        ),
        def(
            "siloforge_edit_anchor",
            "Edit an edge anchor",
            "Change the anchor text (and optionally type) of a non-applied edge.",
            json!({"type":"object","properties":{
                "edge_id":{"type":"string"},
                "anchor":{"type":"string"},
                "anchor_type":{"type":"string","enum":ANCHOR_TYPES}
            },"required":["edge_id","anchor"]}),
            false,
            false,
        ),
        // ---------------- v1 rules ----------------
        def(
            "siloforge_add_keyword_rule",
            "Add keyword-linker rule",
            "Add a rule that links a keyword to a target URL wherever it appears (LinkBoss-style Keyword Linker). Run siloforge_run_keyword_linker after; it creates suggestions that still need approval.",
            json!({"type":"object","properties":{
                "site":{"type":"string","description":SITE_DESC},
                "keyword":{"type":"string"},
                "target_url":{"type":"string"},
                "anchor_type":{"type":"string","enum":ANCHOR_TYPES},
                "max_links":{"type":"integer","minimum":1,"maximum":100}
            },"required":["site","keyword","target_url"]}),
            false,
            false,
        ),
        def(
            "siloforge_run_keyword_linker",
            "Run keyword linker",
            "Apply all active keyword rules → generates suggested keyword_linker edges (not written to WordPress). Returns a job_run_id.",
            site_only(),
            false,
            false,
        ),
        def(
            "siloforge_add_ignore_keyword",
            "Add ignore keyword",
            "Add a phrase to the stop-list so it never becomes an anchor (filters anchor generation + keyword linker).",
            json!({"type":"object","properties":{
                "site":{"type":"string","description":SITE_DESC},
                "phrase":{"type":"string"}
            },"required":["site","phrase"]}),
            false,
            false,
        ),
        // ---------------- v1 export ----------------
        def(
            "siloforge_export_edges_csv",
            "Export decided edges as CSV",
            "Return approved+applied edges as CSV (source_url,target_url,anchor,anchor_type,status,insert_module) for manual placement on any CMS.",
            site_only(),
            true,
            false,
        ),
        // ---------------- v2 ----------------
        def(
            "siloforge_system_status",
            "System status",
            "SiloForge server health: version, uptime, CPU/RAM, database, queued/running job counts. The full system block needs an admin user's token with the `admin` scope; other tokens get the /health part (ok, version, uptime, jobs) plus `system_error`.",
            json!({"type":"object","properties":{}}),
            true,
            false,
        ),
        def(
            "siloforge_job_status",
            "Job status",
            "One job run by id: kind, status (queued|running|done|failed|cancelled), progress/stats, error, timestamps.",
            job_only(""),
            true,
            false,
        ),
        def(
            "siloforge_job_logs",
            "Job logs",
            "Log lines of a job run (seq, ts, level, msg). Use `after` (last seq seen) to tail.",
            json!({"type":"object","properties":{
                "job_id":{"type":"string","description":"job run id (job_run_id)"},
                "after":{"type":"integer","minimum":0,"description":"only lines with seq greater than this"},
                "limit":{"type":"integer","minimum":1,"maximum":1000,"description":"default 200"}
            },"required":["job_id"]}),
            true,
            false,
        ),
        def(
            "siloforge_cancel_job",
            "Cancel a job",
            "Cancel a queued or running job. A running inject stops at its next checkpoint; links already applied stay applied (use siloforge_bulk_undo).",
            job_only(""),
            false,
            false,
        ),
        def(
            "siloforge_wait_for_job",
            "Wait for a job",
            "Block until a job reaches done, failed or cancelled (or the timeout passes), then return its final status and stats. Use after crawl/plan/inject/undo/keyword-linker calls to chain multi-step workflows without polling yourself.",
            json!({"type":"object","properties":{
                "job_id":{"type":"string","description":"job run id (job_run_id)"},
                "timeout_secs":{"type":"integer","minimum":1,"maximum":WAIT_MAX_SECS,"description":"default 600, max 900"},
                "poll_secs":{"type":"integer","minimum":1,"maximum":60,"description":"default 5"}
            },"required":["job_id"]}),
            true,
            false,
        ),
        def(
            "siloforge_run_pipeline",
            "Crawl + plan pipeline",
            "Convenience workflow: crawl the site → wait → plan → wait, then return a summary (stats per step, job ids, number of suggested edges). Produces suggestions only. Nothing is written to WordPress until edges are approved (siloforge_decide_edge / siloforge_bulk_decide) and siloforge_bulk_inject is called. If the time budget runs out the summary lists the job ids to continue with siloforge_wait_for_job.",
            json!({"type":"object","properties":{
                "site":{"type":"string","description":SITE_DESC},
                "topology":{"type":"string","enum":TOPOLOGIES},
                "archive":{"type":"boolean","description":"also build the archive/taxonomy silo"},
                "ai_refine":{"type":"boolean","description":"AI anchor refinement for the plan step (overrides the site setting)"},
                "skip_crawl":{"type":"boolean","description":"plan on the existing crawl"},
                "timeout_secs":{"type":"integer","minimum":1,"maximum":PIPELINE_MAX_SECS,"description":"total time budget, default 900, max 1800"}
            },"required":["site"]}),
            false,
            false,
        ),
        def(
            "siloforge_create_backup",
            "Create a backup",
            "Start a full SiloForge backup (database + settings), encrypted when a backup passphrase is configured. Returns {id, status:\"running\", kind, encrypted, secrets_included}; the archive is built in the background. Requires an admin user's token with the `admin` scope.",
            json!({"type":"object","properties":{"note":{"type":"string","description":"optional label"}}}),
            false,
            false,
        ),
        def(
            "siloforge_list_notifications",
            "List notifications",
            "Recent notifications, newest first. Events: job.done, job.failed, plan.ready, inject.done, backup.done, backup.failed, system.alert. Requires an admin user's token with the `admin` scope.",
            json!({"type":"object","properties":{
                "limit":{"type":"integer","minimum":1,"maximum":200,"description":"default 50"},
                "unread_only":{"type":"boolean"}
            }}),
            true,
            false,
        ),
    ]
}

// ---------------------------------------------------------------- argument parsing

struct Args<'a>(&'a Map<String, Value>);

fn bad(msg: String) -> ToolError {
    ToolError::new(format!("invalid arguments: {msg}"))
}

impl Args<'_> {
    fn raw(&self, k: &str) -> Option<&Value> {
        let v = self.0.get(k).filter(|v| !v.is_null());
        // Enqueue tools return `job_run_id`; agents echo that name back (pilot 2026-10-13).
        if v.is_none() && k == "job_id" {
            return self.0.get("job_run_id").filter(|v| !v.is_null());
        }
        v
    }

    fn str_opt(&self, k: &str) -> ToolResult<Option<String>> {
        match self.raw(k) {
            None => Ok(None),
            Some(Value::String(s)) if s.len() > MAX_STR => {
                Err(bad(format!("`{k}` is too long (max {MAX_STR} chars)")))
            }
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(bad(format!("`{k}` must be a string"))),
        }
    }

    fn str_req(&self, k: &str) -> ToolResult<String> {
        match self.str_opt(k)? {
            Some(s) if !s.trim().is_empty() => Ok(s),
            _ => Err(bad(format!("`{k}` is required (string)"))),
        }
    }

    fn enum_opt(&self, k: &str, allowed: &[&str]) -> ToolResult<Option<String>> {
        match self.str_opt(k)? {
            None => Ok(None),
            Some(s) if allowed.contains(&s.as_str()) => Ok(Some(s)),
            Some(_) => Err(bad(format!("`{k}` must be one of {}", allowed.join("|")))),
        }
    }

    fn enum_req(&self, k: &str, allowed: &[&str]) -> ToolResult<String> {
        self.enum_opt(k, allowed)?
            .ok_or_else(|| bad(format!("`{k}` is required ({})", allowed.join("|"))))
    }

    fn int_opt(&self, k: &str, min: i64, max: i64) -> ToolResult<Option<i64>> {
        let n = match self.raw(k) {
            None => return Ok(None),
            Some(Value::Number(n)) => n.as_i64().or_else(|| {
                n.as_f64()
                    .filter(|f| f.fract() == 0.0 && f.abs() < 1e15)
                    .map(|f| f as i64)
            }),
            Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
            Some(_) => None,
        };
        match n {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            _ => Err(bad(format!(
                "`{k}` must be an integer between {min} and {max}"
            ))),
        }
    }

    fn bool_opt(&self, k: &str) -> ToolResult<Option<bool>> {
        match self.raw(k) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => Ok(Some(true)),
            Some(Value::String(s)) if s.eq_ignore_ascii_case("false") => Ok(Some(false)),
            Some(_) => Err(bad(format!("`{k}` must be a boolean"))),
        }
    }

    fn id_req(&self, k: &str) -> ToolResult<String> {
        let s = self.str_req(k)?;
        check_id(k, s.trim())
    }

    fn ids_opt(&self, k: &str) -> ToolResult<Option<Vec<String>>> {
        let Some(v) = self.raw(k) else {
            return Ok(None);
        };
        let arr = v
            .as_array()
            .ok_or_else(|| bad(format!("`{k}` must be an array of strings")))?;
        if arr.len() > MAX_IDS {
            return Err(bad(format!("`{k}` has too many items (max {MAX_IDS})")));
        }
        arr.iter()
            .map(|x| {
                x.as_str()
                    .ok_or_else(|| bad(format!("`{k}` must contain only strings")))
                    .and_then(|s| check_id(k, s.trim()))
            })
            .collect::<ToolResult<Vec<_>>>()
            .map(Some)
    }
}

/// Ids are interpolated into URL paths: only `[A-Za-z0-9_-]`, 1..=128 chars.
fn check_id(k: &str, s: &str) -> ToolResult<String> {
    let ok = !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(s.to_string())
    } else {
        Err(bad(format!("`{k}` is not a valid id")))
    }
}

fn query(pairs: &[(&str, Option<String>)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    let mut any = false;
    for (k, v) in pairs {
        if let Some(v) = v {
            ser.append_pair(k, v);
            any = true;
        }
    }
    if any {
        format!("?{}", ser.finish())
    } else {
        String::new()
    }
}

/// Builds a JSON object from optional fields, dropping `None` (like JSON.stringify drops undefined).
fn body(pairs: Vec<(&str, Option<Value>)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        if let Some(v) = v {
            m.insert(k.to_string(), v);
        }
    }
    Value::Object(m)
}

// ---------------------------------------------------------------- dispatch

/// True when `name` is one of our tools.
pub fn is_known(name: &str) -> bool {
    catalogue().iter().any(|t| t.name == name)
}

/// Runs one tool. `Err` = the tool failed (rendered as `Error: ...` with isError).
pub async fn call(
    api: &Api,
    name: &str,
    args: &Map<String, Value>,
    ct: &CancellationToken,
) -> ToolResult<Out> {
    let a = Args(args);
    let out = match name {
        "siloforge_list_sites" => Out::Json(api.get("/api/v1/sites").await?),
        "siloforge_dashboard" => site_get(api, &a, "/api/v1/dashboard/", "").await?,
        "siloforge_list_edges" => list_edges(api, &a).await?,
        "siloforge_links_report" => site_get(api, &a, "/api/v1/reports/links/", "").await?,
        "siloforge_anchor_report" => site_get(api, &a, "/api/v1/reports/anchors/", "").await?,
        "siloforge_link_history" => {
            let id = site_id(api, &a).await?;
            let q = query(&[
                ("limit", a.int_opt("limit", 1, 100)?.map(|n| n.to_string())),
                (
                    "offset",
                    a.int_opt("offset", 0, i64::from(u32::MAX))?
                        .map(|n| n.to_string()),
                ),
            ]);
            Out::Json(api.get(&format!("/api/v1/history/{id}{q}")).await?)
        }
        "siloforge_list_jobs" => list_jobs(api, &a).await?,
        "siloforge_list_clusters" => {
            let id = site_id(api, &a).await?;
            Out::Json(api.get(&format!("/api/v1/silo/{id}/clusters")).await?)
        }
        "siloforge_add_site" => add_site(api, &a).await?,
        "siloforge_crawl_site" => {
            let id = site_id(api, &a).await?;
            Out::Json(
                api.post(&format!("/api/v1/sites/{id}/crawl"), json!({}))
                    .await?,
            )
        }
        "siloforge_plan_site" => {
            let id = site_id(api, &a).await?;
            Out::Json(
                api.post(&format!("/api/v1/sites/{id}/plan"), plan_body(&a)?)
                    .await?,
            )
        }
        "siloforge_decide_edge" => {
            let edge = a.id_req("edge_id")?;
            let decision = a.enum_req("decision", DECISIONS)?;
            Out::Json(
                api.post(
                    &format!("/api/v1/edges/{edge}/decide"),
                    json!({"decision": decision}),
                )
                .await?,
            )
        }
        "siloforge_bulk_decide" => {
            let ids = a
                .ids_opt("edge_ids")?
                .filter(|v| !v.is_empty())
                .ok_or_else(|| bad("`edge_ids` needs at least one id".into()))?;
            let decision = a.enum_req("decision", DECISIONS)?;
            Out::Json(
                api.post(
                    "/api/v1/bulk/decide",
                    json!({"edge_ids": ids, "decision": decision}),
                )
                .await?,
            )
        }
        "siloforge_bulk_inject" => bulk(api, &a, "/api/v1/bulk/run").await?,
        "siloforge_bulk_undo" => bulk(api, &a, "/api/v1/bulk/undo").await?,
        "siloforge_edit_anchor" => {
            let edge = a.id_req("edge_id")?;
            let anchor = a.str_req("anchor")?;
            let b = body(vec![
                ("anchor", Some(json!(anchor))),
                (
                    "anchor_type",
                    a.enum_opt("anchor_type", ANCHOR_TYPES)?.map(Value::String),
                ),
            ]);
            Out::Json(
                api.call(
                    Method::PATCH,
                    &format!("/api/v1/edges/{edge}/anchor"),
                    Some(b),
                )
                .await?
                .into_json(),
            )
        }
        "siloforge_add_keyword_rule" => {
            let id = site_id(api, &a).await?;
            let b = body(vec![
                ("keyword", Some(json!(a.str_req("keyword")?))),
                ("target_url", Some(json!(a.str_req("target_url")?))),
                (
                    "anchor_type",
                    a.enum_opt("anchor_type", ANCHOR_TYPES)?.map(Value::String),
                ),
                (
                    "max_links",
                    a.int_opt("max_links", 1, 100)?.map(|n| json!(n)),
                ),
            ]);
            Out::Json(api.post(&format!("/api/v1/rules/keywords/{id}"), b).await?)
        }
        "siloforge_run_keyword_linker" => {
            let id = site_id(api, &a).await?;
            Out::Json(
                api.post(&format!("/api/v1/rules/keywords/{id}/run"), json!({}))
                    .await?,
            )
        }
        "siloforge_add_ignore_keyword" => {
            let id = site_id(api, &a).await?;
            let phrase = a.str_req("phrase")?;
            Out::Json(
                api.post(
                    &format!("/api/v1/rules/ignore/{id}"),
                    json!({"phrase": phrase}),
                )
                .await?,
            )
        }
        "siloforge_export_edges_csv" => {
            let id = site_id(api, &a).await?;
            match api
                .call(Method::GET, &format!("/api/v1/csv/export/{id}"), None)
                .await?
            {
                ApiBody::Text(t) => Out::Text(t),
                ApiBody::Json(v) => Out::Json(v),
            }
        }
        "siloforge_system_status" => system_status(api).await,
        "siloforge_job_status" => Out::Json(job_run(api, &a.id_req("job_id")?).await?),
        "siloforge_job_logs" => {
            let id = a.id_req("job_id")?;
            let q = query(&[
                (
                    "after",
                    a.int_opt("after", 0, i64::MAX)?.map(|n| n.to_string()),
                ),
                (
                    "limit",
                    Some(a.int_opt("limit", 1, 1000)?.unwrap_or(200).to_string()),
                ),
            ]);
            Out::Json(api.get(&format!("/api/v2/jobs/{id}/logs{q}")).await?)
        }
        "siloforge_cancel_job" => {
            let id = a.id_req("job_id")?;
            Out::Json(
                api.post(&format!("/api/v2/jobs/{id}/cancel"), json!({}))
                    .await?,
            )
        }
        "siloforge_wait_for_job" => {
            let id = a.id_req("job_id")?;
            let _permit = long_wait_permit()?;
            let timeout = a
                .int_opt("timeout_secs", 1, WAIT_MAX_SECS as i64)?
                .unwrap_or(600);
            let poll = a.int_opt("poll_secs", 1, 60)?.unwrap_or(5);
            let deadline = Instant::now() + Duration::from_secs(timeout.unsigned_abs());
            let w = wait_job(api, &id, deadline, poll_dur(api, poll), ct).await?;
            Out::Summary(w.to_json())
        }
        "siloforge_run_pipeline" => {
            let _permit = long_wait_permit()?;
            run_pipeline(api, &a, ct).await?
        }
        "siloforge_create_backup" => {
            let b = body(vec![("note", a.str_opt("note")?.map(Value::String))]);
            Out::Json(api.post("/api/v2/backups", b).await?)
        }
        "siloforge_list_notifications" => {
            let q = query(&[
                (
                    "limit",
                    Some(a.int_opt("limit", 1, 200)?.unwrap_or(50).to_string()),
                ),
                (
                    "unread",
                    a.bool_opt("unread_only")?
                        .filter(|b| *b)
                        .map(|_| "1".to_string()),
                ),
            ]);
            Out::Json(api.get(&format!("/api/v2/notifications{q}")).await?)
        }
        _ => {
            return Err(ToolError::new(format!(
                "unknown tool: {}",
                crate::output::truncate_str(name, 100)
            )));
        }
    };
    Ok(out)
}

async fn site_id(api: &Api, a: &Args<'_>) -> ToolResult<String> {
    let site = a.str_req("site")?;
    let id = api.resolve_site(&site).await?;
    check_id("site", &id)
}

async fn site_get(api: &Api, a: &Args<'_>, prefix: &str, suffix: &str) -> ToolResult<Out> {
    let id = site_id(api, a).await?;
    Ok(Out::Json(api.get(&format!("{prefix}{id}{suffix}")).await?))
}

fn plan_body(a: &Args<'_>) -> ToolResult<Value> {
    Ok(body(vec![
        (
            "topology",
            a.enum_opt("topology", TOPOLOGIES)?.map(Value::String),
        ),
        (
            "archive",
            a.bool_opt("archive")?.filter(|b| *b).map(Value::Bool),
        ),
        ("ai_refine", a.bool_opt("ai_refine")?.map(Value::Bool)),
    ]))
}

async fn bulk(api: &Api, a: &Args<'_>, path: &str) -> ToolResult<Out> {
    let id = site_id(api, a).await?;
    let b = body(vec![
        ("site_id", Some(json!(id))),
        ("edge_ids", a.ids_opt("edge_ids")?.map(|v| json!(v))),
    ]);
    Ok(Out::Json(api.post(path, b).await?))
}

async fn add_site(api: &Api, a: &Args<'_>) -> ToolResult<Out> {
    let b = body(vec![
        ("domain", Some(json!(a.str_req("domain")?.trim()))),
        ("wp_url", a.str_opt("wp_url")?.map(Value::String)),
        (
            "mode",
            a.enum_opt("mode", &["wp_rest", "sitemap"])?
                .map(Value::String),
        ),
    ]);
    let v = api.post("/api/v1/sites", b).await?;
    api.invalidate_sites();
    Ok(Out::Json(v))
}

async fn list_edges(api: &Api, a: &Args<'_>) -> ToolResult<Out> {
    let id = site_id(api, a).await?;
    let limit = a.int_opt("limit", 1, 1000)?.unwrap_or(200);
    let offset = a.int_opt("offset", 0, i64::from(u32::MAX))?.unwrap_or(0);
    let q = query(&[
        ("status", a.str_opt("status")?),
        ("tool", a.str_opt("tool")?),
        ("limit", Some(limit.to_string())),
        ("offset", (offset > 0).then(|| offset.to_string())),
    ]);
    let mut v = api.get(&format!("/api/v1/edges/{id}{q}")).await?;
    // Keep paging correct even if the output budget forces us to show fewer edges.
    let max = api.cfg().max_output_chars;
    let shown_cap = fit_json(&v, max).shown_items;
    if let Value::Object(m) = &mut v {
        let mut returned = m.get("edges").and_then(Value::as_array).map_or(0, Vec::len);
        if let (Some(n), Some(Value::Array(edges))) = (shown_cap, m.get_mut("edges")) {
            edges.truncate(n);
            returned = n;
            m.insert(
                "note".into(),
                json!(format!(
                    "output size limit: only {n} edges shown; call again with offset=next_offset"
                )),
            );
        }
        let total = m.get("total").and_then(Value::as_i64).unwrap_or(0);
        let next = offset.saturating_add(returned as i64);
        m.insert("offset".into(), json!(offset));
        m.insert("limit".into(), json!(limit));
        m.insert("returned".into(), json!(returned));
        let has_more = next < total;
        m.insert("has_more".into(), json!(has_more));
        if has_more {
            m.insert("next_offset".into(), json!(next));
        }
    }
    Ok(Out::Json(v))
}

async fn list_jobs(api: &Api, a: &Args<'_>) -> ToolResult<Out> {
    let site = a.str_opt("site")?.filter(|s| !s.trim().is_empty());
    let status = a.enum_opt(
        "status",
        &["queued", "running", "done", "failed", "cancelled"],
    )?;
    let kind = a.str_opt("kind")?;
    let limit = a.int_opt("limit", 1, 200)?;
    let offset = a.int_opt("offset", 0, i64::from(u32::MAX))?;
    let filtered = status.is_some() || kind.is_some() || offset.is_some() || limit.is_some();
    let site_id = match &site {
        Some(s) => {
            let id = api.resolve_site(s).await?;
            Some(check_id("site", &id)?)
        }
        None => None,
    };
    if let (Some(id), false) = (&site_id, filtered) {
        return Ok(Out::Json(api.get(&format!("/api/v1/jobs/{id}")).await?));
    }
    let q = query(&[
        ("site_id", site_id),
        ("status", status),
        ("kind", kind),
        ("limit", Some(limit.unwrap_or(50).to_string())),
        ("offset", offset.map(|n| n.to_string())),
    ]);
    Ok(Out::Json(api.get(&format!("/api/v2/jobs{q}")).await?))
}

async fn system_status(api: &Api) -> Out {
    let system = api.get("/api/v2/system").await;
    let health = api.get("/health").await;
    let mut out = Map::new();
    match system {
        Ok(v) => {
            out.insert("system".into(), v);
        }
        Err(e) => {
            out.insert("system_error".into(), json!(e.0));
        }
    }
    match health {
        Ok(h) => {
            out.insert("ok".into(), h.get("ok").cloned().unwrap_or(Value::Null));
            out.insert(
                "version".into(),
                h.get("version").cloned().unwrap_or(Value::Null),
            );
            out.insert(
                "uptime_secs".into(),
                h.get("uptime_secs").cloned().unwrap_or(Value::Null),
            );
            out.insert("jobs".into(), h.get("jobs").cloned().unwrap_or(Value::Null));
        }
        Err(e) => {
            out.insert("health_error".into(), json!(e.0));
        }
    }
    Out::Summary(Value::Object(out))
}

// ---------------------------------------------------------------- jobs

fn poll_dur(api: &Api, secs: i64) -> Duration {
    Duration::from_secs(secs.unsigned_abs()).max(api.cfg().min_poll)
}

/// `GET /api/v1/jobs/run/:id`, unwrapping `{job: {...}}` if the server wraps it.
async fn job_run(api: &Api, id: &str) -> ToolResult<Value> {
    let v = api.get(&format!("/api/v1/jobs/run/{id}")).await?;
    Ok(match v {
        Value::Object(mut m) if m.contains_key("job") && !m.contains_key("status") => {
            m.remove("job").unwrap_or(Value::Null)
        }
        other => other,
    })
}

fn job_state(job: &Value) -> &str {
    job.get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "done" | "failed" | "cancelled")
}

struct Waited {
    job: Value,
    finished: bool,
    waited: Duration,
    interrupted: bool,
}

impl Waited {
    fn to_json(&self) -> Value {
        let status = job_state(&self.job).to_string();
        let mut v = json!({
            "job_id": self.job.get("id").cloned().unwrap_or(Value::Null),
            "kind": self.job.get("kind").cloned().unwrap_or(Value::Null),
            "status": status,
            "finished": self.finished,
            "waited_secs": self.waited.as_secs(),
            "stats": self.job.get("stats").cloned().unwrap_or(Value::Null),
            "error": self.job.get("error").cloned().unwrap_or(Value::Null),
        });
        if !self.finished {
            let why = if self.interrupted {
                "the request was cancelled"
            } else {
                "timeout reached"
            };
            v["note"] = json!(format!(
                "job still {status} ({why}); call siloforge_wait_for_job again with the same job_id"
            ));
        }
        v
    }
}

async fn wait_job(
    api: &Api,
    id: &str,
    deadline: Instant,
    poll: Duration,
    ct: &CancellationToken,
) -> ToolResult<Waited> {
    let started = Instant::now();
    loop {
        let job = job_run(api, id).await?;
        let status = job_state(&job).to_string();
        if is_terminal(&status) {
            return Ok(Waited {
                job,
                finished: true,
                waited: started.elapsed(),
                interrupted: false,
            });
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(Waited {
                job,
                finished: false,
                waited: started.elapsed(),
                interrupted: false,
            });
        }
        let nap = poll.min(deadline.saturating_duration_since(now));
        tokio::select! {
            () = tokio::time::sleep(nap) => {}
            () = ct.cancelled() => {
                return Ok(Waited { job, finished: false, waited: started.elapsed(), interrupted: true });
            }
        }
    }
}

fn job_id_of(v: &Value) -> ToolResult<String> {
    let id = v
        .get("job_run_id")
        .or_else(|| v.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ToolError::new(format!(
                "API returned no job_run_id: {}",
                crate::output::truncate_str(&v.to_string(), 200)
            ))
        })?;
    check_id("job_run_id", id)
}

async fn run_pipeline(api: &Api, a: &Args<'_>, ct: &CancellationToken) -> ToolResult<Out> {
    let id = site_id(api, a).await?;
    let skip_crawl = a.bool_opt("skip_crawl")?.unwrap_or(false);
    let plan = plan_body(a)?;
    let budget = a
        .int_opt("timeout_secs", 1, PIPELINE_MAX_SECS as i64)?
        .unwrap_or(900);
    let deadline = Instant::now() + Duration::from_secs(budget.unsigned_abs());
    let poll = api.cfg().min_poll;
    let mut steps: Vec<Value> = Vec::new();
    let mut finished = true;
    let mut failed = false;

    let mut stage = |name: &str, w: &Waited, job_id: &str| {
        let mut s = w.to_json();
        s["step"] = json!(name);
        s["job_id"] = json!(job_id);
        steps.push(s);
    };

    if !skip_crawl {
        let job = job_id_of(
            &api.post(&format!("/api/v1/sites/{id}/crawl"), json!({}))
                .await?,
        )?;
        let w = wait_job(api, &job, deadline, poll, ct).await?;
        stage("crawl", &w, &job);
        if !w.finished {
            finished = false;
        } else if job_state(&w.job) != "done" {
            failed = true;
        }
    }
    if finished && !failed {
        let job = job_id_of(&api.post(&format!("/api/v1/sites/{id}/plan"), plan).await?)?;
        let w = wait_job(api, &job, deadline, poll, ct).await?;
        stage("plan", &w, &job);
        if !w.finished {
            finished = false;
        } else if job_state(&w.job) != "done" {
            failed = true;
        }
    }
    let suggested = if finished && !failed {
        api.get(&format!("/api/v1/edges/{id}?status=suggested&limit=1"))
            .await
            .ok()
            .and_then(|v| v.get("total").cloned())
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let next = if failed {
        "A step failed; see its error. Fix the cause and re-run (skip_crawl=true to reuse a good crawl).".to_string()
    } else if !finished {
        "Time budget reached; continue with siloforge_wait_for_job on the unfinished job_id."
            .to_string()
    } else {
        format!("Review suggestions with siloforge_list_edges(status=suggested). {APPROVAL_GATE}")
    };
    Ok(Out::Summary(json!({
        "site_id": id,
        "ok": finished && !failed,
        "finished": finished,
        "steps": steps,
        "suggested_edges": suggested,
        "written_to_wordpress": false,
        "next": next,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_waits_are_bounded() {
        let held: Vec<_> = (0..MAX_LONG_WAITS)
            .filter_map(|_| long_wait_permit().ok())
            .collect();
        assert_eq!(held.len(), MAX_LONG_WAITS);
        assert!(long_wait_permit().is_err());
        drop(held);
        assert!(long_wait_permit().is_ok());
    }
}
