//! Network safety rules (PRD section 5): the manager executes raw shell
//! scripts, so it only ever binds to loopback / private / Tailscale addresses
//! and only answers requests addressed to such a host.

use std::net::IpAddr;

/// True for loopback, RFC 1918 private ranges, link-local, and the
/// 100.64.0.0/10 CGNAT range used by Tailscale. Everything else is public.
pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (o[0] == 100 && (o[1] & 0b1100_0000) == 64)
        }
        IpAddr::V6(v6) => {
            let seg0 = v6.segments()[0];
            v6.is_loopback()
                || (seg0 & 0xfe00) == 0xfc00 // unique local fc00::/7 (incl. Tailscale fd7a:...)
                || (seg0 & 0xffc0) == 0xfe80 // link local
        }
    }
}

/// Extract the host portion of a `Host` header / authority (drops the port).
pub fn host_without_port(authority: &str) -> &str {
    let a = authority.trim();
    if let Some(rest) = a.strip_prefix('[') {
        return rest.split(']').next().unwrap_or("");
    }
    match a.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => a,
    }
}

/// A request is accepted only when its Host header names localhost or a
/// private IP literal. This defeats DNS-rebinding: an attacker-controlled
/// domain name is never an IP literal.
pub fn host_header_allowed(authority: &str) -> bool {
    let host = host_without_port(authority);
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>().map(|ip| is_private_ip(&ip)).unwrap_or(false)
}

/// Cross-site request check: when a browser sends `Origin`, it must be the
/// dashboard's own origin (same authority as the Host header).
pub fn origin_allowed(origin: &str, host_header: &str) -> bool {
    let o = origin.trim();
    let authority = o.strip_prefix("http://").or_else(|| o.strip_prefix("https://"));
    match authority {
        Some(a) => a.trim_end_matches('/').eq_ignore_ascii_case(host_header.trim()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn private_ranges() {
        for ok in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.5",
            "172.16.0.1",
            "100.64.0.1",
            "100.101.102.103",
            "100.127.255.255",
            "::1",
            "fd7a:115c:a1e0::1",
        ] {
            assert!(is_private_ip(&ip(ok)), "{ok} should be allowed");
        }
        for bad in [
            "0.0.0.0",
            "8.8.8.8",
            "100.128.0.1",
            "100.63.255.255",
            "172.32.0.1",
            "2001:4860:4860::8888",
            "::",
        ] {
            assert!(!is_private_ip(&ip(bad)), "{bad} should be rejected");
        }
    }

    #[test]
    fn host_headers() {
        assert!(host_header_allowed("127.0.0.1:7878"));
        assert!(host_header_allowed("localhost:7878"));
        assert!(host_header_allowed("LOCALHOST"));
        assert!(host_header_allowed("[::1]:7878"));
        assert!(host_header_allowed("100.101.102.103:7878"));
        assert!(!host_header_allowed("evil.example.com:7878"));
        assert!(!host_header_allowed("8.8.8.8"));
        assert!(!host_header_allowed(""));
    }

    #[test]
    fn origins() {
        assert!(origin_allowed("http://127.0.0.1:7878", "127.0.0.1:7878"));
        assert!(origin_allowed("http://localhost:7878/", "localhost:7878"));
        assert!(!origin_allowed("http://127.0.0.1:8000", "127.0.0.1:7878"));
        assert!(!origin_allowed("https://evil.example.com", "127.0.0.1:7878"));
        assert!(!origin_allowed("null", "127.0.0.1:7878"));
    }
}
