//! `backup`: `sf_ops::backup::create` (payload `{kind?: full|db|config, note?}`; the archive is
//! encrypted when the configured passphrase env var is set) followed by the keep-N prune.
//! Stats: `{id, path, kind, size_bytes, sha256, encrypted, pruned}`.

use std::sync::Arc;

use serde_json::{Value, json};
use sf_ops::backup::{BackupKind, BackupOptions, configured_passphrase, create, prune};

use crate::ctx::{Job, JobContext, JobResult};

pub async fn run(ctx: &Arc<JobContext>, job: &Job) -> JobResult<Value> {
    let kind = job
        .payload_str("kind", "kind")
        .and_then(|k| BackupKind::parse(&k))
        .unwrap_or_default();
    let opts = BackupOptions {
        kind,
        passphrase: configured_passphrase(&ctx.cfg),
        note: job
            .payload_str("note", "note")
            .unwrap_or_else(|| "job".into()),
        // M-02: without a passphrase, secrets.env is left out of the archive.
        include_secrets: false,
    };
    job.progress("backup", 10.0).await;
    let info = create(&ctx.cfg, &ctx.db, opts).await?;
    let pruned = prune(&ctx.cfg, &ctx.db).await.unwrap_or(0);
    job.info(format!(
        "backup {} written ({} bytes)",
        info.path.display(),
        info.size_bytes
    ))
    .await;
    Ok(json!({
        "id": info.id,
        "path": info.path,
        "kind": info.kind.as_str(),
        "size_bytes": info.size_bytes,
        "sha256": info.sha256,
        "encrypted": info.encrypted,
        "pruned": pruned,
        "progress": {"phase": "done", "pct": 100},
    }))
}
