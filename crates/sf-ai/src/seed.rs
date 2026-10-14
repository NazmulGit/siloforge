//! v2.3 default tier chains, seeded once at `serve` start.
//!
//! Needs the providers `codex-proxy` and `dario` (matched by name). Missing model rows are
//! created under them; a tier chain is created and activated only when no active global chat
//! chain exists for that tier. When a provider is missing nothing is written (logged; retried
//! on the next start). A successful seed sets kv `ai_tiers_seeded` so deleting a seeded chain
//! later is respected.

use serde::Serialize;
use sf_store::rusqlite::{OptionalExtension, Transaction, params};
use sf_store::{Db, err};

pub const SEED_KEY: &str = "ai_tiers_seeded";
pub const CODEX: &str = "codex-proxy";
pub const DARIO: &str = "dario";

/// One seeded chain: (tier, chain name, rungs as (provider, model id)).
pub type SeedChain = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

pub const DEFAULT_CHAINS: &[SeedChain] = &[
    (
        "low",
        "Low: Luna 5.6 -> Haiku 4.5",
        &[(CODEX, "gpt-5.6-luna"), (DARIO, "claude-haiku-4-5")],
    ),
    (
        "main",
        "Main: Luna -> Haiku",
        &[(CODEX, "gpt-6-luna"), (DARIO, "claude-haiku-4-5")],
    ),
    (
        "high",
        "High: Sol -> Haiku 5.5 -> Luna",
        &[
            (CODEX, "gpt-6-sol"),
            (DARIO, "claude-haiku-5-5"),
            (CODEX, "gpt-6-luna"),
        ],
    ),
];

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SeedReport {
    /// Why nothing was done (already seeded / providers missing).
    pub skipped: Option<String>,
    pub created_models: Vec<String>,
    pub created_chains: Vec<String>,
    /// Tiers that already had an active global chain.
    pub kept: Vec<String>,
}

fn provider_id(t: &Transaction<'_>, name: &str) -> sf_core::Result<Option<String>> {
    t.query_row(
        "SELECT id FROM model_providers WHERE name=?1",
        [name],
        |r| r.get(0),
    )
    .optional()
    .map_err(err)
}

fn ensure_model(
    t: &Transaction<'_>,
    provider: &str,
    model_id: &str,
    rep: &mut SeedReport,
) -> sf_core::Result<String> {
    let found: Option<String> = t
        .query_row(
            "SELECT id FROM models WHERE provider_id=?1 AND model_id=?2 AND role='chat' ORDER BY created_at, id LIMIT 1",
            params![provider, model_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(err)?;
    if let Some(id) = found {
        return Ok(id);
    }
    let id = sf_core::new_id();
    t.execute(
        "INSERT INTO models(id, provider_id, model_id, label, role, notes) VALUES(?1, ?2, ?3, ?3, 'chat', 'seeded by v2.3 tiers')",
        params![id, provider, model_id],
    )
    .map_err(err)?;
    rep.created_models.push(model_id.to_string());
    Ok(id)
}

/// Seeds in one transaction; see the module docs.
pub fn seed_in(t: &Transaction<'_>) -> sf_core::Result<SeedReport> {
    let mut rep = SeedReport::default();
    let done: Option<String> = t
        .query_row("SELECT value FROM kv WHERE key=?1", [SEED_KEY], |r| {
            r.get(0)
        })
        .optional()
        .map_err(err)?;
    if done.is_some() {
        rep.skipped = Some("already seeded".into());
        return Ok(rep);
    }
    let (Some(codex), Some(dario)) = (provider_id(t, CODEX)?, provider_id(t, DARIO)?) else {
        rep.skipped = Some(format!(
            "providers `{CODEX}` and `{DARIO}` are not both configured; default tier chains not seeded"
        ));
        return Ok(rep);
    };
    for (tier, name, rungs) in DEFAULT_CHAINS {
        let active: i64 = t
            .query_row(
                "SELECT count(*) FROM model_chains WHERE role='chat' AND tier=?1 AND site_id IS NULL AND active=1",
                [tier],
                |r| r.get(0),
            )
            .map_err(err)?;
        if active > 0 {
            rep.kept.push((*tier).to_string());
            continue;
        }
        let chain = sf_core::new_id();
        t.execute(
            "INSERT INTO model_chains(id, name, role, tier, site_id, active, notes)
             VALUES(?1, ?2, 'chat', ?3, NULL, 1, 'seeded by v2.3 tiers')",
            params![chain, name, tier],
        )
        .map_err(err)?;
        for (pos, (prov, model)) in rungs.iter().enumerate() {
            let pid = if *prov == CODEX { &codex } else { &dario };
            let mid = ensure_model(t, pid, model, &mut rep)?;
            t.execute(
                "INSERT INTO model_chain_rungs(id, chain_id, model_id, position, enabled) VALUES(?1, ?2, ?3, ?4, 1)",
                params![sf_core::new_id(), chain, mid, i64::try_from(pos).unwrap_or(0)],
            )
            .map_err(err)?;
        }
        rep.created_chains.push((*name).to_string());
    }
    let v = serde_json::to_string(&rep).unwrap_or_else(|_| "{}".into());
    t.execute(
        "INSERT INTO kv(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![SEED_KEY, v],
    )
    .map_err(err)?;
    Ok(rep)
}

/// Seeds the default tier chains (idempotent).
pub async fn seed_tier_chains(db: &Db) -> sf_core::Result<SeedReport> {
    let rep = db.write(seed_in).await?;
    match &rep.skipped {
        Some(why) if why != "already seeded" => tracing::info!("{why}"),
        Some(_) => {}
        None => tracing::info!(
            chains = ?rep.created_chains,
            models = ?rep.created_models,
            kept = ?rep.kept,
            "seeded default AI tier chains"
        ),
    }
    Ok(rep)
}
