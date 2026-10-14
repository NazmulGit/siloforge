#!/usr/bin/env bash
# Publish a built SiloForge release to the public GitHub repo (scrubbed snapshot) with both zips attached.
#
#   scripts/publish-github.sh <version> [--dry-run]
#
# Needs the release built first (scripts/release.sh <version>): dist/siloforge-<version>-windows-x64.zip and
# dist/siloforge-connect-<plugin ver>.zip, each with .sha256. Steps (see docs/RELEASING.md):
#   1. checks: VERSION == <version>, zips + .sha256 present and matching, CHANGELOG section, gh logged in
#   2. python -I scripts/publish_snapshot.py D:\tmp\sf-public-new   (scrubs real domains, drops internal docs)
#   3. domain check: no real site domain/brand from the live DB (read-only) left in the snapshot
#   4. clone https://github.com/NazmulGit/siloforge into D:\tmp\sf-pub-repo if missing, else fetch + pull --ff-only
#   5. mirror the snapshot into the clone (keeps .git), domain check again, stage
#   6. commit "SiloForge <version>", annotated tag v<version>, push branch + tag
#   7. gh release create v<version> with both zips + .sha256 files, notes from CHANGELOG.md
#
# --dry-run does 1-5, shows the staged diff and the release notes, then resets the clone: no commit, tag, push or
# release. Only the private repo's tracked files (git ls-files) are published; its history never is.
# Env: PUBLIC_REPO (https://github.com/NazmulGit/siloforge.git), PUB_DIR (D:\tmp\sf-pub-repo),
#      SNAPSHOT_DIR (D:\tmp\sf-public-new), SILOFORGE_DB (D:\Final_Apps\SiloForge\data\siloforge.db).
set -euo pipefail

usage() { sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }

VERSION_ARG=""
DRY=0
for a in "$@"; do
  case "$a" in
    --dry-run) DRY=1 ;;
    -h|--help) usage ;;
    -*) echo "publish: unknown flag $a" >&2; usage ;;
    *) [ -z "$VERSION_ARG" ] && VERSION_ARG="$a" || { echo "publish: extra argument $a" >&2; usage; } ;;
  esac
done
[ -n "$VERSION_ARG" ] || usage

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && (pwd -W 2>/dev/null || pwd))"
cd "$ROOT"
PY="${PYTHON:-python}"
LIB="$ROOT/scripts/release_lib.py"
PUBLIC_REPO="${PUBLIC_REPO:-https://github.com/NazmulGit/siloforge.git}"
PUB_DIR="${PUB_DIR:-D:/tmp/sf-pub-repo}"
SNAPSHOT_DIR="${SNAPSHOT_DIR:-D:/tmp/sf-public-new}"
export SILOFORGE_DB="${SILOFORGE_DB:-D:/Final_Apps/SiloForge/data/siloforge.db}"
TAG="v$VERSION_ARG"

for p in "$PUB_DIR" "$SNAPSHOT_DIR"; do
  case "$p" in
    *[Rr]yzen*) echo "publish: refusing path under D:\\Ryzen: $p" >&2; exit 1 ;;
    D:/tmp/*|d:/tmp/*|D:\\tmp\\*|/d/tmp/*) ;;
    *) echo "publish: $p must be under D:\\tmp (it is deleted/rewritten)" >&2; exit 1 ;;
  esac
done

FAILED=0
ok()   { echo "  ok    $*"; }
bad()  { echo "  FAIL  $*" >&2; FAILED=1; [ "$DRY" = 1 ] || exit 1; }
step() { echo; echo "== $*"; }

step "publish SiloForge $VERSION_ARG to $PUBLIC_REPO$([ "$DRY" = 1 ] && echo ' (dry run)')"

# --- 1. checks ------------------------------------------------------------------------------------
FILE_VER="$(tr -d ' \r\n' < VERSION)"
[ "$FILE_VER" = "$VERSION_ARG" ] && ok "VERSION = $FILE_VER" || bad "VERSION file says '$FILE_VER', not '$VERSION_ARG'"
PLUGIN_VER="$("$PY" -I "$LIB" plugin-version wordpress/siloforge-connect/plugin/siloforge-connect/siloforge-connect.php)" || { PLUGIN_VER="?"; bad "plugin version"; }
APP_ZIP="dist/siloforge-$VERSION_ARG-windows-x64.zip"
PLUGIN_ZIP="dist/siloforge-connect-$PLUGIN_VER.zip"
ASSETS=()
for z in "$APP_ZIP" "$PLUGIN_ZIP"; do
  if [ -f "$z" ] && [ -f "$z.sha256" ]; then
    want="$(cut -d' ' -f1 < "$z.sha256")"
    have="$(sha256sum "$z" | cut -d' ' -f1)"
    [ "$want" = "$have" ] && ok "$(basename "$z") (sha256 matches)" || bad "$(basename "$z"): .sha256 does not match the zip"
    ASSETS+=("$z" "$z.sha256")
  else
    bad "missing $z or $z.sha256 (run scripts/release.sh $VERSION_ARG first)"
  fi
done
NOTES_FILE="$("$PY" -I -c "import tempfile;print(tempfile.mktemp(prefix='sf-notes-',suffix='.md',dir='D:/tmp'))")"
trap 'rm -f "$NOTES_FILE"' EXIT
if BODY="$("$PY" -I "$LIB" notes CHANGELOG.md "$VERSION_ARG" 2>/dev/null)"; then
  ok "CHANGELOG.md section found"
else
  BODY="(no CHANGELOG section for $VERSION_ARG)"; bad "CHANGELOG.md has no '## [$VERSION_ARG]' section"
fi
{
  printf '%s\n\n' "$BODY"
  printf '### Downloads\n\n'
  printf -- '- `siloforge-%s-windows-x64.zip`: SiloForge %s for Windows x64 (single binary + ONNX Runtime DirectML DLLs). Verify with the `.sha256` file.\n' "$VERSION_ARG" "$VERSION_ARG"
  printf -- '- `siloforge-connect-%s.zip`: the WordPress plugin SiloForge Connect %s (requires SiloForge 2.x). WordPress: Plugins -> Add New -> Upload Plugin.\n' "$PLUGIN_VER" "$PLUGIN_VER"
} > "$NOTES_FILE"
if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then ok "gh logged in"; else bad "gh missing or not logged in (gh auth login)"; fi

# --- 2-3. snapshot + domain check -----------------------------------------------------------------
step "snapshot -> $SNAPSHOT_DIR"
"$PY" -I scripts/publish_snapshot.py "$SNAPSHOT_DIR"
if "$PY" -I "$LIB" check-domains "$SNAPSHOT_DIR" "$SILOFORGE_DB"; then ok "snapshot has no real domains"; else bad "snapshot leaks real domains (fix publish_snapshot.py or the files)"; [ "$DRY" = 1 ] || exit 1; fi

# --- 4. public clone ------------------------------------------------------------------------------
step "public clone $PUB_DIR"
if [ -d "$PUB_DIR/.git" ]; then
  if [ -n "$(git -C "$PUB_DIR" status --porcelain)" ]; then bad "$PUB_DIR has local changes; clean it first"; fi
  git -C "$PUB_DIR" fetch --tags origin
  BRANCH="$(git -C "$PUB_DIR" symbolic-ref --short refs/remotes/origin/HEAD 2>/dev/null | sed 's#^origin/##')"
  BRANCH="${BRANCH:-main}"
  git -C "$PUB_DIR" checkout -q "$BRANCH"
  git -C "$PUB_DIR" pull -q --ff-only origin "$BRANCH"
  ok "updated ($BRANCH @ $(git -C "$PUB_DIR" rev-parse --short HEAD))"
else
  git clone -q "$PUBLIC_REPO" "$PUB_DIR"
  BRANCH="$(git -C "$PUB_DIR" branch --show-current)"
  ok "cloned ($BRANCH)"
fi
if git -C "$PUB_DIR" rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then bad "public repo already has tag $TAG"; fi

# --- 5. mirror + stage ----------------------------------------------------------------------------
step "sync snapshot into the clone"
"$PY" -I "$LIB" mirror "$SNAPSHOT_DIR" "$PUB_DIR"
if "$PY" -I "$LIB" check-domains "$PUB_DIR" "$SILOFORGE_DB"; then ok "clone has no real domains"; else bad "clone leaks real domains"; fi
git -C "$PUB_DIR" add -A
echo "  staged changes:"
git -C "$PUB_DIR" diff --cached --stat | tail -n 25 | sed 's/^/    /'

if [ "$DRY" = 1 ]; then
  step "release notes (preview)"
  sed 's/^/    /' "$NOTES_FILE" | head -n 60
  step "dry run: would commit 'SiloForge $VERSION_ARG', tag $TAG, push $BRANCH + $TAG, gh release create $TAG with:"
  for a in "${ASSETS[@]}"; do echo "    $a"; done
  # Leave the clone exactly as it was.
  git -C "$PUB_DIR" reset -q --hard HEAD
  git -C "$PUB_DIR" clean -qfd
  echo "  clone reset to $(git -C "$PUB_DIR" rev-parse --short HEAD)"
  [ "$FAILED" = 1 ] && { echo "dry run: some checks FAILED (see above)"; exit 1; }
  echo "dry run: all checks passed"
  exit 0
fi

# --- 6. commit, tag, push -------------------------------------------------------------------------
step "commit + tag + push"
if git -C "$PUB_DIR" diff --cached --quiet; then
  echo "  no file changes; tagging the current commit"
else
  git -C "$PUB_DIR" commit -q -m "SiloForge $VERSION_ARG"
fi
git -C "$PUB_DIR" tag -a "$TAG" -m "SiloForge $VERSION_ARG (SiloForge Connect $PLUGIN_VER)"
git -C "$PUB_DIR" push -q origin "$BRANCH"
git -C "$PUB_DIR" push -q origin "$TAG"
ok "pushed $BRANCH and $TAG"

# --- 7. GitHub release ----------------------------------------------------------------------------
step "GitHub release $TAG"
REPO_SLUG="$(printf '%s' "$PUBLIC_REPO" | sed -E 's#^https://github.com/##; s#\.git$##')"
if gh release view "$TAG" --repo "$REPO_SLUG" >/dev/null 2>&1; then
  gh release upload "$TAG" "${ASSETS[@]}" --repo "$REPO_SLUG" --clobber
  ok "release existed; assets uploaded"
else
  gh release create "$TAG" "${ASSETS[@]}" --repo "$REPO_SLUG" --title "SiloForge $VERSION_ARG" --notes-file "$NOTES_FILE" --verify-tag
  ok "release created"
fi
echo "published: https://github.com/$REPO_SLUG/releases/tag/$TAG"
