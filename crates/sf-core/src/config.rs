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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    fn rejects_weak_secret() {
        let mut c = Config::default();
        c.auth.jwt_secret = "dev-secret-do-not-use".into();
        assert!(c.validate_for_serve().is_err());
    }
}
