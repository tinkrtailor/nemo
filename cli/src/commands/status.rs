use anyhow::Result;

use crate::api_types::StatusResponse;
use crate::client::NemoClient;

pub async fn fetch(client: &NemoClient, engineer: &str, team: bool) -> Result<StatusResponse> {
    client.get(&status_path(engineer, team)).await
}

fn status_path(engineer: &str, team: bool) -> String {
    if team {
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
    }
}

pub async fn run(client: &NemoClient, engineer: &str, team: bool, json: bool) -> Result<()> {
    let resp = fetch(client, engineer, team).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&resp.loops)?);
        return Ok(());
    }

    if resp.loops.is_empty() {
        println!("No active loops.");
        return Ok(());
    }

    // Table output. ACTIVITY shows the relative time since the
    // reconciler last observed forward progress (new agent log bytes
    // or fresh dispatch); TOKENS shows cumulative LLM token usage
    // across completed rounds as `<input>/<output>`. A loop wedged
    // on dead credentials grows "Xm ago" while TOKENS stays flat;
    // a healthy active loop resets ACTIVITY each tick and ticks
    // TOKENS up after each round.
    println!(
        "{:<38} {:<12} {:<10} {:<20} {:<40} {:<8} {:<10} {:<14}",
        "LOOP ID", "STATE", "STAGE", "ENGINEER", "SPEC", "ROUND", "ACTIVITY", "TOKENS (in/out)"
    );
    println!("{}", "-".repeat(165));

    for l in &resp.loops {
        let state_display = match &l.sub_state {
            Some(sub) => format!("{}/{}", l.state, sub),
            None => l.state.clone(),
        };
        let stage_display = l.current_stage.as_deref().unwrap_or("-");
        let activity_display = format_activity(l.last_activity_at.as_deref());
        let tokens_display = format!(
            "{}/{}",
            format_tokens(l.tokens_input),
            format_tokens(l.tokens_output)
        );
        println!(
            "{:<38} {:<12} {:<10} {:<20} {:<40} {:<8} {:<10} {:<14}",
            l.loop_id,
            state_display,
            stage_display,
            l.engineer,
            l.spec_path,
            l.round,
            activity_display,
            tokens_display
        );
    }

    Ok(())
}

/// Format a token count with k/m suffixes for readability. Mirrors
/// the existing helm cost formatter so `nemo status` and `nemo
/// helm` show consistent units.
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// Render `last_activity_at` as a compact relative time. `None` /
/// unparseable timestamps render as `-` so the column always lines up.
/// Buckets match the existing `nemo ps` age formatter so the two
/// commands feel consistent.
fn format_activity(ts: Option<&str>) -> String {
    let Some(s) = ts else {
        return "-".to_string();
    };
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(s) else {
        return "-".to_string();
    };
    let elapsed = chrono::Utc::now().signed_duration_since(parsed.with_timezone(&chrono::Utc));
    let secs = elapsed.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        if mins == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{mins}m")
        }
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_activity_renders_dash_when_absent_or_unparseable() {
        assert_eq!(format_activity(None), "-");
        assert_eq!(format_activity(Some("not a timestamp")), "-");
    }

    #[test]
    fn format_tokens_compacts_with_k_and_m_suffixes() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_500), "1k");
        assert_eq!(format_tokens(120_000), "120k");
        assert_eq!(format_tokens(2_400_000), "2.4m");
    }

    #[test]
    fn format_activity_buckets_into_seconds_minutes_hours_days() {
        let now = chrono::Utc::now();
        let f = |dur: chrono::Duration| {
            let ts = (now - dur).to_rfc3339();
            format_activity(Some(&ts))
        };
        // Use generous tolerance windows because the test reads its
        // own `now` after the fixture builds, so the boundary is
        // approximate. Pick durations safely inside a bucket.
        assert!(f(chrono::Duration::seconds(5)).ends_with('s'));
        assert!(f(chrono::Duration::minutes(15)).ends_with('m'));
        assert!(f(chrono::Duration::hours(5)).contains('h'));
        assert!(f(chrono::Duration::days(2)).ends_with('d'));
    }
}
