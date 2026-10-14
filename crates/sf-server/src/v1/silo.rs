//! `/silo/:siteId/clusters` list / export / import.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sf_store::err;

use super::common::{JsonBody, audit_user, guard_user_site, query_json, v1_collation};
use crate::auth::User;
use crate::{ApiError, ApiResult, AppState};

const MAX_CLUSTERS: usize = 10_000;
const MAX_MEMBER_URLS: usize = 200_000;

pub async fn list(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &site_id).await?;
    let rows = st
        .db
        .read(move |c| {
            v1_collation(c)?;
            query_json(
                c,
                "SELECT k.id, k.label, k.source, pp.url AS pillar_url, count(m.id) AS members
                   FROM clusters k
                   LEFT JOIN posts pp ON pp.id = k.pillar_post_id
                   LEFT JOIN posts m ON m.cluster_id = k.id
                  WHERE k.site_id=?1
                  GROUP BY k.id
                  ORDER BY k.source, k.label COLLATE V1, k.id",
                &[&site_id],
                &[],
                &[],
            )
        })
        .await?;
    Ok(Json(Value::Array(rows)))
}

pub async fn export(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &site_id).await?;
    let v = st
        .db
        .read(move |c| {
            v1_collation(c)?;
            let mut stmt = c
                .prepare(
                    "SELECT k.id, k.label, pp.url FROM clusters k
                       LEFT JOIN posts pp ON pp.id = k.pillar_post_id
                      WHERE k.site_id=?1 ORDER BY k.label COLLATE V1, k.id",
                )
                .map_err(err)?;
            let clusters = stmt
                .query_map([&site_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            let mut members: HashMap<String, Vec<String>> = HashMap::new();
            let mut stmt = c
                .prepare(
                    "SELECT p.cluster_id, p.url FROM posts p JOIN clusters k ON k.id = p.cluster_id
                      WHERE k.site_id=?1 ORDER BY p.url COLLATE V1",
                )
                .map_err(err)?;
            let rows = stmt
                .query_map([&site_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(err)?;
            for row in rows {
                let (cid, url) = row.map_err(err)?;
                members.entry(cid).or_default().push(url);
            }
            let out: Vec<Value> = clusters
                .into_iter()
                .map(|(id, label, pillar)| {
                    json!({
                        "label": label,
                        "pillar_url": pillar,
                        "member_urls": members.remove(&id).unwrap_or_default(),
                    })
                })
                .collect();
            Ok(json!({ "clusters": out }))
        })
        .await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct ClusterEntry {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    pillar_url: Option<String>,
    #[serde(default)]
    member_urls: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct ImportBody {
    clusters: Vec<ClusterEntry>,
}

/// Replaces the site's custom clusters in ONE transaction (v1 had none, B4): all or nothing.
pub async fn import(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
    JsonBody(b): JsonBody<ImportBody>,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &site_id).await?;
    if b.clusters.len() > MAX_CLUSTERS {
        return Err(ApiError::bad(format!(
            "too many clusters (max {MAX_CLUSTERS})"
        )));
    }
    let urls: usize = b
        .clusters
        .iter()
        .map(|c| c.member_urls.as_ref().map_or(0, Vec::len))
        .sum();
    if urls > MAX_MEMBER_URLS {
        return Err(ApiError::bad(format!(
            "too many member_urls (max {MAX_MEMBER_URLS})"
        )));
    }
    let sid = site_id.clone();
    let (clusters, members, skipped) = st
        .db
        .write(move |t| {
            t.execute(
                "DELETE FROM clusters WHERE site_id=?1 AND source='custom'",
                [&sid],
            )
            .map_err(err)?;
            let mut ins = t
                .prepare(
                    "INSERT INTO clusters(id, site_id, label, source) VALUES(?1,?2,?3,'custom')",
                )
                .map_err(err)?;
            let mut upd = t
                .prepare("UPDATE posts SET cluster_id=?1 WHERE site_id=?2 AND url=?3")
                .map_err(err)?;
            let mut find = t
                .prepare("SELECT id FROM posts WHERE site_id=?1 AND url=?2")
                .map_err(err)?;
            let (mut clusters, mut members, mut skipped) = (0usize, 0usize, 0usize);
            for entry in b.clusters {
                let cid = sf_core::new_id();
                let label: String = entry.label.unwrap_or_default().chars().take(500).collect();
                ins.execute(params![cid, sid, label]).map_err(err)?;
                clusters += 1;
                for url in entry.member_urls.unwrap_or_default() {
                    if upd.execute(params![cid, sid, url]).map_err(err)? > 0 {
                        members += 1;
                    } else {
                        skipped += 1;
                    }
                }
                if let Some(pillar) = entry.pillar_url.filter(|p| !p.is_empty()) {
                    let pid: Option<String> = find
                        .query_row(params![sid, pillar], |r| r.get(0))
                        .optional()
                        .map_err(err)?;
                    if let Some(pid) = pid {
                        t.execute(
                            "UPDATE posts SET cluster_id=?1 WHERE id=?2",
                            params![cid, pid],
                        )
                        .map_err(err)?;
                        t.execute(
                            "UPDATE clusters SET pillar_post_id=?1 WHERE id=?2",
                            params![pid, cid],
                        )
                        .map_err(err)?;
                    }
                }
            }
            Ok((clusters, members, skipped))
        })
        .await?;
    audit_user(
        &st,
        &u,
        "silo.import",
        Some(&site_id),
        json!({ "clusters": clusters, "members": members, "skipped": skipped }),
    )
    .await;
    Ok(Json(
        json!({ "clusters": clusters, "members": members, "skipped": skipped }),
    ))
}
