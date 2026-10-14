//! Where the local rungs look for the model files and `onnxruntime.dll`.

use std::path::{Path, PathBuf};

/// Env override for the onnxruntime library (full path to the DLL).
pub const ENV_ORT_DYLIB: &str = "SILOFORGE_ORT_DYLIB";
/// Env override for the model directory.
pub const ENV_EMBED_MODEL: &str = "SILOFORGE_EMBED_MODEL";
/// Model directory name under `$HOME/models` and `<exe dir>/models`.
pub const MODEL_DIR_NAME: &str = "all-MiniLM-L6-v2";

#[cfg(windows)]
pub const ORT_DLL: &str = "onnxruntime.dll";
#[cfg(target_os = "macos")]
pub const ORT_DLL: &str = "libonnxruntime.dylib";
#[cfg(not(any(windows, target_os = "macos")))]
pub const ORT_DLL: &str = "libonnxruntime.so";

/// Directory of the running executable, if known.
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

/// Candidate paths for the ONNX Runtime library, in lookup order:
/// explicit path, `SILOFORGE_ORT_DYLIB`, `cfg.onnx_runtime_dir`, exe dir, `$HOME/runtime`.
pub fn dylib_candidates(explicit: Option<&Path>, runtime_dir: &str, home: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(p) = explicit {
        v.push(p.to_path_buf());
    }
    if let Some(p) = non_empty_env(ENV_ORT_DYLIB) {
        v.push(PathBuf::from(p));
    }
    if !runtime_dir.trim().is_empty() {
        v.push(Path::new(runtime_dir.trim()).join(ORT_DLL));
    }
    if let Some(d) = exe_dir() {
        v.push(d.join(ORT_DLL));
    }
    v.push(home.join("runtime").join(ORT_DLL));
    v
}

/// First existing candidate, or an error message listing everything that was tried.
pub fn find_dylib(
    explicit: Option<&Path>,
    runtime_dir: &str,
    home: &Path,
) -> Result<PathBuf, String> {
    let c = dylib_candidates(explicit, runtime_dir, home);
    if let Some(p) = c.iter().find(|p| p.is_file()) {
        return Ok(absolute(p));
    }
    let tried: Vec<String> = c.iter().map(|p| p.display().to_string()).collect();
    Err(format!(
        "{ORT_DLL} not found (tried: {}); run `siloforge fetch-runtime` or set {ENV_ORT_DYLIB}",
        tried.join(", ")
    ))
}

fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

/// Model directory: `cfg.model_dir`, else `SILOFORGE_EMBED_MODEL`, else `$HOME/models/all-MiniLM-L6-v2`,
/// else `<exe dir>/models/all-MiniLM-L6-v2` (first one that exists; `$HOME/...` when none does).
pub fn model_dir(cfg_model_dir: &str, home: &Path) -> PathBuf {
    if !cfg_model_dir.trim().is_empty() {
        return PathBuf::from(cfg_model_dir.trim());
    }
    if let Some(p) = non_empty_env(ENV_EMBED_MODEL) {
        return PathBuf::from(p);
    }
    let home_dir = home.join("models").join(MODEL_DIR_NAME);
    if home_dir.is_dir() {
        return home_dir;
    }
    if let Some(d) = exe_dir() {
        let e = d.join("models").join(MODEL_DIR_NAME);
        if e.is_dir() {
            return e;
        }
    }
    home_dir
}

/// `model.onnx` in the model dir, or `onnx/model.onnx` (Hugging Face Xenova layout).
pub fn onnx_model(dir: &Path) -> Option<PathBuf> {
    [dir.join("model.onnx"), dir.join("onnx").join("model.onnx")]
        .into_iter()
        .find(|p| p.is_file())
}

pub fn tokenizer(dir: &Path) -> PathBuf {
    dir.join("tokenizer.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_order() {
        let home = Path::new("D:/tmp/h");
        let c = dylib_candidates(Some(Path::new("D:/x/ort.dll")), "D:/rt", home);
        assert_eq!(
            c.first().map(PathBuf::as_path),
            Some(Path::new("D:/x/ort.dll"))
        );
        assert!(c.contains(&Path::new("D:/rt").join(ORT_DLL)));
        assert_eq!(c.last(), Some(&home.join("runtime").join(ORT_DLL)));
        let err = find_dylib(Some(Path::new("D:/nope/x.dll")), "", Path::new("D:/nope"));
        assert!(err.is_err_and(|e| e.contains("fetch-runtime")));
    }
}
