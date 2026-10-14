---
title: WordPress plugin (SiloForge Connect)
section: Administration
order: 4
---

# WordPress plugin (SiloForge Connect)

**SiloForge Connect 0.2.0** is an optional WordPress plugin. It lets WordPress itself pull approved links from
SiloForge and splice them into posts.

## Do I need it?

| Situation | Plugin needed? |
|---|---|
| Site in `wp_rest` mode with an application password stored in SiloForge | **No.** SiloForge writes links directly through the REST API. This is the recommended setup. |
| You cannot or do not want to give SiloForge an application password | Yes |
| SiloForge cannot reach the site directly, but the site can reach SiloForge | Yes |
| Non-WordPress site | No; use CSV export |

Without credentials, approved links are counted as **marked** at inject time and wait for the plugin to pull them.

## Install

1. Download `siloforge-connect-0.2.0.zip` from **Workspace → Downloads**. Check its SHA-256 if you like.
2. In WordPress: **Plugins → Add New → Upload Plugin**, choose the zip, **Install Now**, then **Activate**.
3. Open **Settings → SiloForge** in WordPress.
4. Enter the **API URL** of your SiloForge server (for example `http://192.168.1.20:8600`; it must be reachable from the
   WordPress server) and the site's **API key** from when you added the site.
5. Click **Test Connection**. The page shows "Connected" and the SiloForge backend version.

> **Warning:** SiloForge listens only on the PC itself (`127.0.0.1`) by default. For a plugin on another machine an admin
> must open the server address to the network, behind a firewall rule for trusted hosts only.

## Use

1. Approve links in SiloForge as usual.
2. In WordPress, **Settings → SiloForge → Pull & Apply Links**.
3. The plugin takes a snapshot of each post (last 3 kept), splices the link and reports back. **Revert** restores the
   last snapshot and tells SiloForge, so the links become re-approvable.

## What is new in 0.2.0

- Before splicing, the plugin checks that the paragraph is unchanged (paragraph hash). If the post was edited since the
  plan, the link is skipped and reported as "content changed" instead of being placed in the wrong spot.
- A link is reported as applied only after WordPress actually saved the post.
- The settings page shows the SiloForge backend version.
- Readme and changelog updated for SiloForge v2.

Uninstalling the plugin removes all of its stored data (`_sfc_*` post meta). Links already in your posts stay.
