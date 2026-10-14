# Releasing SiloForge

A release ships two artefacts from this repo, built and published by three bash scripts (bash + Python stdlib; no
PowerShell, no `zip`):

| Script | What it does |
|---|---|
| `scripts/build-plugin.sh [out_dir]` | WordPress plugin zip `dist/siloforge-connect-<plugin ver>.zip` + `.sha256` |
| `scripts/release.sh <version> [--dry-run]` | checks, `cargo xtask ci`, UI build, release exe, app zip, plugin zip, copy to `D:\Final_Apps\SiloForge\releases`, tag `v<version>` |
| `scripts/publish-github.sh <version> [--dry-run]` | scrubbed snapshot → public repo commit + tag + push → GitHub release with both zips |

`scripts/release_lib.py` holds the shared helpers (zip, sha256, notices, CHANGELOG notes, mirror, domain check).

## Versioning

- **App**: SemVer in `VERSION` (single source). `Cargo.toml` `[workspace.package] version` and `ui/package.json`
  `version` must match it; `siloforge version` and `GET /health` report it.
- **Plugin** (SiloForge Connect): its own SemVer in the plugin header (`Version:` in
  `wordpress/siloforge-connect/plugin/siloforge-connect/siloforge-connect.php`). `SFC_VERSION` and the `Stable tag:` in
  its `readme.txt` must match the header (build-plugin.sh refuses otherwise). Bump it only when the plugin changes; a
  new major is needed only when it stops working with a backend major. The plugin states the backend major it needs
  ("Requires SiloForge 2.x") and warns on the settings page when `/health` reports another one.
- **Tags**: `v<app version>` (annotated) on the private repo and on the public repo. The plugin has no tags of its own.
- **Assets**: every app release on GitHub carries the app zip **and** the current plugin zip, each with a `.sha256`.
  The same four files go to `D:\Final_Apps\SiloForge\releases`, where the UI's Downloads page lists them.
- **Public repo** (`github.com/NazmulGit/siloforge`) = scrubbed snapshots: one commit per release made from the
  tracked files of this repo after `scripts/publish_snapshot.py` (real site domains → `siteNN.example`, internal docs
  and the devlog dropped, personal paths generalised). The private history is never pushed.
- **CHANGELOG.md** has one `## [<version>] - <date>` section per app release; the GitHub release notes are that
  section plus a Downloads list. The plugin has its own `wordpress/siloforge-connect/CHANGELOG.md` and readme changelog.

## Checklist

1. **Bump**: `VERSION`, `Cargo.toml` `[workspace.package] version`, `ui/package.json` (`npm install` once to update the
   lock file), the `CHANGELOG.md` section. If the plugin changed: header `Version:`, `SFC_VERSION`, readme
   `Stable tag:` + changelog, plugin `CHANGELOG.md`.
2. **Commit** everything. The release must come from a clean tree.
3. **Dry run**: `scripts/release.sh <version> --dry-run` (checks only, prints the plan, exit 1 if a check fails).
4. **Build**: `scripts/release.sh <version>`. It runs:
   - `cargo +stable-x86_64-pc-windows-gnu xtask ci` with WinLibs first on PATH and `CARGO_TARGET_DIR=D:\tmp\cargo-target\siloforge`
   - `npm --prefix ui ci` + `npm --prefix ui run build` (npm cache `D:\Programs\npm-cache`)
   - `cargo build --release --locked -p siloforge`, then checks `siloforge.exe version`
   - stages `dist/<version>/`: `siloforge.exe`, `onnxruntime.dll`, `DirectML.dll` (+ `onnxruntime_providers_shared.dll`)
     from `D:\Download\onnxruntime\dml-1.24.4`, `README.md`, `CHANGELOG.md`, `LICENSE`, `siloforge.example.toml`,
     `THIRD_PARTY_NOTICES.txt` + `licenses/` (from `D:\Download\onnxruntime\x`), `SHA256SUMS.txt`
   - zips it to `dist/siloforge-<version>-windows-x64.zip` (root `siloforge-<version>/`) + `.sha256`
   - `scripts/build-plugin.sh` (php -l via docker `php:7.4-cli` when no local php)
   - copies the four files to `D:\Final_Apps\SiloForge\releases`
   - creates the annotated tag `v<version>` if it does not exist (never moves an existing tag)

   Flags: `--skip-ci`, `--skip-ui` (reuse `ui/dist`). Env overrides: `RUNTIME_DIR`, `LICENSE_DIR`, `RELEASES_DIR`,
   `CARGO_TARGET_DIR`.
5. **Deploy** the exe (see `docs/OPERATIONS.md`: backup, disable keepalive, stop by PID, copy, enable + run, check
   `/health`). Upgrade the plugin on the WordPress sites from the plugin zip (Plugins → Add New → Upload).
6. **Publish**: `scripts/publish-github.sh <version> --dry-run`, read the staged diff and the notes preview, then
   `scripts/publish-github.sh <version>`. It:
   - verifies both zips against their `.sha256`, the CHANGELOG section and `gh auth status`
   - runs `python -I scripts/publish_snapshot.py D:\tmp\sf-public-new`
   - fails if any real site domain or brand name (read from `D:\Final_Apps\SiloForge\data\siloforge.db` with
     `mode=ro`) is left in the snapshot; it prints file names and counts only, never the names
   - clones `https://github.com/NazmulGit/siloforge` into `D:\tmp\sf-pub-repo` (or fetches + `pull --ff-only`),
     refuses a dirty clone or an existing tag, mirrors the snapshot (keeps `.git`), checks domains again
   - commits `SiloForge <version>`, tags `v<version>`, pushes branch + tag
   - `gh release create v<version>` with the app zip, the plugin zip and both `.sha256` files, title
     `SiloForge <version>`, notes = CHANGELOG section + Downloads list (if the release already exists the assets are
     re-uploaded with `--clobber`)

   `--dry-run` stops before the commit, shows the diff and notes, and resets the clone.
7. **Log** the release in `log/devlog.md` and `TODO.md`.

## Plugin-only fix

A plugin fix without app changes still ships with the next app release (the plugin zip is attached to every release).
For an urgent plugin fix: bump the plugin version, run `scripts/build-plugin.sh`, copy the zip + `.sha256` to
`D:\Final_Apps\SiloForge\releases` (the Downloads page shows it), and upload it to the latest GitHub release with
`gh release upload v<app version> dist/siloforge-connect-<ver>.zip dist/siloforge-connect-<ver>.zip.sha256 --repo NazmulGit/siloforge`.

## Verify a download

```bash
sha256sum -c siloforge-<version>-windows-x64.zip.sha256
sha256sum -c siloforge-connect-<ver>.zip.sha256
```

Zips are reproducible: the same tree gives the same bytes (fixed timestamps, sorted entries).

## Notes

- `scripts/build-release.ps1` / `package-release.ps1` are the older PowerShell path (PowerShell hangs on the dev PC);
  `release.sh` replaces them (same zip layout, minus the PowerShell install/cutover/rollback scripts).
- Everything is written on `D:\` (`dist/`, `D:\tmp`, `D:\Final_Apps`); the scripts refuse paths outside it and
  anything under `D:\Ryzen`.
- Never print secrets: the scripts touch no `secrets.env`, and the domain check reports file names only.
