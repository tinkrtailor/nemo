//! Nautiloop auth sidecar.
//!
//! The library
//! surface exists so unit tests can exercise the modules without spinning
//! up the full binary.
//!
//! Production code (NFR-6) must never panic in request handlers. We
//! enforce that by denying `unwrap_used` / `expect_used` in non-test
//! cfg. Test modules are exempt because failing tests SHOULD panic —
//! that is the assertion mechanism.

#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![warn(clippy::all)]

pub mod egress;
pub mod git_ssh_proxy;
pub mod git_url;
pub mod health;
pub mod logging;
pub mod model_proxy;
pub mod shutdown;
pub mod ssrf;
pub mod ssrf_connector;
pub mod tls;
