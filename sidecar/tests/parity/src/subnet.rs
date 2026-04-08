//! FR-29 subnet whitelist validator.
//!
//! The harness's Docker bridge network must live inside one of four
//! well-known IPv4 ranges that neither the Go nor Rust sidecar's SSRF
//! blocklist rejects:
//!
//! - `100.64.0.0/10` (RFC6598 Carrier-Grade NAT shared address space)
//! - `192.0.2.0/24`  (RFC5737 TEST-NET-1)
//! - `198.51.100.0/24` (RFC5737 TEST-NET-2)
//! - `203.0.113.0/24` (RFC5737 TEST-NET-3)
//!
//! Cross-checked against `sidecar/src/ssrf.rs:94-99` (Rust) and
//! `images/sidecar/main.go:43-48` (Go). Any subnet the operator supplies
//! MUST be a subset (including equality) of one of these ranges.
//!
//! A whitelist is the correct approach here: a blacklist based on
//! sampling single addresses is unsound because a straddling subnet
//! like `9.255.0.0/15` contains public first and last addresses but
//! includes `10.0.0.0/15` in the middle.
//!
//! The resolver order is: `--subnet` CLI flag (highest) → the
//! `PARITY_NET_SUBNET` env var → the default `100.64.0.0/24`. Whatever
//! the resolved value is, it is validated UNCONDITIONALLY.

use std::str::FromStr;

use ipnet::Ipv4Net;
use thiserror::Error;

/// Default subnet used when neither the CLI flag nor env var is set.
pub const DEFAULT_SUBNET: &str = "100.64.0.0/24";

/// Environment variable consulted if the `--subnet` flag is absent.
pub const SUBNET_ENV_VAR: &str = "PARITY_NET_SUBNET";

/// The four RFC ranges that neither sidecar blocks. Any resolved subnet
/// must be a subset of one of these ranges to be accepted.
const SAFE_RANGES: &[&str] = &[
    "100.64.0.0/10",
    "192.0.2.0/24",
    "198.51.100.0/24",
    "203.0.113.0/24",
];

/// Errors returned by [`resolve_and_validate`] / [`validate_subnet_whitelist`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SubnetError {
    /// The supplied string was not a valid IPv4 CIDR.
    #[error("{SUBNET_ENV_VAR} must be a valid IPv4 CIDR, got {0:?}")]
    InvalidCidr(String),
    /// The subnet is not a subset of any whitelisted range.
    #[error(
        "subnet {subnet} is not within any whitelisted range ({safe:?}). \
         Neither sidecar blocks these ranges; see FR-29 for the rationale."
    )]
    NotWhitelisted {
        /// The user-supplied subnet string (preserved for the message).
        subnet: String,
        /// The whitelist as it appeared at the time of the check.
        safe: Vec<String>,
    },
}

/// Resolve the effective subnet from explicit flag, env var, and
/// default (in that order) and validate it against the whitelist.
///
/// This is the single entry point the harness driver should call at
/// startup. The returned string is safe to pass to `docker compose`
/// via `PARITY_NET_SUBNET`.
pub fn resolve_and_validate(flag: Option<&str>) -> Result<String, SubnetError> {
    let resolved = flag
        .map(|s| s.to_string())
        .or_else(|| std::env::var(SUBNET_ENV_VAR).ok())
        .unwrap_or_else(|| DEFAULT_SUBNET.to_string());
    validate_subnet_whitelist(&resolved)?;
    Ok(resolved)
}

/// Validate that `subnet_str` is a valid IPv4 CIDR and is contained
/// within at least one of the safe whitelist ranges.
///
/// Uses `ipnet::Ipv4Net::contains` which returns `true` when the inner
/// net is a (non-strict) subset of the outer net — the equality case
/// (e.g. `--subnet 100.64.0.0/10`) passes.
pub fn validate_subnet_whitelist(subnet_str: &str) -> Result<(), SubnetError> {
    let subnet: Ipv4Net = Ipv4Net::from_str(subnet_str)
        .map_err(|_| SubnetError::InvalidCidr(subnet_str.to_string()))?;
    for safe_cidr in SAFE_RANGES {
        // The whitelist is compile-time string-const so parse must succeed.
        // We guard against future edits by bubbling a descriptive error
        // rather than panicking — unreachable in practice.
        let Ok(safe) = Ipv4Net::from_str(safe_cidr) else {
            return Err(SubnetError::InvalidCidr(format!(
                "internal whitelist entry {safe_cidr}"
            )));
        };
        if safe.contains(&subnet) {
            return Ok(());
        }
    }
    Err(SubnetError::NotWhitelisted {
        subnet: subnet_str.to_string(),
        safe: SAFE_RANGES.iter().map(|s| s.to_string()).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Whitelist happy path ---

    #[test]
    fn default_subnet_is_whitelisted() {
        validate_subnet_whitelist(DEFAULT_SUBNET).expect("default must pass whitelist");
    }

    #[test]
    fn entire_cgnat_range_is_accepted_equality_case() {
        // Regression for the v5→v6 "strict subset" bug: the equality
        // case must pass because `Ipv4Net::contains` is NOT strict
        // subset. FR-29 explicitly requires this.
        validate_subnet_whitelist("100.64.0.0/10").expect("equality case must pass");
    }

    #[test]
    fn all_three_test_net_ranges_accepted() {
        validate_subnet_whitelist("192.0.2.0/24").expect("TEST-NET-1 must pass");
        validate_subnet_whitelist("198.51.100.0/24").expect("TEST-NET-2 must pass");
        validate_subnet_whitelist("203.0.113.0/24").expect("TEST-NET-3 must pass");
    }

    #[test]
    fn proper_subnet_inside_cgnat_accepted() {
        validate_subnet_whitelist("100.64.1.0/24").expect("proper subset must pass");
        validate_subnet_whitelist("100.127.255.0/24").expect("top of range must pass");
    }

    #[test]
    fn proper_subnet_inside_test_net_1_accepted() {
        validate_subnet_whitelist("192.0.2.128/25").expect("half of TEST-NET-1 must pass");
    }

    // --- Whitelist rejection path ---

    #[test]
    fn rfc1918_rejected() {
        let err = validate_subnet_whitelist("10.0.0.0/8").unwrap_err();
        assert!(
            matches!(err, SubnetError::NotWhitelisted { .. }),
            "10.0.0.0/8 must be NotWhitelisted, got {err:?}"
        );
    }

    #[test]
    fn docker_default_bridge_rejected() {
        // Docker default is 172.16.0.0/12 which the Go sidecar blocks
        // (FR-15). If a user somehow passed it, we must refuse.
        let err = validate_subnet_whitelist("172.17.0.0/16").unwrap_err();
        assert!(matches!(err, SubnetError::NotWhitelisted { .. }));
    }

    #[test]
    fn loopback_rejected() {
        let err = validate_subnet_whitelist("127.0.0.0/24").unwrap_err();
        assert!(matches!(err, SubnetError::NotWhitelisted { .. }));
    }

    #[test]
    fn link_local_rejected() {
        let err = validate_subnet_whitelist("169.254.0.0/16").unwrap_err();
        assert!(matches!(err, SubnetError::NotWhitelisted { .. }));
    }

    #[test]
    fn public_but_not_whitelisted_rejected() {
        // A fully public subnet that isn't in the whitelist must be
        // refused anyway — the whitelist is the contract, not the
        // sidecar's blocklist.
        let err = validate_subnet_whitelist("8.8.8.0/24").unwrap_err();
        assert!(matches!(err, SubnetError::NotWhitelisted { .. }));
    }

    #[test]
    fn straddling_subnet_rejected() {
        // 9.255.0.0/15 spans 9.255.0.0-10.0.255.255. Its first and
        // last addresses differ in whether they are private, and a
        // naive sample-based blacklist would accept it; the whitelist
        // must refuse it because it isn't a subset of any safe range.
        let err = validate_subnet_whitelist("9.255.0.0/15").unwrap_err();
        assert!(matches!(err, SubnetError::NotWhitelisted { .. }));
    }

    #[test]
    fn supernet_of_whitelisted_range_rejected() {
        // /9 contains 100.64.0.0/10 AND 100.0.0.0/9 includes 100.0.0.0
        // which is outside the CGNAT range. The whitelist uses subset
        // containment, so the broader range cannot be accepted.
        let err = validate_subnet_whitelist("100.0.0.0/9").unwrap_err();
        assert!(matches!(err, SubnetError::NotWhitelisted { .. }));
    }

    // --- Parser rejection path ---

    #[test]
    fn non_cidr_input_rejected() {
        assert!(matches!(
            validate_subnet_whitelist("nonsense"),
            Err(SubnetError::InvalidCidr(_))
        ));
    }

    #[test]
    fn ipv6_cidr_rejected() {
        // Ipv4Net::from_str rejects IPv6 inputs.
        assert!(matches!(
            validate_subnet_whitelist("fc00::/7"),
            Err(SubnetError::InvalidCidr(_))
        ));
    }

    #[test]
    fn bare_ipv4_no_prefix_rejected() {
        assert!(matches!(
            validate_subnet_whitelist("100.64.0.0"),
            Err(SubnetError::InvalidCidr(_))
        ));
    }

    // --- resolve_and_validate ---

    #[test]
    fn flag_wins_over_env_and_default() {
        // Safety: env access is shared but this test sets its own
        // value; no parallel test depends on PARITY_NET_SUBNET.
        // SAFETY: single-threaded env access for this test only.
        unsafe {
            std::env::set_var(SUBNET_ENV_VAR, "10.0.0.0/8");
        }
        let got =
            resolve_and_validate(Some("192.0.2.0/24")).expect("flag wins and passes whitelist");
        assert_eq!(got, "192.0.2.0/24");
        unsafe {
            std::env::remove_var(SUBNET_ENV_VAR);
        }
    }

    #[test]
    fn default_used_when_no_flag_no_env() {
        // SAFETY: clear any pre-existing value first.
        unsafe {
            std::env::remove_var(SUBNET_ENV_VAR);
        }
        let got = resolve_and_validate(None).expect("default must pass whitelist");
        assert_eq!(got, DEFAULT_SUBNET);
    }

    #[test]
    fn invalid_default_not_reachable_via_api() {
        // Resolve-and-validate with a bogus flag should surface the
        // InvalidCidr error at the validator layer.
        let err = resolve_and_validate(Some("not-a-cidr")).unwrap_err();
        assert!(matches!(err, SubnetError::InvalidCidr(_)));
    }
}
