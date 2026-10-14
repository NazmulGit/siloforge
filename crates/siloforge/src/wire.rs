//! v2.1 wiring between the live config and services that live in crates sf-server cannot see.

use std::sync::Arc;

use sf_core::LiveConfig;
use sf_embed::{CpuBudget, EmbedChain};

/// Rebuilds the embedding chain when `[embed]` (mode, GPU share, batch size, keys, ...) or the CPU
/// budget changes. `EmbedChain::reconfigure` keeps running batches on their old rungs.
pub fn embed_follows_live(live: &LiveConfig, embed: Arc<EmbedChain>) {
    live.on_change(move |cfg| {
        let budget = CpuBudget::new(cfg.cpu_threads(), cfg.resources.low_priority_jobs);
        embed.reconfigure(&cfg.embed, &cfg.home, budget);
    });
}

/// v2.3: AI calls -> event log (source `ai`); budget exhaustion -> event log + notification.
pub fn ai_observer(
    events: sf_ops::eventlog::EventLog,
    notifier: sf_ops::notify::Notifier,
) -> sf_ai::AiObserver {
    use sf_ops::eventlog::{Event, source};
    Arc::new(move |ev| match ev {
        sf_ai::AiEvent::Call(c) => {
            let what = if c.cache_hit {
                "cache"
            } else {
                c.model.as_str()
            };
            let e = Event::new(source::AI, "system", &format!("ai {} {what}", c.task))
                .target(c.run_id.as_deref())
                .site(c.site_id.as_deref())
                .ok(c.ok)
                .duration(std::time::Duration::from_millis(c.latency_ms))
                .detail(serde_json::json!({
                    "tier": c.tier.as_str(),
                    "task": c.task,
                    "tokens_in": c.tokens_in,
                    "tokens_out": c.tokens_out,
                    "estimated": c.estimated,
                    "cost_usd": c.cost_usd,
                    "cache_hit": c.cache_hit,
                    "items": c.items,
                    "fallback_used": c.fallback_used,
                    "rung": c.rung,
                    "latency_ms": c.latency_ms,
                    "provider": c.provider,
                    "run_id": c.run_id,
                    "error": c.error,
                }));
            events.record(e);
        }
        sf_ai::AiEvent::BudgetExhausted {
            tier,
            scope,
            site_id,
            used,
            limit,
        } => {
            let what = if scope == "site" {
                "this site's".to_string()
            } else {
                format!("the {} tier's", tier.as_str())
            };
            notifier.notify(
                sf_ops::notify::Notification::new(
                    sf_ops::notify::events::AI_BUDGET_EXHAUSTED,
                    "SiloForge AI budget used up",
                    format!(
                        "Today's AI token budget of {what} is used up ({used} of {limit} tokens). Linking continues with heuristics until 00:00 UTC; raise it in Settings > AI Models > Budgets."
                    ),
                )
                .site(site_id.as_deref())
                .data(serde_json::json!({"tier": tier.as_str(), "scope": scope, "used": used, "limit": limit})),
            );
            events.record(
                Event::new(source::AI, "system", "ai budget exhausted")
                    .site(site_id.as_deref())
                    .ok(false)
                    .detail(serde_json::json!({"tier": tier.as_str(), "scope": scope, "used": used, "limit": limit})),
            );
        }
    })
}

/// Settings facts for `GET /api/v2/settings` and the restart hook for `POST /system/restart`.
pub fn settings_hooks(
    embed: Arc<EmbedChain>,
    restart: tokio::sync::watch::Sender<bool>,
) -> sf_server::v2::settings::SettingsHooks {
    sf_server::v2::settings::SettingsHooks {
        active_device: Arc::new(move || embed.active_device()),
        restart: Some(Arc::new(move || {
            let _ = restart.send(true);
        })),
    }
}

/// The MCP tool catalogue as the server lists it (`/api/v2/mcp`, policy defaults, stats).
pub fn mcp_catalogue() -> sf_server::state::McpTools {
    sf_server::state::McpTools(
        sf_mcp::tools::catalogue()
            .into_iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "title": t.title,
                    "description": t.description,
                    "read_only": t.read_only,
                    "destructive": t.destructive,
                    "min_scope": t.min_scope,
                    "default_enabled": t.default_enabled,
                    "category": t.category,
                })
            })
            .collect(),
    )
}
