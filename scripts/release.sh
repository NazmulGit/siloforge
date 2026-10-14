#!/usr/bin/env bash
# Build a SiloForge release (app zip + WordPress plugin zip) and tag it. Nothing is installed, pushed or published.
#
#   scripts/release.sh <version> [--dry-run] [--skip-ci] [--skip-ui]
#
# Steps (see docs/RELEASING.md):
#   1. checks: <version> == VERSION == Cargo.toml [workspace.package] version, CHANGELOG.md has "## [<version>]",
#      plugin header version readable, git tree clean, runtime DLLs present
#   2. cargo xtask ci                      (fmt + clippy -D warnings + tests)
#   3. ui: npm ci + npm run build          (ui/dist is embedded in the exe)
#   4. cargo build --release --locked -p siloforge   (GNU toolchain, WinLibs on PATH, CARGO_TARGET_DIR on D:)
#   5. stage dist/<version>/: siloforge.exe, onnxruntime.dll, DirectML.dll (+ onnxruntime_providers_shared.dll),
#      README.md, CHANGELOG.md, LICENSE, siloforge.example.toml, THIRD_PARTY_NOTICES.txt (+ licenses/), SHA256SUMS.txt
#   6. dist/siloforge-<version>-windows-x64.zip (root siloforge-<version>/) + .sha256
#   7. scripts/build-plugin.sh -> dist/siloforge-connect-<plugin ver>.zip + .sha256
#   8. copy all zips + .sha256 to D:\Final_Apps\SiloForge\releases
#   9. annotated tag v<version> (only if absent)
#
# --dry-run runs every check and prints the plan, but builds, copies and tags nothing (exit 1 if a check failed).
# Env: RUNTIME_DIR (default D:\Download\onnxruntime\dml-1.24.4), LICENSE_DIR (D:\Download\onnxruntime\x),
#      RELEASES_DIR (D:\Final_Apps\SiloForge\releases), CARGO_TARGET_DIR (D:\tmp\cargo-target\siloforge).
set -euo pipefail

usage() { sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }

VERSION_ARG=""
DRY=0
SKIP_CI=0
SKIP_UI=0
for a in "$@"; do
  case "$a" in
    --dry-run) DRY=1 ;;
    --skip-ci) SKIP_CI=1 ;;
    --skip-ui) SKIP_UI=1 ;;
    -h|--help) usage ;;
    -*) echo "release: unknown flag $a" >&2; usage ;;
    *) [ -z "$VERSION_ARG" ] && VERSION_ARG="$a" || { echo "release: extra argument $a" >&2; usage; } ;;
  esac
done
[ -n "$VERSION_ARG" ] || usage

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && (pwd -W 2>/dev/null || pwd))"
cd "$ROOT"
PY="${PYTHON:-python}"
LIB="$ROOT/scripts/release_lib.py"
RUNTIME_DIR="${RUNTIME_DIR:-D:/Download/onnxruntime/dml-1.24.4}"
LICENSE_DIR="${LICENSE_DIR:-D:/Download/onnxruntime/x}"
RELEASES_DIR="${RELEASES_DIR:-D:/Final_Apps/SiloForge/releases}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-D:/tmp/cargo-target/siloforge}"
export PATH="/d/Programs/winlibs/mingw64/bin:$PATH"
export npm_config_cache="${npm_config_cache:-D:/Programs/npm-cache}"
CARGO=(cargo +stable-x86_64-pc-windows-gnu)

for p in "$ROOT" "$RELEASES_DIR" "$CARGO_TARGET_DIR"; do
  case "$p" in
    *[Rr]yzen*) echo "release: refusing path under D:\\Ryzen: $p" >&2; exit 1 ;;
    D:*|d:*|/d/*) ;;
    *) echo "release: path must be on D:\\ : $p" >&2; exit 1 ;;
  esac
done

FAILED=0
ok()   { echo "  ok    $*"; }
bad()  { echo "  FAIL  $*" >&2; FAILED=1; [ "$DRY" = 1 ] || exit 1; }
step() { echo; echo "== $*"; }
run()  { if [ "$DRY" = 1 ]; then echo "  would run: $*"; else echo "  > $*"; "$@"; fi; }

step "SiloForge $VERSION_ARG release$([ "$DRY" = 1 ] && echo ' (dry run)')"

# --- 1. checks ------------------------------------------------------------------------------------
FILE_VER="$(tr -d ' \r\n' < VERSION)"
[ "$FILE_VER" = "$VERSION_ARG" ] && ok "VERSION = $FILE_VER" || bad "VERSION file says '$FILE_VER', not '$VERSION_ARG'"
CARGO_VER="$("$PY" -I -c "import re,sys;t=open(sys.argv[1],encoding='utf-8').read();m=re.search(r'(?s)\[workspace\.package\].*?\bversion\s*=\s*\"([^\"]+)\"',t);print(m.group(1) if m else '')" Cargo.toml)"
[ "$CARGO_VER" = "$VERSION_ARG" ] && ok "Cargo.toml version = $CARGO_VER" || bad "Cargo.toml [workspace.package] version is '$CARGO_VER'"
if UI_VER="$("$PY" -I -c "import json,sys;print(json.load(open(sys.argv[1],encoding='utf-8')).get('version',''))" ui/package.json 2>/dev/null)"; then
  [ "$UI_VER" = "$VERSION_ARG" ] && ok "ui/package.json version = $UI_VER" || bad "ui/package.json version is '$UI_VER'"
fi
if "$PY" -I "$LIB" notes CHANGELOG.md "$VERSION_ARG" >/dev/null 2>&1; then ok "CHANGELOG.md has ## [$VERSION_ARG]"; else bad "CHANGELOG.md has no '## [$VERSION_ARG]' section"; fi
if PLUGIN_VER="$("$PY" -I "$LIB" plugin-version wordpress/siloforge-connect/plugin/siloforge-connect/siloforge-connect.php)"; then
  ok "plugin SiloForge Connect $PLUGIN_VER"
else
  PLUGIN_VER="?"; bad "plugin version check failed"
fi
DIRTY="$(git status --porcelain --untracked-files=normal)"
if [ -z "$DIRTY" ]; then ok "git tree clean"; else bad "git tree not clean ($(printf '%s\n' "$DIRTY" | wc -l | tr -d ' ') paths; commit or stash first)"; fi
for f in onnxruntime.dll DirectML.dll; do
  [ -f "$RUNTIME_DIR/$f" ] && ok "runtime $f" || bad "missing $RUNTIME_DIR/$f"
done
for f in README.md CHANGELOG.md LICENSE siloforge.example.toml; do
  [ -f "$f" ] && ok "$f" || bad "missing $f"
done
TAG="v$VERSION_ARG"
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  echo "  note  tag $TAG already exists (it will be kept, not moved)"
  TAG_EXISTS=1
else
  TAG_EXISTS=0
fi
command -v cargo >/dev/null && ok "cargo found" || bad "cargo not on PATH"
[ "$SKIP_UI" = 1 ] || { command -v npm >/dev/null && ok "npm found" || bad "npm not on PATH (or --skip-ui)"; }

# --- 2-4. ci, ui, build -----------------------------------------------------------------------------
step "verify and build (CARGO_TARGET_DIR=$CARGO_TARGET_DIR)"
if [ "$SKIP_CI" = 1 ]; then echo "  skipped: cargo xtask ci (--skip-ci)"; else run "${CARGO[@]}" xtask ci; fi
if [ "$SKIP_UI" = 1 ]; then
  echo "  skipped: ui build (--skip-ui); reusing ui/dist"
else
  run npm --prefix ui ci --no-audit --no-fund
  run npm --prefix ui run build
fi
[ "$DRY" = 1 ] || [ -f ui/dist/index.html ] || bad "ui/dist/index.html missing (the exe would ship without the UI)"
run "${CARGO[@]}" build --release --locked -p siloforge
EXE="$CARGO_TARGET_DIR/release/siloforge.exe"
if [ "$DRY" = 0 ]; then
  [ -f "$EXE" ] || bad "missing $EXE"
  BUILT_VER="$("$EXE" version 2>/dev/null | tr -d '\r' | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || true)"
  [ "$BUILT_VER" = "$VERSION_ARG" ] && ok "siloforge.exe reports $BUILT_VER" || bad "siloforge.exe reports '$BUILT_VER'"
fi

# --- 5-6. stage + app zip ---------------------------------------------------------------------------
STAGE="dist/$VERSION_ARG"
APP_ZIP="dist/siloforge-$VERSION_ARG-windows-x64.zip"
step "stage $STAGE and zip $APP_ZIP"
if [ "$DRY" = 1 ]; then
  echo "  would stage: siloforge.exe onnxruntime.dll DirectML.dll$([ -f "$RUNTIME_DIR/onnxruntime_providers_shared.dll" ] && echo ' onnxruntime_providers_shared.dll') README.md CHANGELOG.md LICENSE siloforge.example.toml THIRD_PARTY_NOTICES.txt licenses/ SHA256SUMS.txt"
  echo "  would zip:   $APP_ZIP (root siloforge-$VERSION_ARG/) + .sha256"
else
  rm -rf "$STAGE"
  mkdir -p "$STAGE"
  cp "$EXE" "$STAGE/"
  cp "$RUNTIME_DIR/onnxruntime.dll" "$RUNTIME_DIR/DirectML.dll" "$STAGE/"
  [ -f "$RUNTIME_DIR/onnxruntime_providers_shared.dll" ] && cp "$RUNTIME_DIR/onnxruntime_providers_shared.dll" "$STAGE/"
  cp README.md CHANGELOG.md LICENSE siloforge.example.toml "$STAGE/"
  "$PY" -I "$LIB" notices "$VERSION_ARG" "$STAGE/THIRD_PARTY_NOTICES.txt" "$LICENSE_DIR"
  "$PY" -I "$LIB" sums "$STAGE"
  "$PY" -I "$LIB" zip "$STAGE" "$APP_ZIP" "siloforge-$VERSION_ARG"
fi

# --- 7. plugin zip ----------------------------------------------------------------------------------
PLUGIN_ZIP="dist/siloforge-connect-$PLUGIN_VER.zip"
step "plugin zip $PLUGIN_ZIP"
run bash scripts/build-plugin.sh "$ROOT/dist"

# --- 8. copy to releases ----------------------------------------------------------------------------
step "copy to $RELEASES_DIR"
for f in "$APP_ZIP" "$APP_ZIP.sha256" "$PLUGIN_ZIP" "$PLUGIN_ZIP.sha256"; do
  if [ "$DRY" = 1 ]; then echo "  would copy: $f"; else mkdir -p "$RELEASES_DIR"; cp "$f" "$RELEASES_DIR/"; ok "copied $(basename "$f")"; fi
done

# --- 9. tag -----------------------------------------------------------------------------------------
step "tag $TAG"
if [ "$TAG_EXISTS" = 1 ]; then
  echo "  kept existing tag $TAG"
else
  run git tag -a "$TAG" -m "SiloForge $VERSION_ARG (SiloForge Connect $PLUGIN_VER)"
fi

echo
if [ "$DRY" = 1 ]; then
  if [ "$FAILED" = 1 ]; then echo "dry run: some checks FAILED (see above)"; exit 1; fi
  echo "dry run: all checks passed; run without --dry-run to build"
else
  echo "release $VERSION_ARG built: $APP_ZIP, $PLUGIN_ZIP (copied to $RELEASES_DIR), tag $TAG"
  echo "next: install (docs/OPERATIONS.md), then scripts/publish-github.sh $VERSION_ARG"
fi
