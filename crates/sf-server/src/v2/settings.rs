//! `GET/PUT /api/v2/settings` and `POST /api/v2/system/restart` (contract v2.1 §1).
//!
//! * GET reports every section from the **live** configuration plus read-only facts (logical CPUs,
//!   RAM, GPU, active embedding device) and the keys that only take effect after a restart.
//! * PUT takes any subset `{ "<section>": { "<key>": value } }`, validates every key first
//!   (nothing is written when one is bad), persists non-secret keys to `<home>/siloforge.toml`
//!   with `toml_edit` (comments and unknown keys kept) and secrets to `<home>/secrets.env`
//!   (only the changed `KEY=` lines), then swaps the live config so services pick the values up
//!   on their next use. A key whose value comes from a `secrets.env` override (e.g. `EMBED_CHAIN`,
//!   `DARIO_URL`) is written to that line instead, so the change survives a restart.
//!   Secrets are write-only: `"***"` means unchanged, `""` clears.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sf_core::Config;
use sf_core::settings_file::{TomlChange, edit_env_file, edit_toml_file};

use crate::auth::{Admin, audit};
use crate::v1::common::JsonBody;
use crate::{ApiError, ApiResult, AppState};

/// Keys whose new value only takes effect after a restart.
const RESTART_KEYS: &[&str] = &[
    "server.binds",
    "server.cors_origins",
    "resources.cpu_threads",
];

const EMBED_MODES: &[&str] = &["auto", "gpu", "cpu", "hybrid", "remote"];
const EMBED_RUNGS: &[&str] = &["remote", "remote2", "onnx", "local"];
const INSERT_MODULES: &[&str] = &["native", "rephrase", "context"];
const SECRET_SET: &str = "***";

/// Process-wide settings facts the binary registers (embedding device, restart support).
/// Absent in tests: GET then reports `active_device: "unknown"`, `restart_supported: false`.
#[derive(Clone)]
pub struct SettingsHooks {
    /// Device of the embedding rung that serves batches now (`directml`, `cpu`, `hybrid ...`).
    pub active_device: Arc<dyn Fn() -> String + Send + Sync>,
    /// Re-launches the process (see `siloforge serve`); called after the 202 is sent.
    pub restart: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Serialises PUTs so two saves never interleave their file writes.
static SAVE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn logical_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn secrets_path(cfg: &Config) -> std::path::PathBuf {
    cfg.home.join("secrets.env")
}

fn file_env(cfg: &Config) -> Vec<(String, String)> {
    std::fs::read_to_string(secrets_path(cfg))
        .map(|t| sf_core::config::parse_env_file(&t))
        .unwrap_or_default()
}

/// Keys set in the process environment (not `secrets.env`) win over every file: report them.
fn process_env_set(k: &str) -> bool {
    std::env::var(k).is_ok_and(|v| !v.is_empty())
}

/// Environment overrides `Config::load` applies on top of the TOML, per settings key.
fn env_override_keys(section: &str, key: &str) -> &'static [&'static str] {
    match (section, key) {
        ("resources", "cpu_threads") => &["SILOFORGE_CPU_THREADS"],
        ("compute", "embed_chain") => &["EMBED_CHAIN"],
        ("compute", "remote_model") => &["EMBED_REMOTE_MODEL"],
        ("llm", "fallback_url") => &["DARIO_URL"],
        ("llm", "fallback_models") => &["DARIO_MODEL_PRIMARY", "DARIO_MODEL_FALLBACK"],
        ("crawl", "allow_private_hosts") => &["CRAWL_ALLOW_PRIVATE_HOSTS"],
        ("server", "binds") => &["SILOFORGE_BINDS"],
        _ => &[],
    }
}

// ---------------------------------------------------------------------------------------------
// GET

async fn mem_total_mb(st: &AppState) -> u64 {
    if let Some(m) = st.services.get::<sf_ops::monitor::Monitor>() {
        let m2 = m.clone();
        if let Ok(c) = tokio::task::spawn_blocking(move || m2.capacity()).await {
            return c.mem_total_mb;
        }
    }
    let cfg = st.live.get();
    tokio::task::spawn_blocking(move || sf_ops::monitor::compute_capacity(&cfg, None).mem_total_mb)
        .await
        .unwrap_or(0)
}

/// Keys that differ from the boot configuration and need a restart.
pub fn restart_required(boot: &Config, now: &Config) -> Vec<String> {
    let mut out = Vec::new();
    for k in RESTART_KEYS {
        let differs = match *k {
            "server.binds" => boot.server.binds != now.server.binds,
            "server.cors_origins" => boot.server.cors_origins != now.server.cors_origins,
            "resources.cpu_threads" => boot.resources.cpu_threads != now.resources.cpu_threads,
            _ => false,
        };
        if differs {
            out.push((*k).to_string());
        }
    }
    out
}

/// The full settings document (contract §1 shape).
pub async fn document(st: &AppState) -> ApiResult<Value> {
    let cfg = st.live.get();
    let monitor = st.services.get::<sf_ops::monitor::Monitor>();
    let hooks = st.services.get::<SettingsHooks>();
    let gpu_name = monitor.as_ref().and_then(|m| m.gpu_name());
    let active_device = hooks
        .as_ref()
        .map(|h| (h.active_device)())
        .unwrap_or_else(|| "unknown".into());
    let gpu_available =
        active_device.contains("directml") || active_device.contains("hybrid (directml");
    let policy = super::mcp::load_policy(st).await?;
    let passphrase_set = {
        let c = cfg.clone();
        tokio::task::spawn_blocking(move || sf_ops::backup::configured_passphrase(&c).is_some())
            .await
            .unwrap_or(false)
    };
    let mem_total = mem_total_mb(st).await;
    Ok(json!({
        "resources": {
            "cpu_threads": cfg.resources.cpu_threads,
            "cpu_threads_effective": sf_engine::pool::threads_in_use(),
            "cpu_threads_pool": sf_engine::pool::threads(),
            "logical_cpus": logical_cpus(),
            "max_concurrent_jobs": cfg.resources.max_concurrent_jobs,
            "vector_cache_mb": cfg.resources.vector_cache_mb,
            "max_memory_mb": cfg.resources.max_memory_mb,
            "low_priority_jobs": cfg.resources.low_priority_jobs,
            "defer_when_cpu_above": cfg.resources.defer_when_cpu_above,
            "mem_total_mb": mem_total,
            "memory_pressure": monitor.as_ref().is_some_and(|m| m.memory_pressure()),
        },
        "compute": {
            "embed_mode": cfg.embed.mode,
            "gpu_enabled": cfg.embed.gpu,
            "gpu_available": gpu_available || gpu_name.is_some(),
            "gpu_name": gpu_name,
            "hybrid_gpu_share": cfg.embed.hybrid_gpu_share,
            "embed_batch_size": cfg.embed.batch_size,
            "embed_chain": cfg.embed.chain,
            "remote_model": cfg.embed.remote_model,
            "remote_key_set": !cfg.embed.remote_key.trim().is_empty(),
            "remote_key2_set": !cfg.embed.remote_key2.trim().is_empty(),
            "remote_key": if cfg.embed.remote_key.trim().is_empty() { "" } else { SECRET_SET },
            "remote_key2": if cfg.embed.remote_key2.trim().is_empty() { "" } else { SECRET_SET },
            "active_device": active_device,
        },
        "wordpress": {
            "default_insert_modules": cfg.wordpress.default_insert_modules,
            "verify_after_inject": cfg.wordpress.verify_after_inject,
            "default_max_links_per_post": cfg.wordpress.default_max_links_per_post,
            "default_min_links_per_post": cfg.wordpress.default_min_links_per_post,
            "max_new_links_per_pass": cfg.wordpress.max_new_links_per_pass,
            "request_timeout_ms": cfg.wordpress.request_timeout_ms,
            "user_agent": cfg.crawl.user_agent,
        },
        "crawl": {
            "concurrency": cfg.crawl.concurrency,
            "timeout_ms": cfg.crawl.timeout_ms,
            "max_pages": cfg.crawl.max_pages,
            "allow_private_hosts": cfg.crawl.allow_private_hosts,
        },
        "llm": {
            "fallback_url": cfg.llm.fallback_url,
            "fallback_models": cfg.llm.fallback_models,
            "timeout_ms": cfg.llm.timeout_ms,
        },
        "server": {
            "binds": cfg.server.binds,
            "cors_origins": cfg.server.cors_origins,
            "allow_registration": cfg.auth.allow_registration,
            "mcp_http_enabled": policy.http_enabled,
        },
        "backup": {
            "dir": cfg.backups_dir(),
            "keep": cfg.backup.keep,
            "schedule": cfg.backup.schedule,
            "passphrase_set": passphrase_set,
            "passphrase": if passphrase_set { SECRET_SET } else { "" },
        },
        "monitor": {
            "sample_secs": cfg.monitor.sample_secs,
            "retain_days": cfg.monitor.retain_days,
            "alert_cpu_pct": cfg.monitor.alert_cpu_pct,
            "alert_mem_pct": cfg.monitor.alert_mem_pct,
            "alert_disk_free_mb": cfg.monitor.alert_disk_free_mb,
            "maintenance_cron": cfg.monitor.maintenance_cron,
            "event_log_retain_days": cfg.monitor.event_log_retain_days,
            "log_reads": cfg.monitor.log_reads,
        },
        "ai": ai_document(&cfg.ai),
        "restart_required": restart_required(&st.cfg, &cfg),
        "restart_supported": hooks.as_ref().is_some_and(|h| h.restart.is_some()),
    }))
}

pub async fn get(State(st): State<AppState>, _a: Admin) -> ApiResult<Json<Value>> {
    Ok(Json(document(&st).await?))
}

/// The `ai` section (v2.3 §5): every task with its effective tier.
pub fn ai_document(a: &sf_core::config::AiCfg) -> Value {
    json!({
        "always_on": a.always_on,
        "task_tiers": a.effective_task_tiers(),
        "batch_size": a.batch(),
        "confidence_skip": {"anchor": a.confidence_skip.anchor, "relevance": a.confidence_skip.relevance},
        "budget": {
            "daily_tokens": {
                "low": a.budget.daily_tokens.low,
                "main": a.budget.daily_tokens.main,
                "high": a.budget.daily_tokens.high,
            },
            "per_site_daily_tokens": a.budget.per_site_daily_tokens,
        },
        "cache_ttl_days": a.cache_ttl_days,
        "reasoning_effort": {
            "low": a.reasoning_effort.low,
            "main": a.reasoning_effort.main,
            "high": a.reasoning_effort.high,
        },
    })
}

const MAX_BUDGET_TOKENS: u64 = 10_000_000_000;

fn obj(v: &Value) -> R<&serde_json::Map<String, Value>> {
    v.as_object().ok_or_else(|| "must be an object".to_string())
}

fn one_of_str(v: &Value, allowed: &[&str]) -> R<String> {
    let s = v
        .as_str()
        .map(|s| s.trim().to_ascii_lowercase())
        .ok_or_else(|| "must be a string".to_string())?;
    if allowed.contains(&s.as_str()) {
        Ok(s)
    } else {
        Err(format!("must be one of {}", allowed.join(", ")))
    }
}

/// `ai.<key>` (v2.3). Nested objects merge into the current value; the full value is persisted.
fn apply_ai_key(cfg: &mut Config, key: &str, v: &Value) -> R<Vec<Write>> {
    use sf_core::config::ai_tasks::{DEFAULTS, EFFORTS, TIERS};
    let a = &mut cfg.ai;
    let out = match key {
        "always_on" => {
            a.always_on = as_bool(v)?;
            json!(a.always_on)
        }
        "task_tiers" => {
            let m = obj(v)?;
            for (task, t) in m {
                if !DEFAULTS.iter().any(|(k, _)| k == task) {
                    return Err(format!(
                        "unknown task `{task}` (known: {})",
                        DEFAULTS.iter().map(|d| d.0).collect::<Vec<_>>().join(", ")
                    ));
                }
                let tier = one_of_str(t, TIERS).map_err(|e| format!("{task}: {e}"))?;
                a.task_tiers.insert(task.clone(), tier);
            }
            json!(a.effective_task_tiers())
        }
        "batch_size" => {
            a.batch_size = as_uint(v, 1, 50)? as usize;
            json!(a.batch_size)
        }
        "confidence_skip" => {
            for (k, x) in obj(v)? {
                let f = as_float(x, 0.0, 1.0).map_err(|e| format!("{k}: {e}"))?;
                match k.as_str() {
                    "anchor" => a.confidence_skip.anchor = f,
                    "relevance" => a.confidence_skip.relevance = f,
                    _ => return Err(format!("unknown key `{k}` (anchor, relevance)")),
                }
            }
            json!({"anchor": a.confidence_skip.anchor, "relevance": a.confidence_skip.relevance})
        }
        "budget" => {
            for (k, x) in obj(v)? {
                match k.as_str() {
                    "daily_tokens" => {
                        for (t, n) in obj(x).map_err(|e| format!("daily_tokens: {e}"))? {
                            if !TIERS.contains(&t.as_str()) {
                                return Err(format!("daily_tokens: unknown tier `{t}`"));
                            }
                            let n = as_uint(n, 0, MAX_BUDGET_TOKENS)
                                .map_err(|e| format!("daily_tokens.{t}: {e}"))?;
                            a.budget.daily_tokens.set(t, n);
                        }
                    }
                    "per_site_daily_tokens" => {
                        a.budget.per_site_daily_tokens = as_uint(x, 0, MAX_BUDGET_TOKENS)
                            .map_err(|e| format!("per_site_daily_tokens: {e}"))?;
                    }
                    _ => {
                        return Err(format!(
                            "unknown key `{k}` (daily_tokens, per_site_daily_tokens)"
                        ));
                    }
                }
            }
            json!({
                "daily_tokens": {"low": a.budget.daily_tokens.low, "main": a.budget.daily_tokens.main, "high": a.budget.daily_tokens.high},
                "per_site_daily_tokens": a.budget.per_site_daily_tokens,
            })
        }
        "cache_ttl_days" => {
            a.cache_ttl_days = as_uint(v, 0, 365)? as u32;
            json!(a.cache_ttl_days)
        }
        "reasoning_effort" => {
            for (t, e) in obj(v)? {
                if !TIERS.contains(&t.as_str()) {
                    return Err(format!("unknown tier `{t}`"));
                }
                let e = one_of_str(e, EFFORTS).map_err(|x| format!("{t}: {x}"))?;
                a.reasoning_effort.set(t, e);
            }
            json!({"low": a.reasoning_effort.low, "main": a.reasoning_effort.main, "high": a.reasoning_effort.high})
        }
        _ => return Err("unknown setting".into()),
    };
    Ok(vec![toml("ai", key, out)])
}

// ---------------------------------------------------------------------------------------------
// validation helpers

type R<T> = std::result::Result<T, String>;

fn as_uint(v: &Value, min: u64, max: u64) -> R<u64> {
    let n = v
        .as_u64()
        .or_else(|| {
            v.as_f64()
                .filter(|f| f.is_finite() && f.fract() == 0.0 && *f >= 0.0)
                .map(|f| f as u64)
        })
        .ok_or_else(|| "must be a whole number".to_string())?;
    if n < min || n > max {
        return Err(format!("must be {min}..{max}"));
    }
    Ok(n)
}

fn as_float(v: &Value, min: f64, max: f64) -> R<f64> {
    let f = v
        .as_f64()
        .filter(|f| f.is_finite())
        .ok_or_else(|| "must be a number".to_string())?;
    if f < min || f > max {
        return Err(format!("must be {min}..{max}"));
    }
    Ok(f)
}

fn as_bool(v: &Value) -> R<bool> {
    v.as_bool().ok_or_else(|| "must be true or false".into())
}

fn as_text(v: &Value, max_chars: usize) -> R<String> {
    let s = v.as_str().ok_or_else(|| "must be a string".to_string())?;
    if s.chars().count() > max_chars {
        return Err(format!("must be at most {max_chars} characters"));
    }
    if s.chars().any(|c| c.is_control()) {
        return Err("must not contain control characters".into());
    }
    Ok(s.trim().to_string())
}

fn as_list(v: &Value, max_items: usize, max_chars: usize) -> R<Vec<String>> {
    let a = v
        .as_array()
        .ok_or_else(|| "must be an array of strings".to_string())?;
    if a.len() > max_items {
        return Err(format!("at most {max_items} entries"));
    }
    let mut out = Vec::new();
    for x in a {
        let s = as_text(x, max_chars)?;
        if s.is_empty() {
            return Err("entries must not be empty".into());
        }
        if !out.contains(&s) {
            out.push(s);
        }
    }
    Ok(out)
}

fn valid_cron(s: &str) -> R<()> {
    if s.is_empty() {
        return Ok(());
    }
    croner::Cron::new(s)
        .with_seconds_optional()
        .parse()
        .map(|_| ())
        .map_err(|e| format!("invalid cron expression: {e}"))
}

fn valid_http_url(s: &str) -> R<()> {
    let u = url::Url::parse(s).map_err(|e| format!("invalid URL: {e}"))?;
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        return Err("must be an http(s) URL".into());
    }
    Ok(())
}

/// One validated change.
enum Write {
    Toml(TomlChange),
    /// `secrets.env` lines (`None` = remove the line).
    Env(Vec<(String, Option<String>)>),
    /// MCP policy `http_enabled` (kv).
    McpHttp(bool),
}

fn toml(table: &str, key: &str, value: Value) -> Write {
    Write::Toml(TomlChange {
        table: table.into(),
        key: key.into(),
        value,
    })
}

/// Secret value: `"***"` = keep (None), `""` = clear, anything else = set.
fn secret(v: &Value) -> R<Option<String>> {
    let s = v
        .as_str()
        .ok_or_else(|| "must be a string".to_string())?
        .trim();
    if s == SECRET_SET {
        return Ok(None);
    }
    if s.len() > 4096 || s.chars().any(|c| c.is_control()) {
        return Err("invalid secret value".into());
    }
    Ok(Some(s.to_string()))
}

/// Validates one key, applies it to `cfg` and returns how to persist it.
fn apply_key(
    cfg: &mut Config,
    section: &str,
    key: &str,
    v: &Value,
    mem_total_mb: u64,
) -> R<Vec<Write>> {
    let mut w = Vec::new();
    if section == "ai" {
        return apply_ai_key(cfg, key, v);
    }
    match (section, key) {
        ("resources", "cpu_threads") => {
            let n = as_uint(v, 0, logical_cpus() as u64)?;
            cfg.resources.cpu_threads = n as usize;
            w.push(toml("resources", key, json!(n)));
        }
        ("resources", "max_concurrent_jobs") => {
            let n = as_uint(v, 1, 16)?;
            cfg.resources.max_concurrent_jobs = n as usize;
            w.push(toml("resources", key, json!(n)));
        }
        ("resources", "vector_cache_mb") => {
            let n = as_uint(v, 64, 32_768)?;
            cfg.resources.vector_cache_mb = n as usize;
            w.push(toml("resources", key, json!(n)));
        }
        ("resources", "max_memory_mb") => {
            let max = if mem_total_mb >= 256 {
                mem_total_mb
            } else {
                1 << 20
            };
            let n = as_uint(v, 0, max)?;
            if n != 0 && n < 256 {
                return Err(format!("must be 0 (off) or 256..{max}"));
            }
            cfg.resources.max_memory_mb = n;
            w.push(toml("resources", key, json!(n)));
        }
        ("resources", "low_priority_jobs") => {
            let b = as_bool(v)?;
            cfg.resources.low_priority_jobs = b;
            w.push(toml("resources", key, json!(b)));
        }
        ("resources", "defer_when_cpu_above") => {
            let f = as_float(v, 0.0, 100.0)?;
            cfg.resources.defer_when_cpu_above = f as f32;
            w.push(toml("resources", key, json!(f)));
        }
        ("compute", "embed_mode") => {
            let s = as_text(v, 20)?.to_ascii_lowercase();
            if !EMBED_MODES.contains(&s.as_str()) {
                return Err(format!("must be one of {}", EMBED_MODES.join(", ")));
            }
            cfg.embed.mode = s.clone();
            w.push(toml("embed", "mode", json!(s)));
        }
        ("compute", "gpu_enabled") => {
            let b = as_bool(v)?;
            cfg.embed.gpu = b;
            w.push(toml("embed", "gpu", json!(b)));
        }
        ("compute", "hybrid_gpu_share") => {
            let f = as_float(v, 0.1, 0.9)?;
            cfg.embed.hybrid_gpu_share = f as f32;
            w.push(toml("embed", "hybrid_gpu_share", json!(f)));
        }
        ("compute", "embed_batch_size") => {
            let n = as_uint(v, 8, 512)?;
            cfg.embed.batch_size = n as usize;
            w.push(toml("embed", "batch_size", json!(n)));
        }
        ("compute", "embed_chain") => {
            let l = as_list(v, 8, 20)?;
            if l.is_empty() {
                return Err("needs at least one rung".into());
            }
            for r in &l {
                if !EMBED_RUNGS.contains(&r.as_str()) {
                    return Err(format!(
                        "unknown rung `{r}` (allowed: {})",
                        EMBED_RUNGS.join(", ")
                    ));
                }
            }
            cfg.embed.chain = l.clone();
            w.push(toml("embed", "chain", json!(l)));
        }
        ("compute", "remote_model") => {
            let s = as_text(v, 200)?;
            if s.is_empty() {
                return Err("must not be empty".into());
            }
            cfg.embed.remote_model = s.clone();
            w.push(toml("embed", "remote_model", json!(s)));
        }
        ("compute", "remote_key") | ("compute", "remote_key2") => {
            if let Some(s) = secret(v)? {
                let env_key = if key == "remote_key" {
                    cfg.embed.remote_key = s.clone();
                    "OPENROUTER_API_KEY"
                } else {
                    cfg.embed.remote_key2 = s.clone();
                    "OPENROUTER_API_KEY_T2"
                };
                w.push(Write::Env(vec![(
                    env_key.into(),
                    (!s.is_empty()).then_some(s),
                )]));
            }
        }
        ("wordpress", "default_insert_modules") => {
            let l = as_list(v, 3, 20)?;
            for m in &l {
                if !INSERT_MODULES.contains(&m.as_str()) {
                    return Err(format!(
                        "unknown module `{m}` (allowed: {})",
                        INSERT_MODULES.join(", ")
                    ));
                }
            }
            if l.is_empty() {
                return Err("needs at least one module".into());
            }
            cfg.wordpress.default_insert_modules = l.clone();
            w.push(toml("wordpress", key, json!(l)));
        }
        ("wordpress", "verify_after_inject") => {
            let b = as_bool(v)?;
            cfg.wordpress.verify_after_inject = b;
            w.push(toml("wordpress", key, json!(b)));
        }
        ("wordpress", "default_max_links_per_post") => {
            let n = as_uint(v, 1, 200)?;
            cfg.wordpress.default_max_links_per_post = n as u32;
            w.push(toml("wordpress", key, json!(n)));
        }
        ("wordpress", "default_min_links_per_post") => {
            let n = as_uint(v, 0, 200)?;
            cfg.wordpress.default_min_links_per_post = n as u32;
            w.push(toml("wordpress", key, json!(n)));
        }
        ("wordpress", "max_new_links_per_pass") => {
            let n = as_uint(v, 1, 200)?;
            cfg.wordpress.max_new_links_per_pass = n as u32;
            w.push(toml("wordpress", key, json!(n)));
        }
        ("wordpress", "request_timeout_ms") => {
            let n = as_uint(v, 1_000, 600_000)?;
            cfg.wordpress.request_timeout_ms = n;
            w.push(toml("wordpress", key, json!(n)));
        }
        ("wordpress", "user_agent") => {
            let s = as_text(v, 200)?;
            if s.is_empty() {
                return Err("must not be empty".into());
            }
            cfg.crawl.user_agent = s.clone();
            w.push(toml("crawl", "user_agent", json!(s)));
        }
        ("crawl", "concurrency") => {
            let n = as_uint(v, 1, 32)?;
            cfg.crawl.concurrency = n as usize;
            w.push(toml("crawl", key, json!(n)));
        }
        ("crawl", "timeout_ms") => {
            let n = as_uint(v, 1_000, 600_000)?;
            cfg.crawl.timeout_ms = n;
            w.push(toml("crawl", key, json!(n)));
        }
        ("crawl", "max_pages") => {
            let n = as_uint(v, 1, 200_000)?;
            cfg.crawl.max_pages = n as usize;
            w.push(toml("crawl", key, json!(n)));
        }
        ("crawl", "allow_private_hosts") => {
            let b = as_bool(v)?;
            cfg.crawl.allow_private_hosts = b;
            w.push(toml("crawl", key, json!(b)));
        }
        ("llm", "fallback_url") => {
            let s = as_text(v, 500)?;
            if !s.is_empty() {
                valid_http_url(&s)?;
            }
            cfg.llm.fallback_url = s.clone();
            w.push(toml("llm", key, json!(s)));
        }
        ("llm", "fallback_models") => {
            let l = as_list(v, 10, 200)?;
            cfg.llm.fallback_models = l.clone();
            w.push(toml("llm", key, json!(l)));
        }
        ("llm", "timeout_ms") => {
            let n = as_uint(v, 1_000, 900_000)?;
            cfg.llm.timeout_ms = n;
            w.push(toml("llm", key, json!(n)));
        }
        ("server", "binds") => {
            let l = as_list(v, 4, 100)?;
            if l.is_empty() {
                return Err("needs at least one listener".into());
            }
            for b in &l {
                b.parse::<std::net::SocketAddr>()
                    .map_err(|_| format!("`{b}` is not ip:port"))?;
            }
            cfg.server.binds = l.clone();
            w.push(toml("server", key, json!(l)));
        }
        ("server", "cors_origins") => {
            let l = as_list(v, 20, 200)?;
            for o in &l {
                if o != "*" {
                    valid_http_url(o)?;
                }
            }
            cfg.server.cors_origins = l.clone();
            w.push(toml("server", key, json!(l)));
        }
        ("server", "allow_registration") => {
            let b = as_bool(v)?;
            cfg.auth.allow_registration = b;
            w.push(toml("auth", key, json!(b)));
        }
        ("server", "mcp_http_enabled") => w.push(Write::McpHttp(as_bool(v)?)),
        ("backup", "dir") => {
            let s = as_text(v, 500)?;
            if !s.is_empty() && !std::path::Path::new(&s).is_absolute() {
                return Err("must be an absolute path (or empty for <home>/backups)".into());
            }
            cfg.backup.dir = s.clone();
            w.push(toml("backup", key, json!(s)));
        }
        ("backup", "keep") => {
            let n = as_uint(v, 1, 365)?;
            cfg.backup.keep = n as usize;
            w.push(toml("backup", key, json!(n)));
        }
        ("backup", "schedule") => {
            let s = as_text(v, 100)?;
            valid_cron(&s)?;
            cfg.backup.schedule = s.clone();
            w.push(toml("backup", key, json!(s)));
        }
        ("backup", "passphrase") => {
            if let Some(s) = secret(v)? {
                let var = cfg.backup.passphrase_env.trim().to_string();
                let var = if var.is_empty() {
                    cfg.backup.passphrase_env = "SILOFORGE_BACKUP_PASSPHRASE".into();
                    w.push(toml(
                        "backup",
                        "passphrase_env",
                        json!("SILOFORGE_BACKUP_PASSPHRASE"),
                    ));
                    "SILOFORGE_BACKUP_PASSPHRASE".to_string()
                } else {
                    var
                };
                if !s.is_empty() && s.chars().count() < 8 {
                    return Err("must be at least 8 characters (or empty to disable)".into());
                }
                w.push(Write::Env(vec![(var, (!s.is_empty()).then_some(s))]));
            }
        }
        ("monitor", "sample_secs") => {
            let n = as_uint(v, 1, 3600)?;
            cfg.monitor.sample_secs = n;
            w.push(toml("monitor", key, json!(n)));
        }
        ("monitor", "retain_days") => {
            let n = as_uint(v, 1, 3650)?;
            cfg.monitor.retain_days = n as u32;
            w.push(toml("monitor", key, json!(n)));
        }
        ("monitor", "alert_cpu_pct") | ("monitor", "alert_mem_pct") => {
            let f = as_float(v, 0.0, 100.0)?;
            if key == "alert_cpu_pct" {
                cfg.monitor.alert_cpu_pct = f as f32;
            } else {
                cfg.monitor.alert_mem_pct = f as f32;
            }
            w.push(toml("monitor", key, json!(f)));
        }
        ("monitor", "alert_disk_free_mb") => {
            let n = as_uint(v, 0, 1 << 30)?;
            cfg.monitor.alert_disk_free_mb = n;
            w.push(toml("monitor", key, json!(n)));
        }
        ("monitor", "maintenance_cron") => {
            let s = as_text(v, 100)?;
            valid_cron(&s)?;
            cfg.monitor.maintenance_cron = s.clone();
            w.push(toml("monitor", key, json!(s)));
        }
        ("monitor", "event_log_retain_days") => {
            let n = as_uint(v, 0, 3650)?;
            cfg.monitor.event_log_retain_days = n as u32;
            w.push(toml("monitor", key, json!(n)));
        }
        ("monitor", "log_reads") => {
            let b = as_bool(v)?;
            cfg.monitor.log_reads = b;
            w.push(toml("monitor", key, json!(b)));
        }
        (
            "resources",
            "cpu_threads_effective"
            | "cpu_threads_pool"
            | "logical_cpus"
            | "mem_total_mb"
            | "memory_pressure",
        )
        | (
            "compute",
            "gpu_available" | "gpu_name" | "active_device" | "remote_key_set" | "remote_key2_set",
        )
        | ("backup", "passphrase_set") => return Err("is read-only".into()),
        _ => return Err("unknown setting".into()),
    }
    // A value that comes from a secrets.env override is written to that line instead.
    let overrides = env_override_keys(section, key);
    if !overrides.is_empty() {
        let fenv = file_env(cfg);
        if overrides
            .iter()
            .any(|k| fenv.iter().any(|(fk, fv)| fk == k && !fv.is_empty()))
        {
            w.retain(|x| !matches!(x, Write::Toml(_)));
            w.push(Write::Env(env_lines(cfg, section, key)));
        }
    }
    Ok(w)
}

/// The `secrets.env` lines carrying `section.key` (for env-overridden keys).
fn env_lines(cfg: &Config, section: &str, key: &str) -> Vec<(String, Option<String>)> {
    let some = |s: String| (!s.is_empty()).then_some(s);
    match (section, key) {
        ("resources", "cpu_threads") => vec![(
            "SILOFORGE_CPU_THREADS".into(),
            Some(cfg.resources.cpu_threads.to_string()),
        )],
        ("compute", "embed_chain") => {
            vec![("EMBED_CHAIN".into(), some(cfg.embed.chain.join(",")))]
        }
        ("compute", "remote_model") => vec![(
            "EMBED_REMOTE_MODEL".into(),
            some(cfg.embed.remote_model.clone()),
        )],
        ("llm", "fallback_url") => vec![("DARIO_URL".into(), some(cfg.llm.fallback_url.clone()))],
        ("llm", "fallback_models") => vec![
            (
                "DARIO_MODEL_PRIMARY".into(),
                cfg.llm.fallback_models.first().cloned(),
            ),
            (
                "DARIO_MODEL_FALLBACK".into(),
                cfg.llm.fallback_models.get(1).cloned(),
            ),
        ],
        ("crawl", "allow_private_hosts") => vec![(
            "CRAWL_ALLOW_PRIVATE_HOSTS".into(),
            Some(cfg.crawl.allow_private_hosts.to_string()),
        )],
        ("server", "binds") => vec![("SILOFORGE_BINDS".into(), some(cfg.server.binds.join(",")))],
        _ => Vec::new(),
    }
}

/// `PUT /settings`: validate everything, persist, apply live, return the new document.
pub async fn put(
    State(st): State<AppState>,
    Admin(u): Admin,
    JsonBody(body): JsonBody<Value>,
) -> ApiResult<Json<Value>> {
    let Value::Object(sections) = body else {
        return Err(ApiError::bad(
            "body must be an object: {\"<section>\": {\"<key>\": value}}",
        ));
    };
    let _guard = SAVE_LOCK.lock().await;
    let mem_total = mem_total_mb(&st).await;
    let mut cfg = (*st.live.get()).clone();
    let mut errors: BTreeMap<String, String> = BTreeMap::new();
    let mut writes = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    let mut env_warnings: Vec<String> = Vec::new();
    for (section, keys) in &sections {
        if matches!(section.as_str(), "restart_required" | "restart_supported") {
            continue;
        }
        let Value::Object(keys) = keys else {
            errors.insert(section.clone(), "must be an object of keys".into());
            continue;
        };
        if keys.len() > 64 {
            errors.insert(section.clone(), "too many keys".into());
            continue;
        }
        for (key, v) in keys {
            match apply_key(&mut cfg, section, key, v, mem_total) {
                Ok(w) => {
                    if !w.is_empty() {
                        changed.push(format!("{section}.{key}"));
                    }
                    for k in env_override_keys(section, key) {
                        if process_env_set(k) {
                            env_warnings.push(format!(
                                "{section}.{key}: the process environment variable {k} overrides this setting after a restart"
                            ));
                        }
                    }
                    writes.extend(w);
                }
                Err(e) => {
                    errors.insert(format!("{section}.{key}"), e);
                }
            }
        }
    }
    if !errors.is_empty() {
        let msg = errors
            .iter()
            .map(|(k, e)| format!("{k}: {e}"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(ApiError::new(StatusCode::BAD_REQUEST, msg));
    }
    // Persist: TOML first, then secrets, then kv; the live swap happens only after the files.
    let mut toml_changes = Vec::new();
    let mut env_changes: Vec<(String, Option<String>)> = Vec::new();
    let mut mcp_http = None;
    for w in writes {
        match w {
            Write::Toml(t) => toml_changes.push(t),
            Write::Env(lines) => {
                for (k, v) in lines {
                    env_changes.retain(|(ek, _)| ek != &k);
                    env_changes.push((k, v));
                }
            }
            Write::McpHttp(b) => mcp_http = Some(b),
        }
    }
    let home = cfg.home.clone();
    let (tc, ec) = (toml_changes, env_changes.clone());
    tokio::task::spawn_blocking(move || -> sf_core::Result<()> {
        edit_toml_file(&home.join("siloforge.toml"), &tc)?;
        edit_env_file(&home.join("secrets.env"), &ec)?;
        Ok(())
    })
    .await
    .map_err(|e| ApiError::internal(format!("settings writer: {e}")))??;
    if let Some(b) = mcp_http {
        super::mcp::set_http_enabled(&st, b).await?;
    }
    // the live compute-thread limit applies at once (the job layer re-applies it every 5 s)
    if st.live.generation() > 0 || cfg.resources.cpu_threads != st.cfg.resources.cpu_threads {
        sf_engine::pool::set_live_limit(cfg.resources.cpu_threads);
    }
    st.live.set(cfg);
    let secret_keys: Vec<&str> = env_changes.iter().map(|(k, _)| k.as_str()).collect();
    tracing::info!(keys = ?changed, "settings changed");
    crate::eventlog::record_system(
        &st,
        "settings changed",
        true,
        json!({ "keys": changed, "by": u.actor() }),
    );
    audit(
        &st,
        &u.actor(),
        u.via(),
        "settings.update",
        None,
        json!({ "keys": changed, "secrets_env_keys": secret_keys }),
    )
    .await;
    let mut doc = document(&st).await?;
    if let Some(o) = doc.as_object_mut() {
        o.insert("changed".into(), json!(changed));
        if !env_warnings.is_empty() {
            o.insert("warnings".into(), json!(env_warnings));
        }
    }
    Ok(Json(doc))
}

/// `POST /system/restart` -> 202 `{restarting: true}`; the binary's hook re-launches the process.
pub async fn restart(
    State(st): State<AppState>,
    Admin(u): Admin,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let hook = st
        .services
        .get::<SettingsHooks>()
        .and_then(|h| h.restart.clone())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_IMPLEMENTED,
                "restart is not supported by this process",
            )
        })?;
    audit(&st, &u.actor(), u.via(), "system.restart", None, json!({})).await;
    tracing::warn!(by = %u.actor(), "restart requested");
    crate::eventlog::record_system(
        &st,
        "system restart requested",
        true,
        json!({ "by": u.actor() }),
    );
    tokio::spawn(async move {
        // let the 202 reach the client first
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        hook();
    });
    Ok((StatusCode::ACCEPTED, Json(json!({ "restarting": true }))))
}
