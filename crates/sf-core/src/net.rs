//! Small network helpers shared by crates that cannot depend on `sf-crawl` (notifier webhooks,
//! AI provider URLs): the cloud-metadata address check (same rules as `sf_crawl::ssrf`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

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

/// Host names that always mean the instance-metadata service.
pub const METADATA_HOSTS: &[&str] = &["metadata.google.internal", "metadata", "instance-data"];

/// True when a URL host (name or IP literal, `[v6]` allowed) is a cloud-metadata endpoint
/// without DNS: a metadata IP literal or a well-known metadata name.
pub fn is_metadata_host(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let h = h.trim_start_matches('[').trim_end_matches(']');
    if METADATA_HOSTS.contains(&h) {
        return true;
    }
    h.parse::<IpAddr>().is_ok_and(is_metadata_ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_hosts_and_embedded_forms() {
        for h in [
            "169.254.169.254",
            "100.100.100.200",
            "[fd00:ec2::254]",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "2002:a9fe:a9fe::",
            "metadata.google.internal.",
            "METADATA",
        ] {
            assert!(is_metadata_host(h), "{h}");
        }
        for h in ["127.0.0.1", "localhost", "8.8.8.8", "[::1]", "example.com"] {
            assert!(!is_metadata_host(h), "{h}");
        }
    }
}
