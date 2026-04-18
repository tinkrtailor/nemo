use anyhow::Result;

use crate::api_types::InspectResponse;
use crate::client::NemoClient;

pub async fn fetch(client: &NemoClient, branch: &str) -> Result<InspectResponse> {
    client
        .get(&format!("/inspect?branch={}", urlencoding::encode(branch)))
        .await
}

pub async fn run(client: &NemoClient, path: &str) -> Result<()> {
    // Prepend "agent/" if not already present so users can pass "alice/slug-hash"
    let branch = if path.starts_with("agent/") {
        path.to_string()
    } else {
        format!("agent/{path}")
    };

    // Pass branch as query param (not path segment) because branch names contain slashes
    let resp = fetch(client, &branch).await?;

    // FR-6c: structured JSON output; formatted human-readable rendering is a future enhancement.
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}
