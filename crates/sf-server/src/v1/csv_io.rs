//! `POST /csv/import/:siteId` (raw `text/csv` body) and `GET /csv/export/:siteId`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sf_store::err;

use super::common::{
    audit_user, csv_line, csv_response, get_site, guard_user_site, is_http_url, safe_filename_part,
    strip_tags, word_count,
};
use super::reports::CSV_MAX_ROWS;
use crate::auth::User;
use crate::{ApiError, ApiResult, AppState};

const MAX_IMPORT_ROWS: usize = 100_000;

/// v1's tiny CSV parser: quoted fields, `""` escapes, embedded commas/newlines, CRLF or LF.
pub fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut it = text.chars().peekable();
    while let Some(ch) = it.next() {
        if in_quotes {
            if ch == '"' {
                if it.peek() == Some(&'"') {
                    field.push('"');
                    it.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(ch);
            }
            continue;
        }
        match ch {
            '"' => in_quotes = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Upserts posts from CSV (`url`, `content_html`, optional `title`) in ONE transaction.
pub async fn import(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &site_id).await?;
    crate::tenancy::check_posts(&st, &site_id).await?;
    let raw = String::from_utf8(body.to_vec()).map_err(|_| ApiError::bad("CSV must be UTF-8"))?;
    if raw.trim().is_empty() {
        return Err(ApiError::bad("empty CSV body"));
    }
    let rows = parse_csv(&raw);
    if rows.len() < 2 {
        return Err(ApiError::bad(
            "CSV must have a header row and at least one data row",
        ));
    }
    if rows.len() > MAX_IMPORT_ROWS + 1 {
        return Err(ApiError::bad(format!(
            "CSV has too many rows (max {MAX_IMPORT_ROWS})"
        )));
    }
    let header: Vec<String> = rows
        .first()
        .map(|h| {
            h.iter()
                .map(|c| c.trim().trim_start_matches('\u{feff}').to_lowercase())
                .collect()
        })
        .unwrap_or_default();
    let pos = |name: &str| header.iter().position(|h| h == name);
    let (Some(i_url), Some(i_content)) = (pos("url"), pos("content_html")) else {
        return Err(ApiError::bad(
            "CSV header must include at least: url, content_html (title optional)",
        ));
    };
    let i_title = pos("title");
    let received = rows.len() - 1;
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
                    "INSERT INTO posts(id, site_id, url, title, content_html, content_text, content_hash,
                        builder, post_type, post_status, word_count, crawled_at)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,'unknown','post','publish',?8,?9)
                     ON CONFLICT(site_id, url) DO UPDATE SET
                        title=excluded.title, content_html=excluded.content_html,
                        content_text=excluded.content_text, content_hash=excluded.content_hash,
                        word_count=excluded.word_count, embedding=NULL, crawled_at=excluded.crawled_at",
                )
                .map_err(err)?;
            let (mut upserted, mut skipped) = (0usize, 0usize);
            for cells in rows.iter().skip(1) {
                if cells.len() == 1 && cells.first().is_some_and(|c| c.trim().is_empty()) {
                    continue;
                }
                let url = cells.get(i_url).map(|s| s.trim()).unwrap_or("");
                if url.is_empty() || !is_http_url(url) {
                    skipped += 1;
                    continue;
                }
                let html = cells.get(i_content).map(String::as_str).unwrap_or("");
                let title = i_title
                    .and_then(|i| cells.get(i))
                    .map(|s| s.trim())
                    .unwrap_or("");
                let hash = sf_core::crypto::sha256_hex(html.as_bytes());
                let existing: Option<String> = sel
                    .query_row(params![sid, url], |r| r.get(0))
                    .optional()
                    .map_err(err)?;
                if existing.as_deref() == Some(hash.as_str()) {
                    skipped += 1;
                    continue;
                }
                let text = strip_tags(html);
                ups.execute(params![
                    sf_core::new_id(),
                    sid,
                    url,
                    title,
                    html,
                    text,
                    hash,
                    word_count(&text),
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
    audit_user(
        &st,
        &u,
        "csv.import",
        Some(&site_id),
        json!({ "received": received, "upserted": upserted }),
    )
    .await;
    Ok(Json(
        json!({ "received": received, "upserted": upserted, "skipped": skipped }),
    ))
}

pub async fn export(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
) -> ApiResult<Response> {
    guard_user_site(&st, &u, &site_id).await?;
    let site = get_site(&st, &site_id).await?;
    let body = st
        .db
        .read(move |c| {
            let mut stmt = c
                .prepare(
                    "SELECT sp.url, tp.url, e.anchor, e.anchor_type, e.status, e.insert_module
                       FROM link_edges e
                       JOIN posts sp ON sp.id = e.source_post_id
                       JOIN posts tp ON tp.id = e.target_post_id
                      WHERE e.site_id=?1 AND e.status IN ('approved','applied')
                      ORDER BY e.applied_at DESC NULLS LAST, e.decided_at DESC NULLS LAST, e.id ASC
                      LIMIT ?2",
                )
                .map_err(err)?;
            let mut lines =
                vec!["source_url,target_url,anchor,anchor_type,status,insert_module".to_string()];
            let rows = stmt
                .query_map(params![site_id, CSV_MAX_ROWS], |r| {
                    Ok(csv_line(&[
                        &r.get::<_, String>(0)?,
                        &r.get::<_, String>(1)?,
                        &r.get::<_, String>(2)?,
                        &r.get::<_, String>(3)?,
                        &r.get::<_, String>(4)?,
                        &r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    ]))
                })
                .map_err(err)?;
            for line in rows {
                lines.push(line.map_err(err)?);
            }
            Ok(lines.join("\r\n"))
        })
        .await?;
    Ok(csv_response(
        &format!("siloforge-edges-{}.csv", safe_filename_part(&site.domain)),
        body,
    ))
}

#[cfg(test)]
mod tests {
    use super::parse_csv;

    #[test]
    fn parses_quotes_and_newlines() {
        let rows =
            parse_csv("url,title\r\n\"https://a.com/x\",\"He said \"\"hi\"\", ok\nline2\"\nlast,1");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1][1], "He said \"hi\", ok\nline2");
        assert_eq!(rows[2], vec!["last".to_string(), "1".to_string()]);
        for junk in ["\"", "\"\"\"", ",,,\n\n", "\u{feff}a,b"] {
            let _ = parse_csv(junk);
        }
    }
}
