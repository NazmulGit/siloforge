//! sf-crawl: network IO for SiloForge.
//!
//! * [`client`]: [`HttpClient`] (rustls, gzip/brotli, body cap, per-host limiter + politeness,
//!   429/503 adaptive backoff, manual redirects with SSRF per hop, host aliases).
//! * [`ssrf`]: address policy + connect-time guarded resolver.
//! * [`wp`]: WordPress REST crawler (`context=edit` raw content) and [`WpWriter`] with
//!   optimistic concurrency; [`wp::verify_live`].
//! * [`sitemap`]: robots-respecting sitemap crawler for non-WP sites.
//! * [`health`]: link-health checker producing `link_health` rows.
//! * [`breadcrumbs`]: breadcrumb trail + `BreadcrumbList` schema detection (pure, bounded).
//! * [`html`]: minimal scraper helpers (main content, links, live-link lookup).

pub mod breadcrumbs;
pub mod client;
pub mod error;
pub mod health;
pub mod html;
pub mod sitemap;
pub mod ssrf;
pub mod wp;

pub use breadcrumbs::{
    BreadcrumbCrumb, BreadcrumbReport, MAX_BREADCRUMB_HTML_BYTES, detect_breadcrumbs,
};
pub use client::{
    FetchRequest, FetchResponse, HttpClient, HttpClientBuilder, HttpConfig, RedirectHop,
};
pub use error::{CrawlError, Result};
pub use health::{
    FailureKind, LinkCheckInput, LinkCheckOptions, LinkHealthReport, LinkHealthRow,
    LinkHealthStats, TraceOptions, TraceResult, check_links, trace,
};
pub use html::{ExtractedLink, LinkPresence, extract_links};
pub use sitemap::{SitemapCrawlStats, SitemapCrawler, SitemapOptions};
pub use ssrf::{SsrfPolicy, is_blocked_ip};
pub use wp::{
    CrawlPage, CrawledPost, LiveCheck, RawPost, SeoMeta, UpdateOutcome, VerifyOptions,
    WpCrawlStats, WpCrawler, WpCredentials, WpPostType, WpSite, WpWriter, verify_live,
};
