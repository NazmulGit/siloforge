//! Embedded, forward-only schema migrations with a ledger (`schema_version`).
//! Add a file under `migrations/` and append it to `MIGRATIONS`; never edit an applied file.

use rusqlite::Connection;
use sf_core::{Error, Result};

const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "0001_init", include_str!("../migrations/0001_init.sql")),
    (
        2,
        "0002_raw_content",
        include_str!("../migrations/0002_raw_content.sql"),
    ),
    (3, "0003_perf", include_str!("../migrations/0003_perf.sql")),
    (
        4,
        "0004_token_parent",
        include_str!("../migrations/0004_token_parent.sql"),
    ),
    (
        5,
        "0005_reliability",
        include_str!("../migrations/0005_reliability.sql"),
    ),
    (
        6,
        "0006_event_log",
        include_str!("../migrations/0006_event_log.sql"),
    ),
    (
        7,
        "0007_link_runs",
        include_str!("../migrations/0007_link_runs.sql"),
    ),
    (
        8,
        "0008_ai_tiers",
        include_str!("../migrations/0008_ai_tiers.sql"),
    ),
    (9, "0009_saas", include_str!("../migrations/0009_saas.sql")),
];

pub fn current_version(conn: &Connection) -> Result<i64> {
    let exists: bool = conn
        .query_row(
            "SELECT count(*) > 0 FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |r| r.get(0),
        )
        .map_err(crate::err)?;
    if !exists {
        return Ok(0);
    }
    conn.query_row(
        "SELECT ifnull(max(version),0) FROM schema_version",
        [],
        |r| r.get(0),
    )
    .map_err(crate::err)
}

/// Latest schema version this binary knows.
pub fn latest_version() -> i64 {
    MIGRATIONS.last().map(|m| m.0).unwrap_or(0)
}

/// Checksum of a migration file. Line endings are normalised so a CRLF checkout of the same
/// file is not reported as edited.
fn checksum(sql: &str) -> String {
    sf_core::crypto::sha256_hex(sql.replace("\r\n", "\n").as_bytes())
}

/// Names of applied migrations whose stored checksum differs from the embedded file (R19).
pub fn checksum_mismatches(conn: &Connection) -> Result<Vec<String>> {
    if current_version(conn)? == 0 {
        return Ok(Vec::new());
    }
    let mut st = conn
        .prepare("SELECT version, checksum FROM schema_version")
        .map_err(crate::err)?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .map_err(crate::err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(crate::err)?;
    let mut bad = Vec::new();
    for (v, sum) in rows {
        if let Some((_, name, sql)) = MIGRATIONS.iter().find(|m| m.0 == v) {
            // older ledgers stored the hash of the raw (possibly CRLF) bytes
            if sum != checksum(sql) && sum != sf_core::crypto::sha256_hex(sql.as_bytes()) {
                bad.push((*name).to_string());
            }
        }
    }
    Ok(bad)
}

pub fn run(conn: &mut Connection) -> Result<()> {
    run_until(conn, i64::MAX)
}

/// Applies migrations up to and including `max` (tests build older schemas with it, e.g. to
/// prove an upgrade back-fill).
pub fn run_until(conn: &mut Connection, max: i64) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
           version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL,
           applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')))",
    )
    .map_err(crate::err)?;
    let current = current_version(conn)?;
    match checksum_mismatches(conn) {
        Ok(bad) if !bad.is_empty() => tracing::warn!(
            migrations = ?bad,
            "applied migration files were edited after they ran (checksum mismatch); the schema may differ from the code"
        ),
        Ok(_) => {}
        Err(e) => tracing::debug!(error = %e, "migration checksum check skipped"),
    }
    if current > latest_version() {
        return Err(Error::internal(format!(
            "database schema v{current} is newer than this binary (v{}); upgrade SiloForge",
            latest_version()
        )));
    }
    for (version, name, sql) in MIGRATIONS {
        if *version <= current || *version > max {
            continue;
        }
        let checksum = checksum(sql);
        let tx = conn.transaction().map_err(crate::err)?;
        tx.execute_batch(sql)
            .map_err(|e| Error::Db(format!("migration {name}: {e}")))?;
        tx.execute(
            "INSERT INTO schema_version(version,name,checksum) VALUES(?1,?2,?3)",
            rusqlite::params![version, name, checksum],
        )
        .map_err(crate::err)?;
        tx.commit().map_err(crate::err)?;
        tracing::info!(version, name, "applied migration");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(c: &Connection, sql: &str) -> String {
        let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = stmt
            .query_map(["s"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows.join(" | ")
    }

    #[test]
    fn perf_indexes_cover_heavy_report_scans() {
        let mut c = Connection::open_in_memory().unwrap();
        run(&mut c).unwrap();
        assert_eq!(current_version(&c).unwrap(), latest_version());
        // re-running is a no-op
        run(&mut c).unwrap();
        let cases = [
            (
                "SELECT id, url, title, cluster_id, is_orphan, pagerank FROM posts
                  WHERE site_id=?1 ORDER BY pagerank DESC, id ASC LIMIT 500",
                "COVERING INDEX posts_site_rank_idx",
            ),
            (
                "SELECT count(*), sum(is_orphan), sum(is_dead_end) FROM posts WHERE site_id=?1",
                "COVERING INDEX",
            ),
            (
                "SELECT count(*), sum(link_health_checked_at IS NOT NULL), sum(has_breadcrumbs),
                        sum(has_breadcrumb_schema) FROM posts WHERE site_id=?1",
                "COVERING INDEX",
            ),
            (
                "SELECT count(*), sum(scope='internal'), sum(is_broken), sum(failure_kind='dns'),
                        sum(redirect_hops>2), sum(is_ugc), max(checked_at)
                   FROM link_health WHERE site_id=?1",
                "COVERING INDEX link_health_site_summary_idx",
            ),
            (
                "SELECT count(*) FROM link_health WHERE site_id=?1 AND lower(href) LIKE 'http://%'",
                "link_health_http_idx",
            ),
            (
                "SELECT count(*) FROM (SELECT count(*) FROM link_edges
                   WHERE site_id=?1 AND status IN ('applied','approved','suggested')
                   GROUP BY source_post_id, target_post_id)",
                "COVERING INDEX link_edges_site_pair_idx",
            ),
            (
                "SELECT anchor, count(*) FROM link_edges WHERE site_id=?1 AND anchor<>''
                   AND status IN ('suggested','approved','applied') GROUP BY anchor",
                "COVERING INDEX link_edges_site_anchor_idx",
            ),
        ];
        for (sql, want) in cases {
            let p = plan(&c, sql);
            assert!(p.contains(want), "{sql}\n  plan: {p}\n  want: {want}");
        }
        // /edges list pages walk the score index instead of sorting every edge of the site.
        let p = plan(
            &c,
            "SELECT e.*, sp.url, tp.url FROM link_edges e
               JOIN posts sp ON sp.id = e.source_post_id JOIN posts tp ON tp.id = e.target_post_id
              WHERE e.site_id=?1 AND (NULL IS NULL OR e.status=NULL)
              ORDER BY e.composite_score DESC, e.created_at ASC, e.id ASC LIMIT 100 OFFSET 1000",
        );
        assert!(
            p.contains("link_edges_site_score_idx") && !p.contains("ORDER BY"),
            "edges plan: {p}"
        );
    }

    /// R5: every foreign-key child column is the leading column of some index, so deleting a
    /// parent row (paragraphs, posts, snapshots, job_runs, ...) never full-scans the child table.
    #[test]
    fn every_foreign_key_child_column_is_indexed() {
        let mut c = Connection::open_in_memory().unwrap();
        run(&mut c).unwrap();
        let strings = |sql: &str| -> Vec<String> {
            c.prepare(sql)
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let tables = strings(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        );
        let mut missing = Vec::new();
        for t in &tables {
            let fks = strings(&format!(
                "SELECT \"from\" FROM pragma_foreign_key_list('{t}')"
            ));
            let leading = strings(&format!(
                "SELECT ii.name FROM pragma_index_list('{t}') il
                   JOIN pragma_index_info(il.name) ii ON ii.seqno = 0"
            ));
            for col in fks {
                if !leading.contains(&col) {
                    missing.push(format!("{t}.{col}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "FK child columns without an index: {missing:?}"
        );
    }

    /// R19: an edited, already-applied migration is detected.
    #[test]
    fn checksum_mismatch_is_reported() {
        let mut c = Connection::open_in_memory().unwrap();
        run(&mut c).unwrap();
        assert!(checksum_mismatches(&c).unwrap().is_empty());
        c.execute("UPDATE schema_version SET checksum='x' WHERE version=1", [])
            .unwrap();
        assert_eq!(
            checksum_mismatches(&c).unwrap(),
            vec!["0001_init".to_string()]
        );
        // still opens (warning only)
        run(&mut c).unwrap();
    }

    /// 0007 rebuilds site_schedules (CHECK gains 'link_run') without losing rows.
    #[test]
    fn v8_tiers_existing_chains_and_call_log() {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        c.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, name TEXT NOT NULL,
               checksum TEXT NOT NULL, applied_at TEXT NOT NULL DEFAULT 'x')",
        )
        .unwrap();
        for (v, name, sql) in MIGRATIONS.iter().filter(|m| m.0 < 8) {
            c.execute_batch(sql).unwrap();
            c.execute(
                "INSERT INTO schema_version(version,name,checksum) VALUES(?1,?2,?3)",
                rusqlite::params![v, name, checksum(sql)],
            )
            .unwrap();
        }
        c.execute_batch(
            "INSERT INTO model_chains(id,name,role,active) VALUES('c1','Main','chat',1);
             INSERT INTO model_call_log(provider,role,ok,tokens_in) VALUES('p','chat',1,5);",
        )
        .unwrap();
        run(&mut c).unwrap();
        let tier: String = c
            .query_row("SELECT tier FROM model_chains WHERE id='c1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(tier, "main");
        let (hit, items): (i64, i64) = c
            .query_row("SELECT cache_hit, items FROM model_call_log", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((hit, items), (0, 1));
        // one active chain per (role, tier, scope)
        assert!(
            c.execute(
                "INSERT INTO model_chains(id,name,role,active,tier) VALUES('c2','Low','chat',1,'low')",
                []
            )
            .is_ok()
        );
        assert!(
            c.execute(
                "INSERT INTO model_chains(id,name,role,active,tier) VALUES('c3','Main 2','chat',1,'main')",
                []
            )
            .is_err()
        );
        assert!(
            c.execute(
                "INSERT INTO model_chains(id,name,role,active,tier) VALUES('c4','x','chat',0,'ultra')",
                []
            )
            .is_err()
        );
    }

    /// 0009 back-fill: Default workspace for admin/unowned sites, personal workspaces for
    /// non-admin users, tokens follow their user, existing users verified.
    #[test]
    fn v9_saas_backfill() {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        c.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, name TEXT NOT NULL,
               checksum TEXT NOT NULL, applied_at TEXT NOT NULL DEFAULT 'x')",
        )
        .unwrap();
        for (v, name, sql) in MIGRATIONS.iter().filter(|m| m.0 < 9) {
            c.execute_batch(sql).unwrap();
            c.execute(
                "INSERT INTO schema_version(version,name,checksum) VALUES(?1,?2,?3)",
                rusqlite::params![v, name, checksum(sql)],
            )
            .unwrap();
        }
        c.execute_batch(
            "INSERT INTO users(id,email,password_hash,role,created_at) VALUES
               ('a1','a1@example.com','x','admin','2026-01-01T00:00:00Z'),
               ('a2','a2@example.com','x','admin','2026-02-01T00:00:00Z'),
               ('u1','u1@example.com','x','user','2026-03-01T00:00:00Z');
             INSERT INTO sites(id,domain,api_key,user_id) VALUES
               ('s1','one.example','k1','a1'),('s2','two.example','k2',NULL),('s3','three.example','k3','u1');
             INSERT INTO api_tokens(id,user_id,name,token_hash,prefix) VALUES
               ('t1','a1','hermes','h1','p'),('t2','u1','mine','h2','p');",
        )
        .unwrap();
        run(&mut c).unwrap();
        let q = |sql: &str| -> String { c.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            q("SELECT owner_user_id FROM workspaces WHERE id='default'"),
            "a1"
        );
        assert_eq!(
            q("SELECT plan_id FROM workspaces WHERE id='default'"),
            "unlimited"
        );
        assert_eq!(q("SELECT workspace_id FROM sites WHERE id='s1'"), "default");
        assert_eq!(q("SELECT workspace_id FROM sites WHERE id='s2'"), "default");
        assert_eq!(q("SELECT workspace_id FROM sites WHERE id='s3'"), "ws-u1");
        assert_eq!(
            q("SELECT workspace_id FROM api_tokens WHERE id='t1'"),
            "default"
        );
        assert_eq!(
            q("SELECT workspace_id FROM api_tokens WHERE id='t2'"),
            "ws-u1"
        );
        assert_eq!(
            q("SELECT role FROM workspace_members WHERE workspace_id='default' AND user_id='a1'"),
            "owner"
        );
        assert_eq!(
            q("SELECT role FROM workspace_members WHERE workspace_id='default' AND user_id='a2'"),
            "admin"
        );
        assert_eq!(
            q("SELECT role FROM workspace_members WHERE workspace_id='ws-u1' AND user_id='u1'"),
            "owner"
        );
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM workspace_members WHERE workspace_id='default' AND user_id='u1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "non-admins are not members of Default");
        let unverified: i64 = c
            .query_row(
                "SELECT count(*) FROM users WHERE email_verified_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unverified, 0);
        let plans: i64 = c
            .query_row("SELECT count(*) FROM plans", [], |r| r.get(0))
            .unwrap();
        assert_eq!(plans, 4);
    }

    #[test]
    fn v7_keeps_schedules_and_allows_link_run() {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        c.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, name TEXT NOT NULL,
               checksum TEXT NOT NULL, applied_at TEXT NOT NULL DEFAULT 'x')",
        )
        .unwrap();
        for (v, name, sql) in MIGRATIONS.iter().filter(|m| m.0 < 7) {
            c.execute_batch(sql).unwrap();
            c.execute(
                "INSERT INTO schema_version(version,name,checksum) VALUES(?1,?2,?3)",
                rusqlite::params![v, name, checksum(sql)],
            )
            .unwrap();
        }
        c.execute_batch(
            "INSERT INTO sites(id,domain,api_key) VALUES('s1','a.com','k');
             INSERT INTO site_schedules(id,site_id,job_kind,cron,payload) VALUES('r1','s1','audit','0 3 * * *','{\"x\":1}');",
        )
        .unwrap();
        assert!(
            c.execute(
                "INSERT INTO site_schedules(id,site_id,job_kind,cron) VALUES('r2','s1','link_run','* * * * *')",
                []
            )
            .is_err()
        );
        run(&mut c).unwrap();
        assert_eq!(current_version(&c).unwrap(), latest_version());
        let (kind, payload): (String, String) = c
            .query_row(
                "SELECT job_kind, payload FROM site_schedules WHERE id='r1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((kind.as_str(), payload.as_str()), ("audit", "{\"x\":1}"));
        c.execute(
            "INSERT INTO site_schedules(id,site_id,job_kind,cron) VALUES('r2','s1','link_run','* * * * *')",
            [],
        )
        .unwrap();
        assert!(
            c.execute(
                "INSERT INTO site_schedules(id,site_id,job_kind,cron) VALUES('r3','s1','bogus','* * * * *')",
                []
            )
            .is_err()
        );
        c.execute(
            "INSERT INTO link_runs(id,site_id,status) VALUES('lr1','s1','ready')",
            [],
        )
        .unwrap();
        assert!(
            c.execute(
                "INSERT INTO link_runs(id,site_id,status) VALUES('lr2','s1','weird')",
                []
            )
            .is_err()
        );
        // deleting the site cascades the schedule and the run
        c.execute("DELETE FROM sites WHERE id='s1'", []).unwrap();
        let n: i64 = c
            .query_row(
                "SELECT (SELECT count(*) FROM site_schedules) + (SELECT count(*) FROM link_runs)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }
}
