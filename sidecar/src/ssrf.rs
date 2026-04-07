//! SSRF protection — the single point of fix for the two Go bugs.
//!
//! FR-18:
//!
//! 1. Resolve the hostname to a set of `SocketAddr` via
//!    `tokio::net::lookup_host`.
//! 2. If the lookup errors or returns zero addresses, fail closed. (Fix
//!    for the Go `if err == nil` fail-open bug.)
//! 3. If ANY of the returned IPs is in a private range, fail closed.
//! 4. Return a single non-private `SocketAddr` for the caller to dial.
//!    **The caller must dial this `SocketAddr` — never redial by
//!    hostname.** (Fix for the DNS rebinding window.)

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use thiserror::Error;

/// Errors produced by [`resolve_safe`].
#[derive(Debug, Error)]
pub enum SsrfError {
    /// DNS lookup errored.
    #[error("DNS lookup failed: {0}")]
    LookupFailed(String),

    /// Lookup returned zero addresses.
    #[error("hostname resolved to no addresses")]
    NoAddresses,

    /// A resolved IP was in a private range.
    #[error("hostname resolved to private IP: {0}")]
    PrivateIp(IpAddr),
}

/// Resolve `host:port` to a single vetted `SocketAddr` that the caller
/// should dial directly. Fails closed on any error or private IP.
pub async fn resolve_safe(host: &str, port: u16) -> Result<SocketAddr, SsrfError> {
    let target = format!("{host}:{port}");
    let addrs = tokio::net::lookup_host(&target)
        .await
        .map_err(|e| SsrfError::LookupFailed(e.to_string()))?
        .collect::<Vec<_>>();

    if addrs.is_empty() {
        return Err(SsrfError::NoAddresses);
    }

    // If any resolved IP is private, reject the entire resolution.
    // Matches FR-18: "any returned IP is in RFC1918 ... returns 403 ...
    // do not dial."
    for addr in &addrs {
        if is_private_ip(addr.ip()) {
            return Err(SsrfError::PrivateIp(addr.ip()));
        }
    }

    // All addresses are public. Pick the first one.
    Ok(addrs[0])
}

/// Classify an `IpAddr` as private/loopback/link-local/ULA per FR-18.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_ipv4(v4),
        IpAddr::V6(v6) => is_private_ipv6(v6),
    }
}

fn is_private_ipv4(v4: Ipv4Addr) -> bool {
    // RFC1918: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16.
    if v4.is_private() {
        return true;
    }
    // 127.0.0.0/8 loopback.
    if v4.is_loopback() {
        return true;
    }
    // 169.254.0.0/16 link-local.
    if v4.is_link_local() {
        return true;
    }
    // 0.0.0.0/8 "this host on this network" — dialing 0.0.0.0 ends up on
    // loopback on most operating systems; treat as private.
    if v4.octets()[0] == 0 {
        return true;
    }
    // Broadcast 255.255.255.255.
    if v4.is_broadcast() {
        return true;
    }
    // Reserved / documentation / etc. are not covered by Go's original
    // classifier but are worth blocking defensively. We stay strict here
    // because a loop-escape IP like 100.64.0.0/10 (shared address space)
    // could still route internally.
    //
    // NOTE: staying parity-close with Go, we DO NOT block 100.64.0.0/10
    // or TEST-NET ranges. The spec's four categories (RFC1918,
    // link-local, loopback, ULA) are the contract.
    false
}

fn is_private_ipv6(v6: Ipv6Addr) -> bool {
    // Loopback ::1.
    if v6.is_loopback() {
        return true;
    }
    // fe80::/10 link-local.
    //
    // `Ipv6Addr::is_unicast_link_local` is stabilized in recent Rust and
    // matches the fe80::/10 range; we replicate its logic inline to
    // avoid depending on nightly-only methods.
    let segs = v6.segments();
    if (segs[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // fc00::/7 unique local addresses (ULA).
    if (segs[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // IPv4-mapped address — classify using the embedded IPv4 address so
    // that e.g. `::ffff:127.0.0.1` is also blocked.
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_private_ipv4(v4);
    }
    // Unspecified ::.
    if v6.is_unspecified() {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    // --- Pure IP classification tests ---

    #[test]
    fn test_rfc1918_blocked() {
        assert!(is_private_ip(
            "10.0.0.1".parse::<IpAddr>().expect("valid ip")
        ));
        assert!(is_private_ip(
            "10.255.255.255".parse::<IpAddr>().expect("valid ip")
        ));
        assert!(is_private_ip(
            "172.16.0.1".parse::<IpAddr>().expect("valid ip")
        ));
        assert!(is_private_ip(
            "172.31.255.255".parse::<IpAddr>().expect("valid ip")
        ));
        assert!(is_private_ip(
            "192.168.1.1".parse::<IpAddr>().expect("valid ip")
        ));
    }

    #[test]
    fn test_loopback_blocked() {
        assert!(is_private_ip(
            "127.0.0.1".parse::<IpAddr>().expect("valid ip")
        ));
        assert!(is_private_ip(
            "127.255.255.255".parse::<IpAddr>().expect("valid ip")
        ));
        assert!(is_private_ip("::1".parse::<IpAddr>().expect("valid ip")));
        assert!(is_private_ip(
            "::ffff:127.0.0.1".parse::<IpAddr>().expect("valid ip")
        ));
    }

    #[test]
    fn test_link_local_blocked() {
        assert!(is_private_ip(
            "169.254.0.1".parse::<IpAddr>().expect("valid ip")
        ));
        assert!(is_private_ip(
            "169.254.255.255".parse::<IpAddr>().expect("valid ip")
        ));
        assert!(is_private_ip(
            "fe80::1".parse::<IpAddr>().expect("valid ip")
        ));
    }

    #[test]
    fn test_ipv6_ula_blocked() {
        assert!(is_private_ip(
            "fc00::1".parse::<IpAddr>().expect("valid ip")
        ));
        assert!(is_private_ip(
            "fd00::1".parse::<IpAddr>().expect("valid ip")
        ));
    }

    #[test]
    fn test_public_ip_allowed() {
        assert!(!is_private_ip(
            "8.8.8.8".parse::<IpAddr>().expect("valid ip")
        ));
        assert!(!is_private_ip(
            "1.1.1.1".parse::<IpAddr>().expect("valid ip")
        ));
        assert!(!is_private_ip(
            "104.16.0.1".parse::<IpAddr>().expect("valid ip")
        ));
        assert!(!is_private_ip(
            "2606:4700:4700::1111".parse::<IpAddr>().expect("valid ip")
        ));
    }

    #[test]
    fn test_ipv4_unspecified_blocked() {
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))));
    }

    #[test]
    fn test_broadcast_blocked() {
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255))));
    }

    // --- resolve_safe tests ---

    #[tokio::test]
    async fn test_dns_lookup_error_fails_closed() {
        // Use a host that should not resolve in any sensible environment.
        let result = resolve_safe("nonexistent.invalid", 443).await;
        match result {
            Err(SsrfError::LookupFailed(_)) | Err(SsrfError::NoAddresses) => {}
            other => panic!("expected LookupFailed/NoAddresses, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_resolved_private_ip_rejected() {
        // `localhost` resolves to 127.0.0.1 and/or ::1 — both private.
        let result = resolve_safe("localhost", 443).await;
        match result {
            Err(SsrfError::PrivateIp(_)) => {}
            other => panic!("expected PrivateIp, got {other:?}"),
        }
    }
}
