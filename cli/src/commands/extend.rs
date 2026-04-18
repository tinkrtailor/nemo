use anyhow::Result;

use crate::client::NemoClient;

#[derive(serde::Deserialize)]
struct ExtendResponse {
    loop_id: uuid::Uuid,
    prior_max_rounds: u32,
    new_max_rounds: u32,
    resumed_to_state: String,
}

pub async fn run(client: &NemoClient, loop_id: &str, add_rounds: u32) -> Result<()> {
    let resp: ExtendResponse = client
        .post(
            &format!("/extend/{loop_id}"),
            &serde_json::json!({ "add_rounds": add_rounds }),
        )
        .await?;

    println!("Extended loop {}", resp.loop_id);
    println!(
        "  max_rounds: {} -> {} (+{})",
        resp.prior_max_rounds, resp.new_max_rounds, add_rounds
    );
    println!("  Resuming at: {}", resp.resumed_to_state);
    println!("  Loop will continue on next reconciliation tick.");
    Ok(())
}
