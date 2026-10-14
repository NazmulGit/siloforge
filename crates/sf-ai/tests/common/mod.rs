//! Test helpers: an axum stub provider server and DB fixtures. Never calls real providers.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sf_ai::{Ai, AiOptions};
use sf_core::config::LlmCfg;
use sf_store::Db;
use sf_store::rusqlite::params;

pub const KEY: [u8; 32] = [7u8; 32];

#[derive(Debug, Clone)]
pub struct Recorded {
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Value,
}

pub struct Reply {
    pub status: u16,
    pub body: String,
    pub delay: Duration,
}

impl Reply {
    pub fn json(status: u16, v: Value) -> Self {
        Self {
            status,
            body: v.to_string(),
            delay: Duration::ZERO,
        }
    }
    pub fn raw(status: u16, s: &str) -> Self {
        Self {
            status,
            body: s.to_string(),
            delay: Duration::ZERO,
        }
    }
    pub fn delayed(mut self, d: Duration) -> Self {
        self.delay = d;
        self
    }
}

type Script = Arc<dyn Fn(&Recorded, usize) -> Reply + Send + Sync>;

#[derive(Clone)]
pub struct Stub {
    pub base: String,
    pub calls: Arc<Mutex<Vec<Recorded>>>,
}

impl Stub {
    pub fn count(&self) -> usize {
        self.calls.lock().len()
    }
    pub fn last(&self) -> Recorded {
        self.calls.lock().last().cloned().expect("no calls")
    }
}

pub async fn stub<F>(f: F) -> Stub
where
    F: Fn(&Recorded, usize) -> Reply + Send + Sync + 'static,
{
    let script: Script = Arc::new(f);
    let calls: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
    let calls2 = calls.clone();
    let handler = move |uri: Uri, headers: HeaderMap, body: Bytes| {
        let script = script.clone();
        let calls = calls2.clone();
        async move {
            let rec = Recorded {
                path: uri.path().to_string(),
                headers: headers
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.as_str().to_ascii_lowercase(),
                            v.to_str().unwrap_or("").to_string(),
                        )
                    })
                    .collect(),
                body: serde_json::from_slice(&body).unwrap_or(Value::Null),
            };
            let n = {
                let mut c = calls.lock();
                c.push(rec.clone());
                c.len() - 1
            };
            let reply = script(&rec, n);
            if !reply.delay.is_zero() {
                tokio::time::sleep(reply.delay).await;
            }
            let status = StatusCode::from_u16(reply.status).unwrap();
            let resp: Response =
                (status, [("content-type", "application/json")], reply.body).into_response();
            resp
        }
    };
    let app = axum::Router::new().fallback(handler);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Stub {
        base: format!("http://{addr}"),
        calls,
    }
}

/// OpenAI-style chat reply.
pub fn oa_chat(text: &str) -> Value {
    json!({"choices":[{"message":{"role":"assistant","content":text}}],"usage":{"prompt_tokens":11,"completion_tokens":5}})
}

/// OpenAI-style embeddings reply with `n` vectors of width `dim`.
pub fn oa_embed(n: usize, dim: usize) -> Value {
    let data: Vec<Value> = (0..n)
        .map(|i| {
            let v: Vec<f64> = (0..dim).map(|j| ((i + j) % 7) as f64 + 1.0).collect();
            json!({"index": i, "embedding": v})
        })
        .collect();
    json!({"data": data, "usage": {"prompt_tokens": 3}})
}

pub fn fast_opts() -> AiOptions {
    AiOptions {
        chat_timeout: Duration::from_secs(5),
        embed_timeout: Duration::from_secs(5),
        probe_timeout: Duration::from_secs(5),
        chat_deadline: Duration::from_secs(20),
        embed_deadline: Duration::from_secs(20),
        retry_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(50),
        breaker_threshold: 3,
        breaker_cooldown: Duration::from_secs(60),
        status_window: 50,
    }
}

pub fn llm(url: &str, models: &[&str]) -> LlmCfg {
    LlmCfg {
        fallback_url: url.to_string(),
        fallback_models: models.iter().map(|s| s.to_string()).collect(),
        timeout_ms: 5_000,
    }
}

pub fn open_db() -> Db {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("sf-ai-tests");
    let path = dir.join(format!("{}.db", sf_core::new_id()));
    Db::open(&path).unwrap()
}

pub fn ai(db: &Db, llm: LlmCfg, opts: AiOptions) -> Ai {
    Ai::with_options(db.clone(), Arc::new(KEY), llm, opts).unwrap()
}

pub async fn add_site(db: &Db) -> String {
    let id = sf_core::new_id();
    let id2 = id.clone();
    db.write(move |tx| {
        tx.execute(
            "INSERT INTO sites (id, domain, api_key) VALUES (?1, ?2, ?3)",
            params![id2, format!("{id2}.example"), format!("key-{id2}")],
        )
        .map_err(sf_store::err)?;
        Ok(())
    })
    .await
    .unwrap();
    id
}

pub async fn add_provider(
    db: &Db,
    name: &str,
    kind: &str,
    base: &str,
    api_key_enc: &str,
    headers: Value,
) -> String {
    let id = sf_core::new_id();
    let (id2, name, kind, base, enc) = (
        id.clone(),
        name.to_string(),
        kind.to_string(),
        base.to_string(),
        api_key_enc.to_string(),
    );
    db.write(move |tx| {
        tx.execute(
            "INSERT INTO model_providers (id, name, label, kind, base_url, api_key_enc, headers)
             VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6)",
            params![id2, name, kind, base, enc, headers.to_string()],
        )
        .map_err(sf_store::err)?;
        Ok(())
    })
    .await
    .unwrap();
    id
}

pub async fn add_model(
    db: &Db,
    provider_id: &str,
    model_id: &str,
    role: &str,
    dim: Option<i64>,
) -> String {
    let id = sf_core::new_id();
    let (id2, pid, mid, role) = (
        id.clone(),
        provider_id.to_string(),
        model_id.to_string(),
        role.to_string(),
    );
    db.write(move |tx| {
        tx.execute(
            "INSERT INTO models (id, provider_id, model_id, role, dim) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id2, pid, mid, role, dim],
        )
        .map_err(sf_store::err)?;
        Ok(())
    })
    .await
    .unwrap();
    id
}

/// Active chain with rungs `(model_row_id, timeout_ms, max_retries)` in order.
pub async fn add_chain(
    db: &Db,
    role: &str,
    site_id: Option<&str>,
    rungs: &[(&str, Option<i64>, Option<i64>)],
) -> String {
    let id = sf_core::new_id();
    let id2 = id.clone();
    let role = role.to_string();
    let site = site_id.map(str::to_string);
    let rungs: Vec<(String, Option<i64>, Option<i64>)> = rungs
        .iter()
        .map(|(m, t, r)| (m.to_string(), *t, *r))
        .collect();
    db.write(move |tx| {
        tx.execute(
            "INSERT INTO model_chains (id, name, role, site_id, active) VALUES (?1, ?2, ?3, ?4, 1)",
            params![id2, format!("chain-{id2}"), role, site],
        )
        .map_err(sf_store::err)?;
        for (i, (m, t, r)) in rungs.iter().enumerate() {
            tx.execute(
                "INSERT INTO model_chain_rungs (id, chain_id, model_id, position, timeout_ms, max_retries)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![sf_core::new_id(), id2, m, i as i64, t, r],
            )
            .map_err(sf_store::err)?;
        }
        Ok(())
    })
    .await
    .unwrap();
    id
}

pub async fn set_chain_active(db: &Db, chain_id: &str, active: bool) {
    let id = chain_id.to_string();
    db.write(move |tx| {
        tx.execute(
            "UPDATE model_chains SET active = ?2 WHERE id = ?1",
            params![id, active],
        )
        .map_err(sf_store::err)?;
        Ok(())
    })
    .await
    .unwrap();
}

/// `(ok, status_code, error, rung, provider, tokens_in)` rows in insertion order.
pub async fn call_log(
    db: &Db,
) -> Vec<(
    bool,
    Option<i64>,
    Option<String>,
    i64,
    Option<String>,
    Option<i64>,
)> {
    db.read(|c| {
        let mut s = c
            .prepare("SELECT ok, status_code, error, rung, provider, tokens_in FROM model_call_log ORDER BY id")
            .map_err(sf_store::err)?;
        let rows = s
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)? != 0,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .map_err(sf_store::err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sf_store::err)?;
        Ok(rows)
    })
    .await
    .unwrap()
}
