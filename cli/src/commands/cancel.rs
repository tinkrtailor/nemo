use anyhow::Result;

use crate::client::NemoClient;

#[derive(serde::Deserialize)]
struct CancelResponse {
    loop_id: uuid::Uuid,
    state: String,
    cancel_requested: bool,
}

pub async fn run(client: &NemoClient, loop_id: &str) -> Result<()> {
    let resp: CancelResponse = client.delete(&format!("/cancel/{loop_id}")).await?;
    if resp.cancel_requested {
        println!("Cancel requested for loop {}", resp.loop_id);
        println!("  Current state: {}", resp.state);
        println!("  The loop engine will cancel the loop on the next tick.");
    } else {
        println!("Loop {} state: {}", resp.loop_id, resp.state);
    }
    Ok(())
}
