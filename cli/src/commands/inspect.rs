use anyhow::Result;

use crate::api_types::InspectResponse;
use crate::client::NemoClient;

pub async fn fetch_by_id(client: &NemoClient, id: uuid::Uuid) -> Result<InspectResponse> {
    client.get(&format!("/inspect?id={id}")).await
}

pub async fn fetch_by_branch(client: &NemoClient, branch: &str) -> Result<InspectResponse> {
    client
        .get(&format!("/inspect?branch={}", urlencoding::encode(branch)))
        .await
}

/// Back-compat alias for `fetch_by_branch`. Callers that already have
/// a fully-qualified branch name (e.g. the helm TUI) can keep using
/// this; the UUID-routing logic lives in `run`.
pub async fn fetch(client: &NemoClient, branch: &str) -> Result<InspectResponse> {
    fetch_by_branch(client, branch).await
}

/// Run the inspect command. Accepts either a loop UUID or a branch
/// shorthand (e.g. `alice/slug-hash`, which is auto-prefixed with
/// `agent/`). UUID inputs used to be silently treated as branch names,
/// producing 400 "No loop found for branch: agent/<uuid>"; now they
/// are routed to the id-based lookup instead.
///
/// The `_json` parameter is accepted for consistency (so agents can
/// pass `--json` uniformly) but has no effect — output is always JSON.
pub async fn run(client: &NemoClient, path: &str, _json: bool) -> Result<()> {
    let resp = if let Ok(id) = uuid::Uuid::try_parse(path) {
        fetch_by_id(client, id).await?
    } else {
        // Prepend "agent/" if not already present so users can pass "alice/slug-hash"
        let branch = if path.starts_with("agent/") {
            path.to_string()
        } else {
            format!("agent/{path}")
        };
        fetch_by_branch(client, &branch).await?
    };

    // Structured JSON output; formatted human-readable rendering is a future enhancement.
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}
