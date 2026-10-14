=== SiloForge Connect ===
Contributors: siloforge
Tags: internal links, seo, silo, links
Requires at least: 5.8
Tested up to: 6.7
Requires PHP: 7.4
Stable tag: 0.2.0
License: GPLv2 or later
License URI: https://www.gnu.org/licenses/gpl-2.0.html

Connects WordPress to a self-hosted SiloForge 2.x internal-linking backend. Requires SiloForge 2.x.

== Description ==

SiloForge Connect links this WordPress site to a self-hosted SiloForge backend (version 2.x):

* Handshakes with the backend using the site API key (JWT cached for 25 days, re-handshake on 401).
* Sync Now pushes all published posts and pages (rendered content) in byte-budgeted batches to `/api/v1/sync`.
* Pull & Apply Links fetches approved link suggestions from `/api/v1/suggestions/{site_id}`, verifies the target paragraph (`paragraph_hash`), splices the `<a>` tag into that paragraph (never inside an existing link), saves the post, and only then acknowledges each edge. A paragraph that changed since the plan is skipped and reported as "content changed".
* Before the first change to a post, the original content is snapshotted to post meta (`_sfc_snapshot_*`, last 3 kept) so changes can be reverted.
* The settings page shows the backend version (from `/health`), the connection status and the last handshake.
* Exposes `GET /wp-json/siloforge/v1/status` (administrators only).

== Installation ==

1. Upload the zip via Plugins → Add New → Upload Plugin (or copy the `siloforge-connect` folder to `/wp-content/plugins/`).
2. Activate the plugin through the Plugins screen.
3. Go to Settings → SiloForge, enter the backend API URL (e.g. `http://backend-host:8600`) and the site API key, then click Test Connection.

== Frequently Asked Questions ==

= Which SiloForge version do I need? =

SiloForge 2.x. The settings page warns when the backend reports another major version.

= The settings page says "API key rejected". =

The site key was probably rotated in SiloForge (Sites → Rotate key). Paste the new key and save; the plugin reconnects immediately.

= How do I undo an applied link? =

Call `SFC_Linker::revert_last( $post_id )` (e.g. via WP-CLI `wp eval`), or restore the newest `_sfc_snapshot_*` post meta value into the post content.

== Changelog ==

= 0.2.0 =
* Requires SiloForge 2.x.
* New: the target paragraph is verified with `paragraph_hash` before splicing (fallback: the indexed paragraph must still contain the anchor); on mismatch the edge is skipped and acked "content changed".
* Fix: edges are acked as applied only after `wp_update_post` succeeded; a failed save acks them as failed. Undelivered acks are queued and re-sent, never re-applied.
* New: settings page shows the backend version and health (`/health`), connection status and last handshake.
* New: site-key rotation handling: clear "API key rejected" status, no hammering of the handshake with a dead key, immediate reconnect after saving the new key.
* The status REST route adds `plugin_version` and `status`.

= 0.1.1 =
* Fix: sync batches are now flushed by a 3 MB byte budget (plus the 50-post cap), so content-heavy sites no longer hit HTTP 413 on /api/v1/sync.
* Fix: uninstall now removes all _sfc_* post meta (snapshots and applied-edge records).
* Fix: snapshot meta-key suffix is collision-proof when a post is applied to twice in the same second.

= 0.1.0 =
* Initial release: handshake, batch sync, suggestion pull & apply with snapshots and acks, status REST route.

== Upgrade Notice ==

= 0.2.0 =
For SiloForge 2.x backends. Safer link insertion (paragraph verification, ack after save).
