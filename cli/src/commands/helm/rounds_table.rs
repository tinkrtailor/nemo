use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::api_types::{InspectResponse, RoundSummary};
use super::cost::{self, PricingConfig, calculate_loop_round_cost, format_cost, format_tokens, round_total_tokens, round_duration_secs};
use super::is_terminal_state;
use super::themes::Theme;

/// Configuration for the rounds table renderer.
pub struct RoundsTableConfig<'a> {
    pub inspect: Option<&'a InspectResponse>,
    pub inspect_status: &'a str,
    pub selected_row: usize,
    pub scroll: usize,
    pub is_harden: bool,
    pub current_round: i32,
    pub current_stage: Option<&'a str>,
    pub pricing: &'a PricingConfig,
    pub model_implementor: Option<&'a str>,
    pub model_reviewer: Option<&'a str>,
    pub area: Rect,
    pub theme: &'a Theme,
}

/// Render the rounds table (FR-9).
pub fn render_table(cfg: &RoundsTableConfig<'_>) -> Paragraph<'static> {
    let RoundsTableConfig {
        inspect,
        inspect_status,
        selected_row,
        scroll,
        is_harden,
        current_round,
        current_stage,
        pricing,
        model_implementor,
        model_reviewer,
        area,
        theme,
    } = cfg;
    let Some(inspect) = inspect else {
        return placeholder(inspect_status, theme, *area);
    };

    if inspect.rounds.is_empty() {
        return placeholder("No rounds completed yet", theme, *area);
    }

    let mut lines = Vec::new();

    // Header row
    let header = format!(
        " {:<3} {:<12} {:<10} {:<12} {:<5} {:<7} {:<7} {:<8}",
        "#", "Stages", "Verdict", "Issues", "Conf", "Tokens", "$", "Duration"
    );
    lines.push(Line::from(Span::styled(
        header,
        Style::default().fg(theme.muted).add_modifier(Modifier::BOLD),
    )));

    let inner_height = area.height.saturating_sub(3) as usize; // border + header

    for (idx, round) in inspect.rounds.iter().enumerate() {
        if idx < *scroll {
            continue;
        }
        if lines.len() > inner_height {
            break;
        }

        let is_selected = idx == *selected_row;
        let is_current = round.round == *current_round && !is_terminal_state(&inspect.state);

        // Stages column (FR-9b, FR-9e, FR-9f)
        let stages = build_stages_column(round, *is_harden, is_current, *current_stage);

        // Verdict column
        let (verdict_text, verdict_color) = extract_verdict(round, *is_harden, theme);

        // Issues column
        let issues_text = extract_issues(round, *is_harden);

        // Confidence column
        let conf_text = extract_confidence(round, *is_harden);

        // Tokens column
        let (inp, out) = round_total_tokens(round);
        let total_tokens = inp + out;
        let tokens_text = format_tokens(total_tokens);

        // Cost column (split by stage: implementor vs reviewer pricing)
        let cost = calculate_loop_round_cost(pricing, *model_implementor, *model_reviewer, round);
        let cost_text = format_cost(cost);

        // Duration column
        let duration = round_duration_secs(round);
        let duration_text = if duration > 0 {
            cost::format_duration_secs(duration)
        } else {
            "-".to_string()
        };

        let row_text = format!(
            " {:<3} {:<12} {:<10} {:<12} {:<5} {:<7} {:<7} {:<8}",
            round.round, stages, verdict_text, issues_text, conf_text, tokens_text, cost_text, duration_text
        );

        let bg = if is_selected {
            theme.border // distinct from surface for visible highlight
        } else {
            theme.bg
        };

        // Keep verdict color visible for selected rows too
        let fg = verdict_color;

        let mut style = Style::default().fg(fg).bg(bg);
        if is_selected {
            style = style.add_modifier(Modifier::BOLD);
        }

        lines.push(Line::from(Span::styled(row_text, style)));
    }

    Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(Span::styled(
                    " rounds [Enter=detail  Esc/R=back] ",
                    Style::default()
                        .fg(theme.text)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border).bg(theme.surface))
                .style(Style::default().bg(theme.bg)),
        )
        .style(Style::default().fg(theme.text).bg(theme.bg))
}

/// Configuration for the round detail renderer.
pub struct RoundDetailConfig<'a> {
    pub round: &'a RoundSummary,
    pub is_harden: bool,
    pub pricing: &'a PricingConfig,
    pub model_implementor: Option<&'a str>,
    pub model_reviewer: Option<&'a str>,
    pub scroll: usize,
    pub area: Rect,
    pub theme: &'a Theme,
}

/// Render round detail view (FR-9d).
pub fn render_detail(cfg: &RoundDetailConfig<'_>) -> Paragraph<'static> {
    let RoundDetailConfig {
        round,
        is_harden,
        pricing,
        model_implementor,
        model_reviewer,
        scroll,
        area,
        theme,
    } = cfg;
    let mut lines = Vec::new();

    lines.push(Line::from(Span::styled(
        format!("Round {} Detail", round.round),
        Style::default().fg(theme.teal).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled("", Style::default())));

    // Summary info
    let (inp, out) = round_total_tokens(round);
    let cost = calculate_loop_round_cost(pricing, *model_implementor, *model_reviewer, round);
    let duration = round_duration_secs(round);

    lines.push(Line::from(vec![
        Span::styled("Tokens: ", Style::default().fg(theme.muted)),
        Span::styled(format_tokens(inp + out), Style::default().fg(theme.text)),
        Span::styled("  Cost: ", Style::default().fg(theme.muted)),
        Span::styled(format_cost(cost), Style::default().fg(theme.text)),
        Span::styled("  Duration: ", Style::default().fg(theme.muted)),
        Span::styled(
            if duration > 0 { cost::format_duration_secs(duration) } else { "-".to_string() },
            Style::default().fg(theme.text),
        ),
    ]));
    lines.push(Line::from(Span::styled("", Style::default())));

    // Verdict summary
    let verdict_source = if *is_harden { &round.audit } else { &round.review };
    if let Some(data) = verdict_source {
        let verdict = data.get("verdict").unwrap_or(data);
        if let Some(summary) = verdict.get("summary").and_then(|v| v.as_str()) {
            lines.push(Line::from(Span::styled(
                "Verdict Summary",
                Style::default().fg(theme.muted).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                summary.to_string(),
                Style::default().fg(theme.text),
            )));
            lines.push(Line::from(Span::styled("", Style::default())));
        }

        // Issues list
        if let Some(issues) = verdict.get("issues").and_then(|v| v.as_array()).filter(|v| !v.is_empty()) {
            lines.push(Line::from(Span::styled(
                format!("Issues ({})", issues.len()),
                Style::default().fg(theme.amber).add_modifier(Modifier::BOLD),
            )));

            for issue in issues {
                let severity = issue.get("severity").and_then(|v| v.as_str()).unwrap_or("?");
                let category = issue.get("category").and_then(|v| v.as_str()).unwrap_or("");
                let file = issue.get("file").and_then(|v| v.as_str()).unwrap_or("");
                let line_num = issue.get("line").and_then(|v| v.as_u64());
                let desc = issue.get("description").and_then(|v| v.as_str()).unwrap_or("");

                let location = if !file.is_empty() {
                    match line_num {
                        Some(n) => format!("{file}:{n}"),
                        None => file.to_string(),
                    }
                } else {
                    String::new()
                };

                let severity_color = match severity {
                    "critical" => theme.red,
                    "high" => theme.red,
                    "medium" => theme.amber,
                    "low" => theme.muted,
                    _ => theme.text,
                };

                let mut parts = vec![
                    Span::styled(format!("  {severity:<8}"), Style::default().fg(severity_color)),
                ];
                if !category.is_empty() {
                    parts.push(Span::styled(
                        format!(" {category:<12}"),
                        Style::default().fg(theme.muted),
                    ));
                }
                if !location.is_empty() {
                    parts.push(Span::styled(
                        format!(" {location}"),
                        Style::default().fg(theme.blue),
                    ));
                }
                lines.push(Line::from(parts));
                if !desc.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("           {desc}"),
                        Style::default().fg(theme.text),
                    )));
                }
            }
        }
    }

    let inner_height = area.height.saturating_sub(2) as usize;
    let effective_scroll = (*scroll).min(lines.len().saturating_sub(inner_height));

    Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(Span::styled(
                    format!(" round {} detail [Esc=back] ", round.round),
                    Style::default()
                        .fg(theme.text)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border).bg(theme.surface))
                .style(Style::default().bg(theme.bg)),
        )
        .style(Style::default().fg(theme.text).bg(theme.bg))
        .wrap(Wrap { trim: false })
        .scroll((effective_scroll as u16, 0))
}

/// Build the compact stages column (FR-9b, FR-9e, FR-9f).
fn build_stages_column(round: &RoundSummary, is_harden: bool, is_current: bool, current_stage: Option<&str>) -> String {
    if is_harden {
        let audit_marker = stage_marker(&round.audit, "A", is_current && current_stage == Some("audit"));
        let revise_marker = stage_marker(&round.revise, "V", is_current && current_stage == Some("revise"));
        format!("{audit_marker}{revise_marker}")
    } else {
        let impl_marker = stage_marker(&round.implement, "I", is_current && current_stage == Some("implement"));
        let test_marker = stage_marker(&round.test, "T", is_current && current_stage == Some("test"));
        let review_marker = stage_marker(&round.review, "R", is_current && current_stage == Some("review"));
        format!("{impl_marker}{test_marker}{review_marker}")
    }
}

fn stage_marker(data: &Option<serde_json::Value>, label: &str, is_running: bool) -> String {
    if is_running {
        return format!("~{label} ");
    }
    match data {
        None => format!(" {label} "),
        Some(v) => {
            // Check if the stage succeeded
            let succeeded = stage_succeeded(v);
            if succeeded {
                format!("✓{label} ")
            } else {
                format!("✗{label} ")
            }
        }
    }
}

fn stage_succeeded(value: &serde_json::Value) -> bool {
    // Implement: exit_code == 0
    if value.get("exit_code").and_then(|v| v.as_i64()).is_some_and(|c| c != 0) {
        return false;
    }
    // Test: all_passed
    if let Some(all_passed) = value.get("all_passed").and_then(|v| v.as_bool()) {
        return all_passed;
    }
    // Review/Audit: clean
    let verdict = value.get("verdict").unwrap_or(value);
    if let Some(clean) = verdict.get("clean").and_then(|v| v.as_bool()) {
        return clean;
    }
    true
}

fn extract_verdict(round: &RoundSummary, is_harden: bool, theme: &Theme) -> (String, ratatui::style::Color) {
    let source = if is_harden { &round.audit } else { &round.review };
    match source {
        None => ("".to_string(), theme.text),
        Some(data) => {
            let verdict = data.get("verdict").unwrap_or(data);
            match verdict.get("clean").and_then(|v| v.as_bool()) {
                Some(true) => ("clean".to_string(), theme.green),
                Some(false) => ("not clean".to_string(), theme.amber),
                None => ("".to_string(), theme.text),
            }
        }
    }
}

fn extract_issues(round: &RoundSummary, is_harden: bool) -> String {
    let source = if is_harden { &round.audit } else { &round.review };
    let Some(data) = source else { return String::new() };
    let verdict = data.get("verdict").unwrap_or(data);
    let issues = match verdict.get("issues").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return "0".to_string(),
    };

    if issues.is_empty() {
        return "0".to_string();
    }

    let mut critical = 0;
    let mut high = 0;
    let mut medium = 0;
    let mut low = 0;
    for issue in issues {
        match issue.get("severity").and_then(|v| v.as_str()) {
            Some("critical") => critical += 1,
            Some("high") => high += 1,
            Some("medium") => medium += 1,
            Some("low") => low += 1,
            _ => low += 1,
        }
    }

    let total = issues.len();
    let mut parts = Vec::new();
    if critical > 0 { parts.push(format!("{critical}c")); }
    if high > 0 { parts.push(format!("{high}h")); }
    if medium > 0 { parts.push(format!("{medium}m")); }
    if low > 0 { parts.push(format!("{low}l")); }

    format!("{total} ({})", parts.join(" "))
}

fn extract_confidence(round: &RoundSummary, is_harden: bool) -> String {
    let source = if is_harden { &round.audit } else { &round.review };
    let Some(data) = source else { return String::new() };
    let verdict = data.get("verdict").unwrap_or(data);
    match verdict.get("confidence").and_then(|v| v.as_f64()) {
        Some(c) => format!("{:.2}", c),
        None => String::new(),
    }
}

fn placeholder(msg: &str, theme: &Theme, _area: Rect) -> Paragraph<'static> {
    Paragraph::new(Text::from(vec![Line::from(Span::styled(
        msg.to_string(),
        Style::default().fg(theme.muted),
    ))]))
    .block(
        Block::default()
            .title(Span::styled(
                " rounds ",
                Style::default()
                    .fg(theme.text)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border).bg(theme.surface))
            .style(Style::default().bg(theme.bg)),
    )
    .style(Style::default().fg(theme.text).bg(theme.bg))
}
