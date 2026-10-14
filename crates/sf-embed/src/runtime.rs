//! Opt-in downloads (invoked by CLI commands only, never automatically):
//! * [`fetch_runtime`]: ONNX Runtime 1.24.4 (+ DirectML 1.15.4) from the NuGet packages.
//! * [`fetch_model`]: `tokenizer.json`, `config.json`, `model.onnx` from Hugging Face (Xenova).
//! * [`fetch_candle_weights`]: `model.safetensors` for the candle rung.
//!
//! Everything is SHA-256 pinned (values from docs/research/06), written to a temp file first and
//! renamed into place only after verification.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use sf_core::{Error, Result};
use sha2::{Digest, Sha256};

/// ONNX Runtime version the `ort` binding targets (`api-24`).
pub const ORT_VERSION: &str = "1.24.4";
pub const DIRECTML_VERSION: &str = "1.15.4";

const NUGET: &str = "https://api.nuget.org/v3-flatcontainer";
const HF: &str = "https://huggingface.co";

/// Largest single download accepted (DirectML nupkg is ~202 MB).
const MAX_DOWNLOAD: u64 = 400 * 1024 * 1024;
/// Largest file extracted from a package.
const MAX_ENTRY: u64 = 64 * 1024 * 1024;

/// One file we fetch: where from, where to, and the pinned hash.
#[derive(Debug, Clone)]
pub(crate) struct Artifact {
    pub url: String,
    /// Package hash (for nupkgs) or file hash (for direct files).
    pub sha256: &'static str,
    /// For packages: entry to extract and the expected hash of the extracted file.
    pub extract: Option<(&'static str, &'static str)>,
    pub file_name: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Fetched {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    /// False when an identical file was already present.
    pub downloaded: bool,
}

fn nuget(id: &str, ver: &str) -> String {
    format!("{NUGET}/{id}/{ver}/{id}.{ver}.nupkg")
}

pub(crate) fn runtime_artifacts(gpu: bool) -> Vec<Artifact> {
    if gpu {
        vec![
            Artifact {
                url: nuget("microsoft.ml.onnxruntime.directml", ORT_VERSION),
                sha256: "57e9f11b73437bef7a309496135d4c1f96b1a8e9ddba60013fa27bfc1d788681",
                extract: Some((
                    "runtimes/win-x64/native/onnxruntime.dll",
                    "e7eedec6a6f26dc39dc948276a75ef6d2bee3fff944d874ceed0bbd3b97bff40",
                )),
                file_name: "onnxruntime.dll",
            },
            Artifact {
                url: nuget("microsoft.ai.directml", DIRECTML_VERSION),
                sha256: "4e7cb7ddce8cf837a7a75dc029209b520ca0101470fcdf275c1f49736a3615b9",
                extract: Some((
                    "bin/x64-win/DirectML.dll",
                    "9c9e6d822561c6c41b90e6994b3e8857cf1d66dbfb1e0c4c799c7c89b4e92da1",
                )),
                file_name: "DirectML.dll",
            },
        ]
    } else {
        vec![Artifact {
            url: nuget("microsoft.ml.onnxruntime", ORT_VERSION),
            sha256: "4b978d5065b85e7004b6c6f60ca494bd978fbe6836cbf0a0b52d82b61ab99638",
            extract: Some((
                "runtimes/win-x64/native/onnxruntime.dll",
                "b95efb2113b603bbbf3f191061c5516a871ed546893c820e4f3b7b6c358dbf2a",
            )),
            file_name: "onnxruntime.dll",
        }]
    }
}

pub(crate) fn model_artifacts() -> Vec<Artifact> {
    let base = format!("{HF}/Xenova/all-MiniLM-L6-v2/resolve/main");
    vec![
        Artifact {
            url: format!("{base}/tokenizer.json"),
            sha256: "da0e79933b9ed51798a3ae27893d3c5fa4a201126cef75586296df9b4d2c62a0",
            extract: None,
            file_name: "tokenizer.json",
        },
        Artifact {
            url: format!("{base}/config.json"),
            sha256: "7135149f7cffa1a573466c6e4d8423ed73b62fd2332c575bf738a0d033f70df7",
            extract: None,
            file_name: "config.json",
        },
        Artifact {
            url: format!("{base}/onnx/model.onnx"),
            sha256: "759c3cd2b7fe7e93933ad23c4c9181b7396442a2ed746ec7c1d46192c469c46e",
            extract: None,
            file_name: "model.onnx",
        },
    ]
}

/// Download `onnxruntime.dll` (and `DirectML.dll` when `gpu`) into `dest_dir` (normally
/// `$HOME/runtime` or the exe folder). Windows x64 packages.
pub async fn fetch_runtime(dest_dir: &Path, gpu: bool) -> Result<Vec<Fetched>> {
    fetch_all(&runtime_artifacts(gpu), dest_dir).await
}

/// Download the ONNX model + tokenizer + config into `dest_dir` (normally
/// `$HOME/models/all-MiniLM-L6-v2`).
pub async fn fetch_model(dest_dir: &Path) -> Result<Vec<Fetched>> {
    fetch_all(&model_artifacts(), dest_dir).await
}

/// Download `model.safetensors` (sentence-transformers repo) for the candle rung.
pub async fn fetch_candle_weights(dest_dir: &Path) -> Result<Vec<Fetched>> {
    let a = Artifact {
        url: format!("{HF}/sentence-transformers/all-MiniLM-L6-v2/resolve/main/model.safetensors"),
        sha256: "53aa51172d142c89d9012cce15ae4d6cc0ca6895895114379cacb4fab128d9db",
        extract: None,
        file_name: "model.safetensors",
    };
    fetch_all(&[a], dest_dir).await
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        .user_agent(concat!("siloforge/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Error::internal(format!("http client: {e}")))
}

pub(crate) async fn fetch_all(arts: &[Artifact], dest_dir: &Path) -> Result<Vec<Fetched>> {
    tokio::fs::create_dir_all(dest_dir).await?;
    let client = client()?;
    let mut out = Vec::new();
    for a in arts {
        out.push(fetch_one(&client, a, dest_dir).await?);
    }
    Ok(out)
}

async fn fetch_one(client: &reqwest::Client, a: &Artifact, dest_dir: &Path) -> Result<Fetched> {
    let final_path = dest_dir.join(a.file_name);
    let final_sha = a.extract.map_or(a.sha256, |(_, s)| s);
    // already there and correct -> nothing to do
    if final_path.is_file() {
        let p = final_path.clone();
        let (sha, bytes) = tokio::task::spawn_blocking(move || sha256_file(&p))
            .await
            .map_err(|e| Error::internal(format!("hash worker: {e}")))??;
        if sha == final_sha {
            return Ok(Fetched {
                path: final_path,
                bytes,
                sha256: sha,
                downloaded: false,
            });
        }
    }
    let tmp = dest_dir.join(format!(".{}.download", a.file_name));
    let res = async {
        download_verified(client, &a.url, &tmp, a.sha256).await?;
        if let Some((entry, entry_sha)) = a.extract {
            let pkg = tmp.clone();
            let out = dest_dir.join(format!(".{}.extract", a.file_name));
            let out2 = out.clone();
            tokio::task::spawn_blocking(move || extract_verified(&pkg, entry, &out2, entry_sha))
                .await
                .map_err(|e| Error::internal(format!("extract worker: {e}")))??;
            let _ = tokio::fs::remove_file(&tmp).await;
            Ok::<PathBuf, Error>(out)
        } else {
            Ok(tmp.clone())
        }
    }
    .await;
    let ready = match res {
        Ok(p) => p,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
    };
    if let Err(e) = tokio::fs::rename(&ready, &final_path).await {
        let _ = tokio::fs::remove_file(&ready).await;
        return Err(Error::internal(format!(
            "replace {}: {e} (is SiloForge running and holding it? stop it first)",
            final_path.display()
        )));
    }
    let bytes = tokio::fs::metadata(&final_path).await?.len();
    tracing::info!(path = %final_path.display(), bytes, "fetched");
    Ok(Fetched {
        path: final_path,
        bytes,
        sha256: final_sha.to_string(),
        downloaded: true,
    })
}

/// Stream `url` into `dest`, hashing as we go; error (and delete) on size cap or hash mismatch.
pub(crate) async fn download_verified(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    sha256: &str,
) -> Result<u64> {
    use tokio::io::AsyncWriteExt;
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::upstream(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::upstream(format!(
            "GET {url}: HTTP {}",
            resp.status().as_u16()
        )));
    }
    if resp.content_length().is_some_and(|n| n > MAX_DOWNLOAD) {
        return Err(Error::upstream(format!("GET {url}: too large")));
    }
    let mut f = tokio::fs::File::create(dest).await?;
    let mut h = Sha256::new();
    let mut n: u64 = 0;
    let r: Result<()> = async {
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| Error::upstream(format!("GET {url}: {e}")))?
        {
            n = n.saturating_add(chunk.len() as u64);
            if n > MAX_DOWNLOAD {
                return Err(Error::upstream(format!("GET {url}: too large")));
            }
            h.update(&chunk);
            f.write_all(&chunk).await?;
        }
        f.flush().await?;
        Ok(())
    }
    .await;
    drop(f);
    let got = hex::encode(h.finalize());
    let r = r.and_then(|()| {
        if got.eq_ignore_ascii_case(sha256) {
            Ok(())
        } else {
            Err(Error::upstream(format!(
                "GET {url}: SHA-256 mismatch (got {got}, expected {sha256})"
            )))
        }
    });
    if let Err(e) = r {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(e);
    }
    Ok(n)
}

/// Extract one entry of a zip (nupkg) to `out`, verifying its SHA-256.
pub(crate) fn extract_verified(pkg: &Path, entry: &str, out: &Path, sha256: &str) -> Result<()> {
    let f = std::fs::File::open(pkg)?;
    let mut z = zip::ZipArchive::new(f)
        .map_err(|e| Error::upstream(format!("{}: not a zip: {e}", pkg.display())))?;
    let mut e = z
        .by_name(entry)
        .map_err(|e| Error::upstream(format!("{}: missing {entry}: {e}", pkg.display())))?;
    if e.size() > MAX_ENTRY {
        return Err(Error::upstream(format!("{entry}: too large")));
    }
    let r = (|| -> Result<()> {
        let mut w = std::fs::File::create(out)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let k = e.read(&mut buf)?;
            if k == 0 {
                break;
            }
            total = total.saturating_add(k as u64);
            if total > MAX_ENTRY {
                return Err(Error::upstream(format!("{entry}: too large")));
            }
            let chunk = buf.get(..k).unwrap_or(&[]);
            h.update(chunk);
            w.write_all(chunk)?;
        }
        w.flush()?;
        let got = hex::encode(h.finalize());
        if !got.eq_ignore_ascii_case(sha256) {
            return Err(Error::upstream(format!(
                "{entry}: SHA-256 mismatch (got {got}, expected {sha256})"
            )));
        }
        Ok(())
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(out);
    }
    r
}

/// SHA-256 (hex) and size of a file.
pub fn sha256_file(p: &Path) -> Result<(String, u64)> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut n: u64 = 0;
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        n = n.saturating_add(k as u64);
        h.update(buf.get(..k).unwrap_or(&[]));
    }
    Ok((hex::encode(h.finalize()), n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};

    fn sha(b: &[u8]) -> String {
        hex::encode(Sha256::digest(b))
    }

    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    fn make_zip(entry: &str, data: &[u8]) -> Vec<u8> {
        let mut cur = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut cur);
            w.start_file(entry, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(data).unwrap();
            w.finish().unwrap();
        }
        cur.into_inner()
    }

    async fn serve(pkg: Vec<u8>, file: Vec<u8>) -> String {
        let app = Router::new()
            .route("/pkg.nupkg", get(move || async move { pkg.clone() }))
            .route("/file.bin", get(move || async move { file.clone() }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });
        format!("http://{addr}")
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d =
            PathBuf::from("D:/tmp/sf-embed-tests").join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn pinned_artifacts_are_complete() {
        for a in runtime_artifacts(true)
            .iter()
            .chain(&runtime_artifacts(false))
            .chain(&model_artifacts())
        {
            assert_eq!(a.sha256.len(), 64, "{}", a.url);
            assert!(a.url.starts_with("https://"));
        }
        assert_eq!(runtime_artifacts(true).len(), 2);
    }

    #[tokio::test]
    async fn fetch_verifies_extracts_and_is_idempotent() {
        let dll = b"fake dll bytes".to_vec();
        let pkg = make_zip("runtimes/win-x64/native/onnxruntime.dll", &dll);
        let file = b"{\"tokenizer\":true}".to_vec();
        let base = serve(pkg.clone(), file.clone()).await;
        let dir = tmpdir("fetch");
        let arts = vec![
            Artifact {
                url: format!("{base}/pkg.nupkg"),
                sha256: leak(sha(&pkg)),
                extract: Some(("runtimes/win-x64/native/onnxruntime.dll", leak(sha(&dll)))),
                file_name: "onnxruntime.dll",
            },
            Artifact {
                url: format!("{base}/file.bin"),
                sha256: leak(sha(&file)),
                extract: None,
                file_name: "tokenizer.json",
            },
        ];
        let got = fetch_all(&arts, &dir).await.unwrap();
        assert!(got.iter().all(|f| f.downloaded));
        assert_eq!(std::fs::read(dir.join("onnxruntime.dll")).unwrap(), dll);
        assert_eq!(std::fs::read(dir.join("tokenizer.json")).unwrap(), file);
        // no temp files left
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        let again = fetch_all(&arts, &dir).await.unwrap();
        assert!(again.iter().all(|f| !f.downloaded));

        // wrong hash -> error, nothing written
        let bad = vec![Artifact {
            url: format!("{base}/file.bin"),
            sha256: leak("0".repeat(64)),
            extract: None,
            file_name: "x.json",
        }];
        let e = fetch_all(&bad, &dir).await.unwrap_err();
        assert!(e.to_string().contains("mismatch"));
        assert!(!dir.join("x.json").exists());
        assert!(!dir.join(".x.json.download").exists());

        // wrong entry hash / missing entry
        let bad = vec![Artifact {
            url: format!("{base}/pkg.nupkg"),
            sha256: leak(sha(&pkg)),
            extract: Some(("nope/onnxruntime.dll", leak(sha(&dll)))),
            file_name: "y.dll",
        }];
        assert!(fetch_all(&bad, &dir).await.is_err());
        assert!(!dir.join("y.dll").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_rejects_non_zip() {
        let dir = tmpdir("nonzip");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("junk.nupkg");
        std::fs::write(&p, b"not a zip at all").unwrap();
        assert!(extract_verified(&p, "a", &dir.join("a"), "00").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
