use anyhow::Result;

use crate::client::NemoClient;

#[derive(serde::Deserialize, serde::Serialize)]
struct ResumeResponse {
    loop_id: uuid::Uuid,
    state: String,
    resume_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stage_timeout_secs: Option<u32>,
}

pub async fn run(
    client: &NemoClient,
    loop_id: &str,
    json: bool,
    stage_timeout: Option<u32>,
) -> Result<()> {
    let body = match stage_timeout {
        Some(secs) => serde_json::json!({ "stage_timeout_secs": secs }),
        None => serde_json::json!({}),
    };

    let resp: ResumeResponse = client.post(&format!("/resume/{loop_id}"), &body).await?;

    if json {
        let output = serde_json::json!({
            "loop_id": resp.loop_id,
            "state": resp.state,
            "resume_requested": resp.resume_requested,
            "stage_timeout_secs": resp.stage_timeout_secs,
            "message": if resp.resume_requested { "Loop resumed." } else { "Resume not applicable for current state." },
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if resp.resume_requested {
        println!("Resumed loop {}", resp.loop_id);
        println!("  State: {}", resp.state);
        if let Some(secs) = resp.stage_timeout_secs {
            println!("  Stage timeout: {secs}s (per-loop override)");
        }
        println!("  Loop will resume on next reconciliation tick.");
    } else {
        println!(
            "Loop {} is in state {} (resume not applicable)",
            resp.loop_id, resp.state
        );
    }
    Ok(())
}
