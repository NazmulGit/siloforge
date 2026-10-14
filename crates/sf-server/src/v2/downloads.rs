//! Release downloads (contract v2.1 §5): app and WordPress plugin zips from `[downloads] dir`
//! (default `<exe dir>/../releases`).
//!
//! Only names the listing recognises can be downloaded (`siloforge-<ver>-windows-x64.zip`,
//! `siloforge-connect-<ver>.zip`): the request's file name must equal a listed entry exactly, so
//! no path, `..` or other file in the directory is reachable. SHA-256 comes from the
//! `<file>.sha256` sidecar, else it is computed once and cached by (name, size, mtime).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::auth::User;
use crate::{ApiError, ApiResult, AppState};

/// One recognised file.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub file: String,
    pub kind: &'static str,
    pub version: String,
    pub size_bytes: u64,
    pub sha256: Option<String>,
    pub created_at: Option<String>,
    pub latest: bool,
    path: PathBuf,
}

type ShaKey = (String, u64, i64);
static SHA_CACHE: Mutex<Option<HashMap<ShaKey, String>>> = Mutex::new(None);

fn valid_version(v: &str) -> bool {
    let core = v.split(['-', '+']).next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    !v.is_empty()
        && v.len() <= 40
        && parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-+".contains(c))
}

/// `(kind, version)` for a recognised release file name.
pub fn classify(name: &str) -> Option<(&'static str, String)> {
    if let Some(v) = name
        .strip_prefix("siloforge-connect-")
        .and_then(|r| r.strip_suffix(".zip"))
    {
        return valid_version(v).then(|| ("wp-plugin", v.to_string()));
    }
    let v = name
        .strip_prefix("siloforge-")?
        .strip_suffix("-windows-x64.zip")?;
    valid_version(v).then(|| ("app", v.to_string()))
}

/// Semver-ish ordering key (numeric core; a pre-release sorts before the release).
fn version_key(v: &str) -> (u64, u64, u64, bool, String) {
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v.split('+').next().unwrap_or(v), None),
    };
    let mut it = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        pre.is_none(),
        pre.unwrap_or("").to_string(),
    )
}

fn sidecar_sha(path: &Path) -> Option<String> {
    let mut side = path.as_os_str().to_owned();
    side.push(".sha256");
    let meta = std::fs::metadata(&side).ok()?;
    if meta.len() > 4096 {
        return None;
    }
    let text = std::fs::read_to_string(&side).ok()?;
    let tok = text.split_whitespace().next()?.to_ascii_lowercase();
    (tok.len() == 64 && tok.chars().all(|c| c.is_ascii_hexdigit())).then_some(tok)
}

fn compute_sha(path: &Path) -> Option<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(buf.get(..n)?);
    }
    Some(hex::encode(h.finalize()))
}

fn cached_sha(name: &str, path: &Path, size: u64, mtime: i64) -> Option<String> {
    let key = (name.to_string(), size, mtime);
    if let Ok(g) = SHA_CACHE.lock()
        && let Some(v) = g.as_ref().and_then(|m| m.get(&key))
    {
        return Some(v.clone());
    }
    let v = compute_sha(path)?;
    if let Ok(mut g) = SHA_CACHE.lock() {
        let m = g.get_or_insert_with(HashMap::new);
        if m.len() > 256 {
            m.clear();
        }
        m.insert(key, v.clone());
    }
    Some(v)
}

/// Lists recognised files in `dir` (blocking). Newest version first per kind; `latest` marks the
/// highest version of each kind.
pub fn scan(dir: &Path) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = Vec::new();
    for e in rd.flatten().take(10_000) {
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        let Some((kind, version)) = classify(&name) else {
            continue;
        };
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let path = e.path();
        let mtime = meta
            .modified()
            .ok()
            .map(chrono::DateTime::<chrono::Utc>::from);
        let sha = sidecar_sha(&path)
            .or_else(|| cached_sha(&name, &path, meta.len(), mtime.map_or(0, |t| t.timestamp())));
        out.push(Entry {
            file: name,
            kind,
            version,
            size_bytes: meta.len(),
            sha256: sha,
            created_at: mtime.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            latest: false,
            path,
        });
    }
    out.sort_by(|a, b| {
        a.kind
            .cmp(b.kind)
            .then_with(|| version_key(&b.version).cmp(&version_key(&a.version)))
            .then_with(|| a.file.cmp(&b.file))
    });
    let mut seen: Vec<&'static str> = Vec::new();
    for e in &mut out {
        if !seen.contains(&e.kind) {
            seen.push(e.kind);
            e.latest = true;
        }
    }
    out
}

async fn listing(st: &AppState) -> Vec<Entry> {
    let dir = st.live.get().downloads_dir();
    tokio::task::spawn_blocking(move || scan(&dir))
        .await
        .unwrap_or_default()
}

/// `GET /downloads` -> `{downloads: [...], current_version}`.
pub async fn list(State(st): State<AppState>, _u: User) -> ApiResult<Json<Value>> {
    let rows: Vec<Value> = listing(&st)
        .await
        .into_iter()
        .map(|e| {
            json!({
                "file": e.file, "kind": e.kind, "version": e.version, "size_bytes": e.size_bytes,
                "sha256": e.sha256, "created_at": e.created_at, "latest": e.latest,
            })
        })
        .collect();
    Ok(Json(json!({
        "downloads": rows,
        "current_version": sf_core::VERSION,
    })))
}

/// `GET /downloads/{file}`: streams a listed file as an attachment.
pub async fn download(
    State(st): State<AppState>,
    _u: User,
    AxPath(file): AxPath<String>,
) -> ApiResult<Response> {
    // exact match against the listing: no paths, no unlisted files
    if file.len() > 200 || classify(&file).is_none() {
        return Err(ApiError::not_found("download not found"));
    }
    let entry = listing(&st)
        .await
        .into_iter()
        .find(|e| e.file == file)
        .ok_or_else(|| ApiError::not_found("download not found"))?;
    let f = tokio::fs::File::open(&entry.path)
        .await
        .map_err(|_| ApiError::not_found("download not found"))?;
    let stream = tokio_util::io::ReaderStream::new(f);
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{}\"", entry.file))
        .map_err(|_| ApiError::internal("bad file name"))?;
    let mut res = Body::from_stream(stream).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    h.insert(header::CONTENT_DISPOSITION, disposition);
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(entry.size_bytes));
    if let Some(s) = &entry.sha256
        && let Ok(v) = HeaderValue::from_str(s)
    {
        h.insert("x-sha256", v);
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_classified_strictly() {
        assert_eq!(
            classify("siloforge-2.1.0-windows-x64.zip"),
            Some(("app", "2.1.0".into()))
        );
        assert_eq!(
            classify("siloforge-connect-0.2.0.zip"),
            Some(("wp-plugin", "0.2.0".into()))
        );
        assert_eq!(
            classify("siloforge-2.1.0-rc.1-windows-x64.zip"),
            Some(("app", "2.1.0-rc.1".into()))
        );
        for bad in [
            "siloforge-2.1-windows-x64.zip",
            "siloforge-../x-windows-x64.zip",
            "siloforge-2.1.0-windows-x64.zip.sha256",
            "siloforge-connect-0.2.0.tar.gz",
            "../siloforge-2.1.0-windows-x64.zip",
            "siloforge-2.1.0-windows-x64.ZIP",
            "secrets.env",
            "",
        ] {
            assert_eq!(classify(bad), None, "{bad}");
        }
    }

    #[test]
    fn scan_orders_marks_latest_and_hashes() {
        let dir = std::env::temp_dir().join(format!("sf-dl-{}", sf_core::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("siloforge-2.0.2-windows-x64.zip"), b"old").unwrap();
        std::fs::write(dir.join("siloforge-2.0.10-windows-x64.zip"), b"new").unwrap();
        std::fs::write(
            dir.join("siloforge-2.0.10-windows-x64.zip.sha256"),
            format!("{}  siloforge-2.0.10-windows-x64.zip\n", "ab".repeat(32)),
        )
        .unwrap();
        std::fs::write(dir.join("siloforge-connect-0.2.0.zip"), b"plugin").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        let v = scan(&dir);
        let names: Vec<&str> = v.iter().map(|e| e.file.as_str()).collect();
        assert_eq!(
            names,
            [
                "siloforge-2.0.10-windows-x64.zip",
                "siloforge-2.0.2-windows-x64.zip",
                "siloforge-connect-0.2.0.zip"
            ]
        );
        assert!(v[0].latest && !v[1].latest && v[2].latest);
        assert_eq!(v[0].sha256.as_deref(), Some("ab".repeat(32).as_str()));
        // computed: sha256("old")
        assert_eq!(v[1].sha256, Some(sf_core::crypto::sha256_hex(b"old")));
        assert!(scan(Path::new("D:/definitely/missing/dir")).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
