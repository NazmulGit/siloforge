//! `GET /dashboard/:siteId`: all counters (three concurrent reads), health score 0..=100.

use axum::Json;
use axum::extract::{Path, State};
use serde_json::{Map, Value, json};
use sf_store::err;

use super::common::{
    JOB_API_COLS, JOB_API_JSON, count, guard_user_site, load_site, query_json, round2, v1_collation,
};
use crate::auth::User;
use crate::{ApiError, ApiResult, AppState};

/// v1 formula: `100·(1 − .5·orphanRatio − .2·deadEndRatio − .3·(1 − min(1, avgLinks/5)))`, clamped.
pub fn health_score(posts: i64, orphans: i64, dead_ends: i64, live_links: i64) -> (i64, f64) {
    let p = posts as f64;
    let (avg, orphan_ratio, dead_ratio) = if posts > 0 {
        (
            live_links as f64 / p,
            orphans as f64 / p,
            dead_ends as f64 / p,
        )
    } else {
        (0.0, 0.0, 0.0)
    };
    let density = (avg / 5.0).min(1.0);
    let raw = 100.0 * (1.0 - 0.5 * orphan_ratio - 0.2 * dead_ratio - 0.3 * (1.0 - density));
    let score = if raw.is_finite() {
        (raw.round() as i64).clamp(0, 100)
    } else {
        0
    };
    (score, avg)
}

/// Host of an outbound link exactly as v1's SQL derived it: strip an ASCII-case-insensitive
/// `https://` / `http://` prefix, cut at the first `/`, then at the first `:`, ASCII-lowercase.
pub fn outbound_host(url: &str) -> String {
    let has = |p: &str| {
        url.get(..p.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(p))
    };
    let rest = if has("https://") {
        url.get(8..).unwrap_or("")
    } else if has("http://") {
        url.get(7..).unwrap_or("")
    } else {
        url
    };
    let hp = rest.split('/').next().unwrap_or("");
    hp.split(':').next().unwrap_or("").to_ascii_lowercase()
}

/// Distinct non-empty outbound hosts: distinct urls come from the covering
/// `link_health_site_ext_idx`; the host parsing is done in Rust (it was ~28 ms of SQL string
/// functions over ~12k external links on the largest site).
fn outbound_domains(c: &rusqlite::Connection, site_id: &str) -> sf_core::Result<i64> {
    let mut stmt = c
        .prepare(
            "SELECT DISTINCT requested_url FROM link_health WHERE site_id=?1 AND scope='external'",
        )
        .map_err(err)?;
    let mut rows = stmt.query([site_id]).map_err(err)?;
    let mut hosts = std::collections::HashSet::new();
    while let Some(r) = rows.next().map_err(err)? {
        if let rusqlite::types::ValueRef::Text(t) = r.get_ref(0).map_err(err)? {
            let host = outbound_host(&String::from_utf8_lossy(t));
            if !host.is_empty() {
                hosts.insert(host);
            }
        }
    }
    Ok(i64::try_from(hosts.len()).unwrap_or(i64::MAX))
}

fn cannibalization(c: &rusqlite::Connection, site_id: &str) -> sf_core::Result<Vec<Value>> {
    v1_collation(c)?;
    query_json(
        c,
        "WITH f AS (SELECT DISTINCT anchor, target_post_id FROM link_edges
                     WHERE site_id=?1 AND anchor<>'' AND anchor_type IN ('exact','partial')),
              conflicting AS (SELECT anchor FROM f GROUP BY anchor HAVING count(*) > 1)
         SELECT a.anchor AS anchor,
                a.target_post_id AS target_a, ta.url AS target_a_url, ta.title AS target_a_title,
                b.target_post_id AS target_b, tb.url AS target_b_url, tb.title AS target_b_title
           FROM conflicting k
           JOIN f a ON a.anchor = k.anchor
           JOIN f b ON b.anchor = k.anchor AND a.target_post_id < b.target_post_id
           JOIN posts ta ON ta.id = a.target_post_id
           JOIN posts tb ON tb.id = b.target_post_id
          ORDER BY a.anchor COLLATE V1, a.target_post_id, b.target_post_id
          LIMIT 20",
        &[&site_id],
        &[],
        &[],
    )
}

pub async fn get(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &site_id).await?;
    let key = st.secret_key.clone();
    // Three independent reads on separate pooled connections: counters/lists, cannibalization
    // warnings, outbound domains.
    let cannibal = {
        let sid = site_id.clone();
        st.db.read(move |c| cannibalization(c, &sid))
    };
    let domains = {
        let sid = site_id.clone();
        st.db.read(move |c| outbound_domains(c, &sid))
    };
    let main = st.db.read(move |c| {
        v1_collation(c)?;
        let Some(site) = load_site(c, &site_id)? else {
            return Ok(None);
        };
        let sid: &dyn rusqlite::ToSql = &site_id;
        let (posts, orphans, dead_ends) = c
            .query_row(
                "SELECT count(*), coalesce(sum(is_orphan),0), coalesce(sum(is_dead_end),0)
                   FROM posts WHERE site_id=?1",
                [sid],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
            )
            .map_err(err)?;
        let live = count(
            c,
            "SELECT count(*) FROM link_edges WHERE site_id=?1 AND (status='applied' OR origin='existing')",
            &[sid],
        )?;
        let mut by_status = Map::new();
        let mut stmt = c
            .prepare("SELECT status, count(*) FROM link_edges WHERE site_id=?1 GROUP BY status")
            .map_err(err)?;
        let rows = stmt
            .query_map([sid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(err)?;
        for row in rows {
            let (s, n) = row.map_err(err)?;
            by_status.insert(s, Value::from(n));
        }
        let clusters = count(c, "SELECT count(*) FROM clusters WHERE site_id=?1", &[sid])?;
        let recent_jobs = query_json(
            c,
            &format!(
                "SELECT {JOB_API_COLS} FROM job_runs WHERE site_id=?1 ORDER BY created_at DESC, id DESC LIMIT 10"
            ),
            &[sid],
            JOB_API_JSON,
            &[],
        )?;
        let opportunities = count(
            c,
            "SELECT count(*) FROM link_edges WHERE site_id=?1 AND status='suggested'",
            &[sid],
        )?;
        let duplication = count(
            c,
            "SELECT coalesce(sum(cnt - 1), 0) FROM (
               SELECT count(*) AS cnt FROM link_edges
                WHERE site_id=?1 AND status IN ('applied','approved','suggested')
                GROUP BY source_post_id, target_post_id HAVING count(*) > 1)",
            &[sid],
        )?;
        let orphan_posts = query_json(
            c,
            "SELECT id AS post_id, url, title FROM posts WHERE site_id=?1 AND is_orphan=1
              ORDER BY title COLLATE V1 ASC, url COLLATE V1 ASC LIMIT 20",
            &[sid],
            &[],
            &[],
        )?;
        // Served by the partial index `link_health_http_idx` (same expression).
        let http_links = count(
            c,
            "SELECT count(*) FROM link_health WHERE site_id=?1 AND lower(href) LIKE 'http://%'",
            &[sid],
        )?;
        // G5: `http_links` / `outbound_domains` come from link_health; tell "0" from "never
        // checked" (additive field, null until the first link-health run).
        let link_health_checked_at: Option<String> = c
            .query_row(
                "SELECT max(link_health_checked_at) FROM posts WHERE site_id=?1",
                [sid],
                |r| r.get(0),
            )
            .map_err(err)?;
        let (score, avg) = health_score(posts, orphans, dead_ends, live);
        Ok(Some(json!({
            "site": site.to_json(&key, false),
            "health_score": score,
            "posts": posts,
            "orphans": orphans,
            "dead_ends": dead_ends,
            "avg_links_per_post": round2(avg),
            "edges_by_status": by_status,
            "clusters": clusters,
            "recent_jobs": recent_jobs,
            "opportunities": opportunities,
            "http_links": http_links,
            "link_health_checked_at": link_health_checked_at,
            "link_duplication": duplication,
            "orphan_posts": orphan_posts,
        })))
    });
    let (main, cannibal, domains) = tokio::try_join!(main, cannibal, domains)?;
    let mut out = main.ok_or_else(|| ApiError::not_found("site not found"))?;
    if let Some(o) = out.as_object_mut() {
        o.insert("cannibalization_warnings".into(), Value::Array(cannibal));
        o.insert("outbound_domains".into(), Value::from(domains));
    }
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::{health_score, outbound_domains, outbound_host};

    #[test]
    fn health_is_bounded() {
        assert_eq!(health_score(0, 0, 0, 0).0, 70);
        assert_eq!(health_score(10, 0, 0, 50).0, 100);
        assert_eq!(health_score(10, 10, 10, 0).0, 0);
        assert_eq!(health_score(1, 5, 5, 0).0, 0);
        let (s, avg) = health_score(4, 1, 0, 6);
        assert!((0..=100).contains(&s) && (avg - 1.5).abs() < 1e-9);
    }

    /// Rust host parsing equals the SQL expression it replaced (v1 `outbound_domains`).
    #[test]
    fn outbound_domains_match_sql() {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        sf_store::migrate::run(&mut c).unwrap();
        c.execute(
            "INSERT INTO sites(id,domain,api_key) VALUES('s','a.example.com','k')",
            [],
        )
        .unwrap();
        let urls = [
            "https://Example.com/a",
            "http://example.com:8080/x",
            "HTTPS://EXAMPLE.com",
            "HtTp://other.org/",
            "ftp://files.net/a",
            "//cdn.net/x",
            "",
            "https://",
            "http:///path",
            "https://user@host.io:9/",
            "https://Ünï.com/a",
            "mailto:x@y.z",
            "https://例え.jp/パス",
            "https:/broken.com",
        ];
        for (i, u) in urls.iter().enumerate() {
            for scope in ["external", "internal"] {
                c.execute(
                    "INSERT INTO link_health(id,site_id,href,requested_url,scope,anchor)
                     VALUES(?1,'s','h',?2,?3,'a')",
                    rusqlite::params![format!("{scope}{i}"), u, scope],
                )
                .unwrap();
            }
        }
        let hosts = |sql_only: bool| -> Vec<String> {
            let mut stmt = c
                .prepare(
                    "WITH a AS (SELECT CASE WHEN lower(requested_url) LIKE 'https://%' THEN substr(requested_url, 9)
                                        WHEN lower(requested_url) LIKE 'http://%' THEN substr(requested_url, 8)
                                        ELSE requested_url END AS rest, requested_url AS u
                              FROM link_health WHERE site_id='s' AND scope='external'),
                      b AS (SELECT CASE WHEN instr(rest, '/') > 0 THEN substr(rest, 1, instr(rest, '/') - 1)
                                        ELSE rest END AS hp, u FROM a)
                 SELECT lower(CASE WHEN instr(hp, ':') > 0 THEN substr(hp, 1, instr(hp, ':') - 1)
                                   ELSE hp END), u FROM b ORDER BY u",
                )
                .unwrap();
            stmt.query_map([], |r| {
                let (h, u): (String, String) = (r.get(0)?, r.get(1)?);
                Ok(if sql_only { h } else { outbound_host(&u) })
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        };
        assert_eq!(hosts(false), hosts(true));
        let sql_count: i64 = c
            .query_row(
                "WITH a AS (SELECT CASE WHEN lower(requested_url) LIKE 'https://%' THEN substr(requested_url, 9)
                                        WHEN lower(requested_url) LIKE 'http://%' THEN substr(requested_url, 8)
                                        ELSE requested_url END AS rest
                              FROM link_health WHERE site_id='s' AND scope='external'),
                      b AS (SELECT CASE WHEN instr(rest, '/') > 0 THEN substr(rest, 1, instr(rest, '/') - 1)
                                        ELSE rest END AS hp FROM a),
                      h AS (SELECT lower(CASE WHEN instr(hp, ':') > 0 THEN substr(hp, 1, instr(hp, ':') - 1)
                                              ELSE hp END) AS host FROM b)
                 SELECT count(DISTINCT nullif(host, '')) FROM h",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(outbound_domains(&c, "s").unwrap(), sql_count);
        assert_eq!(outbound_domains(&c, "none").unwrap(), 0);
    }
}
