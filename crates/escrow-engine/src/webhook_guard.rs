//! SSRF guard for outbound webhook URLs. A subscription URL is accepted only when it is `https`, names a
//! public host and is not an IP literal in a private, loopback, link-local, multicast, reserved or
//! metadata range. This check is on the URL text. The delivery worker must also check the addresses a
//! hostname resolves to at connect time (DNS rebinding), refuse redirects and cap response size.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Returns the reason a URL must not receive webhook deliveries, or `Ok(())` when it may.
pub fn check_url(raw: &str) -> Result<(), String> {
    let rest = raw.strip_prefix("https://").ok_or_else(|| "webhook URLs must use https".to_string())?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err("webhook URLs must not contain credentials".into());
    }
    let host = host_of(authority).ok_or_else(|| "webhook URL has no host".to_string())?;
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") || lower.ends_with(".internal") {
        return Err("webhook host is a local name".into());
    }
    if let Ok(ip) = lower.parse::<IpAddr>() {
        return if is_public_ip(&ip) { Ok(()) } else { Err("webhook host is a private or reserved address".into()) };
    }
    Ok(())
}

/// The host of an `authority` (`host`, `host:port`, `[v6]` or `[v6]:port`).
fn host_of(authority: &str) -> Option<&str> {
    if let Some(stripped) = authority.strip_prefix('[') {
        return stripped.split(']').next().filter(|h| !h.is_empty());
    }
    let host = authority.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

/// Whether an address may be a webhook destination. IPv4-mapped IPv6 is judged by its IPv4 address.
pub fn is_public_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(&v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(ip: &Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || ip.is_multicast()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10, carrier-grade NAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24, IETF protocol assignments
        || (o[0] == 198 && (o[1] & 0xfe) == 18) // 198.18.0.0/15, benchmarking
        || o[0] >= 240) // 240.0.0.0/4, reserved
}

fn is_public_v6(ip: &Ipv6Addr) -> bool {
    let s = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00 // fc00::/7, unique local
        || (s[0] & 0xffc0) == 0xfe80 // fe80::/10, link-local
        || (s[0] == 0x0064 && s[1] == 0xff9b) // 64:ff9b::/96, NAT64 (may embed a private IPv4)
        || s[0] == 0x2002 // 2002::/16, 6to4 (may embed a private IPv4)
        || (s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0)) // 100::/64, discard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_public_https_urls() {
        assert!(check_url("https://hooks.example.com/escrow").is_ok());
        assert!(check_url("https://example.com:8443/x?y=1").is_ok());
        assert!(check_url("https://8.8.8.8/hook").is_ok());
        assert!(check_url("https://[2606:4700::1111]/hook").is_ok());
    }

    #[test]
    fn rejects_plain_http_and_other_schemes() {
        assert!(check_url("http://hooks.example.com/").is_err());
        assert!(check_url("ftp://hooks.example.com/").is_err());
        assert!(check_url("hooks.example.com/hook").is_err());
    }

    #[test]
    fn rejects_local_names_and_credentials() {
        assert!(check_url("https://localhost/hook").is_err());
        assert!(check_url("https://api.localhost/hook").is_err());
        assert!(check_url("https://db.internal/hook").is_err());
        assert!(check_url("https://user:pass@example.com/hook").is_err());
        assert!(check_url("https:///hook").is_err());
    }

    #[test]
    fn rejects_private_loopback_link_local_and_metadata_addresses() {
        for url in [
            "https://127.0.0.1/hook",
            "https://10.1.2.3/hook",
            "https://172.16.0.1/hook",
            "https://192.168.1.1/hook",
            "https://169.254.169.254/latest/meta-data",
            "https://100.64.0.1/hook",
            "https://0.0.0.0/hook",
            "https://224.0.0.1/hook",
            "https://[::1]/hook",
            "https://[::]/hook",
            "https://[fe80::1]/hook",
            "https://[fd00::1]/hook",
            "https://[::ffff:10.0.0.1]/hook",
            "https://[::ffff:169.254.169.254]/hook",
            "https://[64:ff9b::a00:1]/hook",
        ] {
            assert!(check_url(url).is_err(), "{url} should be refused");
        }
    }
}
