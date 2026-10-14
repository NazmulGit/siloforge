//! Public request/response types and the error type.

use serde::{Deserialize, Serialize};

/// What a chain (and a model row) is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Chat,
    Embed,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Chat => "chat",
            Role::Embed => "embed",
        }
    }
}

/// AI task tier (v2.3): which chain family serves a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Low,
    #[default]
    Main,
    High,
}

impl Tier {
    pub const ALL: [Tier; 3] = [Tier::Low, Tier::Main, Tier::High];

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Low => "low",
            Tier::Main => "main",
            Tier::High => "high",
        }
    }

    /// Unknown values are `main`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Tier::Low,
            "high" => Tier::High,
            _ => Tier::Main,
        }
    }
}

/// `model_providers.kind`. Unknown values are kept (golden rule 5) and fail the rung clearly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAiCompatible,
    OpenRouter,
    Anthropic,
    Ollama,
    Gemini,
    Custom,
    Unknown(String),
}

impl ProviderKind {
    pub fn parse(s: &str) -> Self {
        match s {
            "openai-compatible" => Self::OpenAiCompatible,
            "openrouter" => Self::OpenRouter,
            "anthropic" => Self::Anthropic,
            "ollama" => Self::Ollama,
            "gemini" => Self::Gemini,
            "custom" => Self::Custom,
            other => Self::Unknown(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::OpenAiCompatible => "openai-compatible",
            Self::OpenRouter => "openrouter",
            Self::Anthropic => "anthropic",
            Self::Ollama => "ollama",
            Self::Gemini => "gemini",
            Self::Custom => "custom",
            Self::Unknown(s) => s,
        }
    }

    /// Speaks the OpenAI REST dialect (`/chat/completions`, `/embeddings`).
    pub fn is_openai_shaped(&self) -> bool {
        matches!(
            self,
            Self::OpenAiCompatible | Self::OpenRouter | Self::Custom
        )
    }
}

/// One chat turn. `role` is `system`, `user` or `assistant` (anything else is sent as `user`
/// to providers that are strict about roles).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }
}

/// A chat call. `Default` gives v1's defaults (max_tokens 2000, temperature 0.3, global chain).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Consult this site's active chat chain first (fixes v1 G2).
    pub site_id: Option<String>,
    /// Ask for a JSON reply (native flag where the provider has a safe one, plus a system nudge).
    pub json_mode: bool,
    /// v2.3 task name (`sf_core::config::ai_tasks`): routes to a tier and is accounted.
    #[serde(default)]
    pub task: Option<String>,
    /// Explicit tier (wins over the task routing).
    #[serde(default)]
    pub tier: Option<Tier>,
    /// `none` | `low` | `medium` | `high`; None = the tier's configured effort.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Link Run id (or job id) for accounting.
    #[serde(default)]
    pub run_id: Option<String>,
    /// Edges carried by this (batched) call, for accounting (0 is treated as 1).
    #[serde(default)]
    pub items: u32,
}

impl Default for ChatRequest {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            max_tokens: 2000,
            temperature: 0.3,
            site_id: None,
            json_mode: false,
            task: None,
            tier: None,
            reasoning_effort: None,
            run_id: None,
            items: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub text: String,
    /// Provider name (`model_providers.name`, or `env:dario` for the env fallback).
    pub provider: String,
    /// Wire model id.
    pub model: String,
    /// Index of the serving rung in the resolved chain (0-based).
    pub rung: usize,
    pub latency_ms: u64,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    /// Rungs that failed or were skipped (open circuit) before this one served.
    pub fallbacks_used: usize,
    /// Tier that served the call.
    #[serde(default)]
    pub tier: Tier,
    /// True when the token counts are chars/4 estimates (provider sent no usage).
    #[serde(default)]
    pub tokens_estimated: bool,
    /// From `models.price_in/out` (USD per 1M tokens); 0 when unpriced.
    #[serde(default)]
    pub cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbedResponse {
    /// One L2-normalised vector per input text, in input order.
    pub vectors: Vec<Vec<f32>>,
    pub provider: String,
    pub model: String,
    pub rung: usize,
    pub latency_ms: u64,
    pub tokens_in: Option<i64>,
    pub fallbacks_used: usize,
}

/// Errors from the AI layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiError {
    /// No usable chain for the role (embed: no active embed chain; chat: no chain and no env models).
    /// sf-embed treats this as "use your own chain".
    NotConfigured(String),
    /// Every rung failed (or was skipped); the message lists each rung and why.
    AllFailed(String),
    /// The per-call hard deadline ran out before any rung succeeded.
    DeadlineExceeded(String),
    /// A reply arrived but could not be used (e.g. `chat_json` could not parse it after a repair).
    InvalidOutput(String),
    /// Caller input rejected (too many texts, empty messages, ...).
    BadRequest(String),
    NotFound(String),
    Db(String),
    /// The daily token budget of the tier or the site is used up (v2.3): no provider was called.
    BudgetExhausted(String),
}

impl std::fmt::Display for AiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AiError::NotConfigured(m) => write!(f, "not configured: {m}"),
            AiError::AllFailed(m) => write!(f, "{m}"),
            AiError::DeadlineExceeded(m) => write!(f, "deadline exceeded: {m}"),
            AiError::InvalidOutput(m) => write!(f, "invalid model output: {m}"),
            AiError::BadRequest(m) => write!(f, "{m}"),
            AiError::NotFound(m) => write!(f, "{m}"),
            AiError::Db(m) => write!(f, "database: {m}"),
            AiError::BudgetExhausted(m) => write!(f, "budget exhausted: {m}"),
        }
    }
}

impl std::error::Error for AiError {}

impl From<sf_core::Error> for AiError {
    fn from(e: sf_core::Error) -> Self {
        match e {
            sf_core::Error::NotFound(m) => AiError::NotFound(m),
            sf_core::Error::BadRequest(m) => AiError::BadRequest(m),
            other => AiError::Db(other.to_string()),
        }
    }
}

impl From<AiError> for sf_core::Error {
    fn from(e: AiError) -> Self {
        match e {
            AiError::NotFound(m) => sf_core::Error::NotFound(m),
            AiError::BadRequest(m) => sf_core::Error::BadRequest(m),
            AiError::Db(m) => sf_core::Error::Db(m),
            other => sf_core::Error::Upstream(other.to_string()),
        }
    }
}

/// Where a resolved chain came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChainSource {
    Site,
    Global,
    Env,
    None,
}

/// Display view of one rung (never carries the key).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RungInfo {
    pub position: i64,
    pub model_row_id: Option<String>,
    pub model_id: String,
    pub dim: Option<i64>,
    pub provider_id: Option<String>,
    pub provider_name: String,
    pub provider_kind: String,
    pub base_url: String,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    /// `none` | `ok` | `undecryptable`
    pub key_status: String,
    /// Breaker state for this rung: `closed` | `open` | `half_open`.
    pub breaker: String,
    /// False when the rung is excluded at resolution (see `skip_reason`).
    pub usable: bool,
    pub skip_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChainDetail {
    pub role: Role,
    pub chain_id: Option<String>,
    pub chain_name: Option<String>,
    pub source: ChainSource,
    pub rungs: Vec<RungInfo>,
}

/// Circuit-breaker view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BreakerView {
    /// `closed` | `open` | `half_open`
    pub state: String,
    pub consecutive_failures: u32,
    /// Milliseconds until a half-open probe is allowed (open only).
    pub retry_in_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelStatus {
    pub model_row_id: Option<String>,
    pub model_id: String,
    pub role: String,
    pub enabled: bool,
    pub breaker: BreakerView,
    /// Calls counted in the window (last `STATUS_WINDOW` attempts from `model_call_log`).
    pub recent_calls: u32,
    pub recent_ok: u32,
    pub success_rate: Option<f64>,
    pub avg_latency_ms: Option<f64>,
    pub last_error: Option<String>,
    pub last_error_at: Option<String>,
    pub last_called_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub provider_id: Option<String>,
    pub name: String,
    pub label: String,
    pub kind: String,
    pub enabled: bool,
    pub base_url: String,
    pub has_api_key: bool,
    /// False when a stored key cannot be decrypted with the current key.
    pub key_ok: bool,
    pub last_probe_at: Option<String>,
    pub last_probe_ok: Option<bool>,
    pub last_probe_error: Option<String>,
    pub models: Vec<ModelStatus>,
}
