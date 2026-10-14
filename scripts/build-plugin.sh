#!/usr/bin/env bash
# Build the installable SiloForge Connect zip.
#
#   scripts/build-plugin.sh [out_dir]        (default out_dir: dist/)
#
# -> <out_dir>/siloforge-connect-<ver>.zip + .sha256. The zip root is siloforge-connect/ (a valid WordPress plugin
# folder, so "Upload Plugin" installs or upgrades it). <ver> comes from the plugin header; the header, SFC_VERSION
# and the readme.txt Stable tag must agree. PHP files are linted first when php (or docker) is available.
# bash + python stdlib only (no zip, no PowerShell).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && (pwd -W 2>/dev/null || pwd))"
PY="${PYTHON:-python}"
LIB="$ROOT/scripts/release_lib.py"
PLUGIN="$ROOT/wordpress/siloforge-connect/plugin/siloforge-connect"
OUT="${1:-$ROOT/dist}"

case "$OUT" in
  D:*|d:*|/d/*) ;;
  *) echo "build-plugin: out dir must be on D:\\ (got $OUT)" >&2; exit 1 ;;
esac
case "$OUT" in
  *[Rr]yzen*) echo "build-plugin: refusing to write under D:\\Ryzen" >&2; exit 1 ;;
esac

VER="$("$PY" -I "$LIB" plugin-version "$PLUGIN/siloforge-connect.php")"
echo "build-plugin: SiloForge Connect $VER"

# Lint (optional): local php, else docker php:7.4-cli (the minimum supported PHP).
if [ "${SKIP_LINT:-0}" != "1" ]; then
  if command -v php >/dev/null 2>&1; then
    find "$PLUGIN" -name '*.php' -print0 | while IFS= read -r -d '' f; do php -l "$f" >/dev/null || { echo "php -l failed: $f" >&2; exit 1; }; done
    echo "build-plugin: php -l ok (local php)"
  elif command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    MSYS_NO_PATHCONV=1 docker run --rm -v "$PLUGIN:/p:ro" php:7.4-cli sh -c \
      'rc=0; for f in $(find /p -name "*.php"); do php -l "$f" >/dev/null || rc=1; done; exit $rc' \
      || { echo "build-plugin: php -l failed" >&2; exit 1; }
    echo "build-plugin: php -l ok (docker php:7.4-cli)"
  else
    echo "build-plugin: WARNING no php/docker, lint skipped"
  fi
fi

mkdir -p "$OUT"
"$PY" -I "$LIB" zip "$PLUGIN" "$OUT/siloforge-connect-$VER.zip" siloforge-connect
