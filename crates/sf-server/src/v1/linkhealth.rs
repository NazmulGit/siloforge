//! `POST /sites/:siteId/link-health` and `GET /reports/link-health/:siteId` (JSON + CSV).

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use sf_core::types::JobKind;
use sf_store::err;

use super::common::{
    JsonBody, Params, csv_line, csv_response, enqueue, get_site, guard_user_site, int_param,
    query_json, safe_filename_part, str_param, v1_collate, v1_collation, v1_primary_key,
};
use super::reports::CSV_MAX_ROWS;
use crate::auth::User;
use crate::{ApiResult, AppState};

const LINK_COLUMNS: &str = "lh.id, lh.source_post_id, p.url AS source_url, p.title AS source_title,
  lh.href, lh.requested_url, lh.resolved_url, lh.scope, lh.status_code, lh.is_broken,
  lh.redirect_hops, lh.redirect_chain, lh.has_redirect_loop, lh.has_temp_redirect,
  lh.rel, lh.is_nofollow, lh.is_sponsored, lh.is_ugc, lh.anchor, lh.placement,
  lh.error, lh.failure_kind, lh.is_unverified, lh.checked_at";
const LINK_JSON: &[&str] = &["redirect_chain"];
const LINK_BOOLS: &[&str] = &[
    "is_broken",
    "has_redirect_loop",
    "has_temp_redirect",
    "is_nofollow",
    "is_sponsored",
    "is_ugc",
    "is_unverified",
];

/// Optional body of `POST /sites/:id/link-health` (v2; an empty body is `{}` like v1).
#[derive(serde::Deserialize, Default)]
pub struct RunBody {
    /// Check internal links only (skip external hosts).
    #[serde(default)]
    internal_only: Option<bool>,
}

/// Job payload: `{}` (v1) or `{internal_only}` when the caller set it.
fn run_payload(b: &RunBody) -> Value {
    match b.internal_only {
        Some(v) => json!({ "internal_only": v }),
        None => json!({}),
    }
}

pub async fn run(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
    JsonBody(b): JsonBody<Option<RunBody>>,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &site_id).await?;
    let payload = run_payload(&b.unwrap_or_default());
    enqueue(&st, &u, JobKind::LinkHealth, &site_id, payload).await
}

fn b(v: bool) -> &'static str {
    if v { "true" } else { "false" }
}

pub async fn report(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
    q: Params,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &site_id).await?;
    if str_param(&q, "format") == Some("csv") {
        let site = get_site(&st, &site_id).await?;
        let body = st.db.read(move |c| csv_body(c, &site_id)).await?;
        return Ok(csv_response(
            &format!("link-health-{}.csv", safe_filename_part(&site.domain)),
            body,
        ));
    }
    let limit = int_param(&q, "limit", 100, 1, 1000);
    let offset = int_param(&q, "offset", 0, 0, i64::MAX / 2);
    // Four reads on separate pooled connections: the summary scan in two halves of its covering
    // index, the large nofollow list, and the small lists + breadcrumbs together.
    let half = |upper: bool| {
        let db = st.db.clone();
        let sid = site_id.clone();
        async move { db.read(move |c| tally(c, &sid, Some(upper))).await }
    };
    let nofollow = {
        let sid = site_id.clone();
        st.db.read(move |c| {
            v1_collation(c)?;
            list_page(c, &sid, NOFOLLOW, limit, offset)
        })
    };
    let rest = {
        let sid = site_id.clone();
        st.db.read(move |c| {
            v1_collation(c)?;
            let mut lists = Vec::with_capacity(SMALL_LISTS.len());
            for l in SMALL_LISTS {
                lists.push(list_page(c, &sid, l, limit, offset)?);
            }
            Ok((lists, breadcrumbs(c, &sid, limit, offset)?))
        })
    };
    let (mut summary, upper, nofollow, (lists, breadcrumbs)) =
        tokio::try_join!(half(false), half(true), nofollow, rest)?;
    summary.merge(upper);
    let mut lists = lists.into_iter();
    let mut next = || Value::Array(lists.next().unwrap_or_default());
    let (broken, redirects, unverified, blocked, server_errors) =
        (next(), next(), next(), next(), next());
    let v = json!({
        "summary": summary.to_json(),
        "broken": broken, "redirects": redirects, "nofollow": nofollow,
        "unverified": unverified, "blocked": blocked, "server_errors": server_errors,
        "breadcrumbs": breadcrumbs,
        "limit": limit, "offset": offset,
    });
    Ok(Json(v).into_response())
}

/// (filter, order) of one report list.
type ListSpec = (&'static str, &'static str);

const NOFOLLOW: ListSpec = (
    "lh.is_nofollow=1",
    "(lh.scope='internal') DESC, p.url COLLATE V1 ASC, lh.href COLLATE V1 ASC",
);

/// broken, redirects, unverified, blocked, server_errors (all small subsets with partial indexes).
const SMALL_LISTS: [ListSpec; 5] = [
    (
        "lh.is_broken=1",
        "lh.status_code DESC NULLS LAST, p.url COLLATE V1 ASC, lh.href COLLATE V1 ASC",
    ),
    (
        "lh.redirect_hops>0",
        "lh.redirect_hops DESC, p.url COLLATE V1 ASC, lh.href COLLATE V1 ASC",
    ),
    (
        "lh.is_unverified=1",
        "lh.failure_kind COLLATE V1 ASC, p.url COLLATE V1 ASC, lh.href COLLATE V1 ASC",
    ),
    (
        "lh.failure_kind='blocked'",
        "p.url COLLATE V1 ASC, lh.href COLLATE V1 ASC",
    ),
    (
        "lh.failure_kind='server_error'",
        "lh.status_code ASC, p.url COLLATE V1 ASC, lh.href COLLATE V1 ASC",
    ),
];

fn list_page(
    c: &rusqlite::Connection,
    site_id: &str,
    (wh, order): ListSpec,
    limit: i64,
    offset: i64,
) -> sf_core::Result<Vec<Value>> {
    query_json(
        c,
        &list_sql(wh, order),
        &[&site_id, &limit, &offset],
        LINK_JSON,
        LINK_BOOLS,
    )
}

/// One report list page in two steps: pick the page's rowids from the (partial, covering) index
/// plus the post url, then read only those rows. `lh.rowid` makes ties deterministic.
fn list_sql(wh: &str, order: &str) -> String {
    format!(
        "WITH pick AS MATERIALIZED (
           SELECT lh.rowid AS rid FROM link_health lh LEFT JOIN posts p ON p.id = lh.source_post_id
            WHERE lh.site_id=?1 AND {wh} ORDER BY {order}, lh.rowid LIMIT ?2 OFFSET ?3)
         SELECT {LINK_COLUMNS} FROM pick
           JOIN link_health lh ON lh.rowid = pick.rid
           LEFT JOIN posts p ON p.id = lh.source_post_id
          ORDER BY {order}, lh.rowid"
    )
}

const CSV_HEADER: &str = "Source URL,Href,Requested URL,Resolved URL,Scope,Status,Failure Kind,Broken,\
     Unverified,Redirect Hops,Loop,Temp Redirect,Rel,Nofollow,Anchor,Placement,Error";

/// CSV export, `ORDER BY is_broken DESC, redirect_hops DESC, p.url COLLATE V1, href COLLATE V1`
/// (+ rowid for ties). Done in SQL this took ~650 ms on the largest site (46k links): a posts
/// table lookup per link (posts rows are tens of KB) plus a V1-collated sort of wide rows. Here the
/// site's post urls come from the covering `posts_site_rank_idx`, every link is rendered in one
/// pass, source urls are ranked once per post and links sort by integers, comparing hrefs only
/// within one source post. At most `CSV_MAX_ROWS` lines.
fn csv_body(c: &rusqlite::Connection, site_id: &str) -> sf_core::Result<String> {
    use std::collections::HashMap;
    // Source posts: index -> url, plus id -> index. Posts of other sites (should not happen) are
    // looked up on demand so the result equals the SQL LEFT JOIN.
    let mut urls: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, Option<usize>> = HashMap::new();
    {
        let mut stmt = c
            .prepare("SELECT id, url FROM posts WHERE site_id=?1")
            .map_err(err)?;
        let mut rows = stmt.query([site_id]).map_err(err)?;
        while let Some(r) = rows.next().map_err(err)? {
            by_id.insert(r.get(0).map_err(err)?, Some(urls.len()));
            urls.push(r.get(1).map_err(err)?);
        }
    }
    struct Line {
        rowid: i64,
        broken: i64,
        hops: i64,
        post: Option<usize>,
        href_key: String,
        href: String,
        text: String,
    }
    let mut lines: Vec<Line> = Vec::new();
    let mut stmt = c
        .prepare(&format!(
            "SELECT rowid, source_post_id, {CSV_COLS} FROM link_health WHERE site_id=?1"
        ))
        .map_err(err)?;
    let mut rows = stmt.query([site_id]).map_err(err)?;
    while let Some(r) = rows.next().map_err(err)? {
        let post = match r.get::<_, Option<String>>(1).map_err(err)? {
            None => None,
            Some(id) => match by_id.get(&id) {
                Some(p) => *p,
                None => {
                    let url: Option<String> = c
                        .query_row("SELECT url FROM posts WHERE id=?1", [&id], |x| x.get(0))
                        .map(Some)
                        .or_else(|e| match e {
                            rusqlite::Error::QueryReturnedNoRows => Ok(None),
                            e => Err(e),
                        })
                        .map_err(err)?;
                    let p = url.map(|u| {
                        urls.push(u);
                        urls.len() - 1
                    });
                    by_id.insert(id, p);
                    p
                }
            },
        };
        let src = post.and_then(|i| urls.get(i)).map(String::as_str);
        let href: String = r.get(2).map_err(err)?;
        lines.push(Line {
            rowid: r.get(0).map_err(err)?,
            broken: r.get(2 + 6).map_err(err)?,
            hops: r.get(2 + 8).map_err(err)?,
            post,
            href_key: v1_primary_key(&href),
            text: csv_row(src, r, 2).map_err(err)?,
            href,
        });
    }
    // v1 rank of each post url (equal urls share a rank).
    let mut order: Vec<usize> = (0..urls.len()).collect();
    order
        .sort_by(|&a, &b| v1_collate(urls.get(a).map_or("", |s| s), urls.get(b).map_or("", |s| s)));
    let mut rank = vec![0usize; urls.len()];
    let mut prev: Option<&str> = None;
    let mut current = 0usize;
    for (pos, &i) in order.iter().enumerate() {
        let u = urls.get(i).map(String::as_str);
        if prev != u {
            current = pos;
            prev = u;
        }
        if let Some(slot) = rank.get_mut(i) {
            *slot = current;
        }
    }
    // NULL source url sorts first (SQLite ASC).
    let key = |l: &Line| l.post.and_then(|p| rank.get(p).copied());
    lines.sort_by(|a, b| {
        b.broken
            .cmp(&a.broken)
            .then(b.hops.cmp(&a.hops))
            .then(key(a).cmp(&key(b)))
            .then_with(|| a.href_key.cmp(&b.href_key))
            .then_with(|| v1_collate(&a.href, &b.href))
            .then(a.rowid.cmp(&b.rowid))
    });
    let max = usize::try_from(CSV_MAX_ROWS).unwrap_or(usize::MAX);
    let mut out = String::with_capacity(
        CSV_HEADER.len()
            + lines
                .iter()
                .take(max)
                .map(|l| l.text.len() + 2)
                .sum::<usize>(),
    );
    out.push_str(CSV_HEADER);
    for l in lines.iter().take(max) {
        out.push_str("\r\n");
        out.push_str(&l.text);
    }
    Ok(out)
}

/// `link_health` columns of one CSV line, in [`csv_row`] order.
const CSV_COLS: &str =
    "href, requested_url, resolved_url, scope, status_code, failure_kind, is_broken,
  is_unverified, redirect_hops, has_redirect_loop, has_temp_redirect, rel, is_nofollow, anchor,
  placement, error";

/// One CSV line from the [`CSV_COLS`] starting at column `at` and the source post url.
fn csv_row(source_url: Option<&str>, r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<String> {
    let s = |i: usize| r.get::<_, String>(at + i);
    let opt = |i: usize| -> rusqlite::Result<String> {
        Ok(r.get::<_, Option<String>>(at + i)?.unwrap_or_default())
    };
    let flag =
        |i: usize| -> rusqlite::Result<&'static str> { Ok(b(r.get::<_, i64>(at + i)? != 0)) };
    Ok(csv_line(&[
        source_url.unwrap_or_default(),
        &s(0)?,
        &s(1)?,
        &opt(2)?,
        &s(3)?,
        &r.get::<_, Option<i64>>(at + 4)?
            .map(|n| n.to_string())
            .unwrap_or_default(),
        &s(5)?,
        flag(6)?,
        flag(7)?,
        &r.get::<_, i64>(at + 8)?.to_string(),
        flag(9)?,
        flag(10)?,
        &s(11)?,
        flag(12)?,
        &s(13)?,
        &s(14)?,
        &opt(15)?,
    ]))
}

/// `/reports/link-health` summary counters (all links of the site).
#[cfg(test)]
fn summary(c: &rusqlite::Connection, site_id: &str) -> sf_core::Result<Value> {
    Ok(tally(c, site_id, None)?.to_json())
}

/// Summary counters of the site's links, or of one half of them by `scope` (`Some(false)`:
/// `scope <= 'external'`, `Some(true)`: the rest; both are ranges of the covering
/// `link_health_site_summary_idx`, so the handler scans the halves concurrently and merges).
/// SQL groups the rows by the nine flag columns in index order (no sorter, a few dozen groups)
/// and the groups are tallied in Rust: twenty `sum(expr)` aggregates per row cost ~2.5x more on
/// the largest site (46k links).
fn tally(c: &rusqlite::Connection, site_id: &str, upper: Option<bool>) -> sf_core::Result<Tally> {
    use rusqlite::types::ValueRef;
    // SQLite numeric affinity, as `sum()` / comparisons would see it (NULL counts as 0).
    fn int(v: ValueRef<'_>) -> i64 {
        match v {
            ValueRef::Integer(i) => i,
            ValueRef::Real(f) if f.is_finite() => f as i64,
            ValueRef::Text(t) => std::str::from_utf8(t)
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok())
                .filter(|f| f.is_finite())
                .map(|f| f as i64)
                .unwrap_or(0),
            _ => 0,
        }
    }
    fn text(v: ValueRef<'_>) -> &[u8] {
        match v {
            ValueRef::Text(t) => t,
            _ => b"",
        }
    }
    let range = match upper {
        None => "",
        Some(false) => "AND scope <= 'external'",
        Some(true) => "AND scope > 'external'",
    };
    let mut t = Tally::default();
    let mut stmt = c
        .prepare_cached(&format!(
            "SELECT scope, is_broken, is_unverified, failure_kind, redirect_hops, has_temp_redirect,
                    is_nofollow, is_sponsored, is_ugc, count(*), max(checked_at)
               FROM link_health WHERE site_id=?1 {range}
              GROUP BY scope, is_broken, is_unverified, failure_kind, redirect_hops,
                       has_temp_redirect, is_nofollow, is_sponsored, is_ugc"
        ))
        .map_err(err)?;
    let mut rows = stmt.query([site_id]).map_err(err)?;
    while let Some(r) = rows.next().map_err(err)? {
        let g = |i: usize| r.get_ref(i).map_err(err);
        t.add(
            int(g(9)?),
            text(g(0)?),
            text(g(3)?),
            [int(g(1)?), int(g(2)?), int(g(4)?)],
            [int(g(5)?), int(g(6)?), int(g(7)?), int(g(8)?)],
        );
        if let ValueRef::Text(ts) = g(10)?
            && t.last_checked.as_deref().is_none_or(|m| ts > m.as_bytes())
        {
            t.last_checked = Some(String::from_utf8_lossy(ts).into_owned());
        }
    }
    Ok(t)
}

#[derive(Default)]
struct Tally {
    total: i64,
    internal: i64,
    external: i64,
    broken: i64,
    unverified: i64,
    /// blocked, server_error, timeout, rate_limited, dns, connection, hop_cap, loop
    kinds: [i64; 8],
    redirected: i64,
    over_two: i64,
    /// has_temp_redirect, is_nofollow, is_sponsored, is_ugc
    flags: [i64; 4],
    last_checked: Option<String>,
}

const FAILURE_KINDS: [&[u8]; 8] = [
    b"blocked",
    b"server_error",
    b"timeout",
    b"rate_limited",
    b"dns",
    b"connection",
    b"hop_cap",
    b"loop",
];

impl Tally {
    /// Adds one group of `n` rows that share these column values
    /// (`[is_broken, is_unverified, redirect_hops]`, flags as in [`Tally::flags`]).
    fn add(&mut self, n: i64, scope: &[u8], kind: &[u8], counts: [i64; 3], flags: [i64; 4]) {
        let [broken, unverified, hops] = counts;
        let add = |acc: &mut i64, v: i64| *acc = acc.saturating_add(v.saturating_mul(n));
        add(&mut self.total, 1);
        add(&mut self.internal, i64::from(scope == b"internal"));
        add(&mut self.external, i64::from(scope == b"external"));
        add(&mut self.broken, broken);
        add(&mut self.unverified, unverified);
        if let Some(i) = FAILURE_KINDS.iter().position(|k| *k == kind)
            && let Some(acc) = self.kinds.get_mut(i)
        {
            add(acc, 1);
        }
        add(&mut self.redirected, i64::from(hops > 0));
        add(&mut self.over_two, i64::from(hops > 2));
        for (acc, f) in self.flags.iter_mut().zip(flags) {
            add(acc, f);
        }
    }

    fn merge(&mut self, o: Tally) {
        let add = |a: &mut i64, b: i64| *a = a.saturating_add(b);
        add(&mut self.total, o.total);
        add(&mut self.internal, o.internal);
        add(&mut self.external, o.external);
        add(&mut self.broken, o.broken);
        add(&mut self.unverified, o.unverified);
        for (a, b) in self.kinds.iter_mut().zip(o.kinds) {
            add(a, b);
        }
        add(&mut self.redirected, o.redirected);
        add(&mut self.over_two, o.over_two);
        for (a, b) in self.flags.iter_mut().zip(o.flags) {
            add(a, b);
        }
        if o.last_checked > self.last_checked {
            self.last_checked = o.last_checked;
        }
    }

    fn to_json(&self) -> serde_json::Value {
        let [
            blocked,
            server_error,
            timeouts,
            rate_limited,
            dns,
            connection,
            hop_cap,
            loops,
        ] = self.kinds;
        let [temp, nofollow, sponsored, ugc] = self.flags;
        json!({
            "total_links": self.total, "internal": self.internal, "external": self.external,
            "broken": self.broken, "unverified": self.unverified, "blocked": blocked,
            "server_error": server_error, "timeouts": timeouts, "rate_limited": rate_limited,
            "dns": dns, "connection": connection, "hop_cap": hop_cap,
            "redirected": self.redirected, "chains_over_two": self.over_two, "loops": loops,
            "temp_redirects": temp, "nofollow": nofollow, "sponsored": sponsored,
            "ugc": ugc, "last_checked_at": self.last_checked,
        })
    }
}

fn breadcrumbs(
    c: &rusqlite::Connection,
    site_id: &str,
    limit: i64,
    offset: i64,
) -> sf_core::Result<serde_json::Value> {
    v1_collation(c)?;
    let crumbs = c
        .query_row(
            "SELECT count(*), coalesce(sum(link_health_checked_at IS NOT NULL),0),
                    coalesce(sum(has_breadcrumbs),0), coalesce(sum(has_breadcrumb_schema),0),
                    coalesce(sum(has_breadcrumbs <> has_breadcrumb_schema),0)
               FROM posts WHERE site_id=?1",
            [site_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            },
        )
        .map_err(err)?;
    let missing = query_json(
        c,
        "SELECT id AS post_id, url, title, has_breadcrumbs, has_breadcrumb_schema, breadcrumb_report
           FROM posts WHERE site_id=?1 AND link_health_checked_at IS NOT NULL
            AND (has_breadcrumbs=0 OR has_breadcrumb_schema=0)
          ORDER BY url COLLATE V1 ASC LIMIT ?2 OFFSET ?3",
        &[&site_id, &limit, &offset],
        &["breadcrumb_report"],
        &["has_breadcrumbs", "has_breadcrumb_schema"],
    )?;
    Ok(json!({
        "posts": crumbs.0, "checked": crumbs.1, "with_visible": crumbs.2,
        "with_schema": crumbs.3, "mismatched": crumbs.4, "missing": missing,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn seeded() -> rusqlite::Connection {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        sf_store::migrate::run(&mut c).unwrap();
        v1_collation(&c).unwrap();
        c.execute(
            "INSERT INTO sites(id,domain,api_key) VALUES('s','a.example.com','k')",
            [],
        )
        .unwrap();
        let urls = [
            "https://a.example.com/Zeb/",
            "https://a.example.com/apple/",
            "https://a.example.com/tag/bottle-art/",
            "https://a.example.com/tag/bottle/",
        ];
        for (i, u) in urls.iter().enumerate() {
            c.execute(
                "INSERT INTO posts(id,site_id,url,title) VALUES(?1,'s',?2,'t')",
                params![format!("p{i}"), u],
            )
            .unwrap();
        }
        let kinds = ["none", "blocked", "server_error", "timeout"];
        for n in 0..60_i64 {
            let src = if n % 7 == 0 {
                None
            } else {
                Some(format!("p{}", n % 4))
            };
            c.execute(
                "INSERT INTO link_health(id,site_id,source_post_id,href,requested_url,scope,status_code,
                   is_broken,redirect_hops,is_nofollow,is_unverified,failure_kind,anchor)
                 VALUES(?1,'s',?2,?3,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    format!("l{n}"),
                    src,
                    format!("https://x.example.com/{}", ["A", "b", "a", "B-c"][(n % 4) as usize]),
                    if n % 3 == 0 { "external" } else { "internal" },
                    if n % 5 == 0 { None } else { Some(200 + n % 4 * 100) },
                    n % 2,
                    n % 3,
                    i64::from(n % 4 != 1),
                    i64::from(n % 5 == 0),
                    kinds[(n % 4) as usize],
                    format!("a{n}"),
                ],
            )
            .unwrap();
        }
        c
    }

    /// The Rust-ordered CSV equals the SQL `ORDER BY .. COLLATE V1` export it replaced.
    #[test]
    fn csv_order_matches_sql_order() {
        let c = seeded();
        // a second post with the same url as p1 (other site) and a link without source
        c.execute_batch(
            "INSERT INTO sites(id,domain,api_key) VALUES('s2','b.example.com','k2');
             INSERT INTO posts(id,site_id,url,title) VALUES('q1','s2','https://a.example.com/apple/','t');
             INSERT INTO link_health(id,site_id,source_post_id,href,requested_url,anchor)
               VALUES('lq','s','q1','https://x.example.com/A','r','z');",
        )
        .unwrap();
        let mut stmt = c
            .prepare(&format!(
                "SELECT p.url, {CSV_COLS} FROM link_health lh LEFT JOIN posts p ON p.id = lh.source_post_id
                  WHERE lh.site_id=?1
                  ORDER BY lh.is_broken DESC, lh.redirect_hops DESC, p.url COLLATE V1 ASC,
                           lh.href COLLATE V1 ASC, lh.rowid
                  LIMIT ?2"
            ))
            .unwrap();
        let mut want = vec![CSV_HEADER.to_string()];
        let mut rows = stmt.query(params!["s", CSV_MAX_ROWS]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            let src: Option<String> = r.get(0).unwrap();
            want.push(csv_row(src.as_deref(), r, 1).unwrap());
        }
        assert_eq!(want.len(), 62);
        assert_eq!(csv_body(&c, "s").unwrap(), want.join("\r\n"));
        assert_eq!(csv_body(&c, "nope").unwrap(), CSV_HEADER);
    }

    /// The Rust tally equals the SQL aggregate it replaced (including odd stored types).
    #[test]
    fn summary_matches_sql_aggregates() {
        let c = seeded();
        c.execute(
            "INSERT INTO link_health(id,site_id,href,requested_url,is_broken,redirect_hops,is_ugc,checked_at,failure_kind)
             VALUES('odd','s','h','h','1',3.0,'2','2099-01-01T00:00:00.000Z','loop')",
            [],
        )
        .unwrap();
        let sql = c
            .query_row(
                "SELECT count(*),
                        coalesce(sum(scope='internal'),0), coalesce(sum(scope='external'),0),
                        coalesce(sum(is_broken),0), coalesce(sum(is_unverified),0),
                        coalesce(sum(failure_kind='blocked'),0), coalesce(sum(failure_kind='server_error'),0),
                        coalesce(sum(failure_kind='timeout'),0), coalesce(sum(failure_kind='rate_limited'),0),
                        coalesce(sum(failure_kind='dns'),0), coalesce(sum(failure_kind='connection'),0),
                        coalesce(sum(failure_kind='hop_cap'),0), coalesce(sum(redirect_hops>0),0),
                        coalesce(sum(redirect_hops>2),0), coalesce(sum(failure_kind='loop'),0),
                        coalesce(sum(has_temp_redirect),0), coalesce(sum(is_nofollow),0),
                        coalesce(sum(is_sponsored),0), coalesce(sum(is_ugc),0), max(checked_at)
                   FROM link_health WHERE site_id=?1",
                ["s"],
                |r| {
                    let n = |i: usize| r.get::<_, i64>(i);
                    Ok(json!({
                        "total_links": n(0)?, "internal": n(1)?, "external": n(2)?,
                        "broken": n(3)?, "unverified": n(4)?, "blocked": n(5)?,
                        "server_error": n(6)?, "timeouts": n(7)?, "rate_limited": n(8)?,
                        "dns": n(9)?, "connection": n(10)?, "hop_cap": n(11)?,
                        "redirected": n(12)?, "chains_over_two": n(13)?, "loops": n(14)?,
                        "temp_redirects": n(15)?, "nofollow": n(16)?, "sponsored": n(17)?,
                        "ugc": n(18)?, "last_checked_at": r.get::<_, Option<String>>(19)?,
                    }))
                },
            )
            .unwrap();
        assert_eq!(summary(&c, "s").unwrap(), sql);
        // the two concurrent halves merge to the same counters
        let mut lower = tally(&c, "s", Some(false)).unwrap();
        let upper = tally(&c, "s", Some(true)).unwrap();
        assert!(lower.total > 0 && upper.total > 0);
        lower.merge(upper);
        assert_eq!(lower.to_json(), sql);
        assert_eq!(sql["total_links"], 61);
        assert_eq!(sql["last_checked_at"], "2099-01-01T00:00:00.000Z");
        // empty site: zeros and null
        let empty = summary(&c, "nope").unwrap();
        assert_eq!(empty["total_links"], 0);
        assert!(empty["last_checked_at"].is_null());
    }

    /// The two-step page pick returns exactly what the plain `ORDER BY .. LIMIT .. OFFSET` returns,
    /// for every list, page size and offset (ties broken by rowid in both).
    #[test]
    fn two_step_list_matches_plain_query() {
        let c = seeded();
        let lists = [
            (
                "lh.is_broken=1",
                "lh.status_code DESC NULLS LAST, p.url COLLATE V1 ASC, lh.href COLLATE V1 ASC",
            ),
            (
                "lh.redirect_hops>0",
                "lh.redirect_hops DESC, p.url COLLATE V1 ASC, lh.href COLLATE V1 ASC",
            ),
            (
                "lh.is_nofollow=1",
                "(lh.scope='internal') DESC, p.url COLLATE V1 ASC, lh.href COLLATE V1 ASC",
            ),
            (
                "lh.is_unverified=1",
                "lh.failure_kind COLLATE V1 ASC, p.url COLLATE V1 ASC, lh.href COLLATE V1 ASC",
            ),
            (
                "lh.failure_kind='blocked'",
                "p.url COLLATE V1 ASC, lh.href COLLATE V1 ASC",
            ),
            (
                "lh.failure_kind='server_error'",
                "lh.status_code ASC, p.url COLLATE V1 ASC, lh.href COLLATE V1 ASC",
            ),
        ];
        for (wh, order) in lists {
            let plain = format!(
                "SELECT {LINK_COLUMNS} FROM link_health lh LEFT JOIN posts p ON p.id = lh.source_post_id
                  WHERE lh.site_id=?1 AND {wh} ORDER BY {order}, lh.rowid LIMIT ?2 OFFSET ?3"
            );
            for (limit, offset) in [(1_i64, 0_i64), (5, 3), (100, 0), (10, 50)] {
                let p: [&dyn rusqlite::ToSql; 3] = [&"s", &limit, &offset];
                let a = query_json(&c, &list_sql(wh, order), &p, LINK_JSON, LINK_BOOLS).unwrap();
                let b = query_json(&c, &plain, &p, LINK_JSON, LINK_BOOLS).unwrap();
                assert_eq!(a, b, "{wh} limit={limit} offset={offset}");
            }
        }
    }
}
