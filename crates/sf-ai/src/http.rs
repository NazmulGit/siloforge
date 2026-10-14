//! HTTP transport with SSRF hardening for admin-entered provider URLs (fixes v1 S9).
//!
//! Policy:
//! * Always blocked: link-local and cloud-metadata addresses (169.254.0.0/16, fe80::/10,
//!   fd00:ec2::254, 100.100.100.200), unspecified, multicast and broadcast addresses, and the
//!   `metadata.google.internal` name. Checked for IP literals before sending and for every DNS
//!   answer at connect time (custom resolver), so DNS rebinding cannot reach them either.
//! * Allowed: loopback and private ranges. This is deliberate: the default chat provider (dario)
//!   and Ollama run on 127.0.0.1 or a LAN box. Provider config is admin-only.
//! * Redirects are never followed (a redirect could point at a metadata IP literal).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use crate::adapters::{WireRequest, truncate};

/// Largest response body we read (embedding batches can be big; anything larger is hostile).
const MAX_BODY: usize = 32 * 1024 * 1024;

/// Returns why an IP is forbidden as a provider target, or `None` if allowed.
pub(crate) fn ip_blocked(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => v4_blocked(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_blocked(v4);
            }
            if v6.is_unspecified() {
                return Some("unspecified address");
            }
            if v6.is_multicast() {
                return Some("multicast address");
            }
            if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                return Some("link-local address");
            }
            if v6 == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254) {
                return Some("cloud metadata address");
            }
            None
        }
    }
}

fn v4_blocked(v4: Ipv4Addr) -> Option<&'static str> {
    if v4.is_link_local() {
        return Some("link-local/metadata address");
    }
    if v4 == Ipv4Addr::new(100, 100, 100, 200) {
        return Some("cloud metadata address");
    }
    if v4.octets()[0] == 0 {
        return Some("unspecified address");
    }
    if v4.is_multicast() || v4.is_broadcast() {
        return Some("multicast/broadcast address");
    }
    None
}

/// Validates a request URL: http(s), has a host, not a forbidden IP literal or metadata name.
pub(crate) fn check_url(raw: &str) -> Result<url::Url, String> {
    let u = url::Url::parse(raw).map_err(|e| format!("invalid provider url: {e}"))?;
    if u.scheme() != "http" && u.scheme() != "https" {
        return Err("provider url must be http(s)".into());
    }
    match u.host() {
        None => Err("provider url has no host".into()),
        Some(url::Host::Ipv4(ip)) => match ip_blocked(IpAddr::V4(ip)) {
            Some(why) => Err(format!("blocked provider host {ip}: {why}")),
            None => Ok(u),
        },
        Some(url::Host::Ipv6(ip)) => match ip_blocked(IpAddr::V6(ip)) {
            Some(why) => Err(format!("blocked provider host {ip}: {why}")),
            None => Ok(u),
        },
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            if d == "metadata.google.internal" || d == "metadata" {
                Err(format!("blocked provider host {d}: cloud metadata name"))
            } else {
                Ok(u)
            }
        }
    }
}

/// DNS resolver that drops forbidden answers (connect-time SSRF check).
struct GuardedResolver;

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed: Vec<SocketAddr> = addrs
                .iter()
                .copied()
                .filter(|a| ip_blocked(a.ip()).is_none())
                .collect();
            if allowed.is_empty() {
                let msg = if addrs.is_empty() {
                    format!("{host}: no addresses")
                } else {
                    format!("{host}: resolves only to blocked (link-local/metadata) addresses")
                };
                return Err(msg.into());
            }
            let it: reqwest::dns::Addrs = Box::new(allowed.into_iter());
            Ok(it)
        })
    }
}

pub(crate) fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(GuardedResolver))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(60))
        .user_agent(concat!("SiloForge/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("http client: {e}"))
}

/// Why a send failed.
#[derive(Debug, Clone)]
pub(crate) enum SendError {
    /// Refused before sending (SSRF policy, bad header, bad URL). Not retryable.
    Refused(String),
    /// Connect/read failure or timeout. Retryable.
    Transport(String),
    /// Non-2xx status.
    Http {
        status: u16,
        detail: String,
        retry_after: Option<Duration>,
    },
    /// 2xx with a body that is not JSON (or too large). Not retryable.
    BadBody { status: u16, msg: String },
}

fn header_map(headers: &[(String, String)]) -> Result<HeaderMap, String> {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|_| format!("invalid header name '{}'", truncate(k, 64)))?;
        let value = HeaderValue::from_str(v)
            .map_err(|_| format!("invalid value for header '{}'", truncate(k, 64)))?;
        map.insert(name, value);
    }
    Ok(map)
}

fn clean_detail(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    truncate(s.trim(), 200)
}

fn transport_msg(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".into()
    } else if e.is_connect() {
        // reqwest's Display hides the cause; walk the chain for something useful.
        let mut msg = String::from("connection failed");
        let mut src = std::error::Error::source(e);
        let mut depth = 0;
        while let Some(s) = src {
            msg = format!("connection failed: {s}");
            src = s.source();
            depth += 1;
            if depth > 6 {
                break;
            }
        }
        truncate(&msg, 300)
    } else {
        truncate(&format!("request failed: {e}"), 300)
    }
}

/// POSTs the request with a hard timeout and returns `(status, json body)`.
pub(crate) async fn send(
    client: &reqwest::Client,
    req: &WireRequest,
    timeout: Duration,
) -> Result<(u16, Value), SendError> {
    let url = check_url(&req.url).map_err(SendError::Refused)?;
    let headers = header_map(&req.headers).map_err(SendError::Refused)?;
    let body = serde_json::to_vec(&req.body).map_err(|e| SendError::Refused(e.to_string()))?;
    let fut = async {
        let mut resp = client
            .post(url)
            .headers(headers)
            .body(body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| SendError::Transport(transport_msg(&e)))?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let mut buf: Vec<u8> = Vec::new();
        let mut too_big = false;
        loop {
            match resp.chunk().await {
                Ok(Some(c)) => {
                    if buf.len().saturating_add(c.len()) > MAX_BODY {
                        too_big = true;
                        break;
                    }
                    buf.extend_from_slice(&c);
                }
                Ok(None) => break,
                Err(e) => return Err(SendError::Transport(transport_msg(&e))),
            }
        }
        if !(200..300).contains(&status) {
            return Err(SendError::Http {
                status,
                detail: clean_detail(&buf),
                retry_after,
            });
        }
        if too_big {
            return Err(SendError::BadBody {
                status,
                msg: format!("response body larger than {MAX_BODY} bytes"),
            });
        }
        let v: Value = serde_json::from_slice(&buf).map_err(|e| SendError::BadBody {
            status,
            msg: format!("response is not JSON: {e}"),
        })?;
        Ok((status, v))
    };
    // Belt and braces: reqwest's timeout covers the body too, but never trust one layer.
    match tokio::time::timeout(timeout + Duration::from_millis(250), fut).await {
        Ok(r) => r,
        Err(_) => Err(SendError::Transport("timeout".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_policy() {
        assert!(check_url("http://169.254.169.254/latest").is_err());
        assert!(check_url("http://[fd00:ec2::254]/").is_err());
        assert!(check_url("http://[fe80::1]/").is_err());
        assert!(check_url("http://[::ffff:169.254.1.1]/").is_err());
        assert!(check_url("http://0.0.0.0:80/").is_err());
        assert!(check_url("http://metadata.google.internal/").is_err());
        assert!(check_url("file:///etc/passwd").is_err());
        assert!(check_url("not a url").is_err());
        // Loopback and private are allowed on purpose (dario, ollama).
        assert!(check_url("http://127.0.0.1:3460/v1").is_ok());
        assert!(check_url("http://192.168.1.5:11434").is_ok());
        assert!(check_url("http://[::1]:11434").is_ok());
        assert!(check_url("https://api.openai.com/v1").is_ok());
    }

    #[test]
    fn bad_headers_refused() {
        assert!(header_map(&[("bad name".into(), "v".into())]).is_err());
        assert!(header_map(&[("x".into(), "a\nb".into())]).is_err());
        assert!(header_map(&[("x".into(), "ok".into())]).is_ok());
    }
}
