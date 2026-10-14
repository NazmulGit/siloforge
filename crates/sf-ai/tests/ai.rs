#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};
use sf_ai::{AiError, ChainSource, ChatMessage, ChatRequest, Role};
use sf_store::rusqlite::params;

fn req(site: Option<&str>) -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage::system("be brief"), ChatMessage::user("hello")],
        site_id: site.map(str::to_string),
        ..Default::default()
    }
}

/// An env fallback pointing at a closed port (so tests notice if env is used unexpectedly).
fn no_env() -> sf_core::config::LlmCfg {
    llm("", &[])
}

#[tokio::test]
async fn fallback_order_and_call_log() {
    let db = open_db();
    let bad = stub(|_, _| Reply::json(500, json!({"error": "boom"}))).await;
    let good = stub(|_, _| Reply::json(200, oa_chat("hi there"))).await;
    let p1 = add_provider(&db, "bad", "openai-compatible", &bad.base, "", json!({})).await;
    let p2 = add_provider(&db, "good", "openai-compatible", &good.base, "", json!({})).await;
    let m1 = add_model(&db, &p1, "m-bad", "chat", None).await;
    let m2 = add_model(&db, &p2, "m-good", "chat", None).await;
    add_chain(&db, "chat", None, &[(&m1, None, None), (&m2, None, None)]).await;
    let ai = ai(&db, no_env(), fast_opts());

    let r = ai.chat(req(None)).await.unwrap();
    assert_eq!(r.text, "hi there");
    assert_eq!((r.provider.as_str(), r.model.as_str()), ("good", "m-good"));
    assert_eq!((r.rung, r.fallbacks_used), (1, 1));
    assert_eq!((r.tokens_in, r.tokens_out), (Some(11), Some(5)));
    assert_eq!(bad.count(), 1, "max_retries default 0: one attempt");
    let body = good.last().body;
    assert_eq!(body["model"], "m-good");
    assert_eq!(body["max_tokens"], 2000);
    assert_eq!(body["messages"][1]["content"], "hello");

    let log = call_log(&db).await;
    assert_eq!(log.len(), 2);
    assert!(!log[0].0 && log[0].1 == Some(500) && log[0].3 == 0);
    assert!(log[0].2.as_deref().unwrap().contains("HTTP 500"));
    assert_eq!(log[0].4.as_deref(), Some("bad"));
    assert!(log[1].0 && log[1].1 == Some(200) && log[1].3 == 1);
    assert_eq!(log[1].5, Some(11));
}

#[tokio::test]
async fn retry_on_429_then_success() {
    let db = open_db();
    let s = stub(|_, n| {
        if n == 0 {
            Reply::json(429, json!({"error": "slow down"}))
        } else {
            Reply::json(200, oa_chat("ok"))
        }
    })
    .await;
    let p = add_provider(&db, "p", "openai-compatible", &s.base, "", json!({})).await;
    let m = add_model(&db, &p, "m", "chat", None).await;
    add_chain(&db, "chat", None, &[(&m, None, Some(2))]).await;
    let ai = ai(&db, no_env(), fast_opts());
    let r = ai.chat(req(None)).await.unwrap();
    assert_eq!((r.rung, r.fallbacks_used), (0, 0));
    assert_eq!(s.count(), 2);
    let log = call_log(&db).await;
    assert_eq!(log.len(), 2);
    assert_eq!((log[0].0, log[0].1), (false, Some(429)));
    assert!(log[1].0);
}

#[tokio::test]
async fn non_retryable_400_falls_through() {
    let db = open_db();
    let a = stub(|_, _| Reply::json(400, json!({"error": "bad"}))).await;
    let b = stub(|_, _| Reply::json(200, oa_chat("from b"))).await;
    let pa = add_provider(&db, "a", "openai-compatible", &a.base, "", json!({})).await;
    let pb = add_provider(&db, "b", "openai-compatible", &b.base, "", json!({})).await;
    let ma = add_model(&db, &pa, "ma", "chat", None).await;
    let mb = add_model(&db, &pb, "mb", "chat", None).await;
    add_chain(
        &db,
        "chat",
        None,
        &[(&ma, None, Some(3)), (&mb, None, None)],
    )
    .await;
    let ai = ai(&db, no_env(), fast_opts());
    let r = ai.chat(req(None)).await.unwrap();
    assert_eq!(r.text, "from b");
    assert_eq!(a.count(), 1, "400 is not retried");
}

#[tokio::test]
async fn all_rungs_failed_lists_each() {
    let db = open_db();
    let a = stub(|_, _| Reply::json(401, json!({"error": "nope"}))).await;
    let pa = add_provider(&db, "a", "openai-compatible", &a.base, "", json!({})).await;
    let ma = add_model(&db, &pa, "ma", "chat", None).await;
    add_chain(&db, "chat", None, &[(&ma, None, None)]).await;
    let ai = ai(&db, no_env(), fast_opts());
    match ai.chat(req(None)).await {
        Err(AiError::AllFailed(m)) => assert!(m.contains("a/ma: HTTP 401"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn breaker_opens_skips_and_half_opens() {
    let db = open_db();
    let healthy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let h2 = healthy.clone();
    let a = stub(move |_, _| {
        if h2.load(std::sync::atomic::Ordering::SeqCst) {
            Reply::json(200, oa_chat("a back"))
        } else {
            Reply::json(503, json!({"error": "down"}))
        }
    })
    .await;
    let b = stub(|_, _| Reply::json(200, oa_chat("b"))).await;
    let pa = add_provider(&db, "a", "openai-compatible", &a.base, "", json!({})).await;
    let pb = add_provider(&db, "b", "openai-compatible", &b.base, "", json!({})).await;
    let ma = add_model(&db, &pa, "ma", "chat", None).await;
    let mb = add_model(&db, &pb, "mb", "chat", None).await;
    add_chain(&db, "chat", None, &[(&ma, None, None), (&mb, None, None)]).await;
    let mut opts = fast_opts();
    opts.breaker_threshold = 2;
    opts.breaker_cooldown = Duration::from_millis(300);
    let ai = ai(&db, no_env(), opts);

    for _ in 0..2 {
        assert_eq!(ai.chat(req(None)).await.unwrap().text, "b");
    }
    assert_eq!(a.count(), 2);
    // Open: rung a is skipped without a request.
    let r = ai.chat(req(None)).await.unwrap();
    assert_eq!((r.text.as_str(), r.fallbacks_used), ("b", 1));
    assert_eq!(a.count(), 2, "open breaker must skip the dead rung");

    let st = ai.status().await.unwrap();
    let pa_st = st.iter().find(|p| p.name == "a").unwrap();
    assert_eq!(pa_st.models[0].breaker.state, "open");
    assert_eq!(pa_st.models[0].recent_calls, 2);
    assert_eq!(pa_st.models[0].success_rate, Some(0.0));
    assert!(
        pa_st.models[0]
            .last_error
            .as_deref()
            .unwrap()
            .contains("503")
    );
    let pb_st = st.iter().find(|p| p.name == "b").unwrap();
    assert_eq!(pb_st.models[0].success_rate, Some(1.0));

    let d = ai.resolve_chain_detail(Role::Chat, None).await.unwrap();
    assert_eq!(d.rungs[0].breaker, "open");

    // After the cooldown one half-open probe goes through; success closes the breaker.
    tokio::time::sleep(Duration::from_millis(350)).await;
    healthy.store(true, std::sync::atomic::Ordering::SeqCst);
    let r = ai.chat(req(None)).await.unwrap();
    assert_eq!(r.text, "a back");
    assert_eq!(a.count(), 3);
    let st = ai.status().await.unwrap();
    let pa_st = st.iter().find(|p| p.name == "a").unwrap();
    assert_eq!(pa_st.models[0].breaker.state, "closed");
}

#[tokio::test]
async fn deadline_enforced() {
    let db = open_db();
    let slow = stub(|_, _| Reply::json(200, oa_chat("late")).delayed(Duration::from_secs(5))).await;
    let never = stub(|_, _| Reply::json(200, oa_chat("never"))).await;
    let p1 = add_provider(&db, "slow", "openai-compatible", &slow.base, "", json!({})).await;
    let p2 = add_provider(
        &db,
        "never",
        "openai-compatible",
        &never.base,
        "",
        json!({}),
    )
    .await;
    let m1 = add_model(&db, &p1, "m1", "chat", None).await;
    let m2 = add_model(&db, &p2, "m2", "chat", None).await;
    add_chain(
        &db,
        "chat",
        None,
        &[(&m1, Some(60_000), Some(5)), (&m2, None, None)],
    )
    .await;
    let mut opts = fast_opts();
    opts.chat_deadline = Duration::from_millis(400);
    let ai = ai(&db, no_env(), opts);
    let t = Instant::now();
    let r = ai.chat(req(None)).await;
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "took {:?}",
        t.elapsed()
    );
    assert!(matches!(r, Err(AiError::DeadlineExceeded(_))), "{r:?}");
    assert_eq!(never.count(), 0);
}

#[tokio::test]
async fn embed_validation_and_fallback() {
    let db = open_db();
    let wrong_dim = stub(|_, _| Reply::json(200, oa_embed(2, 3))).await;
    let nan = stub(|_, _| {
        let mut v = oa_embed(2, 384);
        v["data"][1]["embedding"][5] = json!("NaN");
        Reply::json(200, v)
    })
    .await;
    let overflow = stub(|_, _| {
        Reply::raw(
            200,
            &format!(
                "{{\"data\":[{{\"embedding\":[1e39{}]}},{{\"embedding\":[1{}]}}]}}",
                ",1".repeat(383),
                ",1".repeat(383)
            ),
        )
    })
    .await;
    let zero = stub(|_, _| {
        Reply::json(
            200,
            json!({"data":[{"embedding": vec![0.0; 384]},{"embedding": vec![0.0; 384]}]}),
        )
    })
    .await;
    let good = stub(|_, _| Reply::json(200, oa_embed(2, 384))).await;
    let mut models = Vec::new();
    for (i, s) in [&wrong_dim, &nan, &overflow, &zero, &good]
        .iter()
        .enumerate()
    {
        let p = add_provider(
            &db,
            &format!("e{i}"),
            "openai-compatible",
            &s.base,
            "",
            json!({}),
        )
        .await;
        models.push(add_model(&db, &p, &format!("emb{i}"), "embed", None).await);
    }
    let rungs: Vec<(&str, Option<i64>, Option<i64>)> =
        models.iter().map(|m| (m.as_str(), None, Some(2))).collect();
    add_chain(&db, "embed", None, &rungs).await;
    let ai = ai(&db, no_env(), fast_opts());
    let texts = vec!["a".to_string(), "b".to_string()];
    let r = ai.embed(&texts, None).await.unwrap();
    assert_eq!((r.rung, r.provider.as_str()), (4, "e4"));
    assert_eq!(r.vectors.len(), 2);
    for v in &r.vectors {
        assert_eq!(v.len(), 384);
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-4);
    }
    // Validation failures are not retried even with max_retries=2.
    assert_eq!(
        (
            wrong_dim.count(),
            nan.count(),
            overflow.count(),
            zero.count()
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(good.last().body["input"], json!(["a", "b"]));
    let log = call_log(&db).await;
    assert!(log[0].2.as_deref().unwrap().contains("dim 3, expected 384"));
    assert!(log[1].2.as_deref().unwrap().contains("not a finite number"));
    assert!(log[2].2.as_deref().unwrap().contains("not a finite number"));
    assert!(log[3].2.as_deref().unwrap().contains("zero vector"));
}

#[tokio::test]
async fn embed_not_configured_and_dim_guard() {
    let db = open_db();
    let s = stub(|_, _| Reply::json(200, oa_embed(1, 384))).await;
    // Env chat fallback exists, but embed has no env fallback.
    let ai = ai(&db, llm(&s.base, &["x"]), fast_opts());
    let texts = vec!["a".to_string()];
    assert!(matches!(
        ai.embed(&texts, None).await,
        Err(AiError::NotConfigured(_))
    ));

    // B11: a 768-d model in an index embed chain is skipped at resolution.
    let p = add_provider(&db, "p", "openai-compatible", &s.base, "", json!({})).await;
    let m768 = add_model(&db, &p, "big", "embed", Some(768)).await;
    let chain = add_chain(&db, "embed", None, &[(&m768, None, None)]).await;
    assert!(matches!(
        ai.embed(&texts, None).await,
        Err(AiError::NotConfigured(_))
    ));
    let d = ai.resolve_chain_detail(Role::Embed, None).await.unwrap();
    assert_eq!(d.rungs.len(), 1);
    assert!(!d.rungs[0].usable);
    assert!(d.rungs[0].skip_reason.as_deref().unwrap().contains("768"));
    assert_eq!(s.count(), 0);

    // embed_model_dim accepts the model's declared width.
    set_chain_active(&db, &chain, false).await;
    let s768 = stub(|_, _| Reply::json(200, oa_embed(1, 768))).await;
    let p2 = add_provider(&db, "p2", "openai-compatible", &s768.base, "", json!({})).await;
    let m2 = add_model(&db, &p2, "big2", "embed", Some(768)).await;
    add_chain(&db, "embed", None, &[(&m2, None, None)]).await;
    let r = ai.embed_model_dim(&texts, None).await.unwrap();
    assert_eq!(r.vectors[0].len(), 768);
    // Empty input with a configured chain is a no-op.
    assert!(
        ai.embed_model_dim(&[], None)
            .await
            .unwrap()
            .vectors
            .is_empty()
    );
}

#[tokio::test]
async fn site_chain_beats_global_beats_env() {
    let db = open_db();
    let env = stub(|_, _| Reply::json(200, oa_chat("env"))).await;
    let global = stub(|_, _| Reply::json(200, oa_chat("global"))).await;
    let site = stub(|_, _| Reply::json(200, oa_chat("site"))).await;
    let pg = add_provider(&db, "g", "openai-compatible", &global.base, "", json!({})).await;
    let ps = add_provider(&db, "s", "openai-compatible", &site.base, "", json!({})).await;
    let mg = add_model(&db, &pg, "mg", "chat", None).await;
    let ms = add_model(&db, &ps, "ms", "chat", None).await;
    let site_id = add_site(&db).await;
    let other_site = add_site(&db).await;
    let ai = ai(
        &db,
        llm(&format!("{}/", env.base), &["envmodel", "envmodel", "env2"]),
        fast_opts(),
    );

    // Nothing configured -> env (dario, no auth).
    let r = ai.chat(req(Some(&site_id))).await.unwrap();
    assert_eq!((r.text.as_str(), r.provider.as_str()), ("env", "env:dario"));
    assert!(!env.last().headers.contains_key("authorization"));
    assert_eq!(env.last().body["model"], "envmodel");
    let d = ai.resolve_chain_detail(Role::Chat, None).await.unwrap();
    assert_eq!(d.source, ChainSource::Env);
    assert_eq!(d.rungs.len(), 2, "duplicate env models collapse");

    let gchain = add_chain(&db, "chat", None, &[(&mg, None, None)]).await;
    add_chain(&db, "chat", Some(&site_id), &[(&ms, None, None)]).await;
    assert_eq!(ai.chat(req(Some(&site_id))).await.unwrap().text, "site");
    assert_eq!(
        ai.chat(req(Some(&other_site))).await.unwrap().text,
        "global"
    );
    assert_eq!(ai.chat(req(None)).await.unwrap().text, "global");
    let d = ai
        .resolve_chain_detail(Role::Chat, Some(&site_id))
        .await
        .unwrap();
    assert_eq!(d.source, ChainSource::Site);

    set_chain_active(&db, &gchain, false).await;
    assert_eq!(ai.chat(req(None)).await.unwrap().text, "env");
    assert_eq!(ai.chat(req(Some(&site_id))).await.unwrap().text, "site");

    // Unconfigured everywhere -> NotConfigured.
    let ai2 = common::ai(&db, no_env(), fast_opts());
    assert!(matches!(
        ai2.chat(req(Some(&other_site))).await,
        Err(AiError::NotConfigured(_))
    ));
}

#[tokio::test]
async fn undecryptable_key_fails_rung_clearly() {
    let db = open_db();
    let a = stub(|_, _| Reply::json(200, oa_chat("should not be called"))).await;
    let b = stub(|_, _| Reply::json(200, oa_chat("b"))).await;
    let foreign = sf_core::crypto::encrypt_secret(&[9u8; 32], "sk-secret").unwrap();
    let own = sf_core::crypto::encrypt_secret(&KEY, "sk-good").unwrap();
    let pa = add_provider(&db, "a", "openai-compatible", &a.base, &foreign, json!({})).await;
    let pb = add_provider(&db, "b", "openai-compatible", &b.base, &own, json!({})).await;
    let ma = add_model(&db, &pa, "ma", "chat", None).await;
    let mb = add_model(&db, &pb, "mb", "chat", None).await;
    add_chain(&db, "chat", None, &[(&ma, None, None), (&mb, None, None)]).await;
    let ai = ai(&db, no_env(), fast_opts());
    let r = ai.chat(req(None)).await.unwrap();
    assert_eq!(r.text, "b");
    assert_eq!(a.count(), 0, "never sent without auth");
    assert_eq!(
        b.last().headers.get("authorization").map(String::as_str),
        Some("Bearer sk-good")
    );
    let log = call_log(&db).await;
    assert!(
        log[0]
            .2
            .as_deref()
            .unwrap()
            .contains("could not be decrypted")
    );
    let st = ai.status().await.unwrap();
    assert!(!st.iter().find(|p| p.name == "a").unwrap().key_ok);
    let d = ai.resolve_chain_detail(Role::Chat, None).await.unwrap();
    assert_eq!(d.rungs[0].key_status, "undecryptable");
    // Probe reports it too.
    let p = ai.probe_model(&ma).await.unwrap();
    assert_eq!(p["ok"], false);
    assert!(
        p["error"]
            .as_str()
            .unwrap()
            .contains("could not be decrypted")
    );
}

#[tokio::test]
async fn anthropic_ollama_gemini_end_to_end() {
    let db = open_db();
    let anth = stub(|_, _| {
        Reply::json(200, json!({"content":[{"type":"text","text":"claude says"}],"usage":{"input_tokens":7,"output_tokens":2}}))
    })
    .await;
    let oll = stub(|r, _| {
        if r.path == "/api/embeddings" {
            Reply::json(200, json!({"embedding": vec![0.5; 384]}))
        } else {
            Reply::json(
                200,
                json!({"message":{"content":"llama says"},"prompt_eval_count":4,"eval_count":3}),
            )
        }
    })
    .await;
    let gem = stub(|r, _| {
        if r.path.ends_with(":batchEmbedContents") {
            Reply::json(200, json!({"embeddings":[{"values": vec![1.0; 384]},{"values": vec![2.0; 384]}]}))
        } else {
            Reply::json(200, json!({"candidates":[{"content":{"parts":[{"text":"gem"},{"text":"ini"}]}}],"usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":1}}))
        }
    })
    .await;
    let ka = sf_core::crypto::encrypt_secret(&KEY, "ant-key").unwrap();
    let kg = sf_core::crypto::encrypt_secret(&KEY, "gem-key").unwrap();
    let pa = add_provider(
        &db,
        "anth",
        "anthropic",
        &anth.base,
        &ka,
        json!({"X-Extra": "1"}),
    )
    .await;
    let po = add_provider(&db, "oll", "ollama", &oll.base, "", json!({})).await;
    let pg = add_provider(
        &db,
        "gem",
        "gemini",
        &format!("{}/v1beta/", gem.base),
        &kg,
        json!({}),
    )
    .await;
    let ma = add_model(&db, &pa, "claude-x", "chat", None).await;
    let mo = add_model(&db, &po, "llama", "chat", None).await;
    let mg = add_model(&db, &pg, "models/gemini-x", "chat", None).await;
    let moe = add_model(&db, &po, "nomic", "embed", Some(384)).await;
    let mge = add_model(&db, &pg, "text-embedding-004", "embed", Some(384)).await;
    let ai = ai(&db, no_env(), fast_opts());

    let c1 = add_chain(&db, "chat", None, &[(&ma, None, None)]).await;
    let r = ai.chat(req(None)).await.unwrap();
    assert_eq!(
        (r.text.as_str(), r.tokens_in, r.tokens_out),
        ("claude says", Some(7), Some(2))
    );
    let rec = anth.last();
    assert_eq!(rec.path, "/v1/messages");
    assert_eq!(
        rec.headers.get("x-api-key").map(String::as_str),
        Some("ant-key")
    );
    assert_eq!(
        rec.headers.get("anthropic-version").map(String::as_str),
        Some("2023-06-01")
    );
    assert_eq!(rec.headers.get("x-extra").map(String::as_str), Some("1"));
    assert_eq!(rec.body["system"], "be brief");
    set_chain_active(&db, &c1, false).await;

    let c2 = add_chain(&db, "chat", None, &[(&mo, None, None)]).await;
    let r = ai.chat(req(None)).await.unwrap();
    assert_eq!((r.text.as_str(), r.tokens_in), ("llama says", Some(4)));
    assert_eq!(oll.last().body["stream"], false);
    set_chain_active(&db, &c2, false).await;

    add_chain(&db, "chat", None, &[(&mg, None, None)]).await;
    let r = ai.chat(req(None)).await.unwrap();
    assert_eq!((r.text.as_str(), r.tokens_in), ("gemini", Some(9)));
    let rec = gem.last();
    assert_eq!(rec.path, "/v1beta/models/gemini-x:generateContent");
    assert_eq!(
        rec.headers.get("x-goog-api-key").map(String::as_str),
        Some("gem-key")
    );
    assert_eq!(
        rec.body["systemInstruction"]["parts"][0]["text"],
        "be brief"
    );

    let texts = vec!["a".to_string(), "b".to_string()];
    let ce = add_chain(&db, "embed", None, &[(&moe, None, None)]).await;
    let r = ai.embed(&texts, None).await.unwrap();
    assert_eq!(r.vectors.len(), 2);
    assert_eq!(
        oll.calls
            .lock()
            .iter()
            .filter(|c| c.path == "/api/embeddings")
            .count(),
        2
    );
    set_chain_active(&db, &ce, false).await;
    add_chain(&db, "embed", None, &[(&mge, None, None)]).await;
    let r = ai.embed(&texts, None).await.unwrap();
    assert_eq!(r.provider, "gem");
    assert!(
        (r.vectors[1][0] - r.vectors[0][0]).abs() < 1e-6,
        "normalised"
    );
    assert_eq!(
        gem.last().body["requests"][1]["model"],
        "models/text-embedding-004"
    );
}

#[tokio::test]
async fn probe_shapes() {
    let db = open_db();
    let s = stub(|r, _| {
        if r.path == "/embeddings" {
            Reply::json(200, oa_embed(1, 768))
        } else {
            Reply::json(200, oa_chat("pong"))
        }
    })
    .await;
    let denied = stub(|_, _| Reply::raw(401, "{\"error\":\"invalid key\"}\n")).await;
    let p = add_provider(&db, "p", "openai-compatible", &s.base, "", json!({})).await;
    let pe = add_provider(&db, "pe", "openai-compatible", &s.base, "", json!({})).await;
    let pd = add_provider(&db, "pd", "openai-compatible", &denied.base, "", json!({})).await;
    let pt = add_provider(
        &db,
        "pt",
        "openai-compatible",
        "http://127.0.0.1:1",
        "",
        json!({}),
    )
    .await;
    let pm = add_provider(
        &db,
        "pm",
        "openai-compatible",
        "http://169.254.169.254",
        "",
        json!({}),
    )
    .await;
    let empty = add_provider(&db, "empty", "openai-compatible", &s.base, "", json!({})).await;
    let me = add_model(&db, &p, "emb", "embed", None).await;
    let mc = add_model(&db, &p, "chat-m", "chat", None).await;
    let mee = add_model(&db, &pe, "emb-only", "embed", None).await;
    add_model(&db, &pd, "d", "chat", None).await;
    add_model(&db, &pt, "t", "chat", None).await;
    add_model(&db, &pm, "meta", "chat", None).await;
    let ai = ai(&db, no_env(), fast_opts());

    let keys = |v: &Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    // Provider probe prefers the chat model.
    let r = ai.probe_provider(&p).await.unwrap();
    assert_eq!(
        keys(&r),
        ["error", "latency_ms", "ok", "sample", "status_code"]
    );
    assert_eq!(
        (
            r["ok"].clone(),
            r["sample"].clone(),
            r["status_code"].clone()
        ),
        (json!(true), json!("pong"), json!(200))
    );
    assert_eq!(r["error"], Value::Null);
    let probe_body = s.last().body;
    assert_eq!(
        (
            probe_body["max_tokens"].clone(),
            probe_body["messages"][0]["content"].clone()
        ),
        (json!(1), json!("ping"))
    );

    let r = ai.probe_provider(&pe).await.unwrap();
    assert_eq!(
        keys(&r),
        ["dim", "error", "latency_ms", "ok", "status_code"]
    );
    assert_eq!(r["dim"], 768, "embed probes report width, do not judge it");
    assert_eq!(s.last().body["input"], json!(["test"]));

    let r = ai.probe_model(&me).await.unwrap();
    assert_eq!(
        keys(&r),
        [
            "dim",
            "error",
            "index_dim",
            "latency_ms",
            "ok",
            "status_code"
        ]
    );
    assert_eq!(r["index_dim"], 384);
    let r = ai.probe_model(&mc).await.unwrap();
    assert_eq!(r["index_dim"], 384);
    assert_eq!(r["sample"], "pong");

    let r = ai.probe_provider(&pd).await.unwrap();
    assert_eq!(keys(&r), ["error", "latency_ms", "ok", "status_code"]);
    assert_eq!(
        (r["ok"].clone(), r["status_code"].clone()),
        (json!(false), json!(401))
    );
    assert_eq!(r["error"], "pd/d: HTTP 401 {\"error\":\"invalid key\"}");

    let r = ai.probe_provider(&pt).await.unwrap();
    assert_eq!(
        (r["ok"].clone(), r["status_code"].clone()),
        (json!(false), Value::Null)
    );
    assert!(r["error"].as_str().unwrap().contains("connection failed"));

    let r = ai.probe_provider(&pm).await.unwrap();
    assert_eq!(r["ok"], false);
    assert!(r["error"].as_str().unwrap().contains("blocked"), "{r}");

    let r = ai.probe_provider(&empty).await.unwrap();
    assert_eq!(
        r,
        json!({"ok": false, "latency_ms": 0, "status_code": null, "error": "provider has no enabled models to probe — add a model first"})
    );

    assert!(matches!(
        ai.probe_provider("nope").await,
        Err(AiError::NotFound(_))
    ));
    assert!(matches!(
        ai.probe_model("nope").await,
        Err(AiError::NotFound(_))
    ));

    // last_probe_* columns written on provider and model.
    let pd2 = pd.clone();
    let (ok, err, at): (Option<i64>, Option<String>, Option<String>) = db
        .read(move |c| {
            c.query_row(
                "SELECT last_probe_ok, last_probe_error, last_probe_at FROM model_providers WHERE id = ?1",
                params![pd2],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(sf_store::err)
        })
        .await
        .unwrap();
    assert_eq!(ok, Some(0));
    assert!(err.unwrap().contains("401"));
    assert!(at.is_some());
    let mee2 = mee.clone();
    let mok: Option<i64> = db
        .read(move |c| {
            c.query_row(
                "SELECT last_probe_ok FROM models WHERE id = ?1",
                params![mee2],
                |r| r.get(0),
            )
            .map_err(sf_store::err)
        })
        .await
        .unwrap();
    assert_eq!(mok, Some(1));
    // Probes do not write the call log.
    assert!(call_log(&db).await.is_empty());
}

#[tokio::test]
async fn hostile_responses_never_panic() {
    let bodies: Vec<String> = vec![
        "not json".into(),
        "".into(),
        "null".into(),
        "[]".into(),
        "{\"choices\":[{\"message\":{\"content\":null}}]}".into(),
        "{\"choices\":\"x\",\"content\":7,\"message\":[],\"candidates\":{}}".into(),
        format!("{}{}", "[".repeat(5000), "]".repeat(5000)),
        "{\"data\":[{\"embedding\":[1e999]}]}".into(),
        "{\"error\":{\"message\":\"in-band failure\"}}".into(),
        "\u{0}\u{1}garbage\u{fffd}".into(),
    ];
    for kind in [
        "openai-compatible",
        "anthropic",
        "ollama",
        "gemini",
        "openrouter",
        "custom",
    ] {
        let db = open_db();
        let bodies2 = bodies.clone();
        let s = stub(move |_, n| Reply::raw(200, &bodies2[n % bodies2.len()])).await;
        let p = add_provider(&db, "h", kind, &s.base, "", json!({})).await;
        let m = add_model(&db, &p, "m", "chat", None).await;
        let me = add_model(&db, &p, "e", "embed", None).await;
        add_chain(&db, "chat", None, &[(&m, None, None)]).await;
        add_chain(&db, "embed", None, &[(&me, None, None)]).await;
        let mut opts = fast_opts();
        opts.breaker_threshold = 1000;
        let ai = ai(&db, no_env(), opts);
        for _ in 0..bodies.len() {
            let r = ai.chat(req(None)).await;
            assert!(r.is_err(), "{kind}: {r:?}");
            let _ = ai.embed(&["x".to_string()], None).await;
            let _ = ai.probe_model(&m).await.unwrap();
        }
    }
}

#[tokio::test]
async fn ssrf_metadata_blocked_in_chain() {
    let db = open_db();
    let p = add_provider(
        &db,
        "meta",
        "openai-compatible",
        "http://169.254.169.254/v1",
        "",
        json!({}),
    )
    .await;
    let p6 = add_provider(
        &db,
        "meta6",
        "openai-compatible",
        "http://[fd00:ec2::254]/v1",
        "",
        json!({}),
    )
    .await;
    let m = add_model(&db, &p, "m", "chat", None).await;
    let m6 = add_model(&db, &p6, "m6", "chat", None).await;
    add_chain(&db, "chat", None, &[(&m, None, None), (&m6, None, None)]).await;
    let ai = ai(&db, no_env(), fast_opts());
    match ai.chat(req(None)).await {
        Err(AiError::AllFailed(msg)) => {
            assert!(
                msg.contains("blocked provider host 169.254.169.254"),
                "{msg}"
            );
            assert!(msg.contains("fd00:ec2::254"), "{msg}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn chat_json_repairs_once() {
    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct Out {
        anchor: String,
    }
    let db = open_db();
    let s = stub(|_, n| {
        if n == 0 {
            Reply::json(200, oa_chat("Sure! anchor is foo"))
        } else {
            Reply::json(200, oa_chat("```json\n{\"anchor\":\"foo\"}\n```"))
        }
    })
    .await;
    let ai = ai(&db, llm(&s.base, &["dario-m"]), fast_opts());
    let (v, resp): (Out, _) = ai.chat_json(req(None)).await.unwrap();
    assert_eq!(
        v,
        Out {
            anchor: "foo".into()
        }
    );
    assert_eq!(resp.provider, "env:dario");
    assert_eq!(s.count(), 2);
    let second = s.last().body;
    let msgs = second["messages"].as_array().unwrap();
    assert!(
        msgs[0]["content"].as_str().unwrap().contains("JSON"),
        "json nudge first"
    );
    assert!(
        msgs.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("not valid JSON")
    );

    // Gives up after one repair.
    let bad = stub(|_, _| Reply::json(200, oa_chat("never json"))).await;
    let ai = common::ai(&db, llm(&bad.base, &["m"]), fast_opts());
    assert!(matches!(
        ai.chat_json::<Out>(req(None)).await,
        Err(AiError::InvalidOutput(_))
    ));
    assert_eq!(bad.count(), 2);
}

#[tokio::test]
async fn prune_and_bad_requests() {
    let db = open_db();
    db.write(|tx| {
        tx.execute(
            "INSERT INTO model_call_log (role, rung, ok, called_at) VALUES ('chat', 0, 1, '2020-01-01T00:00:00.000Z')",
            [],
        )
        .map_err(sf_store::err)?;
        tx.execute("INSERT INTO model_call_log (role, rung, ok) VALUES ('chat', 0, 1)", [])
            .map_err(sf_store::err)?;
        Ok(())
    })
    .await
    .unwrap();
    let ai = ai(&db, no_env(), fast_opts());
    assert_eq!(ai.prune_call_log(30).await.unwrap(), 1);
    assert_eq!(call_log(&db).await.len(), 1);

    assert!(matches!(
        ai.chat(ChatRequest::default()).await,
        Err(AiError::BadRequest(_))
    ));
    let many = vec!["x".to_string(); sf_ai::MAX_EMBED_TEXTS + 1];
    assert!(matches!(
        ai.embed(&many, None).await,
        Err(AiError::BadRequest(_))
    ));
}
