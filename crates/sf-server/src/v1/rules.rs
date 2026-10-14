//! `/rules/keywords/*` (keyword linker) and `/rules/ignore/*` (stop-list).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sf_core::types::{AnchorType, JobKind};
use sf_store::err;

use super::common::{
    JsonBody, audit_user, conflict_as, enqueue, guard_child, guard_user_site, is_http_url, is_uuid,
    query_json, query_one_json,
};
use crate::auth::User;
use crate::{ApiError, ApiResult, AppState};

const RULE_BOOLS: &[&str] = &["active"];

pub async fn list_keywords(
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
                "SELECT * FROM keyword_rules WHERE site_id=?1 ORDER BY created_at DESC, id ASC",
                &[&site_id],
                &[],
                RULE_BOOLS,
            )
        })
        .await?;
    Ok(Json(Value::Array(rows)))
}

#[derive(Deserialize)]
pub struct KeywordBody {
    keyword: String,
    target_url: String,
    #[serde(default)]
    anchor_type: Option<String>,
    #[serde(default)]
    max_links: Option<i64>,
}

fn check_anchor_type(t: &str) -> ApiResult<()> {
    AnchorType::parse_known(t)
        .map(|_| ())
        .ok_or_else(|| ApiError::bad("invalid anchor_type"))
}

fn check_max_links(n: i64) -> ApiResult<()> {
    if (1..=100).contains(&n) {
        Ok(())
    } else {
        Err(ApiError::bad(
            "max_links must be an integer between 1 and 100",
        ))
    }
}

/// Accepts all seven anchor types incl. `naked`/`cta` (v1 answered 500 for those, B1).
pub async fn create_keyword(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
    JsonBody(b): JsonBody<KeywordBody>,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &site_id).await?;
    let keyword: String = b.keyword.trim().chars().take(200).collect();
    let target = b.target_url.trim().to_string();
    if keyword.is_empty() {
        return Err(ApiError::bad("keyword must not be empty"));
    }
    let anchor_type = b.anchor_type.unwrap_or_else(|| "exact".into());
    check_anchor_type(&anchor_type)?;
    if target.len() > 2048 || !is_http_url(&target) {
        return Err(ApiError::bad("target_url must be an http(s) URL"));
    }
    let max_links = b.max_links.unwrap_or(3);
    check_max_links(max_links)?;
    let id = sf_core::new_id();
    let (id2, sid) = (id.clone(), site_id.clone());
    let row = st
        .db
        .write(move |t| {
            t.execute(
                "INSERT INTO keyword_rules(id, site_id, keyword, target_url, anchor_type, max_links)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![id2, sid, keyword, target, anchor_type, max_links],
            )
            .map_err(err)?;
            query_one_json(
                t,
                "SELECT * FROM keyword_rules WHERE id=?1",
                &[&id2],
                &[],
                RULE_BOOLS,
            )
        })
        .await
        .map_err(|e| conflict_as(e, "a rule for this keyword+target already exists"))?
        .ok_or_else(|| ApiError::internal("rule vanished"))?;
    audit_user(&st, &u, "rule.keyword.create", Some(&id), row.clone()).await;
    Ok((StatusCode::CREATED, Json(row)).into_response())
}

async fn owner_of(st: &AppState, table: &'static str, id: &str, msg: &str) -> ApiResult<String> {
    if !is_uuid(id) {
        return Err(ApiError::not_found(msg));
    }
    let id = id.to_string();
    st.db
        .read(move |c| {
            c.query_row(
                &format!("SELECT site_id FROM {table} WHERE id=?1"),
                [&id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(err)
        })
        .await?
        .ok_or_else(|| ApiError::not_found(msg))
}

#[derive(Deserialize)]
pub struct KeywordPatch {
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    max_links: Option<i64>,
    #[serde(default)]
    anchor_type: Option<String>,
}

/// Body is validated now (v1 had no schema: `max_links` 0/negative/string reached the DB, B2).
pub async fn patch_keyword(
    State(st): State<AppState>,
    u: User,
    Path(rule_id): Path<String>,
    JsonBody(b): JsonBody<KeywordPatch>,
) -> ApiResult<Json<Value>> {
    let site_id = owner_of(&st, "keyword_rules", &rule_id, "rule not found").await?;
    guard_child(&st, &u, &site_id, "rule not found").await?;
    if let Some(t) = &b.anchor_type {
        check_anchor_type(t)?;
    }
    if let Some(n) = b.max_links {
        check_max_links(n)?;
    }
    let rid = rule_id.clone();
    let (active, max_links, at) = (b.active, b.max_links, b.anchor_type.clone());
    let row = st
        .db
        .write(move |t| {
            t.execute(
                "UPDATE keyword_rules SET active=coalesce(?2, active), max_links=coalesce(?3, max_links),
                   anchor_type=coalesce(?4, anchor_type) WHERE id=?1",
                params![rid, active, max_links, at],
            )
            .map_err(err)?;
            query_one_json(
                t,
                "SELECT * FROM keyword_rules WHERE id=?1",
                &[&rid],
                &[],
                RULE_BOOLS,
            )
        })
        .await?
        .ok_or_else(|| ApiError::not_found("rule not found"))?;
    audit_user(&st, &u, "rule.keyword.patch", Some(&rule_id), row.clone()).await;
    Ok(Json(row))
}

pub async fn delete_keyword(
    State(st): State<AppState>,
    u: User,
    Path(rule_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let site_id = owner_of(&st, "keyword_rules", &rule_id, "rule not found").await?;
    guard_child(&st, &u, &site_id, "rule not found").await?;
    let rid = rule_id.clone();
    st.db
        .write(move |t| {
            t.execute("DELETE FROM keyword_rules WHERE id=?1", [&rid])
                .map_err(err)?;
            Ok(())
        })
        .await?;
    audit_user(&st, &u, "rule.keyword.delete", Some(&rule_id), json!({})).await;
    Ok(Json(json!({ "deleted": true })))
}

pub async fn run_keywords(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &site_id).await?;
    enqueue(&st, &u, JobKind::KeywordLink, &site_id, json!({})).await
}

pub async fn list_ignore(
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
                "SELECT * FROM ignore_keywords WHERE site_id=?1 ORDER BY created_at DESC, id ASC",
                &[&site_id],
                &[],
                &[],
            )
        })
        .await?;
    Ok(Json(Value::Array(rows)))
}

#[derive(Deserialize)]
pub struct IgnoreBody {
    phrase: String,
}

pub async fn create_ignore(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
    JsonBody(b): JsonBody<IgnoreBody>,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &site_id).await?;
    let phrase: String = b.phrase.trim().chars().take(200).collect();
    if phrase.is_empty() {
        return Err(ApiError::bad("phrase cannot be empty"));
    }
    let id = sf_core::new_id();
    let (id2, sid) = (id.clone(), site_id.clone());
    let row = st
        .db
        .write(move |t| {
            t.execute(
                "INSERT INTO ignore_keywords(id, site_id, phrase) VALUES(?1,?2,?3)",
                params![id2, sid, phrase],
            )
            .map_err(err)?;
            query_one_json(
                t,
                "SELECT * FROM ignore_keywords WHERE id=?1",
                &[&id2],
                &[],
                &[],
            )
        })
        .await
        .map_err(|e| conflict_as(e, "phrase already on the ignore list"))?
        .ok_or_else(|| ApiError::internal("phrase vanished"))?;
    audit_user(&st, &u, "rule.ignore.create", Some(&id), row.clone()).await;
    Ok((StatusCode::CREATED, Json(row)).into_response())
}

pub async fn delete_ignore(
    State(st): State<AppState>,
    u: User,
    Path(phrase_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let site_id = owner_of(&st, "ignore_keywords", &phrase_id, "phrase not found").await?;
    guard_child(&st, &u, &site_id, "phrase not found").await?;
    let pid = phrase_id.clone();
    st.db
        .write(move |t| {
            t.execute("DELETE FROM ignore_keywords WHERE id=?1", [&pid])
                .map_err(err)?;
            Ok(())
        })
        .await?;
    audit_user(&st, &u, "rule.ignore.delete", Some(&phrase_id), json!({})).await;
    Ok(Json(json!({ "deleted": true })))
}
