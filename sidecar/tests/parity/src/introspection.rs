//! Client for the mock services' `/__harness/logs` + `/__harness/reset`
//! introspection API (FR-13, FR-14).
//!
//! Each mock service exposes these endpoints on a dedicated `:9999`
//! port published to the host as 49990-49993 (mock-openai,
//! mock-anthropic, mock-github-ssh, mock-example). The harness
//! driver connects via `http://127.0.0.1:4999X`.
//!
//! `mock-tcp-echo` has no introspection endpoint (it's a raw TCP
//! echo) — this module's `reset_all` skips it.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::compose::ports;
use crate::result::ObservedMockRequest;

/// One mock's introspection endpoint — logical name + host port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MockEndpoint {
    pub name: &'static str,
    pub port: u16,
}

/// The four introspectable mocks. Order matters for FR-18 step 1
/// reset ordering only in that we reset all four before every case.
pub const INTROSPECTION_ENDPOINTS: &[MockEndpoint] = &[
    MockEndpoint {
        name: "mock-openai",
        port: ports::MOCK_OPENAI_INTROSPECT,
    },
    MockEndpoint {
        name: "mock-anthropic",
        port: ports::MOCK_ANTHROPIC_INTROSPECT,
    },
    MockEndpoint {
        name: "mock-github-ssh",
        port: ports::MOCK_GH_SSH_INTROSPECT,
    },
    MockEndpoint {
        name: "mock-example",
        port: ports::MOCK_EXAMPLE_INTROSPECT,
    },
];

/// Raw introspection record as serialized by the mock services.
/// Kept separate from [`ObservedMockRequest`] so the wire format
/// can drift slightly without touching the diff engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawLogEntry {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub timestamp: String,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub host_header: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body_b64: String,
    pub source_ip: String,
}

/// Reset logs on every introspectable mock. Used once per case (FR-18
/// step 1).
pub async fn reset_all() -> Result<()> {
    let client = build_client()?;
    for ep in INTROSPECTION_ENDPOINTS {
        let url = format!("http://127.0.0.1:{}/__harness/reset", ep.port);
        let resp = client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("POST {url} for {}", ep.name))?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "reset_all: mock {} returned HTTP {}",
                ep.name,
                resp.status()
            ));
        }
    }
    Ok(())
}

/// Fetch all observed requests from all four introspectable mocks
/// and return them split by sidecar source IP.
///
/// Returns `(go_observations, rust_observations)`. Attribution per
/// FR-18: Go = 100.64.0.20, Rust = 100.64.0.21.
///
/// Observations from other source IPs (e.g. the harness driver's
/// own bridge IP if someone reconfigured the network) are silently
/// dropped because FR-18 only cares about the two sidecars.
pub async fn fetch_and_split() -> Result<(Vec<ObservedMockRequest>, Vec<ObservedMockRequest>)> {
    let client = build_client()?;
    let mut go = Vec::new();
    let mut rust = Vec::new();
    for ep in INTROSPECTION_ENDPOINTS {
        let url = format!("http://127.0.0.1:{}/__harness/logs", ep.port);
        let resp = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url} for {}", ep.name))?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "fetch_and_split: mock {} returned HTTP {}",
                ep.name,
                resp.status()
            ));
        }
        let raw: Vec<RawLogEntry> = resp
            .json()
            .await
            .with_context(|| format!("parsing JSON from {url}"))?;
        for entry in raw {
            let converted = ObservedMockRequest {
                mock: ep.name.to_string(),
                method: entry.method,
                path: entry.path,
                host_header: entry.host_header,
                headers: entry.headers,
                body_b64: entry.body_b64,
                source_ip: entry.source_ip.clone(),
            };
            match entry.source_ip.as_str() {
                "100.64.0.20" => go.push(converted),
                "100.64.0.21" => rust.push(converted),
                _ => {
                    // Unknown source — skip silently. Harness driver
                    // does not introspect other containers.
                }
            }
        }
    }
    Ok((go, rust))
}

fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .context("build introspection reqwest client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn introspection_endpoints_cover_four_mocks() {
        assert_eq!(INTROSPECTION_ENDPOINTS.len(), 4);
        assert!(
            INTROSPECTION_ENDPOINTS
                .iter()
                .any(|e| e.name == "mock-openai")
        );
        assert!(
            INTROSPECTION_ENDPOINTS
                .iter()
                .any(|e| e.name == "mock-anthropic")
        );
        assert!(
            INTROSPECTION_ENDPOINTS
                .iter()
                .any(|e| e.name == "mock-github-ssh")
        );
        assert!(
            INTROSPECTION_ENDPOINTS
                .iter()
                .any(|e| e.name == "mock-example")
        );
    }

    #[test]
    fn raw_log_entry_deserializes_minimal_json() {
        let raw: RawLogEntry = serde_json::from_str(
            r#"{"method":"GET","path":"/v1/models","source_ip":"100.64.0.20"}"#,
        )
        .unwrap();
        assert_eq!(raw.method, "GET");
        assert_eq!(raw.path, "/v1/models");
        assert_eq!(raw.source_ip, "100.64.0.20");
    }

    #[test]
    fn raw_log_entry_deserializes_full_json() {
        let raw: RawLogEntry = serde_json::from_str(
            r#"{
                "id": 3,
                "timestamp": "2026-04-08T00:00:00Z",
                "method": "POST",
                "path": "/v1/chat/completions",
                "host_header": "api.openai.com",
                "headers": {"authorization": "Bearer sk-test-openai-key"},
                "body_b64": "eyJ9",
                "source_ip": "100.64.0.21"
            }"#,
        )
        .unwrap();
        assert_eq!(raw.id, 3);
        assert_eq!(raw.headers.len(), 1);
        assert_eq!(raw.body_b64, "eyJ9");
    }
}
