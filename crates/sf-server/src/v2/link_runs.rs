//! v2.2 Link Runs + Auto-Linker (docs/specs/v2.2-link-runs.md §3, §5). User routes (own sites;
//! admin all); writes need the write scope (method-based token scope check). The pipeline and
//! all link-run SQL live in `sf_jobs::pipelines::link_run`.
//!
//! | route | answer |
//! |---|---|
//! | `GET /sites/{id}/opportunities?min_score=` | pre-run overview |
//! | `POST /sites/{id}/link-runs` `{name?, options?}` | 201 `{run, job_run_id}` |
//! | `GET /sites/{id}/link-runs?limit=` | `{runs}` |
//! | `GET /link-runs/{run_id}` | the run |
//! | `GET /link-runs/{run_id}/items?status=&post_id=&strategy=&q=&limit=&offset=&group=post` | `{items,total}` / `{posts,total,total_items}` |
//! | `POST /link-runs/{run_id}/select` `{edge_ids?, all?, deselect?}` | `{changed, run}` |
//! | `PATCH /link-runs/{run_id}/items/{edge_id}` `{anchor?, alternate?, insert_module?}` | `{item, run}` |
//! | `POST /link-runs/{run_id}/deploy\|verify\|repair\|undo` | 202 `{run, job_run_id}` (409 wrong status) |
//! | `POST /link-runs/{run_id}/cancel` | `{run}` |
//! | `GET/PUT /sites/{id}/auto-linker`, `POST /sites/{id}/auto-linker/run` | Auto-Linker |

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{Value, json};
use sf_jobs::pipelines::link_run::{self, store};
use sf_store::err;

use super::clamp_limit;
use crate::auth::User;
use crate::v1::common::{
    JsonBody, audit_user, get_site, guard_site_admin, guard_user_site, int_param, is_uuid,
    str_param,
};
use crate::{ApiError, ApiResult, AppState};

fn events(st: &AppState) -> Option<sf_ops::eventlog::EventLog> {
    st.services
        .get::<sf_ops::eventlog::EventLog>()
        .map(|l| (*l).clone())
}

/// Ownership gate for a run id (foreign / unknown / non-uuid -> 404). Returns the site id.
async fn guard_run(st: &AppState, u: &User, run_id: &str) -> ApiResult<String> {
    if !is_uuid(run_id) {
        return Err(ApiError::not_found("link run not found"));
    }
    let id = run_id.to_string();
    let site = st
        .db
        .read(move |c| store::run_site(c, &id))
        .await?
        .ok_or_else(|| ApiError::not_found("link run not found"))?;
    guard_user_site(st, u, &site)
        .await
        .map_err(|e| match e.status {
            StatusCode::NOT_FOUND | StatusCode::FORBIDDEN => {
                ApiError::not_found("link run not found")
            }
            _ => e,
        })?;
    Ok(site)
}

async fn run_value(st: &AppState, run_id: &str) -> ApiResult<Value> {
    let id = run_id.to_string();
    st.db
        .write(move |t| {
            store::reconcile(t, &id)?;
            store::run_json(t, &id)
        })
        .await?
        .ok_or_else(|| ApiError::not_found("link run not found"))
}

// ---------------------------------------------------------------------------------------------
// opportunities
// ---------------------------------------------------------------------------------------------

/// Posts compared pairwise for `semantic_pairs` (more = estimated from a sample).
const PAIR_CAP: i64 = 1500;

pub async fn opportunities(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &id).await?;
    let threshold = str_param(&q, "min_score")
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(0.55)
        .clamp(0.0, 1.0);
    let sid = id.clone();
    let (mut out, vecs, with_emb) = st
        .db
        .read(move |c| {
            let o = store::opportunity_counts(c, &sid)?;
            let (v, n) = store::post_vectors(c, &sid, PAIR_CAP)?;
            Ok((o, v, n))
        })
        .await?;
    let sample = vecs.len() as i64;
    let pairs = sf_jobs::ctx::compute(move || store::count_pairs(&vecs, threshold))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    // scale a sample up by the pair-count ratio
    let estimated = with_emb > sample && sample > 1;
    let pairs = if estimated {
        let full = (with_emb * (with_emb - 1)) as f64;
        let part = (sample * (sample - 1)) as f64;
        (pairs as f64 * full / part).round() as i64
    } else {
        pairs
    };
    out["counts"]["semantic_pairs"] = json!(pairs);
    out["by_strategy"]["semantic"]["pairs"] = json!(pairs);
    // f32 -> f64 would print 0.550000011920929
    out["by_strategy"]["semantic"]["threshold"] =
        json!((f64::from(threshold) * 1000.0).round() / 1000.0);
    out["by_strategy"]["semantic"]["estimated"] = json!(estimated);
    out["by_strategy"]["semantic"]["posts_with_embeddings"] = json!(with_emb);
    out["by_strategy"]["contextual"]["available"] = json!(true);
    out["site_id"] = json!(id);
    Ok(Json(out))
}

// ---------------------------------------------------------------------------------------------
// runs
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    options: Option<Value>,
}

pub async fn create(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<CreateBody>,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &id).await?;
    if b.name.as_deref().is_some_and(|n| n.len() > 400) {
        return Err(ApiError::bad("name is too long"));
    }
    let opts = link_run::parse_options(b.options.as_ref()).map_err(ApiError::bad)?;
    // v2.4: monthly Link Run quota (+ the AI token quota when the run asks for AI).
    let ws = crate::tenancy::site_ws(&st, &id).await?;
    crate::tenancy::check_quota(&st, ws.as_deref(), "link_runs_per_month", 1).await?;
    if b.options
        .as_ref()
        .and_then(|o| o.get("ai_refine"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        crate::tenancy::check_quota(&st, ws.as_deref(), "ai_tokens_per_month", 1).await?;
    }
    let ev = events(&st);
    let run_id = link_run::create_run(
        &st.db,
        &id,
        b.name.as_deref().unwrap_or(""),
        &opts,
        &u.actor(),
        false,
    )
    .await?;
    link_run::record_event(
        &st.db,
        ev.as_ref(),
        &run_id,
        &id,
        "created",
        "ok",
        json!({"by": u.actor(), "mode": opts.mode}),
    )
    .await;
    let job =
        match link_run::enqueue_step(&st.db, &st.jobs, ev.as_ref(), &run_id, "plan", &u.actor())
            .await
        {
            Ok(j) => j,
            Err(e) => {
                // never leave a draft that blocks the site
                let rid = run_id.clone();
                let msg = e.to_string();
                let _ = st
                    .db
                    .write(move |t| {
                        t.execute(
                            "UPDATE link_runs SET status='failed', error=?2 WHERE id=?1",
                            rusqlite::params![rid, msg],
                        )
                        .map_err(err)
                    })
                    .await;
                return Err(e.into());
            }
        };
    audit_user(
        &st,
        &u,
        "link_run.create",
        Some(&run_id),
        json!({"site_id": id, "job_run_id": job}),
    )
    .await;
    let run = run_value(&st, &run_id).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"run": run, "job_run_id": job})),
    )
        .into_response())
}

pub async fn list(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &id).await?;
    let limit = clamp_limit(str_param(&q, "limit").and_then(|v| v.parse().ok()), 50, 500);
    let sid = id.clone();
    let runs = st
        .db
        .write(move |t| {
            let ids: Vec<String> = {
                let mut s = t
                    .prepare("SELECT id FROM link_runs WHERE site_id=?1 AND status IN ('draft','crawling','planning','deploying')")
                    .map_err(err)?;
                s.query_map([&sid], |r| r.get(0))
                    .map_err(err)?
                    .filter_map(|r| r.ok())
                    .collect()
            };
            for i in ids {
                store::reconcile(t, &i)?;
            }
            store::list_runs(t, &sid, limit)
        })
        .await?;
    Ok(Json(json!({"runs": runs, "total": runs.len()})))
}

pub async fn get_one(
    State(st): State<AppState>,
    u: User,
    Path(run_id): Path<String>,
) -> ApiResult<Json<Value>> {
    guard_run(&st, &u, &run_id).await?;
    Ok(Json(run_value(&st, &run_id).await?))
}

pub async fn items(
    State(st): State<AppState>,
    u: User,
    Path(run_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    guard_run(&st, &u, &run_id).await?;
    let text = |k: &str, max: usize| -> ApiResult<Option<String>> {
        match str_param(&q, k).map(str::trim).filter(|v| !v.is_empty()) {
            Some(v) if v.len() > max => Err(ApiError::bad(format!("{k} is too long"))),
            Some(v) => Ok(Some(v.to_string())),
            None => Ok(None),
        }
    };
    let status = text("status", 40)?;
    if let Some(s) = &status
        && !store::ITEM_STATUSES.contains(&s.as_str())
    {
        return Err(ApiError::bad(format!(
            "status must be one of {}",
            store::ITEM_STATUSES.join(", ")
        )));
    }
    let strategy = text("strategy", 40)?;
    if let Some(s) = &strategy
        && !link_run::options::STRATEGIES.contains(&s.as_str())
    {
        return Err(ApiError::bad(format!(
            "strategy must be one of {}",
            link_run::options::STRATEGIES.join(", ")
        )));
    }
    let group = match str_param(&q, "group") {
        None | Some("") => false,
        Some("post") => true,
        Some(_) => return Err(ApiError::bad("group must be post")),
    };
    let iq = store::ItemQuery {
        status,
        post_id: text("post_id", 64)?,
        strategy,
        q: text("q", 200)?,
        edge_id: None,
        limit: int_param(&q, "limit", 100, 1, 1000),
        offset: int_param(&q, "offset", 0, 0, i64::from(u32::MAX)),
        group_by_post: group,
    };
    let id = run_id.clone();
    Ok(Json(st.db.read(move |c| store::items(c, &id, &iq)).await?))
}

async fn require_status(
    st: &AppState,
    run_id: &str,
    allowed: &[&str],
    what: &str,
) -> ApiResult<()> {
    let id = run_id.to_string();
    let status: Option<String> = st
        .db
        .read(move |c| {
            c.query_row("SELECT status FROM link_runs WHERE id=?1", [&id], |r| {
                r.get(0)
            })
            .optional()
            .map_err(err)
        })
        .await?;
    match status {
        Some(s) if allowed.contains(&s.as_str()) => Ok(()),
        Some(s) => Err(ApiError::conflict(format!(
            "link run is {s}; {what} is only possible while it is {}",
            allowed.join(" or ")
        ))),
        None => Err(ApiError::not_found("link run not found")),
    }
}

#[derive(Deserialize)]
pub struct SelectBody {
    #[serde(default)]
    edge_ids: Option<Vec<String>>,
    #[serde(default)]
    all: Option<bool>,
    #[serde(default)]
    deselect: Option<bool>,
}

pub async fn select(
    State(st): State<AppState>,
    u: User,
    Path(run_id): Path<String>,
    JsonBody(b): JsonBody<SelectBody>,
) -> ApiResult<Json<Value>> {
    guard_run(&st, &u, &run_id).await?;
    require_status(&st, &run_id, &["ready"], "changing the selection").await?;
    let ids = b.edge_ids.unwrap_or_default();
    let all = b.all.unwrap_or(false);
    if !all && ids.is_empty() {
        return Err(ApiError::bad("give edge_ids or all: true"));
    }
    if ids.len() > 50_000 || ids.iter().any(|i| i.len() > 64) {
        return Err(ApiError::bad("edge_ids: at most 50000 ids"));
    }
    let deselect = b.deselect.unwrap_or(false);
    let rid = run_id.clone();
    let changed = st
        .db
        .write(move |t| store::select(t, &rid, &ids, all, deselect))
        .await?;
    Ok(Json(
        json!({"changed": changed, "run": run_value(&st, &run_id).await?}),
    ))
}

#[derive(Deserialize)]
pub struct PatchItemBody {
    #[serde(default)]
    anchor: Option<String>,
    /// One of the item's `alternates` (string) or its index.
    #[serde(default)]
    alternate: Option<Value>,
    #[serde(default)]
    insert_module: Option<String>,
}

pub async fn patch_item(
    State(st): State<AppState>,
    u: User,
    Path((run_id, edge_id)): Path<(String, String)>,
    JsonBody(b): JsonBody<PatchItemBody>,
) -> ApiResult<Json<Value>> {
    guard_run(&st, &u, &run_id).await?;
    require_status(&st, &run_id, &["ready"], "editing items").await?;
    if b.anchor.is_none() && b.alternate.is_none() && b.insert_module.is_none() {
        return Err(ApiError::bad("give anchor, alternate or insert_module"));
    }
    if edge_id.len() > 64 {
        return Err(ApiError::not_found("item not found in this run"));
    }
    let (rid, eid) = (run_id.clone(), edge_id.clone());
    let item = st
        .db
        .write(move |t| {
            let anchor: Option<String> = match (&b.anchor, &b.alternate) {
                (Some(a), _) => Some(a.clone()),
                (None, Some(alt)) => {
                    let bd: Option<String> = t
                        .query_row(
                            "SELECT score_breakdown FROM link_edges WHERE id=?1 AND run_id=?2",
                            [&eid, &rid],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(err)?;
                    let bd: Value = bd
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or(Value::Null);
                    let alts: Vec<String> = bd["alternates"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x["anchor"].as_str().or_else(|| x.as_str()))
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    let pick = match alt {
                        Value::Number(n) => n
                            .as_u64()
                            .and_then(|i| usize::try_from(i).ok())
                            .and_then(|i| alts.get(i).cloned()),
                        Value::String(s) => {
                            alts.iter().find(|a| a.eq_ignore_ascii_case(s)).cloned()
                        }
                        _ => None,
                    };
                    Some(pick.ok_or_else(|| {
                        sf_core::Error::bad("alternate is not one of this item's alternates")
                    })?)
                }
                (None, None) => None,
            };
            store::patch_item(t, &rid, &eid, anchor.as_deref(), b.insert_module.as_deref())
        })
        .await;
    item?;
    // the edited item
    let (rid, eid) = (run_id.clone(), edge_id.clone());
    let it = st
        .db
        .read(move |c| {
            let all = store::items(
                c,
                &rid,
                &store::ItemQuery {
                    edge_id: Some(eid.clone()),
                    limit: 1,
                    ..Default::default()
                },
            )?;
            Ok(all["items"]
                .as_array()
                .and_then(|a| a.iter().find(|i| i["edge_id"] == eid.as_str()).cloned())
                .unwrap_or(Value::Null))
        })
        .await?;
    audit_user(
        &st,
        &u,
        "link_run.item_edit",
        Some(&edge_id),
        json!({"run_id": run_id}),
    )
    .await;
    Ok(Json(
        json!({"item": it, "run": run_value(&st, &run_id).await?}),
    ))
}

async fn start_step(st: &AppState, u: &User, run_id: &str, step: &str) -> ApiResult<Response> {
    guard_run(st, u, run_id).await?;
    let ev = events(st);
    let job =
        link_run::enqueue_step(&st.db, &st.jobs, ev.as_ref(), run_id, step, &u.actor()).await?;
    audit_user(
        st,
        u,
        &format!("link_run.{step}"),
        Some(run_id),
        json!({"job_run_id": job}),
    )
    .await;
    let run = run_value(st, run_id).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"run": run, "job_run_id": job})),
    )
        .into_response())
}

pub async fn deploy(
    State(st): State<AppState>,
    u: User,
    Path(run_id): Path<String>,
) -> ApiResult<Response> {
    start_step(&st, &u, &run_id, "deploy").await
}

pub async fn verify(
    State(st): State<AppState>,
    u: User,
    Path(run_id): Path<String>,
) -> ApiResult<Response> {
    start_step(&st, &u, &run_id, "verify").await
}

pub async fn repair(
    State(st): State<AppState>,
    u: User,
    Path(run_id): Path<String>,
) -> ApiResult<Response> {
    start_step(&st, &u, &run_id, "repair").await
}

pub async fn undo(
    State(st): State<AppState>,
    u: User,
    Path(run_id): Path<String>,
) -> ApiResult<Response> {
    start_step(&st, &u, &run_id, "undo").await
}

pub async fn cancel(
    State(st): State<AppState>,
    u: User,
    Path(run_id): Path<String>,
) -> ApiResult<Json<Value>> {
    guard_run(&st, &u, &run_id).await?;
    let ev = events(&st);
    link_run::cancel_run(&st.db, &st.jobs, ev.as_ref(), &run_id).await?;
    audit_user(&st, &u, "link_run.cancel", Some(&run_id), json!({})).await;
    Ok(Json(json!({"run": run_value(&st, &run_id).await?})))
}

// ---------------------------------------------------------------------------------------------
// Auto-Linker
// ---------------------------------------------------------------------------------------------

async fn auto_linker_value(st: &AppState, site_id: &str) -> ApiResult<Value> {
    let site = get_site(st, site_id).await?;
    let raw = site.settings.get("auto_linker").cloned();
    let cfg = match &raw {
        Some(v) => link_run::parse_auto_linker(v).unwrap_or_default(),
        None => link_run::AutoLinker::default(),
    };
    let sid = site_id.to_string();
    let schedule = st
        .db
        .read(move |c| {
            c.query_row(
                "SELECT active, cron, tz, next_run_at, last_run_at, last_job_run_id FROM site_schedules
                 WHERE site_id=?1 AND job_kind='link_run'",
                [&sid],
                |r| {
                    Ok(json!({
                        "active": r.get::<_, i64>(0)? != 0,
                        "cron": r.get::<_, String>(1)?,
                        "tz": r.get::<_, String>(2)?,
                        "next_run_at": r.get::<_, Option<String>>(3)?,
                        "last_run_at": r.get::<_, Option<String>>(4)?,
                        "last_job_run_id": r.get::<_, Option<String>>(5)?,
                    }))
                },
            )
            .optional()
            .map_err(err)
        })
        .await?;
    let sid = site_id.to_string();
    let last_run = st
        .db
        .read(move |c| {
            let id: Option<String> = c
                .query_row(
                    "SELECT id FROM link_runs WHERE site_id=?1 AND auto=1 ORDER BY created_at DESC LIMIT 1",
                    [&sid],
                    |r| r.get(0),
                )
                .optional()
                .map_err(err)?;
            match id {
                Some(i) => store::run_json(c, &i),
                None => Ok(None),
            }
        })
        .await?;
    Ok(json!({
        "auto_linker": serde_json::to_value(&cfg).unwrap_or(Value::Null),
        "configured": raw.is_some(),
        "schedule": schedule,
        "last_run": last_run,
    }))
}

pub async fn get_auto_linker(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &id).await?;
    Ok(Json(auto_linker_value(&st, &id).await?))
}

/// Save the whole `auto_linker` object (validated; defaults filled in) and sync its schedule.
pub async fn put_auto_linker(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
    JsonBody(b): JsonBody<Value>,
) -> ApiResult<Json<Value>> {
    guard_site_admin(&st, &u, &id).await?;
    let body = b.get("auto_linker").cloned().unwrap_or(b);
    let Value::Object(patch) = body else {
        return Err(ApiError::bad("body must be the auto_linker object"));
    };
    // merged into the stored object (partial updates keep the other fields)
    let site = get_site(&st, &id).await?;
    let mut merged = match site.settings.get("auto_linker") {
        Some(Value::Object(o)) => o.clone(),
        _ => serde_json::Map::new(),
    };
    for (k, v) in patch {
        if v.is_null() {
            merged.remove(&k);
        } else {
            merged.insert(k, v);
        }
    }
    let mut cfg = link_run::parse_auto_linker(&Value::Object(merged)).map_err(ApiError::bad)?;
    if cfg.enabled {
        let ws = crate::tenancy::site_ws(&st, &id).await?;
        crate::tenancy::check_feature(&st, ws.as_deref(), "auto_linker").await?;
    }
    if cfg.enabled {
        cfg.paused_reason = None;
    }
    let val = serde_json::to_value(&cfg).map_err(|e| ApiError::internal(e.to_string()))?;
    let sid = id.clone();
    st.db
        .write(move |t| {
            t.execute(
                "UPDATE sites SET settings=json_set(CASE WHEN json_valid(settings) THEN settings ELSE '{}' END, '$.auto_linker', json(?2)) WHERE id=?1",
                rusqlite::params![sid, val.to_string()],
            )
            .map_err(err)?;
            link_run::sync_auto_linker_schedule(t, &sid, Some(&cfg))
        })
        .await?;
    audit_user(&st, &u, "site.auto_linker", Some(&id), json!({})).await;
    Ok(Json(auto_linker_value(&st, &id).await?))
}

pub async fn run_auto_linker(
    State(st): State<AppState>,
    u: User,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &id).await?;
    let job = link_run::enqueue_auto_now(&st.jobs, &id, &u.actor()).await?;
    audit_user(
        &st,
        &u,
        "site.auto_linker_run",
        Some(&id),
        json!({"job_run_id": job}),
    )
    .await;
    Ok((StatusCode::ACCEPTED, Json(json!({"job_run_id": job}))).into_response())
}
