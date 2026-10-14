//! `/api/v1`: the v1 contract (78 routes) so the React UI, the MCP server and the WP plugin work
//! unchanged, plus three small additions: `POST /sites/:id/rotate-key`, `GET /jobs/run/:id` and
//! optional `job_kind`/`schedule_id` on run-now.
//!
//! Paths that share a prefix use one parameter name (`{id}`) so the router accepts them
//! (`GET /edges/{id}` = site id, `POST /edges/{id}/decide` = edge id, etc.).

pub mod auth;
pub mod common;
pub mod csv_io;
pub mod dashboard;
pub mod edges;
pub mod gsc;
pub mod jobs;
pub mod linkhealth;
pub mod metrics;
pub mod posts;
pub mod providers;
pub mod reports;
pub mod rules;
pub mod schedules;
pub mod silo;
pub mod sites;
pub mod sync;

use axum::Router;
use axum::routing::{get, patch, post};

use crate::AppState;

/// Number of v1 contract routes (method + path) served here, excluding v2 additions.
pub const V1_ROUTE_COUNT: usize = 77;

pub fn router() -> Router<AppState> {
    Router::new()
        // auth
        .route("/auth/register", post(auth::register))
        .route("/auth/login", post(auth::login))
        .route("/auth/me", get(auth::me))
        .route("/auth/change-password", post(auth::change_password))
        .route("/auth/handshake", post(auth::handshake))
        // sites
        .route("/sites", get(sites::list).post(sites::create))
        .route("/sites/{id}", get(sites::get).patch(sites::patch))
        .route("/sites/{id}/rotate-key", post(sites::rotate_key))
        .route("/sites/{id}/crawl", post(sites::crawl))
        .route("/sites/{id}/plan", post(sites::plan))
        .route("/sites/{id}/archive-plan", post(sites::archive_plan))
        .route(
            "/sites/{id}/gsc",
            get(gsc::status).post(gsc::connect).delete(gsc::disconnect),
        )
        .route("/sites/{id}/link-health", post(linkhealth::run))
        .route("/usage", get(posts::usage))
        .route("/posts/{id}", get(posts::list))
        // plugin channel
        .route("/sync", post(sync::sync))
        .route("/suggestions/{id}", get(sync::suggestions))
        .route("/suggestions/{id}/ack", post(sync::ack))
        .route("/suggestions/{id}/revert-ack", post(sync::revert_ack))
        // edges / bulk
        .route("/edges/{id}", get(edges::list))
        .route("/edges/{id}/decide", post(edges::decide))
        .route("/edges/{id}/anchor", patch(edges::anchor))
        .route("/bulk/decide", post(edges::bulk_decide))
        .route("/bulk/run", post(edges::bulk_run))
        .route("/bulk/undo", post(edges::bulk_undo))
        // rules
        .route(
            "/rules/keywords/{id}",
            get(rules::list_keywords)
                .post(rules::create_keyword)
                .patch(rules::patch_keyword)
                .delete(rules::delete_keyword),
        )
        .route("/rules/keywords/{id}/run", post(rules::run_keywords))
        .route(
            "/rules/ignore/{id}",
            get(rules::list_ignore)
                .post(rules::create_ignore)
                .delete(rules::delete_ignore),
        )
        // dashboard / reports
        .route("/dashboard/{id}", get(dashboard::get))
        .route("/reports/links/{id}", get(reports::links))
        .route("/reports/graph/{id}", get(reports::graph))
        .route("/reports/anchors/{id}", get(reports::anchors))
        .route("/reports/link-health/{id}", get(linkhealth::report))
        .route("/reports/metrics/{id}", get(metrics::report))
        .route("/history/{id}", get(reports::history))
        .route("/jobs/{id}", get(jobs::list))
        .route("/jobs/run/{id}", get(jobs::get_run))
        // silo / csv
        .route("/silo/{id}/clusters", get(silo::list))
        .route("/silo/{id}/clusters/export", get(silo::export))
        .route("/silo/{id}/clusters/import", post(silo::import))
        .route("/csv/import/{id}", post(csv_io::import))
        .route("/csv/export/{id}", get(csv_io::export))
        // schedules
        .route(
            "/schedules/{id}",
            get(schedules::list)
                .post(schedules::upsert)
                .delete(schedules::delete),
        )
        .route("/schedules/{id}/run-now", post(schedules::run_now))
        // providers / models / chains (admin)
        .route(
            "/providers",
            get(providers::list_providers).post(providers::create_provider),
        )
        .route("/providers/health", get(providers::providers_health))
        .route(
            "/providers/{id}",
            get(providers::get_provider)
                .patch(providers::patch_provider)
                .delete(providers::delete_provider),
        )
        .route("/providers/{id}/probe", post(providers::probe_provider))
        .route(
            "/models",
            get(providers::list_models).post(providers::create_model),
        )
        .route(
            "/models/{id}",
            patch(providers::patch_model).delete(providers::delete_model),
        )
        .route("/models/{id}/probe", post(providers::probe_model))
        .route(
            "/model-chains",
            get(providers::list_chains).post(providers::create_chain),
        )
        .route(
            "/model-chains/{id}",
            patch(providers::patch_chain).delete(providers::delete_chain),
        )
        .route(
            "/model-chains/{id}/activate",
            post(providers::activate_chain),
        )
        .route(
            "/model-chains/{id}/deactivate",
            post(providers::deactivate_chain),
        )
        .route(
            "/model-chains/{id}/rungs",
            get(providers::list_rungs)
                .put(providers::put_rungs)
                .post(providers::add_rung),
        )
        .route(
            "/model-chains/{id}/rungs/reorder",
            post(providers::reorder_rungs),
        )
        .route(
            "/model-chain-rungs/{id}",
            patch(providers::patch_rung).delete(providers::delete_rung),
        )
        // Unknown /api/v1 paths answer JSON 404, never the SPA index.html.
        .fallback(|| async { crate::ApiError::not_found("route not found") })
}
