---
title: Sites
section: Start here
order: 3
---

# Sites

**Workspace → Sites** lists your sites with mode, post count, link counts and last sync. Each row has **Crawl**,
**Plan** and **Open** buttons. Users see only their own sites; admins see all of them.

## Add a site

1. Click **+ Add Site**.
2. Fill in:
   - **Domain**: `example.com` (without `https://`).
   - **WordPress URL** (optional): `https://example.com`, if WordPress lives at another address or in a subfolder.
   - **Crawl mode**: see the table below.
3. Click **Create site**.
4. A window shows the site's **API key** (`sf_...`). It is shown **once**. Copy it now if you will use the
   [SiloForge Connect plugin](wordpress-plugin). If you lose it, rotate the key; this signs the old plugin out.

| Mode | Reads content from | Can SiloForge write links? |
|---|---|---|
| `wp_rest` (recommended) | The WordPress REST API, raw post content | Yes, with an application password |
| `sitemap` | The XML sitemap and the public pages | No. Export links as CSV, or use the plugin |

## Connect WordPress (application password)

To write links, SiloForge needs a WordPress user that can edit posts, and an **application password** for that user.

1. In WordPress, open **Users → Profile** (or edit the user SiloForge should act as).
2. Under **Application Passwords**, enter a name such as `SiloForge` and click **Add New Application Password**.
3. Copy the generated password (it looks like `abcd efgh ijkl mnop qrst uvwx`).
4. Store it on the site. In 2.1 this is done through the API, with an API token that has `write` scope
   (see [API and MCP](api-and-mcp)):

   ```powershell
   $h = @{ Authorization = "Bearer sfk_your_token" }
   $body = '{"settings":{"wp_user":"siloforge-editor","wp_app_password":"abcd efgh ijkl mnop qrst uvwx"}}'
   Invoke-RestMethod -Method Patch http://127.0.0.1:8600/api/v1/sites/<site id> -Headers $h `
     -ContentType application/json -Body $body
   ```

   The reply shows `has_wp_credentials: true`. The password is encrypted at rest and never shown again: not in the
   UI, not in the API, not to AI agents. The site id is in the browser address bar on any site page
   (`/site/<site id>/dashboard`).
5. Run a crawl. With credentials the crawler reads the raw post content, which keeps Gutenberg blocks intact.

To remove the credentials, send `"wp_app_password": null` the same way. An AI agent can do the same with the MCP tool
`siloforge_site_set_wp_credentials`.

> **Warning:** WordPress disables application passwords on sites served over plain `http://`. Use `https://`. For a
> local test site only, set `WP_ENVIRONMENT_TYPE` to `local` in `wp-config.php`.

> **Tip:** Use a dedicated editor account rather than your own admin login. You can then revoke SiloForge's access in
> WordPress without touching your own account.

## Local and LAN sites

For safety the crawler refuses private addresses (`localhost`, `192.168.x.x`, `site01.example` and similar),
on every redirect too. An admin can allow it for one site with the site setting `allow_private_host: true`, or for
all sites with the global crawl setting **allow private hosts**. Only the site's own hosts may then be private;
cloud metadata addresses are always blocked.

## Per-site settings

Linking rules (density limits, default topology, money pages, root URLs, anchor options, verify) are per site, in
**Settings → Linking Rules**. Pick the site at the top of the page first.

## Non-WordPress sites

For Webflow, Ghost or any other CMS, use the site's **Integrations** page: **CSV Import** takes a file with the header
`url,title,content_html`, and **CSV Export** gives you the approved and applied links
(`source_url,target_url,anchor,anchor_type,status,insert_module`) to place by hand.
