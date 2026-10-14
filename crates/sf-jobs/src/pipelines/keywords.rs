//! Keyword extraction (1–3-gram layered keywords, `sf_engine::nlp::extract_keywords`). Only posts
//! whose content changed since the last extraction are processed (`meta.kw_hash`); their non-GSC
//! keyword rows are replaced in batched transactions (v1 never deleted stale keywords).
//!
//! Perf: the replacement is a diff against the stored rows (delete removed, insert added, update
//! changed scores, leave identical rows alone). Deleting + re-inserting every row churned the
//! random-UUID primary-key index and made this the slowest plan phase on large sites
//! (docs/testing/performance.md).

use std::collections::HashMap;
use std::time::Instant;

use rusqlite::params;
use serde_json::Value;
use sf_engine::nlp::{KeywordInput, NLP_VERSION, extract_keywords};
use sf_store::err;

use crate::ctx::{Job, JobContext, JobResult, compute};

const BATCH: usize = 200;

/// Extract keywords for changed posts. Returns the number of posts processed.
pub async fn extract_site(
    ctx: &JobContext,
    job: &Job,
    site_id: &str,
    phase: &str,
    pct: (f64, f64),
) -> JobResult<usize> {
    let mut done = 0usize;
    let mut after = String::new();
    let stamp_suffix = format!(":kw{NLP_VERSION}");
    let (mut extract_ms, mut write_ms) = (0u128, 0u128);
    let mut changes = WriteStats::default();
    loop {
        job.checkpoint().await?;
        let (sid, a, suf) = (site_id.to_string(), after.clone(), stamp_suffix.clone());
        let rows: Vec<(String, String, String, String, String)> = ctx
            .db
            .read(move |c| {
                let mut st = c
                    .prepare(
                        "SELECT id, title, content_text, meta, content_hash FROM posts
                         WHERE site_id=?1 AND id > ?2
                           AND ifnull(json_extract(CASE WHEN json_valid(meta) THEN meta ELSE '{}' END, '$.kw_hash'), '')
                               <> content_hash || ?3
                         ORDER BY id LIMIT ?4",
                    )
                    .map_err(err)?;
                let v = st
                    .query_map(params![sid, a, suf, BATCH as i64], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                    })
                    .map_err(err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(err)?;
                Ok(v)
            })
            .await?;
        let Some(last) = rows.last() else { break };
        after = last.0.clone();
        let suf = stamp_suffix.clone();
        let t_extract = Instant::now();
        let extracted: Vec<(String, String, Vec<(String, &'static str, f64)>)> =
            compute(move || {
                use rayon::prelude::*;
                rows.par_iter()
                    .map(|(id, title, text, meta, hash)| {
                        let meta: Value = serde_json::from_str(meta).unwrap_or(Value::Null);
                        let headings: Vec<String> = meta
                            .get("h_tags")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default();
                        let desc = meta
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let kws = extract_keywords(&KeywordInput {
                            title,
                            headings: &headings,
                            meta_description: desc,
                            body: text,
                        });
                        let mut seen = std::collections::HashSet::new();
                        let kws = kws
                            .into_iter()
                            .filter(|k| seen.insert((k.keyword.clone(), k.source.as_str())))
                            .map(|k| (k.keyword, k.source.as_str(), k.score))
                            .collect();
                        (id.clone(), format!("{hash}{suf}"), kws)
                    })
                    .collect()
            })
            .await?;
        extract_ms += t_extract.elapsed().as_millis();
        let n = extracted.len();
        let t_write = Instant::now();
        let w = ctx
            .db
            .write(move |t| {
                let mut w = WriteStats::default();
                for (post_id, stamp, kws) in &extracted {
                    w.add(&replace_keywords(t, post_id, kws)?);
                    t.prepare_cached(
                        "UPDATE posts SET meta=json_set(CASE WHEN json_valid(meta) THEN meta ELSE '{}' END, '$.kw_hash', ?2)
                         WHERE id=?1",
                    )
                    .map_err(err)?
                    .execute(params![post_id, stamp])
                    .map_err(err)?;
                }
                Ok(w)
            })
            .await?;
        write_ms += t_write.elapsed().as_millis();
        changes.add(&w);
        done += n;
        job.progress(phase, pct.0 + (pct.1 - pct.0) * 0.5).await;
    }
    if done > 0 {
        job.info(format!(
            "keywords: {done} posts (extract {extract_ms} ms, write {write_ms} ms; rows +{} ~{} -{} ={})",
            changes.inserted, changes.updated, changes.deleted, changes.unchanged
        ))
        .await;
    }
    job.progress(phase, pct.1).await;
    Ok(done)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct WriteStats {
    inserted: usize,
    updated: usize,
    deleted: usize,
    unchanged: usize,
}

impl WriteStats {
    fn add(&mut self, o: &WriteStats) {
        self.inserted += o.inserted;
        self.updated += o.updated;
        self.deleted += o.deleted;
        self.unchanged += o.unchanged;
    }
}

/// Make the post's non-GSC keyword rows equal `kws` with the fewest writes: rows that are gone are
/// deleted, new ones inserted, changed scores updated in place, identical rows left untouched.
fn replace_keywords(
    t: &rusqlite::Transaction<'_>,
    post_id: &str,
    kws: &[(String, &'static str, f64)],
) -> sf_core::Result<WriteStats> {
    let mut w = WriteStats::default();
    let mut old: HashMap<(String, String), f64> = HashMap::new();
    {
        let mut q = t
            .prepare_cached(
                "SELECT keyword, source, score FROM keywords WHERE post_id=?1 AND source <> 'gsc'",
            )
            .map_err(err)?;
        let rows = q
            .query_map([post_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                ))
            })
            .map_err(err)?;
        for row in rows {
            let (k, s, sc) = row.map_err(err)?;
            old.insert((k, s), sc);
        }
    }
    let mut ins = t
        .prepare_cached(
            "INSERT INTO keywords(id, post_id, keyword, source, score) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(post_id, keyword, source) DO UPDATE SET score=excluded.score",
        )
        .map_err(err)?;
    let mut upd = t
        .prepare_cached(
            "UPDATE keywords SET score=?4 WHERE post_id=?1 AND keyword=?2 AND source=?3",
        )
        .map_err(err)?;
    for (kw, src, score) in kws {
        // `remove`, so whatever is left in `old` afterwards is stale
        match old.remove(&(kw.clone(), (*src).to_string())) {
            None => {
                ins.execute(params![super::ids::row_id(), post_id, kw, src, score])
                    .map_err(err)?;
                w.inserted += 1;
            }
            Some(prev) if prev.to_bits() == score.to_bits() => w.unchanged += 1,
            Some(_) => {
                upd.execute(params![post_id, kw, src, score]).map_err(err)?;
                w.updated += 1;
            }
        }
    }
    if !old.is_empty() {
        let mut del = t
            .prepare_cached("DELETE FROM keywords WHERE post_id=?1 AND keyword=?2 AND source=?3")
            .map_err(err)?;
        for (kw, src) in old.keys() {
            w.deleted += del.execute(params![post_id, kw, src]).map_err(err)?;
        }
    }
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(t: &rusqlite::Transaction<'_>) -> Vec<(String, String, String, f64)> {
        t.prepare("SELECT post_id, keyword, source, score FROM keywords ORDER BY post_id, keyword")
            .and_then(|mut q| {
                q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap_or_default()
    }

    #[test]
    fn replace_keywords_writes_only_the_diff() {
        let Ok(mut c) = rusqlite::Connection::open_in_memory() else {
            return;
        };
        let setup = c.execute_batch(
            "CREATE TABLE keywords(id TEXT PRIMARY KEY, post_id TEXT NOT NULL, keyword TEXT NOT NULL,
               source TEXT NOT NULL, score REAL NOT NULL DEFAULT 0, striking INTEGER NOT NULL DEFAULT 0,
               UNIQUE(post_id, keyword, source));
             INSERT INTO keywords(id, post_id, keyword, source, score) VALUES
               ('1','p','keep','body',0.5), ('2','p','change','body',0.5), ('3','p','gone','title',1.0),
               ('4','p','gsc kw','gsc',0.0), ('5','q','other','body',0.1);",
        );
        assert!(setup.is_ok());
        let Ok(t) = c.transaction() else { return };
        let kws = vec![
            ("keep".to_string(), "body", 0.5),
            ("change".to_string(), "body", 0.7),
            ("new".to_string(), "htag", 0.9),
        ];
        let w = replace_keywords(&t, "p", &kws).ok();
        assert_eq!(
            w,
            Some(WriteStats {
                inserted: 1,
                updated: 1,
                deleted: 1,
                unchanged: 1
            })
        );
        let got = rows(&t);
        let names: Vec<(&str, &str)> = got.iter().map(|r| (r.0.as_str(), r.1.as_str())).collect();
        assert_eq!(
            names,
            vec![
                ("p", "change"),
                ("p", "gsc kw"),
                ("p", "keep"),
                ("p", "new"),
                ("q", "other")
            ]
        );
        assert!(
            got.iter()
                .any(|r| r.1 == "change" && (r.3 - 0.7).abs() < 1e-12)
        );
        // an identical second pass writes nothing
        let w2 = replace_keywords(&t, "p", &kws).ok();
        assert_eq!(
            w2,
            Some(WriteStats {
                unchanged: 3,
                ..WriteStats::default()
            })
        );
    }
}
