use anyhow::Result;

use crate::client::NemoClient;

pub async fn run(client: &NemoClient, path: &str) -> Result<()> {
    // Prepend "agent/" if not already present so users can pass "alice/slug-hash"
    let branch_path = if path.starts_with("agent/") {
        path.to_string()
    } else {
        format!("agent/{path}")
    };

    let resp: serde_json::Value = client.get(&format!("/inspect/{branch_path}")).await?;

    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}
