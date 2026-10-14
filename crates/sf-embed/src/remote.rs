//! OpenAI-compatible `/embeddings` rung (OpenRouter `sentence-transformers/all-minilm-l6-v2`).

use std::time::Duration;

use serde::Deserialize;
use sf_core::{Error, Result};

use crate::{EmbedFuture, Embedder, EmbedderKind, check_output, prepare_text};

/// HTTP statuses that are worth a retry (v1 list).
const RETRY_STATUS: [u16; 8] = [408, 409, 425, 429, 500, 502, 503, 504];
/// Cap on any single backoff sleep.
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// Largest response body accepted (64 texts x 384 floats is ~0.5 MB of JSON).
const MAX_BODY: usize = 32 * 1024 * 1024;

pub struct RemoteEmbedder {
    id: String,
    url: String,
    model: String,
    key: String,
    retries: u32,
    max_chars: usize,
    base_delay: Duration,
    client: reqwest::Client,
}

impl std::fmt::Debug for RemoteEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // never print the key
        f.debug_struct("RemoteEmbedder")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Resp {
    data: Vec<Row>,
}

#[derive(Deserialize)]
struct Row {
    embedding: Vec<f32>,
    #[serde(default)]
    index: Option<usize>,
}

impl RemoteEmbedder {
    /// `base_url` like `https://openrouter.ai/api/v1` (`/embeddings` is appended).
    pub fn new(
        id: impl Into<String>,
        base_url: &str,
        model: &str,
        key: &str,
        timeout: Duration,
        retries: u32,
        max_chars: usize,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .build()
            .map_err(|e| Error::internal(format!("http client: {e}")))?;
        Ok(Self {
            id: id.into(),
            url: format!("{}/embeddings", base_url.trim_end_matches('/')),
            model: model.to_string(),
            key: key.to_string(),
            retries,
            max_chars,
            base_delay: Duration::from_millis(500),
            client,
        })
    }

    /// First retry delay (doubles per attempt). Tests shorten it.
    pub fn with_base_delay(mut self, d: Duration) -> Self {
        self.base_delay = d;
        self
    }

    async fn call(&self, input: &[String]) -> std::result::Result<Vec<Vec<f32>>, Attempt> {
        let body = serde_json::json!({ "model": self.model, "input": input });
        let resp = self
            .client
            .post(&self.url)
            .bearer_auth(&self.key)
            .header("x-title", "SiloForge")
            .json(&body)
            .send()
            .await
            .map_err(|e| Attempt::retry(format!("transport: {}", strip_url(&e))))?;
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let bytes = read_capped(resp).await.map_err(Attempt::retry)?;
        if !status.is_success() {
            let snippet: String = String::from_utf8_lossy(&bytes).chars().take(200).collect();
            let msg = format!("HTTP {}: {snippet}", status.as_u16());
            return Err(if RETRY_STATUS.contains(&status.as_u16()) {
                Attempt {
                    msg,
                    retry: true,
                    after: retry_after,
                }
            } else {
                Attempt::fatal(msg)
            });
        }
        let mut parsed: Resp = serde_json::from_slice(&bytes)
            .map_err(|e| Attempt::fatal(format!("bad response json: {e}")))?;
        if parsed.data.iter().all(|r| r.index.is_some()) {
            parsed.data.sort_by_key(|r| r.index.unwrap_or(0));
        }
        let rows = parsed.data.into_iter().map(|r| r.embedding).collect();
        check_output(input.len(), rows).map_err(|e| Attempt::fatal(e.to_string()))
    }
}

/// reqwest errors include the URL; keep them but never any query/credentials.
fn strip_url(e: &reqwest::Error) -> String {
    let mut s = e.to_string();
    if let Some(u) = e.url() {
        let shown = format!(
            "{}://{}{}",
            u.scheme(),
            u.host_str().unwrap_or(""),
            u.path()
        );
        s = s.replace(u.as_str(), &shown);
    }
    s
}

async fn read_capped(mut resp: reqwest::Response) -> std::result::Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("read body: {}", strip_url(&e)))?
    {
        if buf.len() + chunk.len() > MAX_BODY {
            return Err("response body too large".into());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

struct Attempt {
    msg: String,
    retry: bool,
    after: Option<Duration>,
}

impl Attempt {
    fn retry(msg: String) -> Self {
        Self {
            msg,
            retry: true,
            after: None,
        }
    }
    fn fatal(msg: String) -> Self {
        Self {
            msg,
            retry: false,
            after: None,
        }
    }
}

impl Embedder for RemoteEmbedder {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> EmbedderKind {
        EmbedderKind::Remote
    }
    fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(async move {
            if texts.is_empty() {
                return Ok(Vec::new());
            }
            let input: Vec<String> = texts
                .iter()
                .map(|t| prepare_text(t, self.max_chars))
                .collect();
            let mut delay = self.base_delay;
            let mut attempt = 0u32;
            loop {
                match self.call(&input).await {
                    Ok(v) => return Ok(v),
                    Err(a) if a.retry && attempt < self.retries => {
                        let wait = a.after.unwrap_or(delay).min(MAX_BACKOFF);
                        tracing::debug!(rung = %self.id, attempt, error = %a.msg, "embed retry");
                        tokio::time::sleep(wait).await;
                        delay = delay.saturating_mul(2).min(MAX_BACKOFF);
                        attempt += 1;
                    }
                    Err(a) => {
                        return Err(Error::upstream(format!(
                            "{} after {} attempt(s): {}",
                            self.id,
                            attempt + 1,
                            a.msg
                        )));
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct Stub {
        hits: Arc<AtomicUsize>,
        mode: &'static str,
    }

    fn vec_for(i: usize, dim: usize) -> Vec<f32> {
        let mut v = vec![0.01f32; dim];
        if let Some(x) = v.get_mut(i % dim) {
            *x = 2.0;
        }
        v
    }

    async fn handler(
        State(s): State<Stub>,
        headers: axum::http::HeaderMap,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let n = s.hits.fetch_add(1, Ordering::SeqCst);
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if auth != "Bearer k1" {
            return (StatusCode::UNAUTHORIZED, Json(json!({"error": "bad key"})));
        }
        let count = body["input"].as_array().map_or(0, Vec::len);
        let dim = if s.mode == "wrongdim" { 128 } else { 384 };
        if s.mode == "429" && n == 0 {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error": "slow"})),
            );
        }
        if s.mode == "500" {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({})));
        }
        // reversed order with indices: client must sort
        let data: Vec<Value> = (0..count)
            .rev()
            .map(|i| {
                let mut v: Vec<Value> = vec_for(i, dim).into_iter().map(Value::from).collect();
                if s.mode == "nan" && i == 0 {
                    // JSON has no NaN literal; a huge exponent parses to +inf in f32
                    v[3] = json!(1e300);
                }
                json!({"index": i, "embedding": v})
            })
            .collect();
        (StatusCode::OK, Json(json!({ "data": data })))
    }

    pub(crate) async fn stub(mode: &'static str) -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/v1/embeddings", post(handler))
            .with_state(Stub {
                hits: hits.clone(),
                mode,
            });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });
        (format!("http://{addr}/v1"), hits)
    }

    fn emb(url: &str, key: &str, retries: u32) -> RemoteEmbedder {
        RemoteEmbedder::new(
            "remote",
            url,
            "m",
            key,
            Duration::from_secs(5),
            retries,
            2000,
        )
        .unwrap()
        .with_base_delay(Duration::from_millis(5))
    }

    fn texts(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("text {i}")).collect()
    }

    #[tokio::test]
    async fn ok_sorted_by_index_and_normalised() {
        let (url, hits) = stub("ok").await;
        let out = emb(&url, "k1", 0).embed(&texts(5)).await.unwrap();
        assert_eq!(out.len(), 5);
        for (i, v) in out.iter().enumerate() {
            assert_eq!(v.len(), 384);
            let hot = v
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|x| x.0);
            assert_eq!(hot, Some(i));
            let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((n - 1.0).abs() < 1e-5);
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_429_then_ok() {
        let (url, hits) = stub("429").await;
        let out = emb(&url, "k1", 2).embed(&texts(2)).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        // no retries configured -> fails on the 429
        let (url, _) = stub("429").await;
        assert!(emb(&url, "k1", 0).embed(&texts(2)).await.is_err());
    }

    #[tokio::test]
    async fn rejects_wrong_dim_nan_and_auth_without_retrying() {
        for mode in ["wrongdim", "nan"] {
            let (url, hits) = stub(mode).await;
            let e = emb(&url, "k1", 3).embed(&texts(3)).await;
            assert!(e.is_err(), "{mode}");
            assert_eq!(hits.load(Ordering::SeqCst), 1, "{mode} must not retry");
        }
        let (url, hits) = stub("ok").await;
        let e = emb(&url, "wrong", 3).embed(&texts(1)).await.unwrap_err();
        assert!(e.to_string().contains("401"));
        assert!(!e.to_string().contains("wrong"), "key leaked: {e}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_5xx_then_gives_up() {
        let (url, hits) = stub("500").await;
        assert!(emb(&url, "k1", 2).embed(&texts(1)).await.is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn transport_error_and_hostile_input() {
        // nothing listens on port 9 (discard) normally; connection refused -> error, no panic
        let e = emb("http://127.0.0.1:9/v1", "k1", 1).embed(&texts(1)).await;
        assert!(e.is_err());
        let (url, _) = stub("ok").await;
        let r = emb(&url, "k1", 0);
        assert!(r.embed(&[]).await.unwrap().is_empty());
        let hostile = vec![String::new(), " ".into(), "x".repeat(1 << 20)];
        assert_eq!(r.embed(&hostile).await.unwrap().len(), 3);
    }
}
