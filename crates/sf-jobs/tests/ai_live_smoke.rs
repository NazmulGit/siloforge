//! Manual live smoke test of the v2.3 batched low-tier prompts against a real local
//! OpenAI-compatible proxy. Ignored by default and skipped unless `SF_LIVE_AI_URL` and
//! `SF_LIVE_AI_MODEL` are set, e.g.
//! `SF_LIVE_AI_URL=http://127.0.0.1:3460/v1 SF_LIVE_AI_MODEL=claude-haiku-4-5 cargo test -p sf-jobs --test ai_live_smoke -- --ignored`.
//! Makes exactly two calls.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use sf_ai::{Ai, ChatMessage, ChatRequest};
use sf_engine::anchor::batch::{
    AnchorBatchItem, RelevanceBatchItem, anchor_batch_prompt, anchor_max_tokens, parse_batch_reply,
    relevance_batch_prompt, relevance_max_tokens,
};
use sf_store::Db;
use sf_store::rusqlite::params;

#[tokio::test]
#[ignore = "live: needs SF_LIVE_AI_URL + SF_LIVE_AI_MODEL"]
async fn live_batched_anchor_and_relevance() {
    let (Ok(url), Ok(model)) = (
        std::env::var("SF_LIVE_AI_URL"),
        std::env::var("SF_LIVE_AI_MODEL"),
    ) else {
        eprintln!("SF_LIVE_AI_URL / SF_LIVE_AI_MODEL not set: skipped");
        return;
    };
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("sf-live-{}", sf_core::new_id()));
    let db = Db::open(&dir.join("t.db")).unwrap();
    let (u, m) = (url.clone(), model.clone());
    db.write(move |t| {
        t.execute(
            "INSERT INTO model_providers(id,name,kind,base_url) VALUES('p','live','openai-compatible',?1)",
            [&u],
        )
        .map_err(sf_store::err)?;
        t.execute(
            "INSERT INTO models(id,provider_id,model_id,role) VALUES('m','p',?1,'chat')",
            [&m],
        )
        .map_err(sf_store::err)?;
        t.execute(
            "INSERT INTO model_chains(id,name,role,tier,active) VALUES('c','low','chat','low',1)",
            [],
        )
        .map_err(sf_store::err)?;
        t.execute(
            "INSERT INTO model_chain_rungs(id,chain_id,model_id,position) VALUES('r','c','m',0)",
            params![],
        )
        .map_err(sf_store::err)?;
        Ok(())
    })
    .await
    .unwrap();
    let mut cfg = sf_core::Config::default();
    cfg.llm.fallback_models = Vec::new();
    let live = sf_core::LiveConfig::new(cfg);
    let ai = Ai::with_live(db.clone(), Arc::new([1u8; 32]), live).unwrap();

    let items = vec![
        AnchorBatchItem {
            id: 0,
            paragraph: "Before a long ride, clean the bike chain and check the tyre pressure so the drivetrain stays quiet.".into(),
            target_title: "How to Clean a Bike Chain".into(),
            keywords: vec!["bike chain".into(), "chain cleaning".into()],
            candidates: vec!["bike chain".into(), "tyre pressure".into()],
        },
        AnchorBatchItem {
            id: 1,
            paragraph: "Night riding is safer with bright front lights and a red rear light that other road users can see.".into(),
            target_title: "Best Bike Lights for Night Riding".into(),
            keywords: vec!["bike lights".into()],
            candidates: vec!["bright front lights".into(), "red rear light".into()],
        },
    ];
    let (sys, user) = anchor_batch_prompt(&items);
    let r = ai
        .chat(ChatRequest {
            messages: vec![ChatMessage::system(sys), ChatMessage::user(user)],
            max_tokens: anchor_max_tokens(items.len()),
            temperature: 0.0,
            json_mode: true,
            task: Some("anchor_pick".into()),
            items: 2,
            ..Default::default()
        })
        .await
        .unwrap();
    eprintln!(
        "anchor_pick: model={} tier={:?} tokens={:?}/{:?} estimated={} text={}",
        r.model, r.tier, r.tokens_in, r.tokens_out, r.tokens_estimated, r.text
    );
    let parsed = parse_batch_reply(&r.text, &[0, 1]);
    assert!(!parsed.is_empty(), "no item parsed from {}", r.text);

    let rel = vec![RelevanceBatchItem {
        id: 7,
        paragraph: "Clean the bike chain every few hundred kilometres.".into(),
        target_title: "How to Clean a Bike Chain".into(),
    }];
    let (sys, user) = relevance_batch_prompt(&rel);
    let r2 = ai
        .chat(ChatRequest {
            messages: vec![ChatMessage::system(sys), ChatMessage::user(user)],
            max_tokens: relevance_max_tokens(1),
            temperature: 0.0,
            json_mode: true,
            task: Some("relevance_check".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    eprintln!(
        "relevance_check: tokens={:?}/{:?} text={}",
        r2.tokens_in, r2.tokens_out, r2.text
    );
    let p2 = parse_batch_reply(&r2.text, &[7]);
    assert!(
        p2.get(&7).and_then(|x| x.relevance).is_some(),
        "{}",
        r2.text
    );
    let n: i64 = db
        .read(|c| {
            c.query_row(
                "SELECT count(*) FROM model_call_log WHERE tier='low' AND ok=1 AND tokens_in > 0",
                [],
                |r| r.get(0),
            )
            .map_err(sf_store::err)
        })
        .await
        .unwrap();
    assert_eq!(n, 2);
    db.close();
    let _ = std::fs::remove_dir_all(&dir);
}
