//! Health category runner (FR-22 fourth block).
//!
//! `healthz_post_ready_returns_200`: GET `/healthz` on both sidecars,
//! expect 200 and body `{"status":"ok"}`.
//!
//! `healthz_head_method_parity`: HEAD `/healthz`, expect 200 on both
//! (Go's mux does not method-check, Rust's handler behaves the same —
//! matches the unit test in `sidecar/src/health.rs`).

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::compose::ports;
use crate::corpus::CorpusCase;
use crate::result::SideOutput;
use crate::runner::RunnerContext;

#[derive(Debug, Clone, Deserialize)]
struct HealthInput {
    /// `"GET"` or `"HEAD"`. Default `"GET"`.
    #[serde(default = "default_method")]
    method: String,
}

fn default_method() -> String {
    "GET".to_string()
}

pub async fn run(case: &CorpusCase, _ctx: &RunnerContext) -> Result<(SideOutput, SideOutput)> {
    let input: HealthInput = serde_json::from_value(case.input.clone())
        .with_context(|| format!("parsing input for case {}", case.name))?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("build health reqwest client")?;

    let go = issue(&client, &input, ports::GO_HEALTH).await?;
    let rust = issue(&client, &input, ports::RUST_HEALTH).await?;
    Ok((go, rust))
}

async fn issue(client: &reqwest::Client, input: &HealthInput, port: u16) -> Result<SideOutput> {
    let method = reqwest::Method::from_bytes(input.method.as_bytes())
        .with_context(|| format!("parsing HTTP method {:?}", input.method))?;
    let url = format!("http://127.0.0.1:{port}/healthz");
    let resp = client
        .request(method, &url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status().as_u16();
    let mut headers = BTreeMap::new();
    for (k, v) in resp.headers() {
        headers.insert(
            k.as_str().to_ascii_lowercase(),
            v.to_str().unwrap_or("").to_string(),
        );
    }
    let body = resp.text().await.unwrap_or_default();
    Ok(SideOutput::http(status, headers, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_input_default_method() {
        let v = serde_json::json!({});
        let input: HealthInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.method, "GET");
    }

    #[test]
    fn health_input_head_method() {
        let v = serde_json::json!({"method": "HEAD"});
        let input: HealthInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.method, "HEAD");
    }
}
