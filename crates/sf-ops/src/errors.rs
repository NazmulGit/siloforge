//! Error tracker: groups repeated failures by fingerprint in the `errors` table.
//!
//! The fingerprint is `sha256(source + "|" + normalized message)[..16]`, where normalization replaces
//! UUIDs, long hex runs, numbers and quoted URLs' query strings with placeholders, so "post 123 failed"
//! and "post 456 failed" count as one error with `count = 2`.

use std::sync::LazyLock;

use regex::Regex;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
use sf_core::Result;
use sf_store::{Db, err, row_to_json};

/// Longest message stored (characters).
const MAX_MESSAGE: usize = 2000;

static UUID_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b").ok()
});
static HEX_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:0x)?[0-9a-f]{12,}\b").ok());
static NUM_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\d+(?:\.\d+)?").ok());

/// Normalizes a message for fingerprinting (ids, hex and numbers become placeholders).
pub fn normalize(message: &str) -> String {
    let mut s: String = message.chars().take(MAX_MESSAGE).collect();
    if let Some(re) = UUID_RE.as_ref() {
        s = re.replace_all(&s, "<uuid>").into_owned();
    }
    if let Some(re) = HEX_RE.as_ref() {
        s = re.replace_all(&s, "<hex>").into_owned();
    }
    if let Some(re) = NUM_RE.as_ref() {
        s = re.replace_all(&s, "<n>").into_owned();
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Stable fingerprint of `(source, message)` (16 hex chars).
pub fn fingerprint(source: &str, message: &str) -> String {
    let h = sf_core::crypto::sha256_hex(format!("{source}|{}", normalize(message)).as_bytes());
    h.get(..16).unwrap_or(&h).to_string()
}

/// Cheap-to-clone handle over the `errors` table.
#[derive(Clone)]
pub struct ErrorTracker {
    db: Db,
}

impl ErrorTracker {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Records one occurrence; returns the fingerprint. A resolved error that happens again is reopened.
    pub async fn record(&self, source: &str, message: &str, context: Value) -> Result<String> {
        let fp = fingerprint(source, message);
        let (fp2, source, message) = (
            fp.clone(),
            source.chars().take(200).collect::<String>(),
            message.chars().take(MAX_MESSAGE).collect::<String>(),
        );
        let now = sf_core::now_rfc3339();
        self.db
            .write(move |t| {
                t.execute(
                    "INSERT INTO errors(fingerprint, source, message, context, count, first_seen, last_seen)
                     VALUES(?1,?2,?3,?4,1,?5,?5)
                     ON CONFLICT(fingerprint) DO UPDATE SET count = count + 1, last_seen = ?5,
                       message = ?3, context = ?4, resolved_at = NULL",
                    params![fp2, source, message, context.to_string(), now],
                )
                .map_err(err)?;
                Ok(())
            })
            .await?;
        Ok(fp)
    }

    /// Fire-and-forget variant for hot paths (never fails the caller; needs a tokio runtime).
    pub fn record_bg(&self, source: &str, message: &str, context: Value) {
        let me = self.clone();
        let (source, message) = (source.to_string(), message.to_string());
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            h.spawn(async move {
                if let Err(e) = me.record(&source, &message, context).await {
                    tracing::warn!(error = %e, "error tracker write failed");
                }
            });
        }
    }

    /// Lists errors, newest first. `resolved`: None = all, Some(false) = open only, Some(true) = resolved only.
    pub async fn list(&self, resolved: Option<bool>, limit: usize) -> Result<Vec<Value>> {
        let limit = limit.clamp(1, 1000) as i64;
        self.db
            .read(move |c| {
                let filter = match resolved {
                    None => "",
                    Some(false) => "WHERE resolved_at IS NULL",
                    Some(true) => "WHERE resolved_at IS NOT NULL",
                };
                let mut stmt = c
                    .prepare(&format!(
                        "SELECT fingerprint, source, message, context, count, first_seen, last_seen, resolved_at
                         FROM errors {filter} ORDER BY last_seen DESC LIMIT ?1"
                    ))
                    .map_err(err)?;
                let rows = stmt
                    .query_map([limit], |r| Ok(row_to_json(r, &["context"], &[])))
                    .map_err(err)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(err)?;
                Ok(rows)
            })
            .await
    }

    /// Marks an error resolved. Returns false when the fingerprint is unknown.
    pub async fn resolve(&self, fingerprint: &str) -> Result<bool> {
        let fp = fingerprint.to_string();
        self.db
            .write(move |t| {
                let n = t
                    .execute(
                        "UPDATE errors SET resolved_at = ?2 WHERE fingerprint = ?1",
                        params![fp, sf_core::now_rfc3339()],
                    )
                    .map_err(err)?;
                Ok(n > 0)
            })
            .await
    }

    /// Reads one error row.
    pub async fn get(&self, fingerprint: &str) -> Result<Option<Value>> {
        let fp = fingerprint.to_string();
        self.db
            .read(move |c| {
                c.query_row(
                    "SELECT fingerprint, source, message, context, count, first_seen, last_seen, resolved_at
                     FROM errors WHERE fingerprint=?1",
                    [&fp],
                    |r| Ok(row_to_json(r, &["context"], &[])),
                )
                .optional()
                .map_err(err)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalization_groups_variants() {
        let a = fingerprint(
            "crawl",
            "post 123 failed for 3f2504e0-4f89-11d3-9a0c-0305e82c3301",
        );
        let b = fingerprint(
            "crawl",
            "post 98765 failed for 9a2504e0-4f89-11d3-9a0c-0305e82c3302",
        );
        assert_eq!(a, b);
        assert_ne!(a, fingerprint("plan", "post 1 failed for x"));
        assert_eq!(
            normalize("hash deadbeefcafebabe0011 at 1.5s"),
            "hash <hex> at <n>s"
        );
        // hostile input never panics
        let _ = normalize(&"\u{0}é9".repeat(5000));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dedupe_and_resolve() {
        let dir = std::env::temp_dir().join(format!("sf-ops-err-{}", sf_core::new_id()));
        let db = Db::open(&dir.join("t.db")).unwrap();
        let t = ErrorTracker::new(db.clone());
        let f1 = t
            .record("wp", "HTTP 500 on post 12", json!({"site":"a"}))
            .await
            .unwrap();
        let f2 = t
            .record("wp", "HTTP 500 on post 77", json!({"site":"b"}))
            .await
            .unwrap();
        assert_eq!(f1, f2);
        let all = t.list(None, 10).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0]["count"], 2);
        assert_eq!(all[0]["context"]["site"], "b");
        assert!(t.resolve(&f1).await.unwrap());
        assert!(t.list(Some(false), 10).await.unwrap().is_empty());
        assert!(!t.resolve("nope").await.unwrap());
        t.record("wp", "HTTP 500 on post 1", json!({}))
            .await
            .unwrap();
        assert_eq!(t.list(Some(false), 10).await.unwrap().len(), 1);
        db.close();
        let _ = std::fs::remove_dir_all(dir);
    }
}
