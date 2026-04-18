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

    // TODO(stage-1): FR-6c envisions judge_decisions rendered inline with round
    // summaries. For now we output structured JSON; formatted human-readable
    // rendering of judge decisions is deferred to a future pass.
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}
