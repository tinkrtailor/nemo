use std::collections::HashMap;

use super::cost::{
    self, PricingConfig, calculate_loop_round_cost, format_cost, format_tokens,
    round_duration_secs, round_total_tokens,
};
use super::is_terminal_state;
use crate::api_types::{InspectResponse, LoopSummary};

/// Build the compact one-line header summary (FR-1a).
///
/// Format: `nautiloop · <profile> · N active · X impl · Y review · Z harden · W awaiting · T tokens · $C · Dh Dm`
pub fn build_header(
    loops: &[LoopSummary],
    all_inspect: &HashMap<uuid::Uuid, InspectResponse>,
    pricing: &PricingConfig,
    team: bool,
    profile_name: &str,
) -> String {
    let non_terminal: Vec<&LoopSummary> = loops
        .iter()
        .filter(|l| !is_terminal_state(&l.state))
        .collect();

    if non_terminal.is_empty() {
        return if team {
            format!(
                "nautiloop · {profile_name} · team view · no active loops · press s to start a new spec"
            )
        } else {
            format!("nautiloop · {profile_name} · no active loops · press s to start a new spec")
        };
    }

    let active_count = non_terminal.len();

    // Stage breakdown
    let mut impl_count = 0;
    let mut review_count = 0;
    let mut harden_count = 0;
    let mut awaiting_count = 0;
    let mut test_count = 0;
    for l in &non_terminal {
        match l.state.as_str() {
            "IMPLEMENTING" => impl_count += 1,
            "REVIEWING" => review_count += 1,
            "HARDENING" => harden_count += 1,
            "AWAITING_APPROVAL" => awaiting_count += 1,
            "TESTING" => test_count += 1,
            _ => {}
        }
    }

    // Cumulative tokens, cost, and duration from inspect data
    let mut total_input_tokens = 0u64;
    let mut total_output_tokens = 0u64;
    let mut total_duration = 0i64;
    let mut has_unknown_cost = false;
    let mut total_cost = 0.0f64;

    for l in &non_terminal {
        if let Some(inspect) = all_inspect.get(&l.loop_id) {
            for round in &inspect.rounds {
                let (inp, out) = round_total_tokens(round);
                total_input_tokens += inp;
                total_output_tokens += out;
                total_duration += round_duration_secs(round);

                let cost = calculate_loop_round_cost(
                    pricing,
                    l.model_implementor.as_deref(),
                    l.model_reviewer.as_deref(),
                    round,
                );
                match cost {
                    Some(c) => total_cost += c,
                    None => has_unknown_cost = true,
                }
            }
        }
    }

    let total_tokens = total_input_tokens + total_output_tokens;
    let tokens_str = format_tokens(total_tokens);
    let cost_str = if has_unknown_cost {
        format!("{}†", format_cost(Some(total_cost)))
    } else {
        format_cost(Some(total_cost))
    };
    let duration_str = cost::format_duration_secs(total_duration);

    // Build stage parts (always include at least impl count when stages are all zero)
    let mut stage_parts = Vec::new();
    if impl_count > 0 {
        stage_parts.push(format!("{impl_count} impl"));
    }
    if test_count > 0 {
        stage_parts.push(format!("{test_count} test"));
    }
    if review_count > 0 {
        stage_parts.push(format!("{review_count} review"));
    }
    if harden_count > 0 {
        stage_parts.push(format!("{harden_count} harden"));
    }
    if awaiting_count > 0 {
        stage_parts.push(format!("{awaiting_count} awaiting"));
    }
    if stage_parts.is_empty() {
        stage_parts.push("0 active stages".to_string());
    }

    let prefix = if team {
        // FR-1c: label by engineer in team view
        let mut engineer_counts: HashMap<&str, usize> = HashMap::new();
        for l in &non_terminal {
            *engineer_counts.entry(l.engineer.as_str()).or_default() += 1;
        }
        let mut engineers: Vec<_> = engineer_counts.into_iter().collect();
        engineers.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let eng_parts: Vec<String> = engineers.iter().map(|(e, c)| format!("{e}:{c}")).collect();
        format!(
            "nautiloop · {profile_name} · team [{}]",
            eng_parts.join(" ")
        )
    } else {
        format!("nautiloop · {profile_name}")
    };
    let stages = stage_parts.join(" · ");

    format!(
        "{prefix} · {active_count} active · {stages} · {tokens_str} tokens · {cost_str} · {duration_str}"
    )
}

/// Build approval context hints for the footer (FR-10a).
pub fn approval_hints(loop_item: &LoopSummary) -> Vec<(&'static str, &'static str)> {
    let mut hints = Vec::new();
    match loop_item.state.as_str() {
        "AWAITING_APPROVAL" => {
            hints.push(("a", "approve"));
            hints.push(("x", "cancel"));
            hints.push(("R", "see rounds"));
        }
        "CONVERGED" | "HARDENED" | "SHIPPED" => {
            if loop_item.spec_pr_url.is_some() {
                hints.push(("o", "open PR"));
            }
            hints.push(("R", "see rounds"));
        }
        _ => {}
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_loop(state: &str) -> LoopSummary {
        LoopSummary {
            loop_id: uuid::Uuid::new_v4(),
            engineer: "alice".to_string(),
            spec_path: "specs/test.md".to_string(),
            branch: "agent/alice/test".to_string(),
            state: state.to_string(),
            sub_state: None,
            round: 1,
            current_stage: None,
            active_job_name: None,
            spec_pr_url: None,
            failed_from_state: None,
            kind: "implement".to_string(),
            max_rounds: 15,
            model_implementor: None,
            model_reviewer: None,
            created_at: "2026-04-10T10:00:00Z".to_string(),
            updated_at: "2026-04-10T10:00:00Z".to_string(),
        }
    }

    #[test]
    fn header_no_active_loops() {
        let header = build_header(
            &[],
            &HashMap::new(),
            &PricingConfig::default(),
            false,
            "default",
        );
        assert!(header.contains("no active loops"));
    }

    #[test]
    fn header_team_view_empty() {
        let header = build_header(
            &[],
            &HashMap::new(),
            &PricingConfig::default(),
            true,
            "default",
        );
        assert!(header.contains("team view"));
    }

    #[test]
    fn header_team_view_labels_engineers() {
        let mut l1 = make_loop("IMPLEMENTING");
        l1.engineer = "alice".to_string();
        let mut l2 = make_loop("IMPLEMENTING");
        l2.engineer = "bob".to_string();
        let header = build_header(
            &[l1, l2],
            &HashMap::new(),
            &PricingConfig::default(),
            true,
            "default",
        );
        assert!(header.contains("team"));
        assert!(header.contains("alice:1"));
        assert!(header.contains("bob:1"));
    }

    #[test]
    fn header_with_active_loops() {
        let loops = vec![
            make_loop("IMPLEMENTING"),
            make_loop("IMPLEMENTING"),
            make_loop("REVIEWING"),
            make_loop("AWAITING_APPROVAL"),
        ];
        let header = build_header(
            &loops,
            &HashMap::new(),
            &PricingConfig::default(),
            false,
            "default",
        );
        assert!(header.contains("4 active"));
        assert!(header.contains("2 impl"));
        assert!(header.contains("1 review"));
        assert!(header.contains("1 awaiting"));
    }

    #[test]
    fn header_excludes_terminal() {
        let loops = vec![
            make_loop("IMPLEMENTING"),
            make_loop("CONVERGED"),
            make_loop("FAILED"),
        ];
        let header = build_header(
            &loops,
            &HashMap::new(),
            &PricingConfig::default(),
            false,
            "default",
        );
        assert!(header.contains("1 active"));
    }
}
