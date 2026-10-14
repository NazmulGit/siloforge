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

mod backups;
mod gsc;
mod jobs;
mod misc;
mod notifications;
mod system;
mod tokens;

#[cfg(test)]
mod tests;

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
        .route("/tokens/{id}", axum::routing::delete(tokens::revoke))
        .route("/auth/logout-all", post(tokens::logout_all))
        .route("/audit", get(misc::audit))
        .route("/mcp", get(misc::mcp_info))
        .route("/mcp/info", get(misc::mcp_info))
        .route("/sites/{id}/gsc/sync", post(gsc::sync))
}

/// Clamps a `limit` query value.
pub(crate) fn clamp_limit(v: Option<i64>, default: i64, max: i64) -> i64 {
    v.unwrap_or(default).clamp(1, max)
}
