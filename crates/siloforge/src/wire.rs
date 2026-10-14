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
