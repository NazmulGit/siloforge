//! §9 unified event log API.
//!
//! * `GET /logs?source=&actor=&site_id=&status=&q=&since=&until=&limit=&before_id=&after_id=` ->
//!   `{ logs: [...], next_before_id, next_after_id }`. Default: newest first (keyset on id,
//!   `before_id`); with `after_id`: rows newer than it, oldest first (live tail). `limit` 1..1000
//!   (default 100). `source` / `status` accept a comma list. `q` = substring of action, target,
//!   actor or site_id. `since` / `until` = RFC 3339 or relative (`15m`, `24h`, `7d`).
//! * `GET /logs/export?...` -> CSV attachment (same filters, newest first, at most 100k rows).
//! * `GET /logs/stats?since=&until=` -> `{ by_source: {..}, by_status: {..}, total }`.
//!
//! Admins (session or `*`/`admin` token) see every row; everyone else only rows they own
//! (`user_id`).

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use rusqlite::types::Value as Sql;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sf_store::{err, row_to_json};

use crate::auth::User;
use crate::{ApiError, ApiResult, AppState};

const COLS: &str = "id, ts, source, actor, action, target, site_id, status, http_status, duration_ms, detail, user_id";
/// Max rows in one CSV export.
pub const EXPORT_MAX: i64 = 100_000;

#[derive(Deserialize, Default, Debug)]
pub struct LogsQ {
    source: Option<String>,
    actor: Option<String>,
    site_id: Option<String>,
    status: Option<String>,
    q: Option<String>,
    since: Option<String>,
    until: Option<String>,
    limit: Option<String>,
    before_id: Option<String>,
    after_id: Option<String>,
}

fn sees_all(u: &User) -> bool {
    u.is_admin() && (u.token.is_none() || u.scopes.iter().any(|s| s == "*" || s == "admin"))
}

fn opt(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn int(v: &Option<String>, name: &str) -> ApiResult<Option<i64>> {
    match opt(v) {
        None => Ok(None),
        Some(s) => s
            .parse::<i64>()
            .map(Some)
            .map_err(|_| ApiError::bad(format!("{name} must be an integer"))),
    }
}

/// RFC 3339 passes through; `<n>m|h|d` is relative to now.
fn time_arg(v: &Option<String>, name: &str) -> ApiResult<Option<String>> {
    let Some(s) = opt(v) else { return Ok(None) };
    if s.len() > 64 {
        return Err(ApiError::bad(format!("{name} is too long")));
    }
    let unit = s.chars().last().unwrap_or(' ');
    if let Some(num) = s.strip_suffix(['m', 'h', 'd'])
        && let Ok(n) = num.parse::<i64>()
    {
        let mins = match unit {
            'm' => n,
            'h' => n.saturating_mul(60),
            _ => n.saturating_mul(1440),
        }
        .clamp(0, 3650 * 1440);
        let t = chrono::Utc::now() - chrono::Duration::minutes(mins);
        return Ok(Some(t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)));
    }
    match chrono::DateTime::parse_from_rfc3339(s) {
        Ok(t) => Ok(Some(
            t.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        )),
        Err(_) => Err(ApiError::bad(format!(
            "{name} must be RFC 3339 or relative like 24h"
        ))),
    }
}

fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('%');
    for c in s.chars().take(200) {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// Appends `frag` (one `?`) with `v` as the next numbered parameter.
fn bind(sql: &mut String, args: &mut Vec<Sql>, frag: &str, v: Sql) {
    args.push(v);
    sql.push_str(&frag.replace('?', &format!("?{}", args.len())));
}

/// WHERE clause (always starts with `WHERE 1`) + its parameters. `with_cursor` adds the keyset.
fn filters(u: &User, q: &LogsQ, with_cursor: bool) -> ApiResult<(String, Vec<Sql>)> {
    let mut sql = String::from(" WHERE 1");
    let mut args: Vec<Sql> = Vec::new();
    if !sees_all(u) {
        bind(
            &mut sql,
            &mut args,
            " AND user_id = ?",
            Sql::Text(u.user_id.clone()),
        );
    }
    for (col, v) in [("source", &q.source), ("status", &q.status)] {
        if let Some(list) = opt(v) {
            let items: Vec<&str> = list
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .take(10)
                .collect();
            if !items.is_empty() {
                sql.push_str(&format!(" AND {col} IN ("));
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        sql.push(',');
                    }
                    bind(
                        &mut sql,
                        &mut args,
                        "?",
                        Sql::Text(it.chars().take(40).collect()),
                    );
                }
                sql.push(')');
            }
        }
    }
    for (col, v) in [("actor", &q.actor), ("site_id", &q.site_id)] {
        if let Some(s) = opt(v) {
            bind(
                &mut sql,
                &mut args,
                &format!(" AND {col} = ?"),
                Sql::Text(s.chars().take(300).collect()),
            );
        }
    }
    if let Some(s) = opt(&q.q) {
        let pat = like_escape(s);
        args.push(Sql::Text(pat));
        let n = args.len();
        sql.push_str(&format!(
            " AND (action LIKE ?{n} ESCAPE '\\' OR ifnull(target,'') LIKE ?{n} ESCAPE '\\' \
             OR actor LIKE ?{n} ESCAPE '\\' OR ifnull(site_id,'') LIKE ?{n} ESCAPE '\\')"
        ));
    }
    if let Some(t) = time_arg(&q.since, "since")? {
        bind(&mut sql, &mut args, " AND ts >= ?", Sql::Text(t));
    }
    if let Some(t) = time_arg(&q.until, "until")? {
        bind(&mut sql, &mut args, " AND ts <= ?", Sql::Text(t));
    }
    if with_cursor {
        if let Some(b) = int(&q.before_id, "before_id")? {
            bind(&mut sql, &mut args, " AND id < ?", Sql::Integer(b));
        }
        if let Some(a) = int(&q.after_id, "after_id")? {
            bind(&mut sql, &mut args, " AND id > ?", Sql::Integer(a));
        }
    }
    Ok((sql, args))
}

/// `GET /logs`.
pub async fn list(
    State(st): State<AppState>,
    u: User,
    Query(q): Query<LogsQ>,
) -> ApiResult<Json<Value>> {
    let limit = super::clamp_limit(int(&q.limit, "limit")?, 100, 1000);
    let tail = int(&q.after_id, "after_id")?.is_some();
    let (wh, args) = filters(&u, &q, true)?;
    let order = if tail { "ASC" } else { "DESC" };
    let sql = format!(
        "SELECT {COLS} FROM event_log{wh} ORDER BY id {order} LIMIT {}",
        limit + 1
    );
    let mut rows = st
        .db
        .read(move |c| {
            let mut stmt = c.prepare(&sql).map_err(err)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                    Ok(row_to_json(r, &["detail"], &[]))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?;
    let more = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
    rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    let last_id = rows.last().and_then(|r| r["id"].as_i64());
    let (next_before, next_after) = if tail {
        // live tail: keep polling from the newest row seen (or the caller's after_id)
        let after = last_id.or(int(&q.after_id, "after_id")?);
        (Value::Null, json!(after))
    } else {
        let first = rows.first().and_then(|r| r["id"].as_i64());
        (
            if more { json!(last_id) } else { Value::Null },
            json!(first),
        )
    };
    Ok(Json(json!({
        "logs": rows,
        "next_before_id": next_before,
        "next_after_id": next_after,
    })))
}

/// `GET /logs/export` (CSV).
pub async fn export(
    State(st): State<AppState>,
    u: User,
    Query(q): Query<LogsQ>,
) -> ApiResult<Response> {
    let limit = int(&q.limit, "limit")?
        .unwrap_or(EXPORT_MAX)
        .clamp(1, EXPORT_MAX);
    let (wh, args) = filters(&u, &q, true)?;
    let sql = format!("SELECT {COLS} FROM event_log{wh} ORDER BY id DESC LIMIT {limit}");
    let body = st
        .db
        .read(move |c| {
            let mut out = String::from(
                "id,ts,source,actor,action,target,site_id,status,http_status,duration_ms,detail,user_id\n",
            );
            let mut stmt = c.prepare(&sql).map_err(err)?;
            let mut rows = stmt
                .query(rusqlite::params_from_iter(args.iter()))
                .map_err(err)?;
            while let Some(r) = rows.next().map_err(err)? {
                let txt = |i: usize| -> String {
                    match r.get_ref(i) {
                        Ok(rusqlite::types::ValueRef::Text(t)) => {
                            String::from_utf8_lossy(t).into_owned()
                        }
                        Ok(rusqlite::types::ValueRef::Integer(n)) => n.to_string(),
                        _ => String::new(),
                    }
                };
                let cells: Vec<String> = (0..12).map(txt).collect();
                let refs: Vec<&str> = cells.iter().map(String::as_str).collect();
                out.push_str(&crate::v1::common::csv_line(&refs));
                out.push('\n');
            }
            Ok(out)
        })
        .await?;
    let mut res = body.into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"siloforge-logs.csv\""),
    );
    Ok(res)
}

/// `GET /logs/stats`.
pub async fn stats(
    State(st): State<AppState>,
    u: User,
    Query(q): Query<LogsQ>,
) -> ApiResult<Json<Value>> {
    // only the time range (and ownership) applies to the counters
    let scoped = LogsQ {
        since: q.since,
        until: q.until,
        ..Default::default()
    };
    let (wh, args) = filters(&u, &scoped, false)?;
    let (by_source, by_status) = st
        .db
        .read(move |c| {
            let count = |col: &str| -> sf_core::Result<Map<String, Value>> {
                let sql = format!("SELECT {col}, count(*) FROM event_log{wh} GROUP BY {col}");
                let mut stmt = c.prepare(&sql).map_err(err)?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                    })
                    .map_err(err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(err)?;
                Ok(rows.into_iter().map(|(k, n)| (k, json!(n))).collect())
            };
            Ok((count("source")?, count("status")?))
        })
        .await?;
    let total: i64 = by_source.values().filter_map(Value::as_i64).sum();
    Ok(Json(json!({
        "by_source": by_source,
        "by_status": by_status,
        "total": total,
    })))
}
