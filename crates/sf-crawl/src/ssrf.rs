//! SSRF guard.
//!
//! Two layers, both driven by one [`SsrfPolicy`]:
//! 1. [`SsrfPolicy::check_url_static`] runs on every request hop (including each redirect, since
//!    [`crate::HttpClient`] follows redirects manually): scheme must be http/https and literal-IP
//!    hosts must be public.
//! 2. [`GuardedResolver`] is installed as reqwest's DNS resolver. Every connection reqwest opens
//!    resolves through it, so the addresses that are checked are exactly the addresses that are
//!    connected to (no check-then-fetch DNS rebinding window, fixing v1 finding #26). It also
//!    applies `host_aliases` (e.g. `site01.example -> 127.0.0.1`) while the URL, and so the
//!    `Host` header and TLS SNI, keep the original name.
//!
//! Hosts in the allowlist (configured site hosts that an admin allowed to be private) may resolve
//! to private addresses; everything else must be public unless `allow_private` is set. An alias
//! only rewrites the address: it never exempts a host from the check (SSRF H-02), and the client
//! installs aliases only for the job's own site hosts.

use std::collections::{BTreeMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

use crate::error::{CrawlError, Result};

/// Policy deciding which destinations may be contacted.
#[derive(Debug, Clone, Default)]
pub struct SsrfPolicy {
    allow_private: bool,
    allow_hosts: HashSet<String>,
}

impl SsrfPolicy {
    /// `allow_private = true` disables address filtering entirely (dev / LAN installs).
    /// `allow_hosts` are host names or IP literals exempt from the private-range check.
    pub fn new<I, S>(allow_private: bool, allow_hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut p = Self {
            allow_private,
            allow_hosts: HashSet::new(),
        };
        for h in allow_hosts {
            p.allow_host(h.as_ref());
        }
        p
    }

    /// Add a host (name, IP literal, or full URL) to the allowlist.
    pub fn allow_host(&mut self, host: &str) {
        let h = Url::parse(host)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| host.to_string());
        let n = normalize_host(&h);
        if !n.is_empty() {
            self.allow_hosts.insert(n);
        }
    }

    /// True when private addresses are allowed globally.
    pub fn allow_private(&self) -> bool {
        self.allow_private
    }

    /// True when `host` may resolve to private addresses.
    pub fn is_host_exempt(&self, host: &str) -> bool {
        self.allow_private || self.allow_hosts.contains(&normalize_host(host))
    }

    /// Per-hop static check: scheme and literal-IP hosts. DNS names are checked at connect time
    /// by [`GuardedResolver`] (or explicitly via [`SsrfPolicy::guard_url`]).
    pub fn check_url_static(&self, url: &Url) -> Result<()> {
        match url.scheme() {
            "http" | "https" => {}
            other => {
                return Err(CrawlError::Blocked {
                    url: url.to_string(),
                    reason: format!("scheme {other} not allowed"),
                });
            }
        }
        let host = url
            .host()
            .ok_or_else(|| CrawlError::InvalidUrl(format!("{url}: no host")))?;
        let ip = match host {
            Host::Ipv4(v4) => IpAddr::V4(v4),
            Host::Ipv6(v6) => IpAddr::V6(v6),
            Host::Domain(_) => return Ok(()),
        };
        let host_str = url.host_str().unwrap_or_default();
        self.check_resolved(host_str, &[ip])
    }

    /// Reject when ANY of the resolved addresses is blocked (unless the host is exempt).
    /// An empty address list is rejected (v1 semantics: no records = not allowed).
    pub fn check_resolved(&self, host: &str, ips: &[IpAddr]) -> Result<()> {
        // Cloud-metadata addresses are never exempt, not even for allowlisted hosts (M-01).
        if let Some(bad) = ips.iter().find(|ip| is_metadata_ip(**ip)) {
            return Err(CrawlError::Blocked {
                url: host.to_string(),
                reason: format!("resolves to cloud-metadata address {bad}"),
            });
        }
        if self.is_host_exempt(host) {
            return Ok(());
        }
        if ips.is_empty() {
            return Err(CrawlError::Dns {
                host: host.to_string(),
                message: "no address records".into(),
            });
        }
        if let Some(bad) = ips.iter().find(|ip| is_blocked_ip(**ip)) {
            return Err(CrawlError::Blocked {
                url: host.to_string(),
                reason: format!("resolves to non-public address {bad}"),
            });
        }
        Ok(())
    }

    /// Full explicit check: static check plus DNS resolution of named hosts.
    /// Returns the resolved addresses. Prefer the client (which enforces at connect time);
    /// this is for callers that need a yes/no answer up front (e.g. validating a site URL).
    pub async fn guard_url(&self, url: &Url) -> Result<Vec<IpAddr>> {
        self.check_url_static(url)?;
        match url.host() {
            Some(Host::Ipv4(v4)) => Ok(vec![IpAddr::V4(v4)]),
            Some(Host::Ipv6(v6)) => Ok(vec![IpAddr::V6(v6)]),
            Some(Host::Domain(d)) => {
                let ips = lookup(d).await?;
                self.check_resolved(d, &ips)?;
                Ok(ips)
            }
            None => Err(CrawlError::InvalidUrl(url.to_string())),
        }
    }
}

/// Lowercase, strip a trailing dot and IPv6 brackets.
pub fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

async fn lookup(host: &str) -> Result<Vec<IpAddr>> {
    let addrs = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|e| CrawlError::Dns {
            host: host.to_string(),
            message: e.to_string(),
        })?;
    let mut ips: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
    ips.sort();
    ips.dedup();
    Ok(ips)
}

/// True for every address a crawler must never contact: unspecified, loopback, private,
/// link-local, CGNAT, benchmarking, documentation, multicast, reserved/broadcast, IPv6 ULA /
/// site-local / multicast / documentation / discard / Teredo, plus IPv4 embedded in
/// IPv4-mapped, IPv4-compatible, NAT64 (64:ff9b::/96) and 6to4 (2002::/16) addresses.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

/// Cloud-metadata / instance-service addresses that must never be contacted, whatever the
/// allowlist says: 169.254.0.0/16 (AWS/GCP/Azure/OpenStack link-local metadata), Alibaba
/// `100.100.100.200`, AWS IPv6 `fd00:ec2::254`, plus IPv4 embedded in mapped / compatible /
/// NAT64 / 6to4 IPv6 forms.
pub fn is_metadata_ip(ip: IpAddr) -> bool {
    fn v4(ip: Ipv4Addr) -> bool {
        let [a, b, c, d] = ip.octets();
        (a == 169 && b == 254) || (a == 100 && b == 100 && c == 100 && d == 200)
    }
    match ip {
        IpAddr::V4(x) => v4(x),
        IpAddr::V6(x) => {
            if x.segments() == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254] {
                return true;
            }
            embedded_v4(x).is_some_and(v4)
        }
    }
}

/// IPv4 embedded in IPv4-mapped, IPv4-compatible, NAT64 (64:ff9b::/96, 64:ff9b:1::/48) or 6to4.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }
    let s = ip.segments();
    let tail = || {
        Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xff) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xff) as u8,
        )
    };
    if s[..6].iter().all(|x| *x == 0) && !ip.is_unspecified() && !ip.is_loopback() {
        return Some(tail());
    }
    if s[0] == 0x64 && s[1] == 0xff9b && (s[2..6].iter().all(|x| *x == 0) || s[2] == 1) {
        return Some(tail());
    }
    if s[0] == 0x2002 {
        return Some(Ipv4Addr::new(
            (s[1] >> 8) as u8,
            (s[1] & 0xff) as u8,
            (s[2] >> 8) as u8,
            (s[2] & 0xff) as u8,
        ));
    }
    None
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 0                                      // 0.0.0.0/8 "this network"
        || a == 10                              // 10/8
        || a == 127                             // loopback
        || (a == 100 && (64..=127).contains(&b)) // 100.64/10 CGNAT
        || (a == 169 && b == 254)               // link-local (cloud metadata)
        || (a == 172 && (16..=31).contains(&b)) // 172.16/12
        || (a == 192 && b == 0 && c == 0)       // 192.0.0/24 IETF protocol assignments
        || (a == 192 && b == 0 && c == 2)       // TEST-NET-1
        || (a == 192 && b == 168)               // 192.168/16
        || (a == 198 && (b == 18 || b == 19))   // 198.18/15 benchmarking
        || (a == 198 && b == 51 && c == 100)    // TEST-NET-2
        || (a == 203 && b == 0 && c == 113)     // TEST-NET-3
        || a >= 224 // 224/4 multicast, 240/4 reserved, 255.255.255.255
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_blocked_v4(v4);
    }
    let s = ip.segments();
    // ::a.b.c.d (deprecated IPv4-compatible); also covers :: and ::1.
    if s[..6].iter().all(|x| *x == 0) {
        if ip.is_unspecified() || ip.is_loopback() {
            return true;
        }
        let v4 = Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xff) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xff) as u8,
        );
        return is_blocked_v4(v4);
    }
    // NAT64 64:ff9b::/96
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6].iter().all(|x| *x == 0) {
        let v4 = Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xff) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xff) as u8,
        );
        return is_blocked_v4(v4);
    }
    // 6to4 2002:AABB:CCDD::/48 embeds AA.BB.CC.DD
    if s[0] == 0x2002 {
        let v4 = Ipv4Addr::new(
            (s[1] >> 8) as u8,
            (s[1] & 0xff) as u8,
            (s[2] >> 8) as u8,
            (s[2] & 0xff) as u8,
        );
        return is_blocked_v4(v4);
    }
    // I-01: ::ffff:0:a.b.c.d (IPv4-translated, RFC 2765) and 64:ff9b:1::/48 (local-use NAT64)
    if s[..4].iter().all(|x| *x == 0) && s[4] == 0xffff && s[5] == 0 {
        return is_blocked_v4(Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xff) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xff) as u8,
        ));
    }
    if s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1 {
        return true;
    }
    (s[0] & 0xfe00) == 0xfc00          // fc00::/7 ULA
        || (s[0] & 0xffc0) == 0xfe80   // fe80::/10 link-local
        || (s[0] & 0xffc0) == 0xfec0   // fec0::/10 site-local (deprecated)
        || (s[0] & 0xff00) == 0xff00   // ff00::/8 multicast
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || (s[0] == 0x2001 && s[1] == 0)      // Teredo 2001::/32
        || (s[0] == 0x0100 && s[1..4].iter().all(|x| *x == 0)) // 100::/64 discard
}

/// Error returned from the resolver; recognised by the client's error mapper.
#[derive(Debug)]
pub(crate) struct ResolveFailure {
    pub blocked: bool,
    pub host: String,
    pub message: String,
}

impl std::fmt::Display for ResolveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.blocked {
            write!(f, "sf-ssrf-blocked: {}: {}", self.host, self.message)
        } else {
            write!(f, "sf-dns-failed: {}: {}", self.host, self.message)
        }
    }
}

impl std::error::Error for ResolveFailure {}

/// reqwest DNS resolver that applies host aliases and the SSRF policy at connect time.
#[derive(Debug, Clone)]
pub struct GuardedResolver {
    policy: Arc<SsrfPolicy>,
    aliases: Arc<BTreeMap<String, String>>,
}

impl GuardedResolver {
    /// `aliases` maps a host name to a replacement host name or IP literal.
    pub fn new(policy: Arc<SsrfPolicy>, aliases: &BTreeMap<String, String>) -> Self {
        let aliases = aliases
            .iter()
            .map(|(k, v)| (normalize_host(k), normalize_host(v)))
            .filter(|(k, v)| !k.is_empty() && !v.is_empty())
            .collect();
        Self {
            policy,
            aliases: Arc::new(aliases),
        }
    }

    /// Resolve `host` the way the client will: alias first, then DNS, then the policy check.
    pub async fn resolve_host(&self, host: &str) -> std::result::Result<Vec<IpAddr>, String> {
        resolve_inner(&self.policy, &self.aliases, host)
            .await
            .map_err(|e| e.to_string())
    }
}

async fn resolve_inner(
    policy: &SsrfPolicy,
    aliases: &BTreeMap<String, String>,
    host: &str,
) -> std::result::Result<Vec<IpAddr>, ResolveFailure> {
    let original = normalize_host(host);
    let target = aliases
        .get(&original)
        .cloned()
        .unwrap_or_else(|| original.clone());
    let ips = if let Ok(ip) = target.parse::<IpAddr>() {
        vec![ip]
    } else {
        lookup(&target).await.map_err(|e| ResolveFailure {
            blocked: false,
            host: original.clone(),
            message: e.to_string(),
        })?
    };
    if ips.is_empty() {
        return Err(ResolveFailure {
            blocked: false,
            host: original,
            message: "no address records".into(),
        });
    }
    if let Some(bad) = ips.iter().find(|ip| is_metadata_ip(**ip)) {
        return Err(ResolveFailure {
            blocked: true,
            host: original,
            message: format!("resolves to cloud-metadata address {bad}"),
        });
    }
    // An alias is NOT an exemption: only an allowlisted (site) host may be private (H-02).
    if policy.is_host_exempt(&original) {
        return Ok(ips);
    }
    if let Some(bad) = ips.iter().find(|ip| is_blocked_ip(**ip)) {
        return Err(ResolveFailure {
            blocked: true,
            host: original,
            message: format!("resolves to non-public address {bad}"),
        });
    }
    Ok(ips)
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = self.policy.clone();
        let aliases = self.aliases.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let ips = resolve_inner(&policy, &aliases, &host)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            let addrs: Addrs = Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_private_ranges() {
        for s in [
            "0.0.0.0",
            "10.1.2.3",
            "127.0.0.1",
            "100.64.0.1",
            "100.127.255.254",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.8",
            "192.168.1.1",
            "198.18.0.1",
            "224.0.0.1",
            "239.255.255.250",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
            "::10.0.0.1",
            "64:ff9b::a00:1",
            "2002:0a00:0001::1",
            "2002:c0a8:0101::1",
            "2001:db8::1",
        ] {
            assert!(is_blocked_ip(ip(s)), "{s} should be blocked");
        }
    }

    #[test]
    fn allows_public_ranges() {
        for s in [
            "8.8.8.8",
            "1.1.1.1",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.0.1",
            "172.32.0.1",
            "192.169.0.1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:0808:0808::1",
        ] {
            assert!(!is_blocked_ip(ip(s)), "{s} should be allowed");
        }
    }

    #[test]
    fn metadata_never_exempt() {
        for s in [
            "169.254.169.254",
            "100.100.100.200",
            "fd00:ec2::254",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
        ] {
            assert!(is_metadata_ip(ip(s)), "{s}");
        }
        assert!(!is_metadata_ip(ip("127.0.0.1")));
        assert!(!is_metadata_ip(ip("8.8.8.8")));
        let p = SsrfPolicy::new(true, ["meta.example"]);
        assert!(matches!(
            p.check_resolved("meta.example", &[ip("169.254.169.254")]),
            Err(CrawlError::Blocked { .. })
        ));
        // I-01 ranges
        assert!(is_blocked_ip(ip("::ffff:0:a00:1")));
        assert!(is_blocked_ip(ip("64:ff9b:1::1")));
    }

    #[test]
    fn check_resolved_any_blocked_rejects() {
        let p = SsrfPolicy::new(false, ["site.example"]);
        let r = p.check_resolved("evil.example", &[ip("8.8.8.8"), ip("10.0.0.5")]);
        assert!(matches!(r, Err(CrawlError::Blocked { .. })));
        assert!(p.check_resolved("evil.example", &[ip("8.8.8.8")]).is_ok());
        assert!(matches!(
            p.check_resolved("evil.example", &[]),
            Err(CrawlError::Dns { .. })
        ));
        // allowlisted site host may be private
        assert!(p.check_resolved("SITE.example.", &[ip("10.0.0.5")]).is_ok());
        // allow_private disables filtering
        let open = SsrfPolicy::new(true, Vec::<String>::new());
        assert!(open.check_resolved("x", &[ip("127.0.0.1")]).is_ok());
    }

    #[test]
    fn static_check_scheme_and_literals() {
        let p = SsrfPolicy::new(false, ["http://127.0.0.1:9000/"]);
        let u = |s: &str| Url::parse(s).ok();
        let check = |s: &str| u(s).map(|x| p.check_url_static(&x));
        assert!(matches!(
            check("ftp://x.com/"),
            Some(Err(CrawlError::Blocked { .. }))
        ));
        assert!(matches!(check("file:///etc/passwd"), Some(Err(_))));
        assert!(matches!(
            check("http://10.0.0.1/"),
            Some(Err(CrawlError::Blocked { .. }))
        ));
        assert!(matches!(
            check("http://[::1]/"),
            Some(Err(CrawlError::Blocked { .. }))
        ));
        assert!(matches!(check("http://[::ffff:a00:1]/"), Some(Err(_))));
        assert!(matches!(check("http://127.0.0.1:1234/"), Some(Ok(()))));
        assert!(matches!(check("https://example.com/"), Some(Ok(()))));
        // decimal / hex encodings are normalised by the URL parser
        // 2130706433 == 127.0.0.1 (allowlisted); 0x7f000002 == 127.0.0.2 (not)
        assert!(matches!(check("http://2130706433/"), Some(Ok(()))));
        assert!(matches!(
            check("http://0x7f000002/"),
            Some(Err(CrawlError::Blocked { .. }))
        ));
    }

    #[tokio::test]
    async fn resolver_aliases_and_blocks() {
        let policy = Arc::new(SsrfPolicy::new(false, Vec::<String>::new()));
        let mut aliases = BTreeMap::new();
        aliases.insert("site01.example".to_string(), "127.0.0.1".to_string());
        let r = GuardedResolver::new(policy, &aliases);
        // H-02: the alias rewrites the address but the loopback target is still blocked.
        let blocked = r.resolve_host("site01.example").await;
        assert!(blocked.is_err_and(|e| e.contains("sf-ssrf-blocked")));
        let exempt = Arc::new(SsrfPolicy::new(false, ["site01.example"]));
        let r2 = GuardedResolver::new(exempt, &aliases);
        let ok = r2.resolve_host("site01.example").await;
        assert_eq!(ok, Ok(vec![ip("127.0.0.1")]));
        // localhost resolves to loopback and is not allowlisted
        let blocked = r.resolve_host("localhost").await;
        assert!(blocked.is_err_and(|e| e.contains("sf-ssrf-blocked")));
    }
}
