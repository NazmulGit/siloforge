//! Wire adapters: pure request shaping and response parsing per provider kind (spec 5.2).
//!
//! Nothing here touches the network or the clock, so every URL/header/body shape is unit-tested.

use serde_json::{Map, Value, json};

use crate::types::{ChatMessage, ProviderKind};

pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";
pub(crate) const OPENROUTER_REFERER: &str = "https://siloforge.local";
pub(crate) const OPENROUTER_TITLE: &str = "SiloForge";
pub(crate) const JSON_NUDGE: &str =
    "Respond with a single valid JSON value only. No prose, no markdown code fences.";

/// The API key state of a rung. The plaintext lives only for the duration of a call.
#[derive(Clone)]
pub(crate) enum KeyState {
    None,
    Plain(String),
    /// Stored blob exists but cannot be decrypted (wrong master key). The rung fails clearly
    /// instead of going out unauthenticated like v1 did.
    Undecryptable,
}

impl KeyState {
    pub(crate) fn status(&self) -> &'static str {
        match self {
            KeyState::None => "none",
            KeyState::Plain(_) => "ok",
            KeyState::Undecryptable => "undecryptable",
        }
    }
}

/// One executable rung (a chain rung, an env-fallback rung or a probe target).
#[derive(Clone)]
pub(crate) struct Rung {
    pub position: i64,
    pub model_row_id: Option<String>,
    pub model_id: String,
    pub dim: Option<i64>,
    pub provider_id: Option<String>,
    pub provider_name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub key: KeyState,
    pub headers: Vec<(String, String)>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub chain_id: Option<String>,
}

impl std::fmt::Debug for Rung {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key.
        f.debug_struct("Rung")
            .field("position", &self.position)
            .field("model_row_id", &self.model_row_id)
            .field("model_id", &self.model_id)
            .field("provider_name", &self.provider_name)
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("key", &self.key.status())
            .finish()
    }
}

impl Rung {
    pub(crate) fn label(&self) -> String {
        format!("{}/{}", self.provider_name, self.model_id)
    }

    /// Breaker key: the model row, or `env:<model>` for env rungs.
    pub(crate) fn breaker_key(&self) -> String {
        match &self.model_row_id {
            Some(id) => id.clone(),
            None => format!("env:{}", self.model_id),
        }
    }
}

/// A fully-shaped HTTP POST.
#[derive(Debug, Clone)]
pub(crate) struct WireRequest {
    pub url: String,
    /// Applied in order; later entries replace earlier ones case-insensitively (custom headers last).
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

/// Strips trailing slashes (v1 `normalizeBaseUrl`) and applies a per-kind default when empty.
pub(crate) fn normalize_base_url(kind: &ProviderKind, raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    match kind {
        ProviderKind::Anthropic => "https://api.anthropic.com".into(),
        ProviderKind::OpenRouter => "https://openrouter.ai/api/v1".into(),
        ProviderKind::Ollama => "http://127.0.0.1:11434".into(),
        ProviderKind::Gemini => "https://generativelanguage.googleapis.com/v1beta".into(),
        _ => String::new(),
    }
}

/// Parses the `headers` JSON column into string pairs (non-string values ignored).
pub(crate) fn headers_from_json(v: &Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = match v {
        Value::Object(m) => m
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
        _ => Vec::new(),
    };
    out.sort();
    out
}

fn key_str(rung: &Rung) -> Result<&str, String> {
    match &rung.key {
        KeyState::None => Ok(""),
        KeyState::Plain(k) => Ok(k.as_str()),
        KeyState::Undecryptable => Err(format!(
            "api key for provider '{}' could not be decrypted (the encryption key changed?) - re-enter the key in Settings > Models",
            rung.provider_name
        )),
    }
}

fn header_list(rung: &Rung) -> Result<Vec<(String, String)>, String> {
    let key = key_str(rung)?;
    let mut h: Vec<(String, String)> = vec![("content-type".into(), "application/json".into())];
    match &rung.kind {
        ProviderKind::Anthropic => {
            h.push(("anthropic-version".into(), ANTHROPIC_VERSION.into()));
            if !key.is_empty() {
                h.push(("x-api-key".into(), key.into()));
            }
        }
        ProviderKind::Gemini => {
            if !key.is_empty() {
                h.push(("x-goog-api-key".into(), key.into()));
            }
        }
        _ => {
            if !key.is_empty() {
                h.push(("authorization".into(), format!("Bearer {key}")));
            }
        }
    }
    if rung.kind == ProviderKind::OpenRouter {
        h.push(("HTTP-Referer".into(), OPENROUTER_REFERER.into()));
        h.push(("X-Title".into(), OPENROUTER_TITLE.into()));
    }
    // Custom headers merged last: an operator can override anything above.
    h.extend(rung.headers.iter().cloned());
    Ok(h)
}

fn require_base(rung: &Rung) -> Result<&str, String> {
    if rung.base_url.is_empty() {
        return Err(format!("provider '{}' has no base_url", rung.provider_name));
    }
    Ok(rung.base_url.as_str())
}

/// Gemini model path segment: strips `models/`, refuses characters that would change the URL.
fn gemini_model(model_id: &str) -> Result<&str, String> {
    let m = model_id.strip_prefix("models/").unwrap_or(model_id);
    if m.is_empty()
        || !m
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
    {
        return Err(format!("invalid gemini model id '{model_id}'"));
    }
    Ok(m)
}

/// Anthropic base is the host root; tolerate an operator who already appended `/v1`.
fn anthropic_url(base: &str) -> String {
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

fn split_system(messages: &[ChatMessage], json_mode: bool) -> (String, Vec<&ChatMessage>) {
    let mut system: Vec<&str> = Vec::new();
    let mut turns = Vec::new();
    for m in messages {
        if m.role == "system" {
            system.push(m.content.as_str());
        } else {
            turns.push(m);
        }
    }
    if json_mode {
        system.push(JSON_NUDGE);
    }
    (system.join("\n\n"), turns)
}

fn openai_messages(messages: &[ChatMessage], json_mode: bool) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len() + 1);
    if json_mode {
        out.push(json!({"role": "system", "content": JSON_NUDGE}));
    }
    out.extend(
        messages
            .iter()
            .map(|m| json!({"role": m.role, "content": m.content})),
    );
    out
}

/// Builds the chat request for a rung.
pub(crate) fn chat_request(
    rung: &Rung,
    messages: &[ChatMessage],
    max_tokens: u32,
    temperature: f32,
    json_mode: bool,
) -> Result<WireRequest, String> {
    let headers = header_list(rung)?;
    let base = require_base(rung)?;
    let temp = f64::from(temperature);
    match &rung.kind {
        ProviderKind::Anthropic => {
            let (system, turns) = split_system(messages, json_mode);
            let turns: Vec<Value> = turns
                .iter()
                .map(|m| {
                    let role = if m.role == "assistant" {
                        "assistant"
                    } else {
                        "user"
                    };
                    json!({"role": role, "content": m.content})
                })
                .collect();
            let mut body = Map::new();
            body.insert("model".into(), json!(rung.model_id));
            body.insert("max_tokens".into(), json!(max_tokens));
            body.insert("temperature".into(), json!(temp));
            if !system.is_empty() {
                body.insert("system".into(), json!(system));
            }
            body.insert("messages".into(), Value::Array(turns));
            Ok(WireRequest {
                url: anthropic_url(base),
                headers,
                body: Value::Object(body),
            })
        }
        ProviderKind::Ollama => {
            let mut body = json!({
                "model": rung.model_id,
                "messages": messages.iter().map(|m| json!({"role": m.role, "content": m.content})).collect::<Vec<_>>(),
                "stream": false,
                "options": {"temperature": temp, "num_predict": max_tokens},
            });
            if json_mode && let Some(o) = body.as_object_mut() {
                o.insert("format".into(), json!("json"));
            }
            Ok(WireRequest {
                url: format!("{base}/api/chat"),
                headers,
                body,
            })
        }
        ProviderKind::Gemini => {
            let model = gemini_model(&rung.model_id)?;
            let (system, turns) = split_system(messages, json_mode);
            let contents: Vec<Value> = turns
                .iter()
                .map(|m| {
                    let role = if m.role == "assistant" {
                        "model"
                    } else {
                        "user"
                    };
                    json!({"role": role, "parts": [{"text": m.content}]})
                })
                .collect();
            let mut gen_cfg = json!({"maxOutputTokens": max_tokens, "temperature": temp});
            if json_mode && let Some(o) = gen_cfg.as_object_mut() {
                o.insert("responseMimeType".into(), json!("application/json"));
            }
            let mut body = Map::new();
            body.insert("contents".into(), Value::Array(contents));
            if !system.is_empty() {
                body.insert(
                    "systemInstruction".into(),
                    json!({"parts": [{"text": system}]}),
                );
            }
            body.insert("generationConfig".into(), gen_cfg);
            Ok(WireRequest {
                url: format!("{base}/models/{model}:generateContent"),
                headers,
                body: Value::Object(body),
            })
        }
        k if k.is_openai_shaped() => Ok(WireRequest {
            url: format!("{base}/chat/completions"),
            headers,
            body: json!({
                "model": rung.model_id,
                "messages": openai_messages(messages, json_mode),
                "max_tokens": max_tokens,
                "temperature": temp,
            }),
        }),
        other => Err(format!("unsupported provider kind '{}'", other.as_str())),
    }
}

/// One embed request plus how many vectors its response must carry.
pub(crate) struct EmbedCall {
    pub req: WireRequest,
    pub expected: usize,
}

/// Builds the embedding request(s). Ollama `/api/embeddings` takes one prompt per call.
pub(crate) fn embed_requests(rung: &Rung, texts: &[String]) -> Result<Vec<EmbedCall>, String> {
    let headers = header_list(rung)?;
    let base = require_base(rung)?;
    match &rung.kind {
        ProviderKind::Anthropic => Err(
            "anthropic providers have no embeddings endpoint - use an embed-capable provider"
                .into(),
        ),
        ProviderKind::Ollama => Ok(texts
            .iter()
            .map(|t| EmbedCall {
                req: WireRequest {
                    url: format!("{base}/api/embeddings"),
                    headers: headers.clone(),
                    body: json!({"model": rung.model_id, "prompt": t}),
                },
                expected: 1,
            })
            .collect()),
        ProviderKind::Gemini => {
            let model = gemini_model(&rung.model_id)?;
            let full = format!("models/{model}");
            if texts.len() == 1 {
                let text = texts.first().map(String::as_str).unwrap_or("");
                Ok(vec![EmbedCall {
                    req: WireRequest {
                        url: format!("{base}/models/{model}:embedContent"),
                        headers,
                        body: json!({"model": full, "content": {"parts": [{"text": text}]}}),
                    },
                    expected: 1,
                }])
            } else {
                let requests: Vec<Value> = texts
                    .iter()
                    .map(|t| json!({"model": full, "content": {"parts": [{"text": t}]}}))
                    .collect();
                Ok(vec![EmbedCall {
                    req: WireRequest {
                        url: format!("{base}/models/{model}:batchEmbedContents"),
                        headers,
                        body: json!({"requests": requests}),
                    },
                    expected: texts.len(),
                }])
            }
        }
        k if k.is_openai_shaped() => Ok(vec![EmbedCall {
            req: WireRequest {
                url: format!("{base}/embeddings"),
                headers,
                body: json!({"model": rung.model_id, "input": texts}),
            },
            expected: texts.len(),
        }]),
        other => Err(format!("unsupported provider kind '{}'", other.as_str())),
    }
}

/// Parsed chat reply.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ChatParsed {
    pub text: String,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    /// An in-band error object returned with HTTP 200 (OpenRouter does this).
    pub inband_error: Option<String>,
}

fn int_at(v: &Value, path: &[&str]) -> Option<i64> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_i64()
}

fn inband_error(body: &Value) -> Option<String> {
    let e = body.get("error")?;
    let msg = e
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| e.as_str().map(str::to_string))
        .unwrap_or_else(|| e.to_string());
    Some(truncate(&msg, 300))
}

/// Extracts the assistant text and token usage. Never panics on any JSON.
pub(crate) fn parse_chat(kind: &ProviderKind, body: &Value) -> ChatParsed {
    let mut out = ChatParsed {
        inband_error: inband_error(body),
        ..Default::default()
    };
    match kind {
        ProviderKind::Anthropic => {
            if let Some(parts) = body.get("content").and_then(Value::as_array) {
                out.text = parts
                    .iter()
                    .filter(|p| {
                        p.is_object()
                            && match p.get("type") {
                                None => true,
                                Some(t) => t.as_str() == Some("text"),
                            }
                    })
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<String>();
            }
            out.tokens_in = int_at(body, &["usage", "input_tokens"]);
            out.tokens_out = int_at(body, &["usage", "output_tokens"]);
        }
        ProviderKind::Ollama => {
            out.text = body
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            out.tokens_in = int_at(body, &["prompt_eval_count"]);
            out.tokens_out = int_at(body, &["eval_count"]);
        }
        ProviderKind::Gemini => {
            if let Some(parts) = body
                .get("candidates")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| c.get("content"))
                .and_then(|c| c.get("parts"))
                .and_then(Value::as_array)
            {
                out.text = parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<String>();
            }
            out.tokens_in = int_at(body, &["usageMetadata", "promptTokenCount"]);
            out.tokens_out = int_at(body, &["usageMetadata", "candidatesTokenCount"]);
        }
        _ => {
            out.text = body
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            out.tokens_in = int_at(body, &["usage", "prompt_tokens"]);
            out.tokens_out = int_at(body, &["usage", "completion_tokens"]);
        }
    }
    out
}

fn describe(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "null".into(),
        Some(Value::Array(a)) => format!("array({})", a.len()),
        Some(Value::Object(_)) => "object".into(),
        Some(Value::String(_)) => "string".into(),
        Some(Value::Number(_)) => "number".into(),
        Some(Value::Bool(_)) => "bool".into(),
    }
}

fn number_array(v: Option<&Value>, label: &str) -> Result<Vec<f64>, String> {
    let Some(arr) = v.and_then(Value::as_array) else {
        return Err(format!("{label} is not an array (got {})", describe(v)));
    };
    arr.iter()
        .enumerate()
        .map(|(i, n)| {
            n.as_f64()
                .ok_or_else(|| format!("{label}[{i}] is not a finite number"))
        })
        .collect()
}

/// Extracts raw (unvalidated) vectors from one embeddings response, plus prompt tokens if reported.
pub(crate) fn parse_embed(
    kind: &ProviderKind,
    body: &Value,
    expected: usize,
) -> Result<(Vec<Vec<f64>>, Option<i64>), String> {
    if let Some(e) = inband_error(body) {
        return Err(format!("provider error: {e}"));
    }
    match kind {
        ProviderKind::Ollama => {
            let single = body.get("embedding").or_else(|| {
                body.get("embeddings")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
            });
            Ok((
                vec![number_array(single, "ollama: embedding")?],
                int_at(body, &["prompt_eval_count"]),
            ))
        }
        ProviderKind::Gemini => {
            if let Some(list) = body.get("embeddings") {
                let Some(arr) = list.as_array() else {
                    return Err(format!(
                        "gemini: embeddings is not an array (got {})",
                        describe(Some(list))
                    ));
                };
                if arr.len() != expected {
                    return Err(format!("expected {expected} embeddings, got {}", arr.len()));
                }
                let vecs = arr
                    .iter()
                    .enumerate()
                    .map(|(i, e)| number_array(e.get("values"), &format!("embedding[{i}]")))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((vecs, None))
            } else {
                let v = body.get("embedding").and_then(|e| e.get("values"));
                Ok((vec![number_array(v, "gemini: embedding.values")?], None))
            }
        }
        _ => {
            let Some(data) = body.get("data").and_then(Value::as_array) else {
                return Err(format!(
                    "response has no data[] array (got {})",
                    describe(body.get("data"))
                ));
            };
            if data.len() != expected {
                return Err(format!(
                    "expected {expected} embeddings, got {}",
                    data.len()
                ));
            }
            // Providers may answer out of order when they set `index`.
            let mut rows: Vec<&Value> = data.iter().collect();
            if rows
                .iter()
                .all(|r| r.get("index").and_then(Value::as_u64).is_some())
            {
                rows.sort_by_key(|r| r.get("index").and_then(Value::as_u64).unwrap_or(0));
            }
            let vecs = rows
                .iter()
                .enumerate()
                .map(|(i, r)| number_array(r.get("embedding"), &format!("embedding[{i}]")))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((vecs, int_at(body, &["usage", "prompt_tokens"])))
        }
    }
}

/// Truncates to at most `max` chars (char-boundary safe).
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rung(kind: ProviderKind, key: KeyState) -> Rung {
        Rung {
            position: 0,
            model_row_id: Some("m1".into()),
            model_id: "mdl".into(),
            dim: None,
            provider_id: Some("p1".into()),
            provider_name: "prov".into(),
            kind,
            base_url: "http://h".into(),
            key,
            headers: vec![],
            timeout_ms: None,
            max_retries: None,
            chain_id: None,
        }
    }

    fn header<'a>(r: &'a WireRequest, name: &str) -> Option<&'a str> {
        r.headers
            .iter()
            .rev()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn openai_shape() {
        let r = rung(ProviderKind::OpenAiCompatible, KeyState::Plain("k".into()));
        let req = chat_request(&r, &[ChatMessage::user("hi")], 10, 0.5, false).unwrap();
        assert_eq!(req.url, "http://h/chat/completions");
        assert_eq!(header(&req, "authorization"), Some("Bearer k"));
        assert_eq!(req.body["max_tokens"], 10);
        assert_eq!(req.body["messages"][0]["content"], "hi");
        let none = rung(ProviderKind::OpenAiCompatible, KeyState::None);
        let req = chat_request(&none, &[], 1, 0.0, false).unwrap();
        assert_eq!(header(&req, "authorization"), None);
    }

    #[test]
    fn openrouter_headers_and_custom_override() {
        let mut r = rung(ProviderKind::OpenRouter, KeyState::Plain("k".into()));
        r.headers = vec![("Authorization".into(), "Custom x".into())];
        let req = chat_request(&r, &[], 1, 0.0, false).unwrap();
        assert_eq!(header(&req, "HTTP-Referer"), Some(OPENROUTER_REFERER));
        assert_eq!(header(&req, "X-Title"), Some(OPENROUTER_TITLE));
        assert_eq!(header(&req, "authorization"), Some("Custom x"));
    }

    #[test]
    fn anthropic_shape() {
        let r = rung(ProviderKind::Anthropic, KeyState::Plain("k".into()));
        let msgs = [
            ChatMessage::system("sys"),
            ChatMessage::user("u"),
            ChatMessage::assistant("a"),
        ];
        let req = chat_request(&r, &msgs, 5, 0.1, false).unwrap();
        assert_eq!(req.url, "http://h/v1/messages");
        assert_eq!(header(&req, "x-api-key"), Some("k"));
        assert_eq!(header(&req, "anthropic-version"), Some(ANTHROPIC_VERSION));
        assert_eq!(header(&req, "authorization"), None);
        assert_eq!(req.body["system"], "sys");
        assert_eq!(req.body["messages"].as_array().map(Vec::len), Some(2));
        assert!(embed_requests(&r, &["x".into()]).is_err());
        let p = parse_chat(
            &ProviderKind::Anthropic,
            &json!({"content":[{"type":"text","text":"a"},{"type":"tool_use"},{"text":"b"}],"usage":{"input_tokens":3,"output_tokens":4}}),
        );
        assert_eq!(p.text, "ab");
        assert_eq!((p.tokens_in, p.tokens_out), (Some(3), Some(4)));
    }

    #[test]
    fn ollama_shape() {
        let r = rung(ProviderKind::Ollama, KeyState::None);
        let req = chat_request(&r, &[ChatMessage::user("u")], 7, 0.2, true).unwrap();
        assert_eq!(req.url, "http://h/api/chat");
        assert_eq!(req.body["stream"], false);
        assert_eq!(req.body["options"]["num_predict"], 7);
        assert_eq!(req.body["format"], "json");
        let e = embed_requests(&r, &["a".into(), "b".into()]).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].req.url, "http://h/api/embeddings");
        assert_eq!(e[1].req.body["prompt"], "b");
    }

    #[test]
    fn gemini_shape() {
        let mut r = rung(ProviderKind::Gemini, KeyState::Plain("g".into()));
        r.model_id = "models/gemini-x".into();
        let msgs = [ChatMessage::system("s"), ChatMessage::assistant("a")];
        let req = chat_request(&r, &msgs, 9, 0.0, true).unwrap();
        assert_eq!(req.url, "http://h/models/gemini-x:generateContent");
        assert_eq!(header(&req, "x-goog-api-key"), Some("g"));
        assert_eq!(req.body["contents"][0]["role"], "model");
        assert_eq!(
            req.body["generationConfig"]["responseMimeType"],
            "application/json"
        );
        let one = embed_requests(&r, &["a".into()]).unwrap();
        assert!(one[0].req.url.ends_with(":embedContent"));
        let many = embed_requests(&r, &["a".into(), "b".into()]).unwrap();
        assert!(many[0].req.url.ends_with(":batchEmbedContents"));
        assert_eq!(many[0].expected, 2);
        r.model_id = "../evil?x".into();
        assert!(chat_request(&r, &[], 1, 0.0, false).is_err());
    }

    #[test]
    fn undecryptable_key_fails_clearly() {
        let r = rung(ProviderKind::OpenAiCompatible, KeyState::Undecryptable);
        let e = chat_request(&r, &[], 1, 0.0, false).unwrap_err();
        assert!(e.contains("could not be decrypted"));
    }

    #[test]
    fn embed_parse_orders_by_index() {
        let body = json!({"data":[{"index":1,"embedding":[2.0]},{"index":0,"embedding":[1.0]}]});
        let (v, _) = parse_embed(&ProviderKind::OpenAiCompatible, &body, 2).unwrap();
        assert_eq!(v, vec![vec![1.0], vec![2.0]]);
        assert!(parse_embed(&ProviderKind::OpenAiCompatible, &body, 3).is_err());
    }

    #[test]
    fn hostile_json_never_panics() {
        let kinds = [
            ProviderKind::OpenAiCompatible,
            ProviderKind::Anthropic,
            ProviderKind::Ollama,
            ProviderKind::Gemini,
            ProviderKind::Unknown("x".into()),
        ];
        let bodies = [
            json!(null),
            json!(1),
            json!("s"),
            json!([]),
            json!({"choices": "x"}),
            json!({"choices": [null]}),
            json!({"choices": [{"message": {"content": 5}}]}),
            json!({"content": [null, 1, {"text": 2}]}),
            json!({"message": []}),
            json!({"candidates": [{"content": {"parts": [1, null]}}]}),
            json!({"data": [{"embedding": ["a"]}]}),
            json!({"data": [null]}),
            json!({"embedding": {"values": "x"}}),
            json!({"embeddings": [{"values": [1, "x"]}]}),
            json!({"error": {"message": 3}}),
            json!({"usage": {"prompt_tokens": "9"}}),
        ];
        for k in &kinds {
            for b in &bodies {
                let _ = parse_chat(k, b);
                let _ = parse_embed(k, b, 1);
                let _ = parse_embed(k, b, 0);
            }
        }
    }
}
