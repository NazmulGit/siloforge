//! WordPress REST crawler and writer.
//!
//! Crawl: `/wp-json/wp/v2/{posts,pages,<cpt>}` with `per_page=100`, `_fields` to trim the
//! payload and `X-WP-TotalPages` for pagination. With credentials (user + application
//! password, HTTP Basic) it requests `context=edit`, which adds `content.raw` (the Gutenberg /
//! classic SOURCE). v2 injects links into raw content only; rendered HTML must never be pushed
//! back (v1 critical bug #1).
//!
//! Write: [`WpWriter::update_post_content`] re-reads the post and refuses with
//! [`CrawlError::Conflict`] when `modified_gmt` moved since the caller's read (v1 bug #2).
//! WordPress has no `If-Match`, so a tiny window between the re-read and the write remains;
//! the job runner's per-site lock covers our own concurrent writers.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use url::Url;

use crate::client::{FetchRequest, HttpClient};
use crate::error::{CrawlError, Result};
use crate::html::{self, LinkPresence};

/// WordPress application-password credentials. `Debug` never prints the password.
#[derive(Clone)]
pub struct WpCredentials {
    pub user: String,
    app_password: String,
}

impl WpCredentials {
    /// `app_password` may contain the spaces WordPress displays; they are accepted as-is.
    pub fn new(user: impl Into<String>, app_password: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            app_password: app_password.into(),
        }
    }
}

impl std::fmt::Debug for WpCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WpCredentials")
            .field("user", &self.user)
            .field("app_password", &"<redacted>")
            .finish()
    }
}

/// A REST collection to crawl: `rest_base` is the URL segment, `post_type` the stored type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WpPostType {
    pub rest_base: String,
    pub post_type: String,
}

impl WpPostType {
    /// e.g. `WpPostType::new("products", "product")`.
    pub fn new(rest_base: impl Into<String>, post_type: impl Into<String>) -> Self {
        Self {
            rest_base: rest_base.into(),
            post_type: post_type.into(),
        }
    }
}

/// A WordPress site to crawl / write.
#[derive(Debug, Clone)]
pub struct WpSite {
    /// Site root (may include a sub-directory, e.g. `https://ex.com/blog`).
    pub base_url: Url,
    pub credentials: Option<WpCredentials>,
    /// Defaults to posts + pages.
    pub post_types: Vec<WpPostType>,
    /// `status` filter (needs credentials for non-publish). Default `["publish"]`.
    pub statuses: Vec<String>,
    /// Items per page (1..=100). Default 100.
    pub per_page: u32,
    /// Safety cap on pages per post type. Default 10 000.
    pub max_pages: u32,
    /// Body cap per REST page. Default 64 MiB.
    pub max_page_bytes: usize,
}

impl WpSite {
    /// Parse the site root URL.
    pub fn new(base_url: &str) -> Result<Self> {
        let trimmed = base_url.trim().trim_end_matches('/');
        let base_url =
            Url::parse(trimmed).map_err(|e| CrawlError::InvalidUrl(format!("{base_url}: {e}")))?;
        if !matches!(base_url.scheme(), "http" | "https") || base_url.host_str().is_none() {
            return Err(CrawlError::InvalidUrl(base_url.to_string()));
        }
        Ok(Self {
            base_url,
            credentials: None,
            post_types: vec![
                WpPostType::new("posts", "post"),
                WpPostType::new("pages", "page"),
            ],
            statuses: vec!["publish".into()],
            per_page: 100,
            max_pages: 10_000,
            max_page_bytes: 64 * 1024 * 1024,
        })
    }

    /// Attach credentials (enables `context=edit`).
    pub fn with_credentials(mut self, c: WpCredentials) -> Self {
        self.credentials = Some(c);
        self
    }

    /// Replace the crawled collections (e.g. add CPTs).
    pub fn with_post_types(mut self, types: Vec<WpPostType>) -> Self {
        self.post_types = types;
        self
    }

    /// `{base}/wp-json/wp/v2/{path}` with query pairs.
    pub fn api_url(&self, path: &str, query: &[(&str, &str)]) -> Result<Url> {
        let mut u = self.base_url.clone();
        let base_path = u.path().trim_end_matches('/').to_string();
        u.set_path(&format!(
            "{base_path}/wp-json/wp/v2/{}",
            path.trim_start_matches('/')
        ));
        u.set_query(None);
        u.set_fragment(None);
        if !query.is_empty() {
            let mut q = u.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
        }
        Ok(u)
    }

    /// REST base for a stored post type (`post`→`posts`, `page`→`pages`, CPTs via `post_types`).
    pub fn rest_base_for(&self, post_type: &str) -> String {
        if let Some(t) = self.post_types.iter().find(|t| t.post_type == post_type) {
            return t.rest_base.clone();
        }
        match post_type {
            "post" | "posts" => "posts".into(),
            "page" | "pages" => "pages".into(),
            other => other.into(),
        }
    }

    fn authed(&self, req: FetchRequest) -> FetchRequest {
        match &self.credentials {
            Some(c) => req.basic_auth(c.user.clone(), c.app_password.clone()),
            None => req,
        }
    }
}

/// SEO meta exposed through REST (Yoast `yoast_head_json`, or registered Yoast/RankMath meta).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeoMeta {
    pub title: Option<String>,
    pub description: Option<String>,
    pub focus_keyword: Option<String>,
    /// `yoast` | `rankmath` when detected.
    pub source: Option<String>,
}

/// One crawled post/page. For sitemap crawls `wp_post_id` and `content_raw` are `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawledPost {
    pub wp_post_id: Option<u64>,
    pub url: String,
    pub slug: Option<String>,
    /// Plain-text title (tags stripped, entities decoded).
    pub title: String,
    /// `content.rendered` (or the extracted main content for sitemap pages).
    pub content_rendered: String,
    /// `content.raw` (block/classic source) when crawled with `context=edit`.
    pub content_raw: Option<String>,
    /// Plain-text excerpt, max 300 chars.
    pub excerpt: Option<String>,
    pub post_type: String,
    pub status: String,
    /// WP `modified_gmt` exactly as returned (no `Z`); pass back to `update_post_content`.
    pub modified_gmt: Option<String>,
    pub published_gmt: Option<String>,
    pub author: Option<u64>,
    pub categories: Vec<u64>,
    pub tags: Vec<u64>,
    pub seo: SeoMeta,
    /// `elementor` | `divi` | `gutenberg` | `classic`.
    pub builder_hint: String,
}

impl CrawledPost {
    /// v1 meta description: SEO description, else the excerpt.
    pub fn meta_description(&self) -> Option<String> {
        self.seo
            .description
            .clone()
            .filter(|d| !d.trim().is_empty())
            .or_else(|| self.excerpt.clone())
    }
}

/// `YYYY-MM-DDTHH:MM:SS` → `YYYY-MM-DDTHH:MM:SSZ` (idempotent).
pub fn gmt_to_iso(gmt: &str) -> String {
    if gmt.ends_with('Z') {
        gmt.to_string()
    } else {
        format!("{gmt}Z")
    }
}

/// One page of crawl results, streamed to the caller.
#[derive(Debug, Clone)]
pub struct CrawlPage {
    pub post_type: String,
    pub page: u32,
    pub total_pages: Option<u32>,
    pub posts: Vec<CrawledPost>,
}

/// Totals of a crawl.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WpCrawlStats {
    pub posts: usize,
    pub pages_fetched: usize,
    /// Collections that returned 404 on page 1 (not exposed).
    pub types_absent: Vec<String>,
    /// Items skipped because they had no numeric id.
    pub skipped_items: usize,
}

const FIELDS: &str = "id,link,slug,title,content,excerpt,type,status,date_gmt,modified_gmt,author,\
categories,tags,meta,yoast_head_json.title,yoast_head_json.description,\
rank_math_title,rank_math_description,rank_math_focus_keyword";

/// Streaming WordPress REST crawler.
#[derive(Debug, Clone)]
pub struct WpCrawler {
    client: HttpClient,
    site: WpSite,
}

impl WpCrawler {
    /// New crawler over a shared client.
    pub fn new(client: HttpClient, site: WpSite) -> Self {
        Self { client, site }
    }

    /// Crawl every configured collection, sending one [`CrawlPage`] per REST page.
    /// Stops with [`CrawlError::Cancelled`] if the receiver is dropped. Memory stays bounded
    /// to one REST page (plus the channel buffer).
    pub async fn crawl(&self, tx: mpsc::Sender<CrawlPage>) -> Result<WpCrawlStats> {
        let mut stats = WpCrawlStats::default();
        for pt in &self.site.post_types {
            let mut page: u32 = 1;
            let mut total: Option<u32> = None;
            loop {
                if page > self.site.max_pages {
                    tracing::warn!(rest_base = %pt.rest_base, "max_pages reached; stopping");
                    break;
                }
                let Some(res) = self.fetch_page(pt, page).await? else {
                    if page == 1 {
                        stats.types_absent.push(pt.rest_base.clone());
                    }
                    break;
                };
                stats.pages_fetched += 1;
                stats.skipped_items += res.skipped;
                if total.is_none() {
                    total = res.total_pages;
                }
                let n = res.posts.len();
                stats.posts += n;
                if n > 0 {
                    tx.send(CrawlPage {
                        post_type: pt.post_type.clone(),
                        page,
                        total_pages: total,
                        posts: res.posts,
                    })
                    .await
                    .map_err(|_| CrawlError::Cancelled)?;
                }
                let done = match total {
                    Some(t) => page >= t,
                    None => n + res.skipped < self.site.per_page as usize,
                };
                if done || n + res.skipped == 0 {
                    break;
                }
                page += 1;
            }
        }
        Ok(stats)
    }

    /// Fetch one REST page. `Ok(None)` = past the last page, or collection absent (404).
    pub async fn fetch_page(&self, pt: &WpPostType, page: u32) -> Result<Option<FetchedPage>> {
        let per_page = self.site.per_page.clamp(1, 100).to_string();
        let page_s = page.to_string();
        let statuses = self.site.statuses.join(",");
        let mut q: Vec<(&str, &str)> = vec![
            ("per_page", &per_page),
            ("page", &page_s),
            ("_fields", FIELDS),
            ("orderby", "id"),
            ("order", "asc"),
        ];
        if self.site.credentials.is_some() {
            q.push(("context", "edit"));
            q.push(("status", &statuses));
        }
        let url = self.site.api_url(&pt.rest_base, &q)?;
        let req = self
            .site
            .authed(FetchRequest::get(url.clone()).max_body(self.site.max_page_bytes));
        let resp = self.client.fetch(req).await?;
        if resp.status == 400 {
            let code = resp
                .json::<Value>()
                .ok()
                .and_then(|v| v.get("code").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default();
            if code == "rest_post_invalid_page_number" {
                return Ok(None);
            }
        }
        if resp.status == 404 && page == 1 {
            return Ok(None);
        }
        let resp = resp.error_for_status()?;
        let total_pages = resp
            .header("x-wp-totalpages")
            .and_then(|v| v.trim().parse::<u32>().ok());
        let items: Value = resp.json()?;
        let Value::Array(items) = items else {
            return Err(CrawlError::Decode(format!(
                "{url}: expected a JSON array of posts"
            )));
        };
        let mut posts = Vec::with_capacity(items.len());
        let mut skipped = 0;
        for item in &items {
            match parse_post(item, &self.site, &pt.post_type) {
                Some(p) => posts.push(p),
                None => skipped += 1,
            }
        }
        Ok(Some(FetchedPage {
            posts,
            total_pages,
            skipped,
        }))
    }
}

/// One parsed REST page.
#[derive(Debug, Clone)]
pub struct FetchedPage {
    pub posts: Vec<CrawledPost>,
    pub total_pages: Option<u32>,
    pub skipped: usize,
}

fn s(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_str().map(str::to_string)
}

fn non_empty(o: Option<String>) -> Option<String> {
    o.map(|x| x.trim().to_string()).filter(|x| !x.is_empty())
}

fn ids(v: Option<&Value>) -> Vec<u64> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

fn meta_str(item: &Value, key: &str) -> Option<String> {
    non_empty(s(item, &["meta", key]).or_else(|| s(item, &[key])))
}

fn parse_seo(item: &Value) -> SeoMeta {
    let yoast_title = non_empty(s(item, &["yoast_head_json", "title"]))
        .or_else(|| meta_str(item, "_yoast_wpseo_title"));
    let yoast_desc = non_empty(s(item, &["yoast_head_json", "description"]))
        .or_else(|| meta_str(item, "_yoast_wpseo_metadesc"));
    let yoast_kw = meta_str(item, "_yoast_wpseo_focuskw");
    let rm_title = meta_str(item, "rank_math_title");
    let rm_desc = meta_str(item, "rank_math_description");
    let rm_kw = meta_str(item, "rank_math_focus_keyword");
    let has_yoast = yoast_title.is_some() || yoast_desc.is_some() || yoast_kw.is_some();
    let has_rm = rm_title.is_some() || rm_desc.is_some() || rm_kw.is_some();
    SeoMeta {
        title: yoast_title.or(rm_title),
        description: yoast_desc.or(rm_desc),
        // RankMath stores comma-separated keywords; the first is the primary one.
        focus_keyword: yoast_kw
            .or_else(|| rm_kw.and_then(|k| non_empty(k.split(',').next().map(str::to_string)))),
        source: if has_yoast {
            Some("yoast".into())
        } else if has_rm {
            Some("rankmath".into())
        } else {
            None
        },
    }
}

/// Lenient item parser: anything but a numeric `id` may be missing or oddly shaped.
fn parse_post(item: &Value, site: &WpSite, default_type: &str) -> Option<CrawledPost> {
    let id = item.get("id")?.as_u64()?;
    let rendered = s(item, &["content", "rendered"]).unwrap_or_default();
    let raw = s(item, &["content", "raw"]);
    let title = {
        let t = html::strip_tags(&s(item, &["title", "rendered"]).unwrap_or_default());
        if t.is_empty() {
            html::strip_tags(&s(item, &["title", "raw"]).unwrap_or_default())
        } else {
            t
        }
    };
    let excerpt = non_empty(
        s(item, &["excerpt", "rendered"]).map(|e| html::truncate_chars(&html::strip_tags(&e), 300)),
    );
    let url = non_empty(s(item, &["link"])).unwrap_or_else(|| {
        let mut u = site.base_url.clone();
        let p = u.path().trim_end_matches('/').to_string();
        u.set_path(&format!("{p}/"));
        u.set_query(Some(&format!("p={id}")));
        u.to_string()
    });
    let builder_hint = html::detect_builder(&rendered, raw.as_deref()).to_string();
    Some(CrawledPost {
        wp_post_id: Some(id),
        url,
        slug: non_empty(s(item, &["slug"])),
        title,
        content_rendered: rendered,
        content_raw: raw,
        excerpt,
        post_type: non_empty(s(item, &["type"])).unwrap_or_else(|| default_type.to_string()),
        status: non_empty(s(item, &["status"])).unwrap_or_else(|| "publish".into()),
        modified_gmt: non_empty(s(item, &["modified_gmt"])),
        published_gmt: non_empty(s(item, &["date_gmt"])),
        author: item.get("author").and_then(Value::as_u64),
        categories: ids(item.get("categories")),
        tags: ids(item.get("tags")),
        seo: parse_seo(item),
        builder_hint,
    })
}

/// A post's editable source as read just before writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPost {
    pub id: u64,
    pub rest_base: String,
    pub raw: String,
    pub rendered: String,
    pub modified_gmt: String,
    pub status: String,
    pub link: String,
}

/// Result of a successful content update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub id: u64,
    pub modified_gmt: String,
    /// `content.rendered` after the update (for verification).
    pub rendered: String,
    pub link: String,
    /// `content.raw` WordPress stored (R10: compare with what was sent; filters or a concurrent
    /// save between the precondition GET and the POST show up here). None when not returned.
    pub raw: Option<String>,
}

/// Writes post content through the REST API (requires credentials).
#[derive(Debug, Clone)]
pub struct WpWriter {
    client: HttpClient,
    site: WpSite,
}

impl WpWriter {
    /// Fails with `Config` when the site has no credentials.
    pub fn new(client: HttpClient, site: WpSite) -> Result<Self> {
        if site.credentials.is_none() {
            return Err(CrawlError::Config(
                "WP writer needs wp_user + application password".into(),
            ));
        }
        Ok(Self { client, site })
    }

    /// Read `content.raw` + `modified_gmt` (`context=edit`). `post_type` picks the endpoint;
    /// a 404 on `posts`/`pages` retries the other one.
    pub async fn get_post_raw(&self, id: u64, post_type: &str) -> Result<RawPost> {
        let base = self.site.rest_base_for(post_type);
        match self.get_raw_at(id, &base).await {
            Err(CrawlError::Http { status: 404, .. }) if base == "posts" || base == "pages" => {
                let other = if base == "posts" { "pages" } else { "posts" };
                self.get_raw_at(id, other).await
            }
            r => r,
        }
    }

    async fn get_raw_at(&self, id: u64, rest_base: &str) -> Result<RawPost> {
        let url = self.site.api_url(
            &format!("{rest_base}/{id}"),
            &[
                ("context", "edit"),
                ("_fields", "id,link,status,modified_gmt,content"),
            ],
        )?;
        let req = self
            .site
            .authed(FetchRequest::get(url.clone()).max_body(self.site.max_page_bytes));
        let v: Value = self.client.fetch(req).await?.error_for_status()?.json()?;
        let raw = s(&v, &["content", "raw"]).ok_or_else(|| {
            CrawlError::Decode(format!(
                "{url}: content.raw missing (credentials lack edit rights?)"
            ))
        })?;
        Ok(RawPost {
            id,
            rest_base: rest_base.to_string(),
            raw,
            rendered: s(&v, &["content", "rendered"]).unwrap_or_default(),
            modified_gmt: s(&v, &["modified_gmt"]).unwrap_or_default(),
            status: s(&v, &["status"]).unwrap_or_default(),
            link: s(&v, &["link"]).unwrap_or_default(),
        })
    }

    /// Replace `content` (raw source) iff the post's `modified_gmt` still equals
    /// `expected_modified_gmt`; otherwise `CrawlError::Conflict` and nothing is written.
    pub async fn update_post_content(
        &self,
        id: u64,
        post_type: &str,
        new_raw: &str,
        expected_modified_gmt: &str,
    ) -> Result<UpdateOutcome> {
        let current = self.get_post_raw(id, post_type).await?;
        if current.modified_gmt.trim() != expected_modified_gmt.trim() {
            return Err(CrawlError::Conflict {
                post_id: id,
                expected: expected_modified_gmt.to_string(),
                actual: current.modified_gmt,
            });
        }
        let url = self.site.api_url(
            &format!("{}/{id}", current.rest_base),
            &[
                ("context", "edit"),
                ("_fields", "id,link,modified_gmt,content"),
            ],
        )?;
        let req = FetchRequest::new(reqwest::Method::POST, url)
            .json(&serde_json::json!({ "content": new_raw }))?
            .max_body(self.site.max_page_bytes);
        let v: Value = self
            .client
            .fetch(self.site.authed(req))
            .await?
            .error_for_status()?
            .json()?;
        Ok(UpdateOutcome {
            id,
            modified_gmt: s(&v, &["modified_gmt"]).unwrap_or_default(),
            rendered: s(&v, &["content", "rendered"]).unwrap_or_default(),
            link: s(&v, &["link"]).unwrap_or(current.link),
            raw: s(&v, &["content", "raw"]),
        })
    }
}

/// Options for [`verify_live`].
#[derive(Debug, Clone)]
pub struct VerifyOptions {
    /// Total attempts (v1: 3).
    pub attempts: u32,
    /// Backoff before attempt n+1 = `backoff * 2^(n-1)` (v1: 1500 ms).
    pub backoff: Duration,
    /// Body cap for the rendered page (truncated beyond).
    pub max_body_bytes: usize,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self {
            attempts: 3,
            backoff: Duration::from_millis(1500),
            max_body_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Outcome of a live check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveCheck {
    /// Last HTTP status (None when every attempt failed at transport level).
    pub status: Option<u16>,
    pub final_url: Option<String>,
    pub presence: LinkPresence,
    /// Last response looked like a stale cache hit (v1 `evaluateFreshness`).
    pub stale_cache: bool,
    pub attempts: u32,
    /// Last transport error, if any.
    pub error: Option<String>,
}

impl LiveCheck {
    /// Link with the expected anchor is live.
    pub fn found(&self) -> bool {
        self.presence.found()
    }
}

/// v1 `evaluateFreshness`: true when cache headers say the page was served stale.
pub fn is_stale_cache(headers: &reqwest::header::HeaderMap) -> bool {
    let h = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_ascii_lowercase())
    };
    if let Some(cf) = h("cf-cache-status") {
        return !matches!(
            cf.as_str(),
            "miss" | "expired" | "dynamic" | "bypass" | "revalidated"
        );
    }
    if let Some(ls) = h("x-litespeed-cache") {
        return ls.contains("hit");
    }
    if let Some(xc) = h("x-cache") {
        if xc.contains("hit") {
            return true;
        }
        if xc.contains("miss") || xc.contains("bypass") || xc.contains("dynamic") {
            return false;
        }
    }
    h("age")
        .and_then(|a| a.parse::<u64>().ok())
        .is_some_and(|a| a > 0)
}

/// Fetch the rendered page (no-cache headers) and check that `<a href=href>anchor</a>` exists in
/// its content region. Retries on 429/5xx/transport errors and on "missing but stale cache".
pub async fn verify_live(
    client: &HttpClient,
    url: &str,
    href: &str,
    anchor: &str,
    opts: &VerifyOptions,
) -> Result<LiveCheck> {
    let page_url = Url::parse(url).map_err(|e| CrawlError::InvalidUrl(format!("{url}: {e}")))?;
    let attempts = opts.attempts.max(1);
    let mut last = LiveCheck {
        status: None,
        final_url: None,
        presence: LinkPresence::default(),
        stale_cache: false,
        attempts: 0,
        error: None,
    };
    for n in 1..=attempts {
        if n > 1 {
            let wait = opts.backoff.saturating_mul(2u32.saturating_pow(n - 2));
            tokio::time::sleep(wait).await;
        }
        last.attempts = n;
        let req = FetchRequest::get(page_url.clone())
            .header("cache-control", "no-cache")?
            .header("pragma", "no-cache")?
            .max_body(opts.max_body_bytes)
            .truncate();
        let resp = match client.fetch(req).await {
            Ok(r) => r,
            Err(e @ CrawlError::Blocked { .. }) | Err(e @ CrawlError::InvalidUrl(_)) => {
                return Err(e);
            }
            Err(e) => {
                last.error = Some(e.to_string());
                continue;
            }
        };
        last.status = Some(resp.status);
        last.final_url = Some(resp.url.to_string());
        last.stale_cache = is_stale_cache(&resp.headers);
        last.error = None;
        if resp.status == 429 || resp.status >= 500 {
            continue;
        }
        if resp.status != 200 {
            last.presence = LinkPresence::default();
            return Ok(last);
        }
        let body = resp.text();
        let final_url = resp.url.to_string();
        let (h, a) = (href.to_string(), anchor.to_string());
        let presence =
            tokio::task::spawn_blocking(move || html::find_link(&body, &final_url, &h, &a))
                .await
                .map_err(|e| CrawlError::Other(format!("verify parse task: {e}")))?;
        last.presence = presence;
        if last.found() || !last.stale_cache {
            return Ok(last);
        }
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_post_lenient() {
        let site = WpSite::new("https://ex.com/blog/").unwrap();
        let v = serde_json::json!({
            "id": 7, "title": {"rendered": "A &amp; B"}, "content": {"rendered": "<p>x</p>", "raw": "<!-- wp:paragraph --><p>x</p>"},
            "excerpt": {"rendered": "<p>Short</p>"}, "type": "post", "status": "publish",
            "modified_gmt": "2026-01-01T00:00:00", "categories": [1, "bad", 2], "tags": null,
            "yoast_head_json": {"description": "Desc"}, "meta": {"rank_math_focus_keyword": "bikes, locks"}
        });
        let p = parse_post(&v, &site, "post").unwrap();
        assert_eq!(p.title, "A & B");
        assert_eq!(p.url, "https://ex.com/blog/?p=7");
        assert_eq!(p.builder_hint, "gutenberg");
        assert_eq!(p.categories, vec![1, 2]);
        assert_eq!(p.seo.source.as_deref(), Some("yoast"));
        assert_eq!(p.seo.focus_keyword.as_deref(), Some("bikes"));
        assert_eq!(p.meta_description().as_deref(), Some("Desc"));
        assert!(parse_post(&serde_json::json!({"id": "x"}), &site, "post").is_none());
        assert!(parse_post(&serde_json::json!([1, 2]), &site, "post").is_none());
        assert_eq!(gmt_to_iso("2026-01-01T00:00:00"), "2026-01-01T00:00:00Z");
    }

    #[test]
    fn api_url_and_rest_base() {
        let site = WpSite::new("https://ex.com/blog/").unwrap();
        let u = site.api_url("posts", &[("page", "2")]).unwrap();
        assert_eq!(u.as_str(), "https://ex.com/blog/wp-json/wp/v2/posts?page=2");
        assert_eq!(site.rest_base_for("page"), "pages");
        assert_eq!(site.rest_base_for("product"), "product");
        let dbg = format!("{:?}", WpCredentials::new("u", "secret pass"));
        assert!(!dbg.contains("secret"));
    }

    #[test]
    fn freshness() {
        let mut h = reqwest::header::HeaderMap::new();
        assert!(!is_stale_cache(&h));
        h.insert(
            "cf-cache-status",
            reqwest::header::HeaderValue::from_static("HIT"),
        );
        assert!(is_stale_cache(&h));
        h.insert(
            "cf-cache-status",
            reqwest::header::HeaderValue::from_static("DYNAMIC"),
        );
        assert!(!is_stale_cache(&h));
    }
}
