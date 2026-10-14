//! `/schedules/*`: list, upsert (one per site+kind), delete, run-now.
//!
//! The in-process scheduler (WP J) reads `site_schedules` directly, so saving a row is all that is
//! needed; `next_run_at` is computed here for display. The cron validator mirrors v1 exactly so the
//! UI shows the same error messages.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sf_core::types::JobKind;
use sf_store::err;

use super::common::{
    JsonBody, audit_user, enqueue_raw, guard_child, guard_user_site, is_uuid, query_json,
    query_one_json,
};
use super::sites::topology_param;
use crate::auth::User;
use crate::{ApiError, ApiResult, AppState};

const KINDS: &[&str] = &["crawl", "plan", "link_health", "audit"];
const SCHED_JSON: &[&str] = &["payload"];
const SCHED_BOOLS: &[&str] = &["active"];

struct Field {
    name: &'static str,
    min: u32,
    max: u32,
    names: &'static [(&'static str, u32)],
    question: bool,
}

const MONTHS: &[(&str, u32)] = &[
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("may", 5),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];
const DAYS: &[(&str, u32)] = &[
    ("sun", 0),
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
];
const SECOND: Field = Field {
    name: "second",
    min: 0,
    max: 59,
    names: &[],
    question: false,
};
const FIELDS: [Field; 5] = [
    Field {
        name: "minute",
        min: 0,
        max: 59,
        names: &[],
        question: false,
    },
    Field {
        name: "hour",
        min: 0,
        max: 23,
        names: &[],
        question: false,
    },
    Field {
        name: "day-of-month",
        min: 1,
        max: 31,
        names: &[],
        question: true,
    },
    Field {
        name: "month",
        min: 1,
        max: 12,
        names: MONTHS,
        question: false,
    },
    Field {
        name: "day-of-week",
        min: 0,
        max: 7,
        names: DAYS,
        question: true,
    },
];

fn cron_value(raw: &str, f: &Field) -> Option<u32> {
    let t = raw.trim().to_lowercase();
    if t.is_empty() {
        return None;
    }
    if let Some((_, n)) = f.names.iter().find(|(k, _)| *k == t) {
        return Some(*n);
    }
    if !t.bytes().all(|b| b.is_ascii_digit()) || t.len() > 9 {
        return None;
    }
    t.parse::<u32>()
        .ok()
        .filter(|n| (f.min..=f.max).contains(n))
}

fn cron_field(expr: &str, f: &Field) -> Option<String> {
    if expr.is_empty() {
        return Some(format!("{} is empty", f.name));
    }
    if expr == "?" {
        return (!f.question).then(|| format!("\"?\" is not allowed in the {} field", f.name));
    }
    for part in expr.split(',') {
        let mut pieces = part.split('/');
        let range = pieces.next().unwrap_or("");
        let step = pieces.next();
        if pieces.next().is_some() {
            return Some(format!("{}: \"{part}\" has more than one step", f.name));
        }
        if let Some(s) = step
            && (s.is_empty()
                || !s.bytes().all(|b| b.is_ascii_digit())
                || s.parse::<u64>().map_or(true, |n| n < 1))
        {
            return Some(format!("{}: \"{part}\" has an invalid step", f.name));
        }
        if range == "*" {
            continue;
        }
        let bounds: Vec<&str> = range.split('-').collect();
        if bounds.len() > 2 {
            return Some(format!("{}: \"{part}\" is not a valid range", f.name));
        }
        let first = bounds.first().copied().unwrap_or("");
        let Some(from) = cron_value(first, f) else {
            return Some(format!(
                "{}: \"{first}\" is not in {}-{}",
                f.name, f.min, f.max
            ));
        };
        if let Some(second) = bounds.get(1) {
            let Some(to) = cron_value(second, f) else {
                return Some(format!(
                    "{}: \"{second}\" is not in {}-{}",
                    f.name, f.min, f.max
                ));
            };
            if to < from {
                return Some(format!(
                    "{}: range \"{range}\" ends before it starts",
                    f.name
                ));
            }
        }
    }
    None
}

/// 5-field (or 6 with seconds) cron; `@daily`-style macros are rejected (v1 parity).
pub fn validate_cron(expr: &str) -> Result<(), String> {
    let t = expr.trim();
    if t.is_empty() {
        return Err("cron expression is empty".into());
    }
    if t.starts_with('@') {
        return Err(format!(
            "macro \"{t}\" is not supported — use a 5-field cron expression"
        ));
    }
    let parts: Vec<&str> = t.split_whitespace().collect();
    if parts.len() != 5 && parts.len() != 6 {
        return Err(format!("expected 5 or 6 fields, got {}", parts.len()));
    }
    let fields: Vec<&Field> = if parts.len() == 6 {
        std::iter::once(&SECOND).chain(FIELDS.iter()).collect()
    } else {
        FIELDS.iter().collect()
    };
    for (p, f) in parts.iter().zip(fields) {
        if let Some(e) = cron_field(p, f) {
            return Err(e);
        }
    }
    Ok(())
}

/// Next fire time (UTC RFC 3339) for display; `None` when croner cannot evaluate the pattern.
pub fn next_run(cron: &str, tz: &str) -> Option<String> {
    let zone: chrono_tz::Tz = tz.parse().ok()?;
    let mut c = croner::Cron::new(cron);
    c.with_seconds_optional();
    let parsed = c.parse().ok()?;
    let now = chrono::Utc::now().with_timezone(&zone);
    let next = parsed.find_next_occurrence(&now, false).ok()?;
    Some(
        next.with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    )
}

pub async fn list(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &site_id).await?;
    let rows = st
        .db
        .read(move |c| {
            query_json(
                c,
                "SELECT * FROM site_schedules WHERE site_id=?1 ORDER BY job_kind ASC",
                &[&site_id],
                SCHED_JSON,
                SCHED_BOOLS,
            )
        })
        .await?;
    Ok(Json(Value::Array(rows)))
}

#[derive(Deserialize, Default)]
pub struct ScheduleBody {
    #[serde(default)]
    job_kind: Option<String>,
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    tz: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
    #[serde(default)]
    active: Option<bool>,
}

pub async fn upsert(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
    JsonBody(b): JsonBody<Option<ScheduleBody>>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &site_id).await?;
    let b = b.unwrap_or_default();
    let kind = b.job_kind.unwrap_or_else(|| "audit".into());
    if !KINDS.contains(&kind.as_str()) {
        return Err(ApiError::bad(format!(
            "job_kind must be one of {}",
            KINDS.join(", ")
        )));
    }
    let cron = b.cron.unwrap_or_default().trim().to_string();
    validate_cron(&cron).map_err(|e| ApiError::bad(format!("invalid cron: {e}")))?;
    let tz = b.tz.unwrap_or_else(|| "UTC".into()).trim().to_string();
    if tz.is_empty() || tz.parse::<chrono_tz::Tz>().is_err() {
        return Err(ApiError::bad(format!("unknown timezone: {tz}")));
    }
    let payload = match b.payload {
        None | Some(Value::Null) => json!({}),
        Some(Value::Object(o)) => {
            if let Some(t) = o.get("topology").and_then(Value::as_str) {
                topology_param(Some(t))?;
            }
            Value::Object(o)
        }
        Some(_) => return Err(ApiError::bad("payload must be an object")),
    };
    if payload.to_string().len() > 16 * 1024 {
        return Err(ApiError::bad("payload is too large"));
    }
    let active = b.active != Some(false);
    let next = if active { next_run(&cron, &tz) } else { None };
    let (sid, k2) = (site_id.clone(), kind.clone());
    let row = st
        .db
        .write(move |t| {
            let now = sf_core::now_rfc3339();
            t.execute(
                "INSERT INTO site_schedules(id, site_id, job_kind, cron, tz, payload, active, next_run_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(site_id, job_kind) DO UPDATE SET cron=excluded.cron, tz=excluded.tz,
                   payload=excluded.payload, active=excluded.active, next_run_at=excluded.next_run_at,
                   updated_at=?9",
                params![sf_core::new_id(), sid, k2, cron, tz, payload.to_string(), active, next, now],
            )
            .map_err(err)?;
            query_one_json(
                t,
                "SELECT * FROM site_schedules WHERE site_id=?1 AND job_kind=?2",
                &[&sid, &k2],
                SCHED_JSON,
                SCHED_BOOLS,
            )
        })
        .await?
        .ok_or_else(|| ApiError::internal("schedule vanished"))?;
    let mut row = row;
    if let Some(o) = row.as_object_mut() {
        o.insert("scheduled".into(), Value::Bool(true));
    }
    audit_user(
        &st,
        &u,
        "schedule.upsert",
        Some(&site_id),
        json!({ "job_kind": kind, "active": active }),
    )
    .await;
    Ok(Json(row))
}

pub async fn delete(
    State(st): State<AppState>,
    u: User,
    Path(schedule_id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&schedule_id) {
        return Err(ApiError::not_found("schedule not found"));
    }
    let id = schedule_id.clone();
    let site_id: String = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT site_id FROM site_schedules WHERE id=?1",
                [&id],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)
        })
        .await?
        .ok_or_else(|| ApiError::not_found("schedule not found"))?;
    guard_child(&st, &u, &site_id, "schedule not found").await?;
    let id = schedule_id.clone();
    st.db
        .write(move |t| {
            t.execute("DELETE FROM site_schedules WHERE id=?1", [&id])
                .map_err(err)?;
            Ok(())
        })
        .await?;
    audit_user(&st, &u, "schedule.delete", Some(&schedule_id), json!({})).await;
    Ok(Json(json!({ "deleted": true })))
}

#[derive(Deserialize, Default)]
pub struct RunNowBody {
    #[serde(default)]
    topology: Option<String>,
    #[serde(default)]
    skip_crawl: Option<bool>,
    /// v2: which schedule to run (by id) ...
    #[serde(default)]
    schedule_id: Option<String>,
    /// ... or by kind. Without either the run is an `audit` (v1 behaviour).
    #[serde(default)]
    job_kind: Option<String>,
}

fn job_payload(
    kind: &str,
    stored: &Value,
    topology: Option<String>,
    skip_crawl: Option<bool>,
) -> Value {
    let topo = topology.or_else(|| {
        stored
            .get("topology")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let flag = |k: &str, alt: &str| {
        stored
            .get(k)
            .or_else(|| stored.get(alt))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    // Every kind gets the stored schedule payload (link_health `internal_only`, keyword_link
    // options, ...); audit/plan then normalise their known keys on top of it.
    let mut out = match stored {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    match kind {
        "audit" => {
            out.remove("skipCrawl");
            out.insert("topology".into(), json!(topo));
            out.insert(
                "skip_crawl".into(),
                json!(skip_crawl.unwrap_or_else(|| flag("skip_crawl", "skipCrawl"))),
            );
        }
        "plan" => {
            out.insert("topology".into(), json!(topo));
            out.insert("archive".into(), json!(flag("archive", "archive")));
        }
        _ => {}
    }
    Value::Object(out)
}

/// Runs the requested schedule's own kind (fixes v1 B5, which always ran an audit). Answers 200
/// `{job_run_id}` like v1, plus `job_kind` and `deduplicated`. Stamps the schedule row when one exists.
pub async fn run_now(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
    JsonBody(b): JsonBody<Option<RunNowBody>>,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &site_id).await?;
    let b = b.unwrap_or_default();
    let topology = topology_param(b.topology.as_deref())?;
    let sid = site_id.clone();
    let (sched_id, kind_req) = (b.schedule_id.clone(), b.job_kind.clone());
    if let Some(k) = &kind_req
        && !KINDS.contains(&k.as_str())
    {
        return Err(ApiError::bad(format!(
            "job_kind must be one of {}",
            KINDS.join(", ")
        )));
    }
    if let Some(s) = &sched_id
        && !is_uuid(s)
    {
        return Err(ApiError::not_found("schedule not found"));
    }
    let found: Option<(String, String, String)> = st
        .db
        .read(move |c| {
            let row = |sql: &str, a: &str, b: &str| {
                c.query_row(sql, params![a, b], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                    .optional()
                    .map_err(err)
            };
            match sched_id {
                // An unknown schedule id is a 404 (None).
                Some(s) => row(
                    "SELECT id, job_kind, payload FROM site_schedules WHERE id=?1 AND site_id=?2",
                    &s,
                    &sid,
                ),
                None => {
                    let kind = kind_req.unwrap_or_else(|| "audit".into());
                    let hit = row(
                        "SELECT id, job_kind, payload FROM site_schedules WHERE site_id=?1 AND job_kind=?2",
                        &sid,
                        &kind,
                    )?;
                    Ok(Some(hit.unwrap_or((String::new(), kind, "{}".into()))))
                }
            }
        })
        .await?;
    let Some((row_id, kind, stored)) = found else {
        return Err(ApiError::not_found("schedule not found"));
    };
    let stored: Value = serde_json::from_str(&stored).unwrap_or_else(|_| json!({}));
    let payload = job_payload(&kind, &stored, topology, b.skip_crawl);
    let job_kind = JobKind::parse(&kind);
    let out = enqueue_raw(&st, &u.actor(), job_kind, &site_id, payload).await?;
    if !row_id.is_empty() {
        let (rid, jid) = (row_id.clone(), out.job_run_id.clone());
        st.db
            .write(move |t| {
                t.execute(
                    "UPDATE site_schedules SET last_run_at=?2, last_job_run_id=?3 WHERE id=?1",
                    params![rid, sf_core::now_rfc3339(), jid],
                )
                .map_err(err)?;
                Ok(())
            })
            .await?;
    }
    audit_user(
        &st,
        &u,
        "schedule.run_now",
        Some(&site_id),
        json!({ "job_kind": kind, "job_run_id": out.job_run_id }),
    )
    .await;
    Ok(Json(json!({
        "job_run_id": out.job_run_id,
        "job_kind": kind,
        "deduplicated": out.deduplicated,
    }))
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_validation_matches_v1_messages() {
        assert!(validate_cron("*/15 * * * *").is_ok());
        assert!(validate_cron("0 0 3 * * mon-fri").is_ok());
        assert!(validate_cron("0 3 ? * sun").is_ok());
        assert_eq!(
            validate_cron("@daily").err().as_deref(),
            Some("macro \"@daily\" is not supported — use a 5-field cron expression")
        );
        assert_eq!(
            validate_cron("* * *").err().as_deref(),
            Some("expected 5 or 6 fields, got 3")
        );
        assert_eq!(
            validate_cron("61 * * * *").err().as_deref(),
            Some("minute: \"61\" is not in 0-59")
        );
        assert_eq!(
            validate_cron("? * * * *").err().as_deref(),
            Some("\"?\" is not allowed in the minute field")
        );
        assert_eq!(
            validate_cron("5-1 * * * *").err().as_deref(),
            Some("minute: range \"5-1\" ends before it starts")
        );
        assert!(validate_cron("*/0 * * * *").is_err());
        assert!(validate_cron("1/2/3 * * * *").is_err());
        assert!(validate_cron("").is_err());
        for junk in [
            "-",
            "--",
            "*/",
            "/",
            ",",
            "99999999999999999999 * * * *",
            "é * * * *",
        ] {
            let _ = validate_cron(junk);
        }
    }

    #[test]
    fn next_run_and_payloads() {
        assert!(next_run("*/5 * * * *", "Europe/Berlin").is_some());
        assert!(next_run("*/5 * * * *", "Mars/Olympus").is_none());
        let p = job_payload(
            "audit",
            &json!({"skipCrawl": true, "topology": "circle"}),
            None,
            None,
        );
        assert_eq!(p["skip_crawl"], json!(true));
        assert_eq!(p["topology"], json!("circle"));
        // every kind keeps the stored payload (E2E pilot: run-now dropped it)
        assert_eq!(
            job_payload("crawl", &json!({"x":1}), None, None),
            json!({"x":1})
        );
        assert_eq!(
            job_payload("link_health", &json!({"internal_only": true}), None, None),
            json!({"internal_only": true})
        );
        assert_eq!(job_payload("crawl", &json!([1]), None, None), json!({}));
    }
}
