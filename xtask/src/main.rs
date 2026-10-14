//! Project automation. `cargo xtask <ci|ui|release|clean-target>`.
//!
//! * `ci`       fmt check + clippy -D warnings + tests (the one verify command), then
//!   `cargo audit` when cargo-audit is installed (I-05; `SILOFORGE_SKIP_AUDIT=1` skips).
//! * `ui`       build the React UI into `ui/dist` (embedded by `siloforge-server`).
//! * `release`  ui + `cargo build --release --locked` + zip + SHA256 into `dist/<version>/`.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let task = args.first().map(String::as_str).unwrap_or("help");
    let res = match task {
        "ci" => ci(),
        "ui" => ui(),
        "release" => release(),
        _ => {
            eprintln!("usage: cargo xtask <ci|ui|release>");
            Ok(())
        }
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask {task} failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn run(cmd: &mut Command) -> Result<(), String> {
    eprintln!("> {cmd:?}");
    let status = cmd.status().map_err(|e| format!("spawn {cmd:?}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd:?} exited with {status}"))
    }
}

fn cargo() -> Command {
    let mut c = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    c.current_dir(root());
    c
}

fn ci() -> Result<(), String> {
    run(cargo().args(["fmt", "--all", "--", "--check"]))?;
    run(cargo().args([
        "clippy",
        "--workspace",
        "--all-targets",
        "--locked",
        "-q",
        "--",
        "-D",
        "warnings",
    ]))?;
    run(cargo().args(["test", "--workspace", "--locked", "-q"]))?;
    audit()
}

/// I-05: RustSec advisory check. Optional tool (not installed by xtask): when `cargo audit` is
/// missing this prints how to get it and passes; when present, a vulnerable dependency fails CI.
fn audit() -> Result<(), String> {
    if std::env::var("SILOFORGE_SKIP_AUDIT").is_ok_and(|v| v == "1") {
        eprintln!("> cargo audit skipped (SILOFORGE_SKIP_AUDIT=1)");
        return Ok(());
    }
    let present = cargo()
        .args(["audit", "--version"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !present {
        eprintln!(
            "> cargo audit not installed: skipped (install with `cargo install cargo-audit --locked --root D:/Programs/cargo-tools`)"
        );
        return Ok(());
    }
    run(cargo().args(["audit"]))
}

fn npm() -> Command {
    let mut c = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "npm"]);
        c
    } else {
        Command::new("npm")
    };
    c.current_dir(root().join("ui"));
    c
}

fn ui() -> Result<(), String> {
    if !root().join("ui").join("node_modules").exists() {
        run(npm().args(["ci", "--no-audit", "--no-fund"]))?;
    }
    run(npm().args(["run", "build"]))
}

fn version() -> Result<String, String> {
    std::fs::read_to_string(root().join("VERSION"))
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("read VERSION: {e}"))
}

fn release() -> Result<(), String> {
    let ver = version()?;
    ui()?;
    run(cargo().args(["build", "--release", "--locked", "-p", "siloforge"]))?;
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root().join("target"));
    let exe_name = if cfg!(windows) {
        "siloforge.exe"
    } else {
        "siloforge"
    };
    let exe = target_dir.join("release").join(exe_name);
    let out = root().join("dist").join(&ver);
    std::fs::create_dir_all(&out).map_err(|e| format!("mkdir {}: {e}", out.display()))?;
    let dest = out.join(exe_name);
    std::fs::copy(&exe, &dest).map_err(|e| format!("copy {}: {e}", exe.display()))?;
    for f in [
        "README.md",
        "CHANGELOG.md",
        "LICENSE",
        "siloforge.example.toml",
    ] {
        let src = root().join(f);
        if src.exists() {
            std::fs::copy(&src, out.join(f)).map_err(|e| format!("copy {f}: {e}"))?;
        }
    }
    eprintln!("release staged in {}", out.display());
    eprintln!("package with: scripts/package-release.ps1 {ver}");
    Ok(())
}
