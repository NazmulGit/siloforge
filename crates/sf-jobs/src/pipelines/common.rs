//! Helpers shared by the pipelines: site loading (with decrypted credentials), hashing, text
//! truncation, settings access and small SQL utilities.

use std::collections::{BTreeSet, HashMap};

use rusqlite::OptionalExtension;
use serde_json::Value;
use sf_core::Error;
use sf_core::secrets::SiteSecrets;
use sf_crawl::{WpCredentials, WpSite};
use sf_store::err;

use crate::ctx::{JobContext, JobError, JobResult};

/// A site row with its decrypted credentials (never logged).
#[derive(Clone)]
pub struct SiteInfo {
    pub id: String,
    pub domain: String,
    pub wp_url: Option<String>,
    pub mode: String,
    pub settings: Value,
    pub secrets: SiteSecrets,
}

impl std::fmt::Debug for SiteInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SiteInfo")
            .field("id", &self.id)
            .field("domain", &self.domain)
            .field("mode", &self.mode)
            .field("has_wp_credentials", &self.secrets.has_wp_credentials())
            .finish()
    }
}

impl SiteInfo {
    /// Site root: `wp_url`, else `https://domain`.
    pub fn base_url(&self) -> String {
        let base = match self.wp_url.as_deref().map(str::trim) {
            Some(u) if !u.is_empty() => u.to_string(),
            _ => {
                let d = self.domain.trim();
                if d.starts_with("http://") || d.starts_with("https://") {
                    d.to_string()
                } else {
                    format!("https://{d}")
                }
            }
        };
        base.trim_end_matches('/').to_string()
    }

    /// The site's own hosts/URLs (domain, www.domain, wp_url host).
    pub fn allow_hosts(&self) -> Vec<String> {
        let mut out = vec![self.base_url()];
        if let Some(h) = sf_engine::graph::configured_host(&self.domain) {
            out.push(h.clone());
            out.push(format!("www.{h}"));
        }
        if let Some(h) = self
            .wp_url
            .as_deref()
            .and_then(|u| url::Url::parse(u).ok())
            .and_then(|u| u.host_str().map(str::to_string))
        {
            out.push(h);
        }
        out.sort();
        out.dedup();
        out
    }

    /// True when an admin allowed this site's own hosts to resolve to private addresses
    /// (`settings.allow_private_host`, admin-only) - see SSRF M-01/H-02.
    pub fn private_host_allowed(&self) -> bool {
        self.setting_bool("allow_private_host")
    }

    /// HTTP client for this site's jobs. Host aliases apply only to the site's own hosts, and
    /// those hosts may be private only when the global `crawl.allow_private_hosts` or the
    /// site's admin-only `allow_private_host` is set. Every other destination (external links,
    /// redirect targets, sitemap children) must resolve to public addresses (SSRF H-02).
    pub fn http_client(&self, ctx: &JobContext) -> JobResult<sf_crawl::HttpClient> {
        self.http_client_from(ctx.http_config())
    }

    /// HTTP client for WordPress REST writes (inject/undo writers): like
    /// [`SiteInfo::http_client`] with `[wordpress] request_timeout_ms`.
    pub fn wp_http_client(&self, ctx: &JobContext) -> JobResult<sf_crawl::HttpClient> {
        self.http_client_from(ctx.wp_http_config())
    }

    fn http_client_from(&self, mut cfg: sf_crawl::HttpConfig) -> JobResult<sf_crawl::HttpClient> {
        let hosts = self.allow_hosts();
        if self.private_host_allowed() {
            cfg.allow_hosts.extend(hosts.iter().cloned());
        }
        cfg.site_hosts.extend(hosts);
        sf_crawl::HttpClient::from_http_config(cfg).map_err(JobError::from)
    }

    pub fn has_wp_credentials(&self) -> bool {
        self.secrets.has_wp_credentials()
    }

    pub fn wp_credentials(&self) -> Option<WpCredentials> {
        match (&self.secrets.wp_user, &self.secrets.wp_app_password) {
            (Some(u), Some(p)) if !u.is_empty() && !p.is_empty() => {
                Some(WpCredentials::new(u.clone(), p.clone()))
            }
            _ => None,
        }
    }

    /// `WpSite` for the REST crawler / writer (credentials attached when present).
    pub fn wp_site(&self, with_credentials: bool) -> JobResult<WpSite> {
        let mut site = WpSite::new(&self.base_url())?;
        if with_credentials && let Some(c) = self.wp_credentials() {
            site = site.with_credentials(c);
        }
        Ok(site)
    }

    pub fn setting_bool(&self, key: &str) -> bool {
        self.settings
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    pub fn setting_str(&self, key: &str) -> Option<String> {
        self.settings
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    }

    pub fn setting_f64(&self, key: &str) -> Option<f64> {
        self.settings.get(key).and_then(Value::as_f64)
    }

    /// Normalized money-page URLs (`settings.money_page_urls`).
    pub fn money_urls(&self) -> BTreeSet<String> {
        self.settings
            .get("money_page_urls")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(sf_engine::html::normalize_url)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Allowed insert modules (`settings.insert_modules`, default native).
    pub fn insert_modules(&self) -> Vec<sf_core::types::InsertModule> {
        let v: Vec<_> = self
            .settings
            .get("insert_modules")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .filter_map(sf_core::types::InsertModule::parse_known)
                    .collect()
            })
            .unwrap_or_default();
        if v.is_empty() {
            vec![sf_core::types::InsertModule::Native]
        } else {
            v
        }
    }
}

/// Load a site (404-style error when missing). Credentials come from `secrets_enc`; legacy
/// plaintext `settings.wp_user/wp_app_password` (not yet migrated) are honoured in memory only.
pub async fn load_site(ctx: &JobContext, site_id: &str) -> JobResult<SiteInfo> {
    let sid = site_id.to_string();
    let row = ctx
        .db
        .read(move |c| {
            c.query_row(
                "SELECT id, domain, wp_url, mode, settings, secrets_enc FROM sites WHERE id=?1",
                [&sid],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(err)
        })
        .await?;
    let Some((id, domain, wp_url, mode, settings, secrets_enc)) = row else {
        return Err(JobError::fatal(Error::not_found(format!(
            "site not found: {site_id}"
        ))));
    };
    let mut settings: Value =
        serde_json::from_str(&settings).unwrap_or_else(|_| Value::Object(Default::default()));
    let mut secrets = match SiteSecrets::load(&ctx.secret_key, &secrets_enc) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(site = %id, error = %e, "site secrets could not be decrypted; continuing without credentials");
            SiteSecrets::default()
        }
    };
    if !secrets.has_wp_credentials() {
        let mut legacy = SiteSecrets::default();
        let mut copy = settings.clone();
        if legacy.absorb_from_settings(&mut copy) && legacy.has_wp_credentials() {
            secrets.wp_user = legacy.wp_user;
            secrets.wp_app_password = legacy.wp_app_password;
        }
    }
    // Never keep plaintext credentials around in the settings value.
    if let Some(o) = settings.as_object_mut() {
        o.remove("wp_app_password");
    }
    apply_wordpress_defaults(&mut settings, &ctx.live.get().wordpress);
    Ok(SiteInfo {
        id,
        domain,
        wp_url,
        mode,
        settings,
        secrets,
    })
}

/// Fills keys a site leaves unset from the install-wide `[wordpress]` defaults (v2.1). The
/// defaults equal the engine defaults, so an untouched section changes nothing. In memory only.
pub fn apply_wordpress_defaults(settings: &mut Value, wp: &sf_core::config::WordpressCfg) {
    let Some(o) = settings.as_object_mut() else {
        return;
    };
    let mut fill = |k: &str, v: Value| {
        if o.get(k).is_none_or(Value::is_null) {
            o.insert(k.to_string(), v);
        }
    };
    fill(
        "max_links_per_post",
        Value::from(wp.default_max_links_per_post),
    );
    fill(
        "min_links_per_post",
        Value::from(wp.default_min_links_per_post),
    );
    fill(
        "max_new_links_per_pass",
        Value::from(wp.max_new_links_per_pass),
    );
    if !wp.default_insert_modules.is_empty() {
        fill(
            "insert_modules",
            Value::from(wp.default_insert_modules.clone()),
        );
    }
    if !wp.verify_after_inject {
        fill("skip_inject_verify", Value::Bool(true));
    }
}

/// v2 content hash: SHA-256 of the rendered HTML (v1-compatible when there is no raw source),
/// with the raw source folded in when present so raw-only edits are detected.
pub fn content_hash(rendered: &str, raw: Option<&str>) -> String {
    match raw {
        Some(r) if !r.is_empty() => {
            let mut s = String::with_capacity(rendered.len() + r.len() + 8);
            s.push_str(rendered);
            s.push_str("\u{0}raw\u{0}");
            s.push_str(r);
            sf_core::crypto::sha256_hex(s.as_bytes())
        }
        _ => sf_core::crypto::sha256_hex(rendered.as_bytes()),
    }
}

/// First `max` UTF-16 code units of `s` (v1 `.slice(0, max)` semantics, never splits a char).
pub fn truncate_utf16(s: &str, max: usize) -> String {
    let mut units = 0usize;
    let mut end = 0usize;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > max {
            break;
        }
        end = i + c.len_utf8();
    }
    s.get(..end).unwrap_or("").to_string()
}

/// `?,?,?` placeholder list for `n` parameters starting at `?{start}`.
pub fn placeholders(start: usize, n: usize) -> String {
    (0..n)
        .map(|i| format!("?{}", start + i))
        .collect::<Vec<_>>()
        .join(",")
}

/// Map of `url_path_key(url)` → post id for a site (first id wins, deterministic by id).
pub async fn path_index(
    ctx: &JobContext,
    site_id: &str,
) -> JobResult<(HashMap<String, String>, Vec<String>)> {
    let sid = site_id.to_string();
    let rows: Vec<(String, String)> = ctx
        .db
        .read(move |c| {
            let mut st = c
                .prepare("SELECT id, url FROM posts WHERE site_id=?1 ORDER BY id")
                .map_err(err)?;
            let v = st
                .query_map([&sid], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            Ok(v)
        })
        .await?;
    let mut map = HashMap::new();
    let mut urls = Vec::with_capacity(rows.len());
    for (id, url) in rows {
        if let Some(k) = sf_engine::graph::url_path_key(&url, None) {
            map.entry(k).or_insert(id);
        }
        urls.push(url);
    }
    Ok((map, urls))
}

/// Site hosts for internal-link detection: configured domain ∪ wp_url host ∪ hosts of post URLs.
pub fn site_hosts(site: &SiteInfo, post_urls: &[String]) -> Vec<String> {
    let mut hosts =
        sf_engine::graph::site_hosts(&site.domain, post_urls.iter().map(String::as_str));
    if let Some(h) = site
        .wp_url
        .as_deref()
        .and_then(sf_engine::graph::url_host_key)
    {
        hosts.insert(h);
    }
    hosts.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_truncation_and_hash() {
        assert_eq!(truncate_utf16("abc", 2), "ab");
        assert_eq!(truncate_utf16("a😀b", 2), "a");
        assert_eq!(truncate_utf16("a😀b", 3), "a😀");
        assert_eq!(truncate_utf16("", 5), "");
        assert_eq!(
            content_hash("<p>x</p>", None),
            sf_core::crypto::sha256_hex(b"<p>x</p>")
        );
        assert_ne!(
            content_hash("<p>x</p>", Some("raw")),
            content_hash("<p>x</p>", None)
        );
        assert_eq!(placeholders(2, 3), "?2,?3,?4");
    }
}
