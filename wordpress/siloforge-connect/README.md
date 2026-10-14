# SiloForge Connect — WordPress plugin for SiloForge

**SiloForge Connect** is the WordPress plugin for the self-hosted [SiloForge](https://github.com/NazmulGit/siloforge)
internal-linking platform. It syncs your posts up for analysis, then pulls the **approved** internal-link suggestions
back down and inserts them, with paragraph verification, an anchor-nest guard, per-post snapshots and revert.

- **Version:** 0.2.0 · **Requires:** SiloForge 2.x, WordPress 5.8+, PHP 7.4+ · **License:** GPLv2 or later
- **Backend:** a running SiloForge 2.x server (default API `http://<host>:8600`)

## What it does
- **Sync** (`POST /api/v1/sync`): published posts and pages (rendered content), byte-budget batches (no HTTP 413).
- **Pull & Apply** (`GET /api/v1/suggestions/{site_id}`): for each approved edge, finds the target paragraph by its
  `paragraph_hash`, inserts the link inside that paragraph (never inside an existing `<a>`), saves the post with
  `wp_update_post`, and only then acks `POST /suggestions/{site_id}/ack {edge_id, ok:true}`. A paragraph that no longer
  matches is skipped and acked `{ok:false, error:"content changed"}`.
- **Revert**: original content is snapshotted to post meta before the first change (last 3 kept);
  `SFC_Linker::revert_last( $post_id )` restores it and sends `/revert-ack` so the edges become re-approvable.
- **Settings → SiloForge**: API URL + site API key, connection panel (backend version from `GET /health`, status,
  last handshake), Test Connection, Sync Now, Pull & Apply Links.

### Paragraph verification (how `paragraph_hash` is checked)
The backend hashes paragraphs with its canonical splitter (SHA-1 of the whitespace-normalised, entity-decoded text of
each block of 40+ UTF-16 units, duplicates dropped). That splitter cannot be reproduced exactly in PHP, so the plugin:
1. hashes every candidate block (`<p>`, list items and lists, quotes, table cells, divs, blank-line runs) of the raw
   content in three text-join variants and looks for the expected hash; if the backend hashed the rendered content,
   the rendered block is mapped back to the raw block with the same text;
2. otherwise accepts the block at `paragraph_index` (±2, nearest first) **only if it still contains the anchor text**
   outside existing links;
3. otherwise skips the edge and acks `content changed`.

Measured on the live database: the PHP hashes matched 98.4% of 10,566 paragraphs and 89% of paragraphs that carry
link edges; the rest go through step 2.

### Site-key rotation
When a site key is rotated in SiloForge, the plugin's JWT stops working (401). The plugin re-handshakes once; if the
key is rejected it shows **API key rejected** on the settings page and stops retrying that key for 10 minutes (the
backend rate-limits failed handshakes). Paste the new key and save: it reconnects immediately.

## Layout
```
wordpress/siloforge-connect/
├── plugin/siloforge-connect/      # the installable plugin (the zip root is siloforge-connect/)
│   ├── siloforge-connect.php      # main file + plugin header (Version: is the plugin version)
│   ├── includes/
│   │   ├── class-sfc-client.php   # handshake + Bearer JWT client, /health, key-rotation state
│   │   ├── class-sfc-sync.php     # batched post sync -> POST /api/v1/sync
│   │   ├── class-sfc-linker.php   # pull, verify paragraph, splice <a>, save, ack, snapshots, revert
│   │   ├── class-sfc-admin.php    # Settings -> SiloForge page
│   │   └── class-sfc-rest.php     # /wp-json/siloforge/v1/status
│   ├── readme.txt
│   └── uninstall.php
├── tests/linker-test.php           # behaviour tests with stubbed WP (run with docker php:7.4-cli)
└── CHANGELOG.md
```

## Install
1. Download `siloforge-connect-<version>.zip` from the SiloForge GitHub release (or the SiloForge UI → Downloads).
2. WordPress admin → Plugins → Add New → Upload Plugin → choose the zip → Activate (upgrading replaces the old version).
3. Settings → SiloForge: set **API URL** and **API Key** (shown when you add the site in SiloForge), then **Test Connection**.
4. **Sync Now**, run a plan in SiloForge, approve links, then **Pull & Apply Links** here.

## Build and test
- Zip: `scripts/build-plugin.sh` (repo root) → `dist/siloforge-connect-<version>.zip` + `.sha256`.
- Lint: `docker run --rm -v "$PWD/wordpress/siloforge-connect/plugin:/p" php:7.4-cli sh -c 'for f in $(find /p -name "*.php"); do php -l $f; done'`
- Tests: `docker run --rm -v "$PWD/wordpress/siloforge-connect:/w" php:7.4-cli php /w/tests/linker-test.php`
- Release process: `docs/RELEASING.md`.
