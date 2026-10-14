//! Table map v1 Postgres -> v2 SQLite. Column names are identical in both schemas; only types change.

/// How a CSV value is converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    /// TEXT NOT NULL ('' when empty/NULL).
    Text,
    /// TEXT nullable.
    NText,
    /// INTEGER NOT NULL (0 when NULL).
    Int,
    NInt,
    /// REAL NOT NULL (0 when NULL).
    Real,
    NReal,
    /// boolean t/f -> 1/0, NOT NULL (0 when NULL).
    Bool,
    NBool,
    /// jsonb -> TEXT; invalid/NULL -> `{}`.
    JsonObj,
    /// jsonb -> TEXT; invalid/NULL -> `[]`.
    JsonArr,
    /// timestamptz -> RFC 3339 UTC, NOT NULL (now when NULL).
    Ts,
    NTs,
    /// pgvector text -> f32 LE BLOB (NULL when absent or not 384-d).
    Vector,
}

impl Ty {
    pub fn nullable(self) -> bool {
        matches!(
            self,
            Ty::NText | Ty::NInt | Ty::NReal | Ty::NBool | Ty::NTs | Ty::Vector
        )
    }
}

/// Row filter used with `--site`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Not site-specific: always imported in full.
    Global,
    /// `sites` itself (`id = site`).
    SelfId,
    /// `site_id = site`.
    Site,
    /// `site_id IS NULL OR site_id = site`.
    SiteOrNull,
    /// `post_id` belongs to one of the site's posts.
    Post,
}

pub struct Table {
    pub name: &'static str,
    pub scope: Scope,
    pub cols: &'static [(&'static str, Ty)],
}

use Ty::*;

/// Import order respects foreign keys (clusters before posts with `pillar_post_id` deferred).
pub const TABLES: &[Table] = &[
    Table {
        name: "users",
        scope: Scope::Global,
        cols: &[
            ("id", Text),
            ("email", Text),
            ("password_hash", Text),
            ("name", Text),
            ("role", Text),
            ("created_at", Ts),
        ],
    },
    Table {
        name: "sites",
        scope: Scope::SelfId,
        cols: &[
            ("id", Text),
            ("domain", Text),
            ("wp_url", NText),
            ("mode", Text),
            ("api_key", Text),
            ("settings", JsonObj),
            ("created_at", Ts),
            ("last_sync_at", NTs),
            ("user_id", NText),
        ],
    },
    Table {
        name: "clusters",
        scope: Scope::Site,
        cols: &[
            ("id", Text),
            ("site_id", Text),
            ("label", Text),
            ("pillar_post_id", NText),
            ("source", Text),
            ("created_at", Ts),
        ],
    },
    Table {
        name: "posts",
        scope: Scope::Site,
        cols: &[
            ("id", Text),
            ("site_id", Text),
            ("wp_post_id", NInt),
            ("url", Text),
            ("slug", NText),
            ("title", Text),
            ("content_html", Text),
            ("content_text", Text),
            ("content_hash", Text),
            ("builder", Text),
            ("post_type", Text),
            ("post_status", Text),
            ("meta", JsonObj),
            ("word_count", Int),
            ("cluster_id", NText),
            ("pagerank", Real),
            ("click_depth", Int),
            ("is_orphan", Bool),
            ("is_dead_end", Bool),
            ("embedding", Vector),
            ("published_at", NTs),
            ("modified_at", NTs),
            ("crawled_at", Ts),
            ("outbound_link_count", Int),
            ("internal_link_count", Int),
            ("broken_link_count", Int),
            ("nofollow_link_count", Int),
            ("redirect_link_count", Int),
            ("has_breadcrumbs", Bool),
            ("has_breadcrumb_schema", Bool),
            ("breadcrumb_report", JsonObj),
            ("link_health_checked_at", NTs),
            ("is_contextual_orphan", Bool),
            ("is_nav_only", Bool),
            ("body_inbound_count", Int),
            ("nav_inbound_count", Int),
        ],
    },
    Table {
        name: "paragraphs",
        scope: Scope::Post,
        cols: &[
            ("id", Text),
            ("post_id", Text),
            ("idx", Int),
            ("text", Text),
            ("hash", Text),
            ("embedding", Vector),
        ],
    },
    Table {
        name: "keywords",
        scope: Scope::Post,
        cols: &[
            ("id", Text),
            ("post_id", Text),
            ("keyword", Text),
            ("source", Text),
            ("score", Real),
            ("volume", NInt),
            ("position", NReal),
            ("striking", Bool),
        ],
    },
    Table {
        name: "snapshots",
        scope: Scope::Post,
        cols: &[
            ("id", Text),
            ("post_id", Text),
            ("content_html", Text),
            ("reason", Text),
            ("taken_at", Ts),
        ],
    },
    Table {
        name: "link_edges",
        scope: Scope::Site,
        cols: &[
            ("id", Text),
            ("site_id", Text),
            ("source_post_id", Text),
            ("target_post_id", Text),
            ("paragraph_id", NText),
            ("anchor", Text),
            ("anchor_type", Text),
            ("composite_score", Real),
            ("score_breakdown", JsonObj),
            ("silo_topology", NText),
            ("insert_module", NText),
            ("status", Text),
            ("origin", Text),
            ("snapshot_id", NText),
            ("error", NText),
            ("failure_kind", NText),
            ("created_at", Ts),
            ("decided_at", NTs),
            ("applied_at", NTs),
        ],
    },
    Table {
        name: "job_runs",
        scope: Scope::SiteOrNull,
        cols: &[
            ("id", Text),
            ("site_id", NText),
            ("kind", Text),
            ("status", Text),
            ("stats", JsonObj),
            ("error", NText),
            ("started_at", NTs),
            ("finished_at", NTs),
            ("created_at", Ts),
        ],
    },
    Table {
        name: "keyword_rules",
        scope: Scope::Site,
        cols: &[
            ("id", Text),
            ("site_id", Text),
            ("keyword", Text),
            ("target_url", Text),
            ("anchor_type", Text),
            ("max_links", Int),
            ("active", Bool),
            ("created_at", Ts),
        ],
    },
    Table {
        name: "ignore_keywords",
        scope: Scope::Site,
        cols: &[
            ("id", Text),
            ("site_id", Text),
            ("phrase", Text),
            ("created_at", Ts),
        ],
    },
    Table {
        name: "link_health",
        scope: Scope::Site,
        cols: &[
            ("id", Text),
            ("site_id", Text),
            ("source_post_id", NText),
            ("href", Text),
            ("requested_url", Text),
            ("resolved_url", NText),
            ("target_post_id", NText),
            ("scope", Text),
            ("status_code", NInt),
            ("ok", Bool),
            ("is_broken", Bool),
            ("redirect_hops", Int),
            ("redirect_chain", JsonArr),
            ("has_redirect_loop", Bool),
            ("has_temp_redirect", Bool),
            ("rel", Text),
            ("is_nofollow", Bool),
            ("is_sponsored", Bool),
            ("is_ugc", Bool),
            ("anchor", Text),
            ("placement", Text),
            ("error", NText),
            ("checked_at", Ts),
            ("failure_kind", Text),
            ("is_unverified", Bool),
        ],
    },
    Table {
        name: "site_schedules",
        scope: Scope::Site,
        cols: &[
            ("id", Text),
            ("site_id", Text),
            ("job_kind", Text),
            ("cron", Text),
            ("tz", Text),
            ("active", Bool),
            ("payload", JsonObj),
            ("last_run_at", NTs),
            ("last_job_run_id", NText),
            ("next_run_at", NTs),
            ("created_at", Ts),
            ("updated_at", Ts),
        ],
    },
    Table {
        name: "site_metrics_history",
        scope: Scope::Site,
        // m_* generated columns are never inserted.
        cols: &[
            ("id", Text),
            ("site_id", Text),
            ("job_run_id", NText),
            ("source", Text),
            ("metrics", JsonObj),
            ("captured_at", Ts),
        ],
    },
    Table {
        name: "model_providers",
        scope: Scope::Global,
        cols: &[
            ("id", Text),
            ("name", Text),
            ("label", Text),
            ("kind", Text),
            ("base_url", Text),
            ("api_key_enc", Text),
            ("headers", JsonObj),
            ("enabled", Bool),
            ("last_probe_at", NTs),
            ("last_probe_ok", NBool),
            ("last_probe_error", NText),
            ("last_probe_ms", NInt),
            ("created_at", Ts),
            ("updated_at", Ts),
        ],
    },
    Table {
        name: "models",
        scope: Scope::Global,
        cols: &[
            ("id", Text),
            ("provider_id", Text),
            ("model_id", Text),
            ("label", Text),
            ("role", Text),
            ("context_tokens", NInt),
            ("dim", NInt),
            ("enabled", Bool),
            ("price_in", NReal),
            ("price_out", NReal),
            ("notes", Text),
            ("last_probe_at", NTs),
            ("last_probe_ok", NBool),
            ("last_probe_error", NText),
            ("last_probe_ms", NInt),
            ("created_at", Ts),
        ],
    },
    Table {
        name: "model_chains",
        scope: Scope::SiteOrNull,
        cols: &[
            ("id", Text),
            ("name", Text),
            ("role", Text),
            ("site_id", NText),
            ("active", Bool),
            ("notes", Text),
            ("created_at", Ts),
            ("updated_at", Ts),
        ],
    },
    Table {
        name: "model_chain_rungs",
        scope: Scope::Global,
        cols: &[
            ("id", Text),
            ("chain_id", Text),
            ("model_id", Text),
            ("position", Int),
            ("timeout_ms", NInt),
            ("max_retries", NInt),
            ("enabled", Bool),
        ],
    },
    Table {
        name: "model_call_log",
        scope: Scope::Global,
        cols: &[
            ("id", Int),
            ("model_row_id", NText),
            ("chain_id", NText),
            ("provider", NText),
            ("role", Text),
            ("rung", Int),
            ("ok", Bool),
            ("status_code", NInt),
            ("latency_ms", NInt),
            ("tokens_in", NInt),
            ("tokens_out", NInt),
            ("error", NText),
            ("called_at", Ts),
        ],
    },
];

pub fn find(name: &str) -> Option<&'static Table> {
    TABLES.iter().find(|t| t.name == name)
}
