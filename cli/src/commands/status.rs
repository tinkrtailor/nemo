use anyhow::Result;

use crate::client::NemoClient;

#[derive(serde::Deserialize)]
struct StatusResponse {
    loops: Vec<LoopSummary>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct LoopSummary {
    loop_id: uuid::Uuid,
    engineer: String,
    spec_path: String,
    branch: String,
    state: String,
    sub_state: Option<String>,
    round: i32,
    created_at: String,
    updated_at: String,
}

pub async fn run(client: &NemoClient, engineer: &str, team: bool, json: bool) -> Result<()> {
    let path = if team {
        "/status?team=true".to_string()
    } else {
        // Percent-encode engineer name to handle special characters
        let encoded: String = engineer
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' {
                    format!("{}", b as char)
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        format!("/status?engineer={encoded}")
    };

    let resp: StatusResponse = client.get(&path).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&resp.loops)?);
        return Ok(());
    }

    if resp.loops.is_empty() {
        println!("No active loops.");
        return Ok(());
    }

    // Table output
    println!(
        "{:<38} {:<12} {:<20} {:<45} {:<8}",
        "LOOP ID", "STATE", "ENGINEER", "SPEC", "ROUND"
    );
    println!("{}", "-".repeat(123));

    for l in &resp.loops {
        let state_display = match &l.sub_state {
            Some(sub) => format!("{}/{}", l.state, sub),
            None => l.state.clone(),
        };
        println!(
            "{:<38} {:<12} {:<20} {:<45} {:<8}",
            l.loop_id, state_display, l.engineer, l.spec_path, l.round
        );
    }

    Ok(())
}
