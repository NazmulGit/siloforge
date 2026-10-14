//! /api/v2 routes (new features; the v1 contract is untouched).
//!
//! Admin-only unless noted. Services are looked up with `AppState::service::<T>()`; the binary must
//! register `sf_ops::monitor::Monitor` and `sf_ops::notify::Notifier` (503 otherwise, except where a
//! route can degrade gracefully).
//!
//! | route | who |
//! |---|---|
//! | `GET /system`, `GET /system/stats`, `GET /system/metrics?window=1h\|24h\|7d` | admin |
//! | `GET /jobs?status=&kind=&site_id=&limit=`, `GET /jobs/{id}`, `GET /jobs/{id}/logs?after_id=`, `POST /jobs/{id}/cancel` | user (own sites; admin all) |
//! | `GET/POST /notifications/channels`, `PATCH/DELETE /notifications/channels/{id}`, `POST /notifications/channels/{id}/test` | admin |
//! | `GET /notifications?unread=`, `POST /notifications/read` | admin |
//! | `GET /errors`, `POST /errors/{fp}/resolve` | admin |
//! | `GET/POST /backups`, `GET /backups/{id}/download`, `POST /backups/{id}/verify`, `DELETE /backups/{id}` | admin |
//! | `GET/POST /tokens`, `DELETE /tokens/{id}` | user (own tokens) |
//! | `POST /auth/logout-all` | user session (ends all sessions, revokes own API tokens; I-02) |
//! | `GET /audit?limit=` | admin |
//! | `GET /mcp` (alias `/mcp/info`) | user |
//! | `GET/PUT /settings`, `POST /system/restart` | admin (v2.1 §1) |
//! | `PATCH /tokens/{id}` | owner or admin (v2.1 §2) |
//! | `GET/PUT /mcp/policy`, `GET /mcp/stats` | admin (v2.1 §3) |
//! | `GET /mcp/policy/effective`, `POST /mcp/usage` | any user / API token (v2.1 §3) |
//! | `GET /docs`, `GET /docs/{slug}` | user (v2.1 §4) |
//! | `GET /downloads`, `GET /downloads/{file}` | user (v2.1 §5) |
//! | `GET /logs?source=&actor=&site_id=&status=&q=&since=&until=&limit=&before_id=&after_id=` | user (admin: all rows; others: own rows) |
//! | `GET /logs/export?...` (CSV), `GET /logs/stats?since=&until=` | user (same row visibility) |
//! | `GET /sites/{id}/opportunities`, `GET/POST /sites/{id}/link-runs`, `GET /link-runs/{id}[/items]`, `POST /link-runs/{id}/select\|deploy\|verify\|repair\|undo\|cancel`, `PATCH /link-runs/{id}/items/{edge}` | user, own sites (v2.2, `link_runs.rs`) |
//! | `GET/PUT /sites/{id}/auto-linker`, `POST /sites/{id}/auto-linker/run` | user, own sites (v2.2 §5) |
//! | `GET /ai/usage`, `GET /ai/tiers`, `GET/PUT /ai/settings` | admin (v2.3, `ai.rs`) |
//! | `POST /sites/{id}/link-runs/estimate` | user, own sites (v2.3) |
//! | `GET /auth/config`, `POST /auth/login\|register\|2fa/login\|verify-email\|forgot\|reset`, `GET/POST /invites/{token}` | public (v2.4, `auth2.rs`, `workspaces.rs`) |
//! | `GET/PATCH /auth/me`, `/auth/2fa/*`, `/auth/sessions`, `/auth/logout`, `/auth/change-email`, `/auth/verify-email/resend` | user (v2.4) |
//! | `/workspaces/*`, `GET /plans`, `GET /billing` | user, by workspace role (v2.4) |
//! | `/admin/*` | platform admin (v2.4, `admin.rs`) |

pub mod admin;
pub mod ai;
pub mod auth2;
mod backups;
pub mod docs;
pub mod downloads;
mod gsc;
mod jobs;
pub mod link_runs;
pub(crate) mod logs;
pub mod mcp;
mod misc;
mod notifications;
pub mod settings;
mod system;
mod tokens;
pub mod workspaces;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_v21;
#[cfg(test)]
mod tests_v23;

use axum::Router;
use axum::routing::{get, patch, post};

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/system", get(system::info))
        .route("/system/stats", get(system::stats))
        .route("/system/metrics", get(system::metrics))
        .route("/jobs", get(jobs::list))
        .route("/jobs/{id}", get(jobs::get_one))
        .route("/jobs/{id}/logs", get(jobs::logs))
        .route("/jobs/{id}/cancel", post(jobs::cancel))
        .route(
            "/notifications/channels",
            get(notifications::list_channels).post(notifications::create_channel),
        )
        .route(
            "/notifications/channels/{id}",
            patch(notifications::update_channel).delete(notifications::delete_channel),
        )
        .route(
            "/notifications/channels/{id}/test",
            post(notifications::test_channel),
        )
        .route("/notifications", get(notifications::list))
        .route("/notifications/read", post(notifications::mark_read))
        .route("/errors", get(misc::errors))
        .route("/errors/{fp}/resolve", post(misc::resolve_error))
        .route("/backups", get(backups::list).post(backups::create))
        .route(
            "/backups/{id}",
            get(backups::get_one).delete(backups::delete),
        )
        .route("/backups/{id}/download", get(backups::download))
        .route("/backups/{id}/verify", post(backups::verify))
        .route("/tokens", get(tokens::list).post(tokens::create))
        .route(
            "/tokens/{id}",
            axum::routing::delete(tokens::revoke).patch(tokens::patch),
        )
        .route("/settings", get(settings::get).put(settings::put))
        .route("/system/restart", post(settings::restart))
        .route("/mcp/policy", get(mcp::get_policy).put(mcp::put_policy))
        .route("/mcp/policy/effective", get(mcp::effective))
        .route("/mcp/stats", get(mcp::stats))
        .route("/mcp/usage", post(mcp::usage))
        .route("/docs", get(docs::list))
        .route("/docs/{slug}", get(docs::get_one))
        .route("/downloads", get(downloads::list))
        .route("/downloads/{file}", get(downloads::download))
        .route("/auth/logout-all", post(tokens::logout_all))
        .route("/audit", get(misc::audit))
        .route("/logs", get(logs::list))
        .route("/logs/export", get(logs::export))
        .route("/logs/stats", get(logs::stats))
        .route("/mcp", get(misc::mcp_info))
        .route("/mcp/info", get(misc::mcp_info))
        .route("/sites/{id}/gsc/sync", post(gsc::sync))
        // v2.2 link runs + Auto-Linker
        .route("/sites/{id}/opportunities", get(link_runs::opportunities))
        // v2.3 AI tiers, usage, Link Run estimate
        .route("/ai/usage", get(ai::usage))
        .route("/ai/tiers", get(ai::tiers))
        .route("/ai/settings", get(ai::settings_get).put(ai::settings_put))
        .route("/sites/{id}/link-runs/estimate", post(ai::estimate))
        .route(
            "/sites/{id}/link-runs",
            get(link_runs::list).post(link_runs::create),
        )
        .route(
            "/sites/{id}/auto-linker",
            get(link_runs::get_auto_linker).put(link_runs::put_auto_linker),
        )
        .route(
            "/sites/{id}/auto-linker/run",
            post(link_runs::run_auto_linker),
        )
        .route("/link-runs/{run_id}", get(link_runs::get_one))
        .route("/link-runs/{run_id}/items", get(link_runs::items))
        .route(
            "/link-runs/{run_id}/items/{edge_id}",
            patch(link_runs::patch_item),
        )
        .route("/link-runs/{run_id}/select", post(link_runs::select))
        .route("/link-runs/{run_id}/deploy", post(link_runs::deploy))
        .route("/link-runs/{run_id}/verify", post(link_runs::verify))
        .route("/link-runs/{run_id}/repair", post(link_runs::repair))
        .route("/link-runs/{run_id}/undo", post(link_runs::undo))
        .route("/link-runs/{run_id}/cancel", post(link_runs::cancel))
        // v2.4 accounts
        .route("/auth/config", get(auth2::config))
        .route("/auth/login", post(auth2::login))
        .route("/auth/2fa/login", post(auth2::login_2fa))
        .route("/auth/register", post(auth2::register))
        .route("/auth/verify-email", post(auth2::verify_email))
        .route(
            "/auth/verify-email/resend",
            post(auth2::resend_verification),
        )
        .route("/auth/forgot", post(auth2::forgot))
        .route("/auth/reset", post(auth2::reset))
        .route("/auth/change-email", post(auth2::change_email))
        .route("/auth/me", get(auth2::me).patch(auth2::patch_me))
        .route("/auth/2fa/setup", post(auth2::twofa_setup))
        .route("/auth/2fa/enable", post(auth2::twofa_enable))
        .route("/auth/2fa/disable", post(auth2::twofa_disable))
        .route("/auth/2fa/recovery-codes", post(auth2::twofa_recovery))
        .route("/auth/sessions", get(auth2::sessions_list))
        .route(
            "/auth/sessions/{id}",
            axum::routing::delete(auth2::session_revoke),
        )
        .route("/auth/logout", post(auth2::logout))
        // v2.4 workspaces / plans / billing
        .route(
            "/workspaces",
            get(workspaces::list).post(workspaces::create),
        )
        .route(
            "/workspaces/{id}",
            get(workspaces::get_one)
                .patch(workspaces::patch)
                .delete(workspaces::delete),
        )
        .route("/workspaces/{id}/transfer", post(workspaces::transfer))
        .route("/workspaces/{id}/members", get(workspaces::members))
        .route(
            "/workspaces/{id}/members/{user_id}",
            patch(workspaces::patch_member).delete(workspaces::remove_member),
        )
        .route(
            "/workspaces/{id}/invites",
            get(workspaces::invites).post(workspaces::create_invite),
        )
        .route(
            "/workspaces/{id}/invites/{invite_id}",
            axum::routing::delete(workspaces::revoke_invite),
        )
        .route("/workspaces/{id}/usage", get(workspaces::usage))
        .route(
            "/workspaces/{id}/mcp-policy",
            get(mcp::ws_get_policy).put(mcp::ws_put_policy),
        )
        .route("/invites/{token}", get(workspaces::invite_info))
        .route("/invites/{token}/accept", post(workspaces::accept_invite))
        .route("/plans", get(workspaces::plans))
        .route("/billing", get(workspaces::billing))
        .route("/billing/webhook", post(workspaces::billing_webhook))
        // v2.4 admin console
        .route("/admin/overview", get(admin::overview))
        .route("/admin/users", get(admin::users).post(admin::user_create))
        .route(
            "/admin/users/{id}",
            get(admin::user_get)
                .patch(admin::user_patch)
                .delete(admin::user_delete),
        )
        .route("/admin/users/{id}/{action}", post(admin::user_action))
        .route("/admin/workspaces", get(admin::workspaces))
        .route(
            "/admin/workspaces/{id}",
            get(admin::workspace_get)
                .patch(admin::workspace_patch)
                .delete(admin::workspace_delete),
        )
        .route(
            "/admin/workspaces/{id}/{action}",
            post(admin::workspace_action),
        )
        .route("/admin/plans", post(admin::plan_create))
        .route(
            "/admin/plans/{id}",
            patch(admin::plan_patch).delete(admin::plan_delete),
        )
        .route("/admin/invites", get(admin::invites))
        .route("/admin/email", get(admin::email))
        .route("/admin/email/test", post(admin::email_test))
        .route(
            "/admin/email/outbox/{id}",
            axum::routing::delete(admin::outbox_delete),
        )
        .route(
            "/admin/platform",
            get(admin::platform_get).put(admin::platform_put),
        )
        .route("/admin/audit", get(admin::audit_log))
}

/// Clamps a `limit` query value.
pub(crate) fn clamp_limit(v: Option<i64>, default: i64, max: i64) -> i64 {
    v.unwrap_or(default).clamp(1, max)
}
