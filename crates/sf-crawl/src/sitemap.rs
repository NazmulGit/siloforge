//! Sitemap crawler for non-WordPress sites (mode `sitemap`).
//!
//! robots.txt → `Sitemap:` lines (fallback `/sitemap.xml`, `/sitemap_index.xml`,
//! `/wp-sitemap.xml`) → sitemap-index recursion (bounded depth and count; every child goes
//! through the SSRF-guarded client, fixing v1 #26) → same-site URL filter → robots `Disallow`
//! filter → page fetch with bounded concurrency → main-content extraction.

use std::collections::{HashSet, VecDeque};
use std::io::Read;
use std::time::Duration;

use futures::StreamExt;
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use url::Url;

use crate::client::{FetchRequest, HttpClient};
use crate::error::{CrawlError, Result};
use crate::html;
use crate::wp::{CrawledPost, SeoMeta};

/// Options for [`SitemapCrawler`].
#[derive(Debug, Clone)]
pub struct SitemapOptions {
    /// Stop collecting page URLs past this count.
    pub max_urls: usize,
    /// Max sitemap documents fetched (index + children).
    pub max_sitemaps: usize,
    /// Max index nesting depth (root = 0).
    pub max_depth: u32,
    /// Concurrent page fetches (the client's per-host limit still applies).
    pub concurrency: usize,
    /// Posts per batch sent to the caller.
    pub batch_size: usize,
    pub respect_robots: bool,
    /// User-agent token matched against robots.txt groups.
    pub robots_agent: String,
    pub max_sitemap_bytes: usize,
    /// Page bodies beyond this are truncated.
    pub max_page_bytes: usize,
}

impl Default for SitemapOptions {
    fn default() -> Self {
        Self {
            max_urls: 50_000,
            max_sitemaps: 500,
            max_depth: 3,
            concurrency: 4,
            batch_size: 25,
            respect_robots: true,
            robots_agent: "SiloForge".into(),
            max_sitemap_bytes: 50 * 1024 * 1024,
            max_page_bytes: 5 * 1024 * 1024,
        }
    }
}

/// A parsed sitemap document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SitemapDoc {
    /// `<sitemapindex>`: child sitemap URLs.
    Index(Vec<String>),
    /// `<urlset>`: page URLs.
    UrlSet(Vec<String>),
}

/// Parse sitemap XML (optionally gzip-compressed). At most `max_locs` locations are kept.
/// Malformed XML is a `Decode` error, never a panic.
pub fn parse_sitemap(bytes: &[u8], max_locs: usize, max_inflated: usize) -> Result<SitemapDoc> {
    let inflated;
    let data: &[u8] = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes)
            .take(max_inflated as u64)
            .read_to_end(&mut out)
            .map_err(|e| CrawlError::Decode(format!("gzip sitemap: {e}")))?;
        inflated = out;
        &inflated
    } else {
        bytes
    };
    let mut reader = quick_xml::Reader::from_reader(data);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut is_index: Option<bool> = None;
    let mut in_loc = false;
    let mut cur = String::new();
    let mut locs = Vec::new();
    loop {
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| CrawlError::Decode(format!("sitemap xml: {e}")))?;
        match ev {
            Event::Start(e) => match e.local_name().as_ref() {
                b"sitemapindex" if is_index.is_none() => is_index = Some(true),
                b"urlset" if is_index.is_none() => is_index = Some(false),
                b"loc" => {
                    in_loc = true;
                    cur.clear();
                }
                _ => {}
            },
            Event::Text(t) if in_loc => {
                let txt = t
                    .unescape()
                    .map_err(|e| CrawlError::Decode(format!("sitemap text: {e}")))?;
                cur.push_str(&txt);
            }
            Event::CData(t) if in_loc => {
                cur.push_str(&String::from_utf8_lossy(&t.into_inner()));
            }
            Event::End(e) if e.local_name().as_ref() == b"loc" => {
                in_loc = false;
                let loc = cur.trim();
                if !loc.is_empty() && locs.len() < max_locs {
                    locs.push(loc.to_string());
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    match is_index {
        Some(true) => Ok(SitemapDoc::Index(locs)),
        Some(false) => Ok(SitemapDoc::UrlSet(locs)),
        None => Err(CrawlError::Decode(
            "not a sitemap (no <urlset> or <sitemapindex>)".into(),
        )),
    }
}

/// robots.txt rules for one site.
#[derive(Debug)]
pub struct RobotsRules {
    robot: Option<texting_robots::Robot>,
    disallow_all: bool,
    /// `Sitemap:` URLs.
    pub sitemaps: Vec<String>,
    /// `Crawl-delay` for our agent (capped at 10 s).
    pub crawl_delay: Option<Duration>,
}

impl RobotsRules {
    /// Allow-all rules (no robots.txt).
    pub fn allow_all() -> Self {
        Self {
            robot: None,
            disallow_all: false,
            sitemaps: Vec::new(),
            crawl_delay: None,
        }
    }

    /// Parse robots.txt content; unparseable content is treated as allow-all.
    pub fn parse(agent: &str, txt: &[u8]) -> Self {
        match texting_robots::Robot::new(agent, txt) {
            Ok(r) => Self {
                sitemaps: r.sitemaps.clone(),
                crawl_delay: r
                    .delay
                    .filter(|d| d.is_finite() && *d > 0.0)
                    .map(|d| Duration::from_secs_f32(d.min(10.0))),
                robot: Some(r),
                disallow_all: false,
            },
            Err(_) => Self::allow_all(),
        }
    }

    /// May `url` be fetched?
    pub fn allowed(&self, url: &str) -> bool {
        if self.disallow_all {
            return false;
        }
        self.robot.as_ref().is_none_or(|r| r.allowed(url))
    }
}

/// URLs discovered from sitemaps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitemapUrls {
    pub urls: Vec<String>,
    pub sitemaps_fetched: usize,
    /// Skipped: other host.
    pub offsite: usize,
    /// Skipped: robots disallow.
    pub disallowed: usize,
    /// Non-fatal errors (failed child sitemaps etc.).
    pub errors: Vec<String>,
}

/// Totals of a sitemap crawl.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitemapCrawlStats {
    pub discovered: usize,
    pub crawled: usize,
    pub failed: usize,
    pub skipped_non_html: usize,
    pub disallowed: usize,
    pub errors: Vec<String>,
}

/// Sitemap-mode crawler.
#[derive(Debug, Clone)]
pub struct SitemapCrawler {
    client: HttpClient,
    base: Url,
    hosts: HashSet<String>,
    opts: SitemapOptions,
}

const MAX_ERRORS_KEPT: usize = 50;

fn push_err(errors: &mut Vec<String>, e: String) {
    if errors.len() < MAX_ERRORS_KEPT {
        errors.push(e);
    }
}

impl SitemapCrawler {
    /// `base_url` is the site root; its host (www-insensitive) defines "same site".
    pub fn new(client: HttpClient, base_url: &str, opts: SitemapOptions) -> Result<Self> {
        let base = Url::parse(base_url.trim())
            .map_err(|e| CrawlError::InvalidUrl(format!("{base_url}: {e}")))?;
        if !matches!(base.scheme(), "http" | "https") {
            return Err(CrawlError::InvalidUrl(base.to_string()));
        }
        let hosts = html::site_host_set([base.as_str()]);
        Ok(Self {
            client,
            base,
            hosts,
            opts,
        })
    }

    fn root(&self, path: &str) -> Option<Url> {
        self.base.join(path).ok()
    }

    /// Fetch and parse `/robots.txt`. 4xx = allow all; 5xx/unreachable = disallow all (RFC 9309).
    pub async fn robots(&self) -> RobotsRules {
        let Some(url) = self.root("/robots.txt") else {
            return RobotsRules::allow_all();
        };
        let req = FetchRequest::get(url).max_body(512 * 1024).truncate();
        match self.client.fetch(req).await {
            Ok(r) if r.is_success() => RobotsRules::parse(&self.opts.robots_agent, &r.body),
            Ok(r) if r.status < 500 => RobotsRules::allow_all(),
            Ok(r) => {
                tracing::warn!(
                    status = r.status,
                    "robots.txt unavailable; treating as disallow-all"
                );
                RobotsRules {
                    disallow_all: true,
                    ..RobotsRules::allow_all()
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "robots.txt unreachable; treating as disallow-all");
                RobotsRules {
                    disallow_all: true,
                    ..RobotsRules::allow_all()
                }
            }
        }
    }

    fn same_site(&self, url: &str) -> bool {
        html::host_key(url).is_some_and(|h| self.hosts.contains(&h))
    }

    /// Discover page URLs from sitemaps (recursing into indexes).
    pub async fn discover_urls(&self, robots: &RobotsRules) -> SitemapUrls {
        let mut out = SitemapUrls::default();
        let mut roots: Vec<String> = robots.sitemaps.clone();
        let fallbacks: Vec<String> = ["/sitemap.xml", "/sitemap_index.xml", "/wp-sitemap.xml"]
            .iter()
            .filter_map(|p| self.root(p).map(|u| u.to_string()))
            .collect();
        let use_fallbacks = roots.is_empty();
        if use_fallbacks {
            roots = fallbacks;
        }
        let mut seen_sitemaps: HashSet<String> = HashSet::new();
        let mut seen_urls: HashSet<String> = HashSet::new();
        for root in roots {
            self.walk(&root, robots, &mut seen_sitemaps, &mut seen_urls, &mut out)
                .await;
            // With guessed roots, the first one that yields URLs wins (they usually overlap).
            if use_fallbacks && !out.urls.is_empty() {
                break;
            }
            if out.urls.len() >= self.opts.max_urls {
                break;
            }
        }
        out
    }

    async fn walk(
        &self,
        root: &str,
        robots: &RobotsRules,
        seen_sitemaps: &mut HashSet<String>,
        seen_urls: &mut HashSet<String>,
        out: &mut SitemapUrls,
    ) {
        let mut queue: VecDeque<(String, u32)> = VecDeque::new();
        queue.push_back((root.to_string(), 0));
        while let Some((sm, depth)) = queue.pop_front() {
            if out.sitemaps_fetched >= self.opts.max_sitemaps
                || out.urls.len() >= self.opts.max_urls
            {
                return;
            }
            if !seen_sitemaps.insert(sm.clone()) {
                continue;
            }
            let Ok(url) = Url::parse(&sm) else {
                push_err(&mut out.errors, format!("bad sitemap url {sm}"));
                continue;
            };
            out.sitemaps_fetched += 1;
            let req = FetchRequest::get(url).max_body(self.opts.max_sitemap_bytes);
            let body = match self
                .client
                .fetch(req)
                .await
                .and_then(|r| r.error_for_status())
            {
                Ok(r) => r.body,
                Err(e) => {
                    push_err(&mut out.errors, format!("{sm}: {e}"));
                    continue;
                }
            };
            let remaining = self.opts.max_urls.saturating_sub(out.urls.len()).max(1);
            let cap = remaining.max(self.opts.max_sitemaps);
            match parse_sitemap(&body, cap, self.opts.max_sitemap_bytes) {
                Ok(SitemapDoc::Index(children)) => {
                    if depth >= self.opts.max_depth {
                        push_err(&mut out.errors, format!("{sm}: max sitemap depth reached"));
                        continue;
                    }
                    for c in children {
                        // A sitemap index may only point at sitemaps on the site itself (SSRF H-02).
                        if !self.same_site(&c) {
                            out.offsite += 1;
                            continue;
                        }
                        queue.push_back((c, depth + 1));
                    }
                }
                Ok(SitemapDoc::UrlSet(urls)) => {
                    for u in urls {
                        if out.urls.len() >= self.opts.max_urls {
                            break;
                        }
                        if !self.same_site(&u) {
                            out.offsite += 1;
                            continue;
                        }
                        if self.opts.respect_robots && !robots.allowed(&u) {
                            out.disallowed += 1;
                            continue;
                        }
                        if seen_urls.insert(u.clone()) {
                            out.urls.push(u);
                        }
                    }
                }
                Err(e) => push_err(&mut out.errors, format!("{sm}: {e}")),
            }
        }
    }

    /// Full crawl: robots → sitemaps → pages, streamed to `tx` in batches.
    pub async fn crawl(&self, tx: mpsc::Sender<Vec<CrawledPost>>) -> Result<SitemapCrawlStats> {
        let robots = if self.opts.respect_robots {
            self.robots().await
        } else {
            RobotsRules::allow_all()
        };
        let found = self.discover_urls(&robots).await;
        let mut stats = SitemapCrawlStats {
            discovered: found.urls.len(),
            disallowed: found.disallowed,
            errors: found.errors,
            ..Default::default()
        };
        let delay = robots.crawl_delay.filter(|_| self.opts.respect_robots);
        let conc = if delay.is_some() {
            1
        } else {
            self.opts.concurrency.max(1)
        };
        let mut results = futures::stream::iter(found.urls)
            .map(|u| async move {
                let r = self.fetch_page(&u).await;
                if let Some(d) = delay {
                    tokio::time::sleep(d).await;
                }
                (u, r)
            })
            .buffered(conc);
        let batch_size = self.opts.batch_size.max(1);
        let mut batch = Vec::with_capacity(batch_size);
        while let Some((u, r)) = results.next().await {
            match r {
                Ok(Some(p)) => {
                    stats.crawled += 1;
                    batch.push(p);
                }
                Ok(None) => stats.skipped_non_html += 1,
                Err(e) => {
                    stats.failed += 1;
                    push_err(&mut stats.errors, format!("{u}: {e}"));
                }
            }
            if batch.len() >= batch_size {
                let full = std::mem::replace(&mut batch, Vec::with_capacity(batch_size));
                tx.send(full).await.map_err(|_| CrawlError::Cancelled)?;
            }
        }
        if !batch.is_empty() {
            tx.send(batch).await.map_err(|_| CrawlError::Cancelled)?;
        }
        Ok(stats)
    }

    /// Fetch one page and extract its main content. `Ok(None)` for non-HTML or off-site
    /// redirect targets; `Err` for transport/HTTP failures.
    pub async fn fetch_page(&self, url: &str) -> Result<Option<CrawledPost>> {
        let u = Url::parse(url).map_err(|e| CrawlError::InvalidUrl(format!("{url}: {e}")))?;
        let req = FetchRequest::get(u)
            .max_body(self.opts.max_page_bytes)
            .truncate();
        let resp = self.client.fetch(req).await?.error_for_status()?;
        let ct = resp
            .header("content-type")
            .unwrap_or("text/html")
            .to_ascii_lowercase();
        if !(ct.contains("html") || ct.contains("xml")) {
            return Ok(None);
        }
        let final_url = resp.url.to_string();
        if !self.same_site(&final_url) {
            return Ok(None);
        }
        let page = resp.text();
        let (title, content, builder) = tokio::task::spawn_blocking(move || {
            let (t, c) = html::extract_main_content(&page);
            let b = html::detect_builder(&page, None).to_string();
            (t, c, b)
        })
        .await
        .map_err(|e| CrawlError::Other(format!("extract task: {e}")))?;
        Ok(Some(CrawledPost {
            wp_post_id: None,
            slug: slug_from_url(&final_url),
            url: final_url,
            title,
            content_rendered: content,
            content_raw: None,
            excerpt: None,
            post_type: "post".into(),
            status: "publish".into(),
            modified_gmt: None,
            published_gmt: None,
            author: None,
            categories: Vec::new(),
            tags: Vec::new(),
            seo: SeoMeta::default(),
            builder_hint: builder,
        }))
    }
}

/// Last non-empty path segment.
pub fn slug_from_url(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    u.path_segments()?
        .rfind(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_index_and_urlset() {
        let idx = br#"<?xml version="1.0"?><sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
            <sitemap><loc>https://e.com/a.xml</loc></sitemap><sitemap><loc> https://e.com/b.xml </loc></sitemap></sitemapindex>"#;
        assert_eq!(
            parse_sitemap(idx, 100, 1000).unwrap(),
            SitemapDoc::Index(vec![
                "https://e.com/a.xml".into(),
                "https://e.com/b.xml".into()
            ])
        );
        let set = br#"<urlset><url><loc>https://e.com/x?a=1&amp;b=2</loc></url><url><loc><![CDATA[https://e.com/y]]></loc></url></urlset>"#;
        assert_eq!(
            parse_sitemap(set, 100, 1000).unwrap(),
            SitemapDoc::UrlSet(vec![
                "https://e.com/x?a=1&b=2".into(),
                "https://e.com/y".into()
            ])
        );
    }

    #[test]
    fn hostile_sitemaps_never_panic() {
        for bad in [
            &b""[..],
            b"<urlset><url><loc>https://e.com/</url>",
            b"<<<>>>",
            b"\x1f\x8b\x00garbage",
            b"<html><body>not a sitemap</body></html>",
            b"<urlset><loc>&bogus;</loc></urlset>",
        ] {
            let _ = parse_sitemap(bad, 10, 1000);
        }
        assert!(parse_sitemap(b"<html/>", 10, 10).is_err());
        let many = format!(
            "<urlset>{}</urlset>",
            "<url><loc>https://e.com/</loc></url>".repeat(500)
        );
        match parse_sitemap(many.as_bytes(), 10, 10).unwrap() {
            SitemapDoc::UrlSet(v) => assert_eq!(v.len(), 10),
            SitemapDoc::Index(_) => panic!("wrong kind"),
        }
    }

    #[test]
    fn robots_rules() {
        let r = RobotsRules::parse(
            "SiloForge",
            b"User-agent: *\nDisallow: /private/\nCrawl-delay: 2\nSitemap: https://e.com/s.xml\n",
        );
        assert!(r.allowed("https://e.com/ok"));
        assert!(!r.allowed("https://e.com/private/x"));
        assert_eq!(r.sitemaps, vec!["https://e.com/s.xml".to_string()]);
        assert_eq!(r.crawl_delay, Some(Duration::from_secs(2)));
        let _ = RobotsRules::parse("SiloForge", &[0xff, 0xfe, 0x00]);
        assert_eq!(slug_from_url("https://e.com/a/b/"), Some("b".into()));
    }
}
