//! Configuration: `$SILOFORGE_HOME/siloforge.toml`, then environment overrides.
//!
//! v1 environment names (`JWT_SECRET`, `DARIO_URL`, `OPENROUTER_API_KEY`, ...) are still honoured so
//! the v1 `.env` can be reused during migration. Secrets should live in env or `secrets.env`, not in TOML.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(skip)]
    pub home: PathBuf,
    pub server: ServerCfg,
    pub auth: AuthCfg,
    pub resources: ResourcesCfg,
    pub embed: EmbedCfg,
    pub llm: LlmCfg,
    pub crawl: CrawlCfg,
    pub backup: BackupCfg,
    pub monitor: MonitorCfg,
    /// Install-wide WordPress defaults (v2.1): used when a site has no own value.
    pub wordpress: WordpressCfg,
    /// Release downloads offered by `GET /api/v2/downloads` (v2.1).
    pub downloads: DownloadsCfg,
    /// AI task tiers, batching, cache, budgets and "Always use AI" (v2.3).
    pub ai: AiCfg,
    /// Never serialised (secret), see [`GscCfg`].
    #[serde(skip_serializing)]
    pub gsc: GscCfg,
}

/// Install-wide Google Search Console fallback (v1 parity, G2-5): `GSC_CREDENTIALS_JSON` from the
/// environment or `secrets.env`, used by the gsc job when a site has no per-site credentials.
/// The value is a service-account JSON object, or a path to a file holding one. It is a secret:
/// never serialised, and `Debug` only says whether it is set.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GscCfg {
    #[serde(skip_serializing)]
    pub credentials_json: String,
}

/// L-14: `Debug` for config sections that hold secrets prints the serialised form, which skips
/// every `#[serde(skip_serializing)]` secret field, so `{:?}` can never leak a key.
macro_rules! redacted_debug {
    ($t:ty) => {
        impl std::fmt::Debug for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let shown = serde_json::to_string(self).unwrap_or_default();
                write!(f, "{} {shown} (secrets redacted)", stringify!($t))
            }
        }
    };
}
redacted_debug!(AuthCfg);
redacted_debug!(EmbedCfg);

impl std::fmt::Debug for GscCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GscCfg")
            .field(
                "credentials_json",
                &(!self.credentials_json.is_empty()).then_some("<redacted>"),
            )
            .finish()
    }
}

/// Largest accepted `GSC_CREDENTIALS_JSON` (inline or file), bytes.
pub const MAX_GSC_CREDENTIALS: usize = 64 * 1024;

impl GscCfg {
    /// The fallback service-account JSON, if configured and well-formed (a JSON object).
    /// A value that does not start with `{` is read as a file path. Never logs the value.
    pub fn fallback_credentials(&self) -> Option<String> {
        let raw = self.credentials_json.trim();
        if raw.is_empty() {
            return None;
        }
        let text = if raw.starts_with('{') {
            raw.to_string()
        } else {
            let meta = std::fs::metadata(raw).ok()?;
            if !meta.is_file() || meta.len() > MAX_GSC_CREDENTIALS as u64 {
                return None;
            }
            std::fs::read_to_string(raw).ok()?
        };
        if text.len() > MAX_GSC_CREDENTIALS {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
        v.is_object().then(|| v.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerCfg {
    /// API + UI listeners. v1 used 8600 (API) and 8601 (UI); both serve everything in v2.
    pub binds: Vec<String>,
    pub body_limit_mb: usize,
    /// Allowed CORS origins (empty = same-origin only).
    pub cors_origins: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthCfg {
    #[serde(skip_serializing)]
    pub jwt_secret: String,
    /// Master key for stored secrets. v1 fell back to the JWT secret; v2 keeps that fallback only for migration.
    #[serde(skip_serializing)]
    pub encryption_key: String,
    pub allow_registration: bool,
    pub admin_email: String,
    #[serde(skip_serializing)]
    pub admin_password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourcesCfg {
    /// Threads for CPU-heavy work (embedding, scoring, graph). 0 = physical cores / 2.
    pub cpu_threads: usize,
    /// Jobs that may run at once (different sites). Same-site mutating jobs never overlap.
    pub max_concurrent_jobs: usize,
    /// Soft RAM budget for vector caches, MB.
    pub vector_cache_mb: usize,
    /// Lower process priority while heavy jobs run (Windows BELOW_NORMAL / Unix nice 10).
    pub low_priority_jobs: bool,
    /// Do not start a heavy job while machine CPU is above this percent (0 = disabled).
    pub defer_when_cpu_above: f32,
    /// Soft cap on this process' resident memory, MB (0 = off). Above it the vector cache is
    /// shrunk, heavy jobs are deferred and one notification is sent (v2.1).
    pub max_memory_mb: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbedCfg {
    /// Ordered rungs: `remote` (OpenAI-compatible, e.g. OpenRouter), `remote2`, `onnx` (ONNX Runtime CPU/DirectML), `local` (candle, pure Rust).
    pub chain: Vec<String>,
    pub remote_url: String,
    pub remote_model: String,
    #[serde(skip_serializing)]
    pub remote_key: String,
    #[serde(skip_serializing)]
    pub remote_key2: String,
    pub batch_size: usize,
    pub max_chars: usize,
    pub timeout_ms: u64,
    pub retries: u32,
    /// Directory holding config.json / tokenizer.json / model.safetensors (and model.onnx). Empty = `$HOME/models/all-MiniLM-L6-v2`.
    pub model_dir: String,
    /// Directory holding onnxruntime.dll (+ DirectML.dll). Empty = next to the exe, then `$HOME/runtime`.
    pub onnx_runtime_dir: String,
    /// Try the GPU (DirectML on Windows) for the onnx rung.
    pub gpu: bool,
    /// `auto` (GPU if available else CPU) | `gpu` | `cpu` | `hybrid` (GPU and CPU share each job) |
    /// `remote` (remote rungs first). v2.1.
    pub mode: String,
    /// Share of the work the GPU takes in `hybrid` mode (0.1..0.9); the GPU may steal more when
    /// the CPU is slower.
    pub hybrid_gpu_share: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmCfg {
    /// Env fallback chat endpoint (OpenAI-compatible), used when no model chain is active. v1: dario.
    pub fallback_url: String,
    pub fallback_models: Vec<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrawlCfg {
    pub user_agent: String,
    pub allow_private_hosts: bool,
    /// Host rewrites for native runs, e.g. `site01.example = "127.0.0.1"`.
    pub host_aliases: std::collections::BTreeMap<String, String>,
    pub concurrency: usize,
    pub timeout_ms: u64,
    pub max_pages: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BackupCfg {
    /// Where backup archives go. Empty = `$HOME/backups`.
    pub dir: String,
    /// Keep this many automatic backups.
    pub keep: usize,
    /// Cron for automatic backups (empty = off).
    pub schedule: String,
    /// Env var holding the archive passphrase (empty = unencrypted archives).
    pub passphrase_env: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorCfg {
    pub sample_secs: u64,
    pub retain_days: u32,
    pub job_log_retain_days: u32,
    pub alert_cpu_pct: f32,
    pub alert_mem_pct: f32,
    pub alert_disk_free_mb: u64,
    /// Cron (local time) for the daily `maintenance` job: retention prune + ANALYZE/optimize.
    /// Empty = never (nothing is pruned then).
    pub maintenance_cron: String,
    /// Finished job runs older than this are deleted (0 = keep forever) ...
    pub job_runs_retain_days: u32,
    /// ... except the newest N finished runs per site and kind.
    pub job_runs_keep_per_site: u32,
    /// Snapshots not backing an applied edge are deleted after this many days (0 = never).
    pub snapshot_retain_days: u32,
    /// Resolved errors whose last occurrence is older than this are deleted (0 = never).
    pub errors_retain_days: u32,
    /// Sent notifications (0 = never).
    pub notifications_retain_days: u32,
    /// Audit log (0 = never).
    pub audit_log_retain_days: u32,
    /// Unified `event_log` rows (0 = never). v2.1.
    pub event_log_retain_days: u32,
    /// Also record successful GET API requests in the event log (non-GET, MCP and errors are
    /// always recorded).
    pub log_reads: bool,
}

/// Install-wide WordPress defaults. Per-site settings win; these fill in what a site leaves unset.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WordpressCfg {
    pub default_insert_modules: Vec<String>,
    /// Verify the live page after injecting (a site's `skip_inject_verify` still wins).
    pub verify_after_inject: bool,
    pub default_max_links_per_post: u32,
    pub default_min_links_per_post: u32,
    pub max_new_links_per_pass: u32,
    /// Timeout for WordPress REST reads/writes, ms.
    pub request_timeout_ms: u64,
}

impl Default for WordpressCfg {
    fn default() -> Self {
        // Same values as the engine defaults (sf_engine::density::LINK_DENSITY), so an untouched
        // section changes nothing.
        Self {
            default_insert_modules: vec!["native".into()],
            verify_after_inject: true,
            default_max_links_per_post: 10,
            default_min_links_per_post: 3,
            max_new_links_per_pass: 5,
            request_timeout_ms: 20_000,
        }
    }
}

/// AI tasks (v2.3, `docs/specs/v2.3-ai-tiers.md`).
pub mod ai_tasks {
    pub const ANCHOR_PICK: &str = "anchor_pick";
    pub const RELEVANCE_CHECK: &str = "relevance_check";
    pub const LINK_VERIFY_JUDGE: &str = "link_verify_judge";
    pub const REPHRASE: &str = "rephrase";
    pub const CONTEXT_SENTENCE: &str = "context_sentence";
    pub const REPAIR_INSERT: &str = "repair_insert";
    pub const MONEY_PAGE: &str = "money_page";
    pub const PRIORITY: &str = "priority";
    /// Every task with its default tier.
    pub const DEFAULTS: &[(&str, &str)] = &[
        (ANCHOR_PICK, "low"),
        (RELEVANCE_CHECK, "low"),
        (LINK_VERIFY_JUDGE, "low"),
        (REPHRASE, "main"),
        (CONTEXT_SENTENCE, "main"),
        (REPAIR_INSERT, "main"),
        (MONEY_PAGE, "high"),
        (PRIORITY, "high"),
    ];
    pub const TIERS: &[&str] = &["low", "main", "high"];
    pub const EFFORTS: &[&str] = &["none", "low", "medium", "high"];
}

/// Heuristic confidence above which no AI call is made.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AiConfidenceSkip {
    pub anchor: f64,
    pub relevance: f64,
}

impl Default for AiConfidenceSkip {
    fn default() -> Self {
        Self {
            anchor: 0.85,
            relevance: 0.80,
        }
    }
}

/// Per-tier values.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PerTier<T> {
    pub low: T,
    pub main: T,
    pub high: T,
}

impl<T: Clone> PerTier<T> {
    pub fn get(&self, tier: &str) -> T {
        match tier {
            "low" => self.low.clone(),
            "high" => self.high.clone(),
            _ => self.main.clone(),
        }
    }
    pub fn set(&mut self, tier: &str, v: T) {
        match tier {
            "low" => self.low = v,
            "high" => self.high = v,
            _ => self.main = v,
        }
    }
}

impl Default for PerTier<u64> {
    fn default() -> Self {
        Self {
            low: 2_000_000,
            main: 1_000_000,
            high: 300_000,
        }
    }
}

impl Default for PerTier<String> {
    fn default() -> Self {
        Self {
            low: "low".into(),
            main: "low".into(),
            high: "medium".into(),
        }
    }
}

/// Daily token budgets (UTC day, tokens in + out; 0 = unlimited).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AiBudget {
    pub daily_tokens: PerTier<u64>,
    pub per_site_daily_tokens: u64,
}

impl Default for AiBudget {
    fn default() -> Self {
        Self {
            daily_tokens: PerTier::default(),
            per_site_daily_tokens: 300_000,
        }
    }
}

/// `[ai]` (v2.3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AiCfg {
    /// "Always use AI for perfect linking": AI anchor/relevance checks on every plan and run.
    pub always_on: bool,
    /// task -> tier (missing tasks use [`ai_tasks::DEFAULTS`]).
    pub task_tiers: std::collections::BTreeMap<String, String>,
    /// Edges per batched low-tier request.
    pub batch_size: usize,
    pub confidence_skip: AiConfidenceSkip,
    pub budget: AiBudget,
    /// 0 = cache off.
    pub cache_ttl_days: u32,
    /// Per tier: `none` | `low` | `medium` | `high`.
    pub reasoning_effort: PerTier<String>,
}

impl Default for AiCfg {
    fn default() -> Self {
        Self {
            always_on: false,
            task_tiers: ai_tasks::DEFAULTS
                .iter()
                .map(|(t, v)| ((*t).to_string(), (*v).to_string()))
                .collect(),
            batch_size: 20,
            confidence_skip: AiConfidenceSkip::default(),
            budget: AiBudget::default(),
            cache_ttl_days: 30,
            reasoning_effort: PerTier::default(),
        }
    }
}

impl AiCfg {
    /// The tier a task runs on (configured, else its default, else `main`).
    pub fn tier_for(&self, task: &str) -> String {
        if let Some(t) = self.task_tiers.get(task)
            && ai_tasks::TIERS.contains(&t.as_str())
        {
            return t.clone();
        }
        ai_tasks::DEFAULTS
            .iter()
            .find(|(t, _)| *t == task)
            .map(|(_, v)| (*v).to_string())
            .unwrap_or_else(|| "main".into())
    }

    /// Every known task with its effective tier (for the settings document).
    pub fn effective_task_tiers(&self) -> std::collections::BTreeMap<String, String> {
        ai_tasks::DEFAULTS
            .iter()
            .map(|(t, _)| ((*t).to_string(), self.tier_for(t)))
            .collect()
    }

    pub fn batch(&self) -> usize {
        self.batch_size.clamp(1, 50)
    }
}

/// Where release zips are listed from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadsCfg {
    /// Empty = `<exe dir>/../releases` (the installed layout `app\` + `releases\`).
    pub dir: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            home: PathBuf::from("."),
            server: ServerCfg::default(),
            auth: AuthCfg::default(),
            resources: ResourcesCfg::default(),
            embed: EmbedCfg::default(),
            llm: LlmCfg::default(),
            crawl: CrawlCfg::default(),
            backup: BackupCfg::default(),
            monitor: MonitorCfg::default(),
            wordpress: WordpressCfg::default(),
            downloads: DownloadsCfg::default(),
            ai: AiCfg::default(),
            gsc: GscCfg::default(),
        }
    }
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            binds: vec!["127.0.0.1:8600".into(), "127.0.0.1:8601".into()],
            body_limit_mb: 25,
            cors_origins: Vec::new(),
        }
    }
}

impl Default for AuthCfg {
    fn default() -> Self {
        Self {
            jwt_secret: String::new(),
            encryption_key: String::new(),
            allow_registration: false,
            admin_email: "admin@siloforge.local".into(),
            admin_password: String::new(),
        }
    }
}

impl Default for ResourcesCfg {
    fn default() -> Self {
        Self {
            cpu_threads: 0,
            max_concurrent_jobs: 3,
            vector_cache_mb: 512,
            low_priority_jobs: true,
            defer_when_cpu_above: 0.0,
            max_memory_mb: 0,
        }
    }
}

impl Default for EmbedCfg {
    fn default() -> Self {
        Self {
            chain: vec![
                "remote".into(),
                "remote2".into(),
                "onnx".into(),
                "local".into(),
            ],
            remote_url: "https://openrouter.ai/api/v1".into(),
            remote_model: "sentence-transformers/all-minilm-l6-v2".into(),
            remote_key: String::new(),
            remote_key2: String::new(),
            batch_size: 64,
            max_chars: 2000,
            timeout_ms: 20_000,
            retries: 2,
            model_dir: String::new(),
            onnx_runtime_dir: String::new(),
            gpu: true,
            mode: "auto".into(),
            hybrid_gpu_share: 0.5,
        }
    }
}

impl Default for LlmCfg {
    fn default() -> Self {
        Self {
            fallback_url: "http://127.0.0.1:3460/v1".into(),
            fallback_models: vec!["claude-sonnet-5-5".into()],
            timeout_ms: 120_000,
        }
    }
}

impl Default for CrawlCfg {
    fn default() -> Self {
        let mut aliases = std::collections::BTreeMap::new();
        aliases.insert("site01.example".into(), "127.0.0.1".into());
        Self {
            user_agent: format!("SiloForge/{} (+internal-linking)", crate::VERSION),
            allow_private_hosts: false,
            host_aliases: aliases,
            concurrency: 4,
            timeout_ms: 20_000,
            max_pages: 20_000,
        }
    }
}

impl Default for BackupCfg {
    fn default() -> Self {
        Self {
            dir: String::new(),
            keep: 10,
            schedule: "17 3 * * *".into(),
            passphrase_env: "SILOFORGE_BACKUP_PASSPHRASE".into(),
        }
    }
}

impl Default for MonitorCfg {
    fn default() -> Self {
        Self {
            sample_secs: 5,
            retain_days: 14,
            job_log_retain_days: 30,
            alert_cpu_pct: 95.0,
            alert_mem_pct: 92.0,
            alert_disk_free_mb: 2048,
            maintenance_cron: "30 4 * * *".into(),
            job_runs_retain_days: 180,
            job_runs_keep_per_site: 50,
            snapshot_retain_days: 180,
            errors_retain_days: 90,
            notifications_retain_days: 90,
            audit_log_retain_days: 365,
            event_log_retain_days: 90,
            log_reads: false,
        }
    }
}

/// Default home: `$SILOFORGE_HOME`, else `<exe dir>/data` (portable install), else `./data`.
pub fn default_home() -> PathBuf {
    if let Ok(h) = std::env::var("SILOFORGE_HOME")
        && !h.trim().is_empty()
    {
        return PathBuf::from(h);
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("data")))
        .unwrap_or_else(|| PathBuf::from("data"))
}

impl Config {
    /// Load `home/siloforge.toml` (optional), `home/secrets.env` (optional, KEY=VALUE), then process env.
    pub fn load(home: &Path) -> Result<Self> {
        let path = home.join("siloforge.toml");
        let mut cfg: Config = if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            toml::from_str(&text).map_err(|e| Error::bad(format!("{}: {e}", path.display())))?
        } else {
            Config::default()
        };
        cfg.home = home.to_path_buf();
        let secrets = home.join("secrets.env");
        let file_env = if secrets.exists() {
            parse_env_file(&std::fs::read_to_string(&secrets)?)
        } else {
            Vec::new()
        };
        let get = |k: &str| -> Option<String> {
            std::env::var(k)
                .ok()
                .or_else(|| {
                    file_env
                        .iter()
                        .find(|(a, _)| a == k)
                        .map(|(_, v)| v.clone())
                })
                .filter(|v| !v.is_empty())
        };
        cfg.apply_env(&get);
        Ok(cfg)
    }

    fn apply_env(&mut self, get: &dyn Fn(&str) -> Option<String>) {
        if let Some(v) = get("JWT_SECRET") {
            self.auth.jwt_secret = v;
        }
        if let Some(v) = get("SILOFORGE_ENCRYPTION_KEY") {
            self.auth.encryption_key = v;
        }
        if let Some(v) = get("ADMIN_EMAIL") {
            self.auth.admin_email = v;
        }
        if let Some(v) = get("ADMIN_PASSWORD") {
            self.auth.admin_password = v;
        }
        if let Some(v) = get("SILOFORGE_BINDS") {
            self.server.binds = v.split(',').map(|s| s.trim().to_string()).collect();
        }
        if let Some(v) = get("OPENROUTER_URL") {
            self.embed.remote_url = v;
        }
        if let Some(v) = get("OPENROUTER_API_KEY") {
            self.embed.remote_key = v;
        }
        if let Some(v) = get("OPENROUTER_API_KEY_T2") {
            self.embed.remote_key2 = v;
        }
        if let Some(v) = get("EMBED_REMOTE_MODEL") {
            self.embed.remote_model = v;
        }
        if let Some(v) = get("EMBED_CHAIN") {
            self.embed.chain = v
                .split(',')
                .map(|s| match s.trim() {
                    "openrouter" | "openrouter1" => "remote".to_string(),
                    "openrouter2" => "remote2".to_string(),
                    other => other.to_string(),
                })
                .collect();
        }
        if let Some(v) = get("DARIO_URL") {
            self.llm.fallback_url = v.replace("site01.example", "127.0.0.1");
        }
        let mut models = Vec::new();
        for k in ["DARIO_MODEL_PRIMARY", "DARIO_MODEL_FALLBACK"] {
            if let Some(m) = get(k)
                && !models.contains(&m)
            {
                models.push(m);
            }
        }
        if !models.is_empty() {
            self.llm.fallback_models = models;
        }
        if let Some(v) = get("CRAWL_ALLOW_PRIVATE_HOSTS") {
            self.crawl.allow_private_hosts = matches!(v.as_str(), "1" | "true" | "yes");
        }
        if let Some(v) = get("GSC_CREDENTIALS_JSON") {
            self.gsc.credentials_json = v;
        }
        if let Some(v) = get("SILOFORGE_CPU_THREADS").and_then(|v| v.parse().ok()) {
            self.resources.cpu_threads = v;
        }
    }

    /// Fatal misconfiguration check, run at `serve` startup.
    pub fn validate_for_serve(&self) -> Result<()> {
        if self.auth.jwt_secret.len() < 16 || self.auth.jwt_secret == "dev-secret-do-not-use" {
            return Err(Error::bad(
                "JWT_SECRET must be set (>= 16 chars, not the v1 dev default); run `siloforge init` to generate one",
            ));
        }
        Ok(())
    }

    /// Master secret for stored-secret encryption (v1 fallback: the JWT secret).
    pub fn encryption_master(&self) -> &str {
        if self.auth.encryption_key.is_empty() {
            &self.auth.jwt_secret
        } else {
            &self.auth.encryption_key
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.home.join("siloforge.db")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.home.join("logs")
    }
    pub fn models_dir(&self) -> PathBuf {
        if self.embed.model_dir.is_empty() {
            self.home.join("models").join("all-MiniLM-L6-v2")
        } else {
            PathBuf::from(&self.embed.model_dir)
        }
    }
    pub fn backups_dir(&self) -> PathBuf {
        if self.backup.dir.is_empty() {
            self.home.join("backups")
        } else {
            PathBuf::from(&self.backup.dir)
        }
    }
    /// Release downloads directory (`downloads.dir`, else `<exe dir>/../releases`).
    pub fn downloads_dir(&self) -> PathBuf {
        if !self.downloads.dir.trim().is_empty() {
            return PathBuf::from(self.downloads.dir.trim());
        }
        std::env::current_exe()
            .ok()
            .and_then(|p| {
                p.parent()
                    .and_then(Path::parent)
                    .map(|d| d.join("releases"))
            })
            .unwrap_or_else(|| self.home.join("releases"))
    }

    pub fn cpu_threads(&self) -> usize {
        if self.resources.cpu_threads > 0 {
            return self.resources.cpu_threads;
        }
        let logical = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        // logical/4 ~= physical/2 on SMT machines; never below 1.
        (logical / 4).max(1)
    }
}

/// Minimal `KEY=VALUE` parser (comments, blank lines, optional quotes).
pub fn parse_env_file(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let l = l.strip_prefix("export ").unwrap_or(l);
            let (k, v) = l.split_once('=')?;
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(v);
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_parsing() {
        let v = parse_env_file("# c\nA=1\nexport B=\"two\"\nC='3'\n\nbad\n");
        assert_eq!(
            v,
            vec![
                ("A".into(), "1".into()),
                ("B".into(), "two".into()),
                ("C".into(), "3".into())
            ]
        );
    }

    #[test]
    fn v1_env_names_map() {
        let mut c = Config::default();
        let env = [
            ("JWT_SECRET", "x".repeat(20)),
            ("EMBED_CHAIN", "openrouter,openrouter2,local".to_string()),
            (
                "DARIO_URL",
                "http://site01.example:3460/v1".to_string(),
            ),
            ("DARIO_MODEL_PRIMARY", "m1".to_string()),
            ("DARIO_MODEL_FALLBACK", "m1".to_string()),
        ];
        c.apply_env(&|k| env.iter().find(|(a, _)| *a == k).map(|(_, v)| v.clone()));
        assert_eq!(c.embed.chain, vec!["remote", "remote2", "local"]);
        assert_eq!(c.llm.fallback_url, "http://127.0.0.1:3460/v1");
        assert_eq!(c.llm.fallback_models, vec!["m1"]);
        assert!(c.validate_for_serve().is_ok());
        assert_eq!(c.encryption_master(), c.auth.jwt_secret);
    }

    #[test]
    fn gsc_fallback_from_env_file_is_never_serialised_or_debug_printed() {
        let dir = std::env::temp_dir().join(format!("sf-core-gsc-{}", crate::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let json =
            r#"{"type":"service_account","client_email":"a@b.iam","private_key":"SECRET-PEM"}"#;
        std::fs::write(
            dir.join("secrets.env"),
            format!(
                "GSC_CREDENTIALS_JSON='{json}'
"
            ),
        )
        .unwrap();
        let c = Config::load(&dir).unwrap();
        let got = c.gsc.fallback_credentials().unwrap();
        assert!(got.contains("a@b.iam"));
        assert!(!serde_json::to_string(&c).unwrap().contains("SECRET-PEM"));
        assert!(!format!("{c:?}").contains("SECRET-PEM"));
        assert!(format!("{:?}", c.gsc).contains("<redacted>"));
        // A path works too; garbage and non-objects are ignored.
        let f = dir.join("sa.json");
        std::fs::write(&f, json).unwrap();
        let by_path = GscCfg {
            credentials_json: f.display().to_string(),
        };
        assert_eq!(by_path.fallback_credentials(), Some(got));
        for bad in ["", "  ", "{not json", "[1,2]", "D:/definitely/missing.json"] {
            let g = GscCfg {
                credentials_json: bad.into(),
            };
            assert!(g.fallback_credentials().is_none(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_weak_secret() {
        let mut c = Config::default();
        c.auth.jwt_secret = "dev-secret-do-not-use".into();
        assert!(c.validate_for_serve().is_err());
    }

    #[test]
    fn debug_never_prints_secrets() {
        let mut c = Config::default();
        c.auth.jwt_secret = "JWT-SECRET-VALUE-123".into();
        c.auth.encryption_key = "ENC-KEY-VALUE".into();
        c.auth.admin_password = "ADMIN-PW-VALUE".into();
        c.embed.remote_key = "sk-or-REMOTE".into();
        c.embed.remote_key2 = "sk-or-REMOTE2".into();
        let d = format!("{c:?}");
        for leak in ["JWT-SECRET", "ENC-KEY", "ADMIN-PW", "sk-or-REMOTE"] {
            assert!(!d.contains(leak), "{leak} in {d}");
        }
        assert!(d.contains("secrets redacted"));
    }
}
