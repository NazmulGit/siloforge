//! v2.3: tier resolution, token accounting, budgets, cache and the default-chain seed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;

use common::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sf_ai::{Ai, AiCallMeta, AiError, AiEvent, ChatMessage, ChatRequest, Tier};
use sf_store::Db;
use sf_store::rusqlite::params;

fn live(cfg_fn: impl FnOnce(&mut sf_core::Config)) -> sf_core::LiveConfig {
    let mut cfg = sf_core::Config::default();
    cfg.llm.fallback_models = Vec::new();
    cfg_fn(&mut cfg);
    sf_core::LiveConfig::new(cfg)
}

fn live_ai(db: &Db, live: &sf_core::LiveConfig) -> Ai {
    Ai::with_live(db.clone(), Arc::new(KEY), live.clone()).unwrap()
}

async fn set_tier(db: &Db, chain: &str, tier: &str) {
    let (c, t) = (chain.to_string(), tier.to_string());
    db.write(move |tx| {
        tx.execute("UPDATE model_chains SET tier=?2 WHERE id=?1", params![c, t])
            .map_err(sf_store::err)?;
        Ok(())
    })
    .await
    .unwrap();
}

/// Active chain of `tier` with one rung.
async fn tier_chain(db: &Db, tier: &str, site: Option<&str>, model: &str) -> String {
    let id = sf_core::new_id();
    let (id2, t, s, m) = (
        id.clone(),
        tier.to_string(),
        site.map(str::to_string),
        model.to_string(),
    );
    db.write(move |tx| {
        tx.execute(
            "INSERT INTO model_chains (id, name, role, tier, site_id, active) VALUES (?1, ?1, 'chat', ?2, ?3, 1)",
            params![id2, t, s],
        )
        .map_err(sf_store::err)?;
        tx.execute(
            "INSERT INTO model_chain_rungs (id, chain_id, model_id, position) VALUES (?1, ?2, ?3, 0)",
            params![sf_core::new_id(), id2, m],
        )
        .map_err(sf_store::err)?;
        Ok(())
    })
    .await
    .unwrap();
    id
}

/// One provider per model so the served model is visible in the stub path-less reply.
async fn chat_model(db: &Db, base: &str, model: &str) -> String {
    let p = add_provider(
        db,
        &format!("p-{model}"),
        "openai-compatible",
        base,
        "",
        json!({}),
    )
    .await;
    add_model(db, &p, model, "chat", None).await
}

fn req(task: &str, site: Option<&str>) -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage::user("hello there")],
        max_tokens: 60,
        temperature: 0.0,
        site_id: site.map(str::to_string),
        task: Some(task.into()),
        run_id: Some("run-1".into()),
        ..Default::default()
    }
}

async fn rows(db: &Db) -> Vec<Value> {
    db.read(|c| {
        let mut s = c
            .prepare(
                "SELECT task, tier, site_id, run_id, cache_hit, cost_usd, estimated, items, model_id, tokens_in, tokens_out, ok, provider
                   FROM model_call_log ORDER BY id",
            )
            .map_err(sf_store::err)?;
        let v = s
            .query_map([], |r| {
                Ok(json!({
                    "task": r.get::<_, Option<String>>(0)?,
                    "tier": r.get::<_, Option<String>>(1)?,
                    "site_id": r.get::<_, Option<String>>(2)?,
                    "run_id": r.get::<_, Option<String>>(3)?,
                    "cache_hit": r.get::<_, i64>(4)?,
                    "cost_usd": r.get::<_, f64>(5)?,
                    "estimated": r.get::<_, i64>(6)?,
                    "items": r.get::<_, i64>(7)?,
                    "model_id": r.get::<_, Option<String>>(8)?,
                    "tokens_in": r.get::<_, Option<i64>>(9)?,
                    "tokens_out": r.get::<_, Option<i64>>(10)?,
                    "ok": r.get::<_, i64>(11)?,
                    "provider": r.get::<_, Option<String>>(12)?,
                }))
            })
            .map_err(sf_store::err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sf_store::err)?;
        Ok(v)
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn tier_resolution_order() {
    let s = stub(|r, _| Reply::json(200, oa_chat(r.body["model"].as_str().unwrap_or("?")))).await;
    let db = open_db();
    let site = add_site(&db).await;
    let lv = live(|_| {});
    let ai = live_ai(&db, &lv);
    let m_main = chat_model(&db, &s.base, "main-model").await;
    let m_low = chat_model(&db, &s.base, "low-model").await;
    let m_site_low = chat_model(&db, &s.base, "site-low-model").await;
    let m_site_main = chat_model(&db, &s.base, "site-main-model").await;

    // nothing configured, no env models -> not configured
    assert!(matches!(
        ai.chat(req("anchor_pick", None)).await,
        Err(AiError::NotConfigured(_))
    ));

    add_chain(&db, "chat", None, &[(&m_main, None, None)]).await;
    // low task, no low chain -> global main
    let r = ai.chat(req("anchor_pick", None)).await.unwrap();
    assert_eq!(r.text, "main-model");
    assert_eq!(r.tier, Tier::Main);

    tier_chain(&db, "low", None, &m_low).await;
    let r = ai.chat(req("anchor_pick", None)).await.unwrap();
    assert_eq!(r.text, "low-model");
    assert_eq!(r.tier, Tier::Low);
    // main task still on main; high task falls back to main
    assert_eq!(
        ai.chat(req("rephrase", None)).await.unwrap().text,
        "main-model"
    );
    assert_eq!(
        ai.chat(req("money_page", None)).await.unwrap().text,
        "main-model"
    );

    // site main chain beats the global main chain for high (falls back to main: site -> global)
    add_chain(&db, "chat", Some(&site), &[(&m_site_main, None, None)]).await;
    assert_eq!(
        ai.chat(req("money_page", Some(&site))).await.unwrap().text,
        "site-main-model"
    );
    // global low beats site main for a low task (tier first)
    assert_eq!(
        ai.chat(req("anchor_pick", Some(&site))).await.unwrap().text,
        "low-model"
    );
    tier_chain(&db, "low", Some(&site), &m_site_low).await;
    assert_eq!(
        ai.chat(req("anchor_pick", Some(&site))).await.unwrap().text,
        "site-low-model"
    );

    // routing is configurable: anchor_pick -> high (no high chain -> main)
    let mut cfg = (*lv.get()).clone();
    cfg.ai
        .task_tiers
        .insert("anchor_pick".into(), "high".into());
    lv.set(cfg);
    assert_eq!(ai.tier_for_task("anchor_pick"), Tier::High);
    assert_eq!(
        ai.chat(req("anchor_pick", None)).await.unwrap().text,
        "main-model"
    );
    // explicit tier wins over routing
    let mut r = req("anchor_pick", None);
    r.tier = Some(Tier::Low);
    assert_eq!(ai.chat(r).await.unwrap().text, "low-model");

    let tiers = ai.tier_details(None).await;
    assert_eq!(tiers.len(), 3);
    assert_eq!(tiers[0].resolved_from, "tier");
    assert_eq!(tiers[2].resolved_from, "main");
}

#[tokio::test]
async fn accounting_rows_and_events() {
    // first reply has usage, second does not (estimated chars/4)
    let s = stub(|_, n| {
        if n == 0 {
            Reply::json(200, oa_chat("ok"))
        } else {
            Reply::json(200, json!({"choices":[{"message":{"content":"abcdefgh"}}]}))
        }
    })
    .await;
    let db = open_db();
    let site = add_site(&db).await;
    let lv = live(|_| {});
    let ai = live_ai(&db, &lv);
    let m = chat_model(&db, &s.base, "gpt-5.6-luna").await;
    let mm = m.clone();
    db.write(move |t| {
        t.execute(
            "UPDATE models SET price_in=2.0, price_out=10.0 WHERE id=?1",
            [&mm],
        )
        .map_err(sf_store::err)?;
        Ok(())
    })
    .await
    .unwrap();
    tier_chain(&db, "low", None, &m).await;
    let events: Arc<Mutex<Vec<AiEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let ev2 = events.clone();
    ai.set_observer(Arc::new(move |e| ev2.lock().push(e)));

    let mut r1 = req("anchor_pick", Some(&site));
    r1.items = 7;
    let a = ai.chat(r1).await.unwrap();
    assert_eq!(
        (a.tokens_in, a.tokens_out, a.tokens_estimated),
        (Some(11), Some(5), false)
    );
    assert!((a.cost_usd - (11.0 * 2.0 + 5.0 * 10.0) / 1e6).abs() < 1e-12);
    // reasoning effort of the low tier ("low") is sent to a gpt-5 model
    assert_eq!(s.last().body["reasoning_effort"], "low");
    assert_eq!(s.last().body["max_tokens"], 60);
    let b = ai.chat(req("relevance_check", None)).await.unwrap();
    assert!(b.tokens_estimated);
    assert_eq!(b.tokens_out, Some(2));

    let rs = rows(&db).await;
    assert_eq!(rs.len(), 2);
    assert_eq!(rs[0]["task"], "anchor_pick");
    assert_eq!(rs[0]["tier"], "low");
    assert_eq!(rs[0]["site_id"], json!(site));
    assert_eq!(rs[0]["run_id"], "run-1");
    assert_eq!(rs[0]["items"], 7);
    assert_eq!(rs[0]["estimated"], 0);
    assert_eq!(rs[0]["model_id"], "gpt-5.6-luna");
    assert!(rs[0]["cost_usd"].as_f64().unwrap() > 0.0);
    assert_eq!(rs[1]["estimated"], 1);
    assert_eq!(rs[1]["tokens_in"], 3); // "hello there" = 11 chars -> 3

    let ev = events.lock().clone();
    assert_eq!(ev.len(), 2);
    match &ev[0] {
        AiEvent::Call(c) => {
            assert!(c.ok);
            assert_eq!(c.task, "anchor_pick");
            assert_eq!(c.tier, Tier::Low);
            assert_eq!(c.model, "gpt-5.6-luna");
            assert_eq!(c.tokens_in, 11);
            assert_eq!(c.items, 7);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn budget_exhaustion_blocks_and_notifies_once() {
    let s = stub(|_, _| Reply::json(200, oa_chat("ok"))).await;
    let db = open_db();
    let site = add_site(&db).await;
    let lv = live(|c| {
        c.ai.budget.daily_tokens.low = 20; // one call = 16 tokens
        c.ai.budget.daily_tokens.main = 0; // unlimited
        c.ai.budget.per_site_daily_tokens = 0;
    });
    let ai = live_ai(&db, &lv);
    let m = chat_model(&db, &s.base, "m").await;
    add_chain(&db, "chat", None, &[(&m, None, None)]).await;
    let events: Arc<Mutex<Vec<AiEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let ev2 = events.clone();
    ai.set_observer(Arc::new(move |e| ev2.lock().push(e)));

    ai.chat(req("anchor_pick", None)).await.unwrap(); // 16 of 20
    ai.chat(req("anchor_pick", None)).await.unwrap(); // 32 of 20 (overshoot allowed once)
    let calls = s.count();
    let e = ai.chat(req("anchor_pick", None)).await.unwrap_err();
    assert!(matches!(e, AiError::BudgetExhausted(_)), "{e:?}");
    assert!(ai.chat(req("relevance_check", None)).await.is_err());
    assert_eq!(
        s.count(),
        calls,
        "no provider call once the budget is used up"
    );
    // other tiers keep working
    ai.chat(req("rephrase", None)).await.unwrap();
    let st = ai.budget(Tier::Low, None).await;
    assert_eq!(st.exhausted(), Some("tier"));
    assert_eq!(st.tier_remaining(), Some(0));
    let n = events
        .lock()
        .iter()
        .filter(|e| matches!(e, AiEvent::BudgetExhausted { scope: "tier", .. }))
        .count();
    assert_eq!(n, 1, "notified once per day");

    // per-site budget
    let mut cfg = (*lv.get()).clone();
    cfg.ai.budget.daily_tokens.low = 0;
    cfg.ai.budget.per_site_daily_tokens = 10;
    lv.set(cfg);
    ai.chat(req("anchor_pick", Some(&site))).await.unwrap();
    assert!(matches!(
        ai.chat(req("anchor_pick", Some(&site))).await,
        Err(AiError::BudgetExhausted(_))
    ));
    // a different site / no site is not affected by that site's budget
    ai.chat(req("anchor_pick", None)).await.unwrap();
}

#[tokio::test]
async fn cache_hits_ttl_and_accounting() {
    let db = open_db();
    let lv = live(|_| {});
    let ai = live_ai(&db, &lv);
    let k = Ai::cache_key("anchor_pick", "fam", &["para", "target"]);
    assert!(ai.cache_get(&k).await.is_none());
    ai.cache_put(&k, "anchor_pick", "fam", &json!({"a": "anchor"}))
        .await;
    assert_eq!(ai.cache_get(&k).await, Some(json!({"a": "anchor"})));
    // expired rows are ignored and pruned
    let k2 = Ai::cache_key("anchor_pick", "fam", &["old"]);
    let kk = k2.clone();
    db.write(move |t| {
        t.execute(
            "INSERT INTO ai_cache(key, task, family, result, expires_at) VALUES(?1,'anchor_pick','fam','{}','2000-01-01T00:00:00.000Z')",
            [&kk],
        )
        .map_err(sf_store::err)?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(ai.cache_get(&k2).await.is_none());
    assert_eq!(ai.prune_cache().await.unwrap(), 1);
    // cache off
    let mut cfg = (*lv.get()).clone();
    cfg.ai.cache_ttl_days = 0;
    lv.set(cfg);
    assert!(ai.cache_get(&k).await.is_none());

    let mut meta = AiCallMeta::new("anchor_pick", Tier::Low);
    meta.run_id = Some("r".into());
    ai.record_cache_hits(&meta, "fam", 4).await;
    let rs = rows(&db).await;
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0]["cache_hit"], 1);
    assert_eq!(rs[0]["items"], 4);
    assert_eq!(rs[0]["tokens_in"], 0);
    assert_eq!(rs[0]["provider"], "cache");
    // cache hits do not count toward the budget
    assert_eq!(ai.budget(Tier::Low, None).await.tier_used, 0);
}

#[tokio::test]
async fn seed_default_tier_chains() {
    let db = open_db();
    // providers missing -> skipped, nothing written, retried later
    let r = sf_ai::seed::seed_tier_chains(&db).await.unwrap();
    assert!(r.skipped.is_some());
    let codex = add_provider(
        &db,
        "codex-proxy",
        "openai-compatible",
        "http://127.0.0.1:9/v1",
        "",
        json!({}),
    )
    .await;
    let dario = add_provider(
        &db,
        "dario",
        "openai-compatible",
        "http://127.0.0.1:9/v1",
        "",
        json!({}),
    )
    .await;
    let luna = add_model(&db, &codex, "gpt-6-luna", "chat", None).await;
    let haiku = add_model(&db, &dario, "claude-haiku-4-5", "chat", None).await;
    // existing production main chain is kept
    add_chain(
        &db,
        "chat",
        None,
        &[(&luna, None, None), (&haiku, None, None)],
    )
    .await;
    let r = sf_ai::seed::seed_tier_chains(&db).await.unwrap();
    assert_eq!(r.skipped, None);
    assert_eq!(r.kept, vec!["main".to_string()]);
    assert_eq!(r.created_chains.len(), 2);
    let mut created = r.created_models.clone();
    created.sort();
    assert_eq!(
        created,
        vec!["claude-haiku-5-5", "gpt-5.6-luna", "gpt-6-sol"]
    );
    let chains: Vec<(String, String, i64)> = db
        .read(|c| {
            let mut s = c
                .prepare(
                    "SELECT c.tier, group_concat(m.model_id, '>'), c.active FROM model_chains c
                       JOIN model_chain_rungs r ON r.chain_id=c.id JOIN models m ON m.id=r.model_id
                      GROUP BY c.id ORDER BY c.tier",
                )
                .map_err(sf_store::err)?;
            let v = s
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(sf_store::err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sf_store::err)?;
            Ok(v)
        })
        .await
        .unwrap();
    assert_eq!(chains.len(), 3);
    assert_eq!(chains[0].0, "high");
    assert_eq!(chains[0].1, "gpt-6-sol>claude-haiku-5-5>gpt-6-luna");
    assert_eq!(chains[1].0, "low");
    assert_eq!(chains[1].1, "gpt-5.6-luna>claude-haiku-4-5");
    assert!(chains.iter().all(|c| c.2 == 1));
    // idempotent
    let r = sf_ai::seed::seed_tier_chains(&db).await.unwrap();
    assert_eq!(r.skipped.as_deref(), Some("already seeded"));
}

#[tokio::test]
async fn one_active_chain_per_role_tier_scope() {
    let db = open_db();
    let p = add_provider(
        &db,
        "p",
        "openai-compatible",
        "http://127.0.0.1:9/v1",
        "",
        json!({}),
    )
    .await;
    let m = add_model(&db, &p, "m", "chat", None).await;
    let a = add_chain(&db, "chat", None, &[(&m, None, None)]).await;
    set_tier(&db, &a, "low").await;
    // a second active main chain is fine (different tier) ...
    add_chain(&db, "chat", None, &[(&m, None, None)]).await;
    // ... a second active low chain is not
    let bad = db
        .write(|t| {
            t.execute(
                "INSERT INTO model_chains(id, name, role, tier, active) VALUES('x','x','chat','low',1)",
                [],
            )
            .map_err(sf_store::err)
        })
        .await;
    assert!(bad.is_err());
    let bad_tier = db
        .write(|t| {
            t.execute(
                "INSERT INTO model_chains(id, name, role, tier, active) VALUES('y','y','chat','turbo',0)",
                [],
            )
            .map_err(sf_store::err)
        })
        .await;
    assert!(bad_tier.is_err());
}
