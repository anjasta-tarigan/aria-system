//! `web_fetch` with an SSRF guard, and the IP classification it relies on.
//!
//! Every resolved address must be globally routable: loopback, private,
//! link-local, unspecified, multicast and documentation ranges are refused
//! (issue 07).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::time::Duration;

use serde::Deserialize;

use super::{Result, ToolError, default_limit, truncate};

#[derive(Debug, Deserialize)]
pub struct WebFetchArgs {
    pub url: String,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOutcome {
    pub status: u16,
    pub text: String,
}

pub async fn web_fetch(args: WebFetchArgs) -> Result<String> {
    let out = fetch_url(&args.url, default_limit(args.max_bytes, 100_000)).await?;
    Ok(format!(
        "HTTP {} ({} bytes)\n{}",
        out.status,
        out.text.len(),
        out.text
    ))
}

/// Fetch `url` and return at most `max_bytes` decoded from UTF-8-lossy.
///
/// Resolution happens twice on purpose: we check every address the host
/// resolves to *before* connecting, and the HTTP client is given no redirect
/// following, so a redirect cannot smuggle in a private address.
pub async fn fetch_url(url: &str, max_bytes: usize) -> Result<FetchOutcome> {
    let parsed = url::Url::parse(url).map_err(|e| ToolError::Args(format!("invalid URL: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ToolError::Refused(format!(
            "only http and https URLs are allowed, got {}",
            parsed.scheme()
        )));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| ToolError::Args("URL has no host".into()))?
        .to_string();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addrs: Vec<IpAddr> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| ToolError::Refused(format!("cannot resolve {host}: {e}")))?
        .map(|a| a.ip())
        .collect();
    check_host_allowed(&addrs)?;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| ToolError::Refused(e.to_string()))?;
    let response = client
        .get(parsed.clone())
        .header("user-agent", "aria/0.1")
        .send()
        .await
        .map_err(|e| ToolError::Refused(format!("request failed: {e}")))?;
    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .map_err(|e| ToolError::Refused(format!("body failed: {e}")))?;
    let text = String::from_utf8_lossy(&body[..body.len().min(max_bytes)]).to_string();
    Ok(FetchOutcome {
        status,
        text: truncate(text, max_bytes),
    })
}

pub fn check_host_allowed(addrs: &[IpAddr]) -> Result<()> {
    if addrs.is_empty() {
        return Err(ToolError::Refused("host resolved to no addresses".into()));
    }
    for ip in addrs {
        if !classify_ip(ip) {
            return Err(ToolError::Refused(format!(
                "refusing to fetch from a non-public address ({ip})"
            )));
        }
    }
    Ok(())
}

/// True when `ip` is a public, globally routable address.
pub fn classify_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(*v4),
        IpAddr::V6(v6) => is_public_v6(*v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        // 100.64.0.0/10 carrier-grade NAT
        || (o[0] == 100 && (64..=127).contains(&o[1]))
        // 0.0.0.0/8 "this network"
        || o[0] == 0
        // 198.18.0.0/15 benchmarking
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        // 240.0.0.0/4 reserved
        || o[0] >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        // IPv4-mapped addresses inherit the IPv4 rules.
        || ip.to_ipv4_mapped().is_some_and(|v4| !is_public_v4(v4))
        // 2001:db8::/32 documentation
        || ip.segments()[0] == 0x2001 && ip.segments()[1] == 0x0db8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn v6(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_private_and_special_ranges() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.1.1",
            "198.18.0.1",
            "255.255.255.255",
            "224.0.0.1",
        ] {
            assert!(!classify_ip(&v4(ip)), "{ip} should be refused");
        }
    }

    #[test]
    fn blocks_ipv6_private_and_mapped_loopback() {
        for ip in [
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "::ffff:127.0.0.1",
            "::ffff:10.1.2.3",
        ] {
            assert!(!classify_ip(&v6(ip)), "{ip} should be refused");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for ip in ["1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(classify_ip(&ip), "{ip} should be allowed");
        }
    }
}
