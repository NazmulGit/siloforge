# Changelog — SiloForge Connect

## 0.2.0 — 2026-10-14
For the SiloForge 2.x backend (the Rust single binary). Requires SiloForge 2.x.

- **Paragraph verification before splicing**: suggestions now carry `paragraph_hash` (SHA-1 of the backend's
  canonical paragraph text). The plugin finds the paragraph whose text hashes to it (raw `post_content`, or the
  rendered content mapped back to the raw block) and inserts the link **inside that paragraph only**. If no block
  matches, it falls back to the block at `paragraph_index` (±2) that still contains the anchor text outside links.
  If neither holds, the suggestion is skipped and acked `{ok:false, error:"content changed"}`.
  The backend's splitter cannot be reproduced byte-for-byte in PHP; the approximation (three text-join variants)
  matched 98.4% of 10,566 live paragraph hashes and 89% of paragraphs that carry links; the rest use the anchor check.
- **Ack only after the save (fix R8)**: `ok:true` acks are sent only after `wp_update_post()` succeeded. If the save
  fails, every edge of that post is acked `ok:false` ("wp_update_post failed: ...") and the snapshot is discarded.
  Acks that cannot be delivered after a save are queued (`sfc_pending_acks`), re-sent on the next Pull & Apply,
  and those edges are never applied twice.
- **Settings page**: new Connection panel with the backend version and health from `GET /health`, connection status
  (connected / not connected / key rejected), last handshake time and pending acks. Warns when the backend is not 2.x.
- **Site-key rotation**: a 401 drops the JWT and re-handshakes once; if the key itself was rotated the plugin reports
  "API key rejected", stops retrying the dead key for 10 minutes (the backend rate-limits failed handshakes), and
  reconnects as soon as the new key is saved. HTTP 429 on the handshake gets its own message.
- `/wp-json/siloforge/v1/status` adds `plugin_version` and `status` (additive).
- Uninstall also removes `sfc_auth_state`, `sfc_pending_acks` and the `sfc_health` cache.
- Release zips are built by `scripts/build-plugin.sh` in the SiloForge repo and attached to every app release.


## 0.1.1 — 2026-09-28
Packages the 2026-07-17 fix (commit `1c9bb67`), which landed after the 0.1.0 zip was built.

- **Sync 413 blocker**: batches are flushed by a byte budget (`MAX_BATCH_BYTES` = 3 MB serialized) as well as the 50-post cap, whichever comes first, keeping each POST to `/api/v1/sync` well under the backend 25 MB body limit. A single post larger than the budget is still sent on its own.
- **Uninstall cleanup**: `uninstall.php` now deletes every `_sfc_*` post meta key (snapshots and their `_sfc_applied_*` edge records), not only `_sfc_snapshot_*`.
- **Snapshot suffix**: snapshot keys use `time()_<uniqid tail>` so two applies in the same second no longer collide; ordering stays chronological by the leading timestamp, then deterministic by the full suffix.

## 0.1.0 — 2026-07-16 (first release)
First release, shipped alongside SILOFORGE v0.1.0.

- Handshake auth: paste API key → Bearer JWT cached in a transient, single re-auth on 401.
- **Sync**: batched (50/page) POST of published posts + pages to `/api/v1/sync` — rendered content, meta description, builder detection (Elementor/Divi/Gutenberg/classic), timestamps.
- **Pull & Apply**: GET `/api/v1/suggestions/{site_id}`, native anchor splicing with a DOM-based anchor-nest guard (never links text already inside an `<a>`), paragraph replacement for LLM modules, `wp_update_post`, per-edge `/ack`.
- **Snapshots + revert**: original content stored in post meta (last 3), one-click `revert_last`.
- **Revert-ack**: revert notifies the backend (`/suggestions/{site_id}/revert-ack`) so applied edges become re-approvable and backend state matches the live site.
- Admin settings page (Settings → SiloForge), `/wp-json/siloforge/v1/status` REST route, clean uninstall.
- Verified end-to-end against a live WordPress install: 19 links injected + curl-verified, revert/re-apply confirmed. PHP-lint clean on 7.4 and 8.2; 12/12 splice-logic tests pass.
