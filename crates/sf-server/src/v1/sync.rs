//! Plugin channel: `POST /sync`, `GET /suggestions/:siteId`, `POST /suggestions/:siteId/ack`,
//! `POST /suggestions/:siteId/revert-ack`.

use axum::Json;
use axum::extract::{Path, State};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sf_store::err;

use super::common::{
    EDGE_JSON, JsonBody, Params, audit_principal, guard_site, int_param, is_http_url, is_uuid,
    query_one_json, str_param, strip_tags, word_count,
};
use crate::auth::{Auth, Principal};
use crate::{ApiError, ApiResult, AppState};

const MAX_SYNC_POSTS: usize = 1000;
const BUILDERS: &[&str] = &["gutenberg", "elementor", "divi", "classic", "unknown"];

#[derive(Deserialize)]
pub struct SyncPost {
    wp_post_id: i64,
    url: String,
    slug: String,
    title: String,
    content_html: String,
    #[serde(default)]
    meta_description: Option<String>,
    post_type: String,
    post_status: String,
    #[serde(default)]
    builder: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    modified_at: Option<String>,
}

#[derive(Deserialize)]
pub struct SyncBatch {
    #[allow(dead_code)]
    domain: String,
    posts: Vec<SyncPost>,
}

/// Upserts a plugin batch in ONE transaction (v1 did 2 sequential queries per post, P2). Unchanged
/// content (same sha256) is skipped; changed content clears the stale post embedding (fixes B8);
/// non-http(s) URLs are skipped (fixes S11).
pub async fn sync(
    State(st): State<AppState>,
    Auth(who): Auth,
    q: Params,
    JsonBody(b): JsonBody<SyncBatch>,
) -> ApiResult<Json<Value>> {
    let site_id = match &who {
        Principal::Site { site_id } => site_id.clone(),
        Principal::User { .. } => str_param(&q, "site_id")
            .map(str::to_string)
            .ok_or_else(|| ApiError::bad("user sync requires ?site_id="))?,
    };
    guard_site(&st, &who, &site_id).await?;
    if b.posts.len() > MAX_SYNC_POSTS {
        return Err(ApiError::bad(format!(
            "too many posts in one batch (max {MAX_SYNC_POSTS})"
        )));
    }
    for p in &b.posts {
        if p.url.is_empty() {
            return Err(ApiError::bad(
                "body/posts/url must NOT have fewer than 1 characters",
            ));
        }
        if let Some(bl) = &p.builder
            && !BUILDERS.contains(&bl.as_str())
        {
            return Err(ApiError::bad(format!(
                "body/posts/builder must be one of {}",
                BUILDERS.join(", ")
            )));
        }
    }
    let received = b.posts.len();
    let sid = site_id.clone();
    let (upserted, skipped) = st
        .db
        .write(move |t| {
            let now = sf_core::now_rfc3339();
            let mut sel = t
                .prepare("SELECT content_hash FROM posts WHERE site_id=?1 AND url=?2")
                .map_err(err)?;
            let mut ups = t
                .prepare(
                    "INSERT INTO posts(id, site_id, wp_post_id, url, slug, title, content_html, content_text,
                        content_hash, builder, post_type, post_status, meta, word_count, published_at,
                        modified_at, crawled_at)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,coalesce(?10,'unknown'),?11,?12,?13,?14,?15,?16,?17)
                     ON CONFLICT(site_id, url) DO UPDATE SET
                        wp_post_id=excluded.wp_post_id, slug=excluded.slug, title=excluded.title,
                        content_html=excluded.content_html, content_text=excluded.content_text,
                        content_hash=excluded.content_hash, builder=coalesce(?10, posts.builder),
                        post_type=excluded.post_type, post_status=excluded.post_status,
                        meta=json_patch(posts.meta, excluded.meta), word_count=excluded.word_count,
                        published_at=excluded.published_at, modified_at=excluded.modified_at,
                        crawled_at=excluded.crawled_at, embedding=NULL",
                )
                .map_err(err)?;
            let (mut upserted, mut skipped) = (0usize, 0usize);
            for p in b.posts {
                if !is_http_url(&p.url) {
                    skipped += 1;
                    continue;
                }
                let hash = sf_core::crypto::sha256_hex(p.content_html.as_bytes());
                let existing: Option<String> = sel
                    .query_row(params![sid, p.url], |r| r.get(0))
                    .optional()
                    .map_err(err)?;
                if existing.as_deref() == Some(hash.as_str()) {
                    skipped += 1;
                    continue;
                }
                let text = strip_tags(&p.content_html);
                let meta = match p.meta_description.filter(|d| !d.is_empty()) {
                    Some(d) => json!({ "description": d }),
                    None => json!({}),
                };
                ups.execute(params![
                    sf_core::new_id(),
                    sid,
                    p.wp_post_id,
                    p.url,
                    p.slug,
                    p.title,
                    p.content_html,
                    text,
                    hash,
                    p.builder,
                    p.post_type,
                    p.post_status,
                    meta.to_string(),
                    word_count(&text),
                    p.published_at,
                    p.modified_at,
                    now
                ])
                .map_err(err)?;
                upserted += 1;
            }
            t.execute(
                "UPDATE sites SET last_sync_at=?2 WHERE id=?1",
                params![sid, now],
            )
            .map_err(err)?;
            Ok((upserted, skipped))
        })
        .await?;
    audit_principal(
        &st,
        &who,
        "sync",
        Some(&site_id),
        json!({ "received": received, "upserted": upserted }),
    )
    .await;
    Ok(Json(
        json!({ "received": received, "upserted": upserted, "skipped": skipped }),
    ))
}

/// Approved edges for the plugin. Paged (`?limit` default/max 5000, `?offset`) instead of unbounded.
pub async fn suggestions(
    State(st): State<AppState>,
    Auth(who): Auth,
    Path(site_id): Path<String>,
    q: Params,
) -> ApiResult<Json<Value>> {
    guard_site(&st, &who, &site_id).await?;
    let limit = int_param(&q, "limit", 5000, 1, 5000);
    let offset = int_param(&q, "offset", 0, 0, i64::MAX / 2);
    let rows = st
        .db
        .read(move |c| {
            let mut stmt = c
                .prepare(
                    "SELECT e.id, sp.wp_post_id, tp.url, e.anchor, g.idx, e.insert_module
                       FROM link_edges e
                       JOIN posts sp ON sp.id = e.source_post_id
                       JOIN posts tp ON tp.id = e.target_post_id
                       LEFT JOIN paragraphs g ON g.id = e.paragraph_id
                      WHERE e.site_id=?1 AND e.status='approved' AND sp.wp_post_id IS NOT NULL
                        AND (tp.url LIKE 'http://%' OR tp.url LIKE 'https://%')
                      ORDER BY e.created_at ASC, e.id ASC
                      LIMIT ?2 OFFSET ?3",
                )
                .map_err(err)?;
            let rows = stmt
                .query_map(params![site_id, limit, offset], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                    ))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(rows)
        })
        .await?;
    let out: Vec<Value> = rows
        .into_iter()
        .filter(|r| is_http_url(&r.2))
        .map(|(edge_id, wp, url, anchor, idx, module)| {
            json!({
                "edge_id": edge_id,
                "wp_post_id": wp,
                "target_url": url,
                "anchor": anchor,
                "paragraph_index": idx,
                "replacement_html": Value::Null,
                "insert_module": module.unwrap_or_else(|| "native".into()),
            })
        })
        .collect();
    Ok(Json(Value::Array(out)))
}

#[derive(Deserialize)]
pub struct AckBody {
    edge_id: String,
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

/// Only `approved -> applied|failed` (fixes v1 S7). Repeating the same ack is idempotent; any other
/// current status answers 409.
pub async fn ack(
    State(st): State<AppState>,
    Auth(who): Auth,
    Path(site_id): Path<String>,
    JsonBody(b): JsonBody<AckBody>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&site_id) || !is_uuid(&b.edge_id) {
        return Err(ApiError::not_found("edge not found"));
    }
    guard_site(&st, &who, &site_id).await?;
    let (sid, eid, ok) = (site_id.clone(), b.edge_id.clone(), b.ok);
    let error: String = b
        .error
        .unwrap_or_else(|| "plugin reported failure".into())
        .chars()
        .take(2000)
        .collect();
    let res: Result<Value, ApiError> = st
        .db
        .write(move |t| {
            let status: Option<String> = t
                .query_row(
                    "SELECT status FROM link_edges WHERE id=?1 AND site_id=?2",
                    params![eid, sid],
                    |r| r.get(0),
                )
                .optional()
                .map_err(err)?;
            let Some(status) = status else {
                return Ok(Err(ApiError::not_found("edge not found")));
            };
            let target = if ok { "applied" } else { "failed" };
            if status == "approved" {
                if ok {
                    t.execute(
                        "UPDATE link_edges SET status='applied', applied_at=?2, error=NULL WHERE id=?1",
                        params![eid, sf_core::now_rfc3339()],
                    )
                } else {
                    t.execute(
                        "UPDATE link_edges SET status='failed', error=?2 WHERE id=?1",
                        params![eid, error],
                    )
                }
                .map_err(err)?;
            } else if status != target {
                return Ok(Err(ApiError::conflict(format!(
                    "edge is '{status}', only approved edges can be acknowledged"
                ))));
            }
            let row = query_one_json(
                t,
                "SELECT * FROM link_edges WHERE id=?1",
                &[&eid],
                EDGE_JSON,
                &[],
            )?;
            Ok(row.ok_or_else(|| ApiError::not_found("edge not found")))
        })
        .await?;
    let row = res?;
    audit_principal(
        &st,
        &who,
        "suggestion.ack",
        Some(&b.edge_id),
        json!({ "ok": b.ok }),
    )
    .await;
    Ok(Json(row))
}

#[derive(Deserialize)]
pub struct RevertAckBody {
    edge_id: String,
}

pub async fn revert_ack(
    State(st): State<AppState>,
    Auth(who): Auth,
    Path(site_id): Path<String>,
    JsonBody(b): JsonBody<RevertAckBody>,
) -> ApiResult<Json<Value>> {
    if !is_uuid(&site_id) || !is_uuid(&b.edge_id) {
        return Err(ApiError::not_found("edge not found"));
    }
    guard_site(&st, &who, &site_id).await?;
    let (sid, eid) = (site_id.clone(), b.edge_id.clone());
    let row = st
        .db
        .write(move |t| {
            // L-03: only edges the plugin itself applied (no server-side WP REST snapshot) and
            // that SiloForge manages (not crawled `existing` links) can be acknowledged as reverted.
            let n = t
                .execute(
                    "UPDATE link_edges SET status='reverted', applied_at=NULL
                      WHERE id=?1 AND site_id=?2 AND status='applied'
                        AND snapshot_id IS NULL AND origin <> 'existing'",
                    params![eid, sid],
                )
                .map_err(err)?;
            if n == 0 {
                return Ok(None);
            }
            query_one_json(
                t,
                "SELECT * FROM link_edges WHERE id=?1",
                &[&eid],
                EDGE_JSON,
                &[],
            )
        })
        .await?
        .ok_or_else(|| ApiError::not_found("no applied edge to revert"))?;
    audit_principal(
        &st,
        &who,
        "suggestion.revert_ack",
        Some(&b.edge_id),
        json!({ "source": match &who {
            crate::auth::Principal::Site { .. } => "plugin",
            crate::auth::Principal::User { .. } => "user",
        } }),
    )
    .await;
    Ok(Json(row))
}
