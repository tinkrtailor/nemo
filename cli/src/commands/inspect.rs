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

    // Header
    println!("Loop:     {}", resp.loop_id);
    println!("Engineer: {}", resp.engineer);
    println!("Branch:   {}", resp.branch);
    println!("State:    {}", resp.state);
    println!();

    // Rounds
    for round in &resp.rounds {
        println!("── Round {} ──", round.round);

        if let Some(ref v) = round.implement {
            print_stage("implement", v);
        }
        if let Some(ref v) = round.test {
            print_stage("test", v);
        }
        if let Some(ref v) = round.review {
            print_stage("review", v);
        }
        if let Some(ref v) = round.audit {
            print_stage("audit", v);
        }
        if let Some(ref v) = round.revise {
            print_stage("revise", v);
        }

        // Highlight judge decisions inline with the round (FR-6c)
        if let Some(ref jd) = round.judge_decision {
            println!(
                "  judge:     decision={} confidence={} trigger={} ({}ms)",
                jd.decision,
                jd.confidence
                    .map(|c| format!("{:.2}", c))
                    .unwrap_or_else(|| "n/a".to_string()),
                jd.trigger,
                jd.duration_ms,
            );
            if let Some(ref reasoning) = jd.reasoning {
                println!("             reasoning: {reasoning}");
            }
            if let Some(ref hint) = jd.hint {
                println!("             hint: {hint}");
            }
        }

        println!();
    }

    // Show any judge decisions not rendered inline (edge case: round number mismatch)
    let rendered_rounds: std::collections::HashSet<i32> = resp
        .rounds
        .iter()
        .filter(|r| r.judge_decision.is_some())
        .map(|r| r.round)
        .collect();
    let orphaned_count = resp
        .judge_decisions
        .iter()
        .filter(|jd| !rendered_rounds.contains(&jd.round))
        .count();
    if orphaned_count > 0 {
        println!(
            "{orphaned_count} additional judge decision(s) not shown inline (use --json for details)"
        );
        println!();
    }

    Ok(())
}

fn print_stage(name: &str, value: &serde_json::Value) {
    // Show a compact one-line summary for each stage
    let clean = value
        .get("clean")
        .or_else(|| value.get("verdict").and_then(|v| v.get("clean")));
    let issues = value
        .get("issues")
        .or_else(|| value.get("verdict").and_then(|v| v.get("issues")))
        .and_then(|v| v.as_array())
        .map(|a| a.len());

    let mut summary = String::new();
    if let Some(clean) = clean {
        summary.push_str(&format!("clean={clean}"));
    }
    if let Some(count) = issues {
        if !summary.is_empty() {
            summary.push_str(", ");
        }
        summary.push_str(&format!("issues={count}"));
    }
    if summary.is_empty() {
        summary.push_str("completed");
    }

    println!("  {name:10} {summary}");
}
