//! `keyword_link`: keyword rules → `sf_engine::linker` → suggested edges with
//! `origin='keyword_linker'` (v1 semantics: `composite_score = 0.5`, `score_breakdown =
//! {"keyword_rule": true}`, anchor = rule keyword). Word-boundary matching, blocked spans and
//! existing (source, target) pairs of any live/pending/rejected status are respected, and each
//! source post gets at most the site's `max_links_per_post` keyword suggestions per run.
//! Stats: `{created, max_per_source}`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rusqlite::params;
use serde_json::{Value, json};
use sf_core::types::AnchorType;
use sf_engine::linker::{
    KeywordRule, LinkerOptions, LinkerParagraph, LinkerPost, run_keyword_linker, word_boundary_hit,
};
use sf_store::err;

use super::chunk::SPLIT_MODE;
use super::common::load_site;
use crate::ctx::{Job, JobContext, JobResult, compute};

pub async fn run(ctx: &Arc<JobContext>, job: &Job) -> JobResult<Value> {
    let site = load_site(ctx, job.site()?).await?;
    let sid = site.id.clone();
    let (rules, ignore, pairs, used) = ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare(
                    "SELECT id, keyword, target_url, anchor_type, max_links, active FROM keyword_rules
                     WHERE site_id=?1 AND active=1 ORDER BY created_at, id",
                )
                .map_err(err)?;
            let rules: Vec<KeywordRule> = st
                .query_map([&sid], |r| {
                    Ok(KeywordRule {
                        id: r.get(0)?,
                        keyword: r.get(1)?,
                        target_url: r.get(2)?,
                        anchor_type: AnchorType::parse(&r.get::<_, String>(3)?),
                        max_links: usize::try_from(r.get::<_, i64>(4)?).unwrap_or(0),
                        active: r.get::<_, i64>(5)? != 0,
                    })
                })
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            let mut st = c
                .prepare("SELECT phrase FROM ignore_keywords WHERE site_id=?1")
                .map_err(err)?;
            let ignore: Vec<String> = st
                .query_map([&sid], |r| r.get::<_, String>(0))
                .map_err(err)?
                .filter_map(|r| r.ok())
                .map(|p| p.trim().to_lowercase())
                .collect();
            let mut st = c
                .prepare(
                    "SELECT e.source_post_id, e.target_post_id, e.status, pa.idx FROM link_edges e
                     LEFT JOIN paragraphs pa ON pa.id = e.paragraph_id
                     WHERE e.site_id=?1 AND (e.status IN ('suggested','approved','applied','rejected') OR e.origin='existing')",
                )
                .map_err(err)?;
            let mut pairs = HashSet::new();
            let mut used = HashSet::new();
            let rows = st
                .query_map([&sid], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                    ))
                })
                .map_err(err)?;
            for row in rows {
                let (s, t, status, idx) = row.map_err(err)?;
                if status != "rejected"
                    && let Some(i) = idx.and_then(|i| usize::try_from(i).ok())
                {
                    used.insert((s.clone(), i));
                }
                pairs.insert((s, t));
            }
            Ok((rules, ignore, pairs, used))
        })
        .await?;
    if rules.is_empty() {
        return Ok(json!({"created": 0, "rules": 0}));
    }
    job.progress("match", 10.0).await;
    // Candidate sources: published posts whose text contains any rule keyword; targets by url.
    let keywords: Vec<String> = rules.iter().map(|r| r.keyword.clone()).collect();
    let mut posts: Vec<LinkerPost> = Vec::new();
    let mut para_ids: HashMap<(String, usize), String> = HashMap::new();
    let mut after = String::new();
    loop {
        job.checkpoint().await?;
        let (sid, a) = (site.id.clone(), after.clone());
        let batch: Vec<(String, String, String, String, String, Option<String>)> = ctx
            .db
            .read(move |c| {
                let mut st = c
                    .prepare(
                        "SELECT id, url, post_status, content_text, content_html, content_raw FROM posts
                         WHERE site_id=?1 AND id > ?2 ORDER BY id LIMIT 200",
                    )
                    .map_err(err)?;
                let v = st
                    .query_map(params![sid, a], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                    })
                    .map_err(err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(err)?;
                Ok(v)
            })
            .await?;
        let Some(last) = batch.last() else { break };
        after = last.0.clone();
        let kws = keywords.clone();
        let built: Vec<(LinkerPost, Vec<(usize, String)>)> = compute(move || {
            batch
                .into_iter()
                .map(|(id, url, status, text, html, raw)| {
                    let published = status == "publish";
                    let hit = published && kws.iter().any(|k| word_boundary_hit(&text, k));
                    let mut paragraphs = Vec::new();
                    let mut hashes = Vec::new();
                    if hit {
                        let source = match raw {
                            Some(r) if !r.trim().is_empty() => r,
                            _ => html,
                        };
                        for p in
                            sf_engine::html::paragraphs(&source, SPLIT_MODE).unwrap_or_default()
                        {
                            hashes.push((p.index, p.hash.clone()));
                            paragraphs.push(LinkerParagraph {
                                idx: p.index,
                                text: p.text,
                                blocked: p.blocked,
                            });
                        }
                    }
                    (
                        LinkerPost {
                            id,
                            url,
                            published,
                            paragraphs,
                        },
                        hashes,
                    )
                })
                .collect()
        })
        .await?;
        for (p, hashes) in built {
            if !hashes.is_empty() {
                let pid = p.id.clone();
                let rows: Vec<(String, i64, String)> = ctx
                    .db
                    .read(move |c| {
                        let mut st = c
                            .prepare("SELECT id, idx, hash FROM paragraphs WHERE post_id=?1")
                            .map_err(err)?;
                        let v = st
                            .query_map([&pid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                            .map_err(err)?
                            .filter_map(|r| r.ok())
                            .collect();
                        Ok(v)
                    })
                    .await?;
                for (idx, hash) in hashes {
                    if let Some((id, _, _)) = rows
                        .iter()
                        .find(|(_, i, h)| usize::try_from(*i).ok() == Some(idx) && *h == hash)
                    {
                        para_ids.insert((p.id.clone(), idx), id.clone());
                    }
                }
            }
            posts.push(p);
        }
    }
    job.progress("link", 60.0).await;
    // #20: keyword rules no longer bypass the site's density budget entirely: at most
    // `max_links_per_post` keyword suggestions per source post per run (resolved like plan).
    let limits = sf_engine::density::resolve_limits(&super::plan::density_settings(&site));
    let max_per_source = Some(limits.max_per_post);
    let opts = LinkerOptions {
        existing_pairs: pairs,
        used_paragraphs: used,
        max_per_source,
    };
    let edges = compute(move || run_keyword_linker(&posts, &rules, &ignore, &opts)).await?;
    let sid = site.id.clone();
    let created = ctx
        .db
        .write(move |t| {
            let mut st = t
                .prepare(
                    "INSERT INTO link_edges(id, site_id, source_post_id, target_post_id, paragraph_id, anchor, anchor_type,
                       composite_score, score_breakdown, insert_module, status, origin)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'native','suggested','keyword_linker')
                     ON CONFLICT(site_id, source_post_id, target_post_id, anchor) DO NOTHING",
                )
                .map_err(err)?;
            let mut n = 0usize;
            for e in &edges {
                let pid = para_ids.get(&(e.source_post_id.clone(), e.paragraph_idx)).cloned();
                n += st
                    .execute(params![
                        sf_core::new_id(),
                        sid,
                        e.source_post_id,
                        e.target_post_id,
                        pid,
                        e.anchor,
                        e.anchor_type.as_str(),
                        e.composite_score,
                        json!({"keyword_rule": true, "rule_id": e.rule_id}).to_string()
                    ])
                    .map_err(err)?;
            }
            Ok(n)
        })
        .await?;
    job.info(format!("keyword linker created {created} suggestions"))
        .await;
    Ok(
        json!({"created": created, "max_per_source": max_per_source, "progress": {"phase": "done", "pct": 100}}),
    )
}
