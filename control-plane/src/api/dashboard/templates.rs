use maud::{html, Markup};

use super::aggregate::{
    DashboardLoop, DashboardStateResponse, FeedResponse, SpecsResponse, StatsResponse,
};
use crate::config::NautiloopConfig;
use crate::types::verdict::{
    ImplResultData, ReviewResultData, ReviseResultData, TestResultData,
};
use crate::types::{LoopRecord, LoopState, RoundRecord};

// ── Helpers ──

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn layout(title: &str, viewer: &str, body_content: Markup, extra_attrs: &str) -> String {
    // Build the full HTML string manually since maud doesn't support dynamic attributes
    let inner = html! {
        (body_content)
        // Confirm modal
        div #confirm-modal class="modal-overlay" {
            div class="modal" {
                h3 #modal-title { "" }
                p #modal-body { "" }
                div class="modal-actions" {
                    button #modal-cancel class="btn" { "Cancel" }
                    button #modal-confirm class="btn btn-danger" { "Confirm" }
                }
            }
        }
        // Toast container
        div #toast-container class="toast-container" {}
        script src="/dashboard/static/dashboard.js" {}
    };

    format!(
        r#"<!DOCTYPE html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{title}</title><link rel="stylesheet" href="/dashboard/static/dashboard.css"></head><body data-viewer="{viewer}"{extra_attrs}>{inner}</body></html>"#,
        title = html_escape(title),
        viewer = html_escape(viewer),
        extra_attrs = extra_attrs,
        inner = inner.into_string(),
    )
}

fn nav_bar(active: &str, viewer: &str) -> Markup {
    html! {
        header class="dash-header" {
            h1 { a href="/dashboard" style="color:inherit;text-decoration:none" { "nautiloop" } }
            nav {
                a href="/dashboard" class=(if active == "grid" { "active" } else { "" }) { "Loops" }
                a href="/dashboard/feed" class=(if active == "feed" { "active" } else { "" }) { "Feed" }
                a href="/dashboard/stats" class=(if active == "stats" { "active" } else { "" }) { "Stats" }
            }
            div class="header-menu" {
                button #header-menu-toggle class="header-menu-btn" { "\u{22EF}" }
                div #header-menu-dropdown class="header-menu-dropdown" {
                    button #kill-switch-btn data-action="cancel-all" style="display:none" {
                        "Cancel all active loops"
                    }
                    button #bell-toggle style="color:var(--text)" {
                        "Bell: off"
                    }
                    form action="/dashboard/logout" method="post" style="margin:0" {
                        button type="submit" style="color:var(--text)" { "Logout (" (viewer) ")" }
                    }
                }
            }
        }
    }
}

fn badge_class(state: &str) -> &'static str {
    match state {
        "CONVERGED" | "HARDENED" | "SHIPPED" => "badge badge-green",
        "FAILED" | "CANCELLED" => "badge badge-red",
        "AWAITING_APPROVAL" | "PAUSED" | "AWAITING_REAUTH" => "badge badge-amber",
        "IMPLEMENTING" | "TESTING" | "REVIEWING" | "HARDENING" => "badge badge-blue",
        _ => "badge badge-gray",
    }
}

fn fmt_cost(cost: Option<f64>) -> String {
    match cost {
        Some(c) => format!("${:.2}", c),
        None => "\u{2014}".to_string(),
    }
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{}K", n / 1000)
    } else {
        n.to_string()
    }
}

fn fmt_rate(rate: Option<f64>) -> String {
    match rate {
        Some(r) => format!("{}%", (r * 100.0).round() as i32),
        None => "\u{2014}".to_string(),
    }
}

fn spec_filename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn short_id(id: &str) -> &str {
    if id.len() >= 8 {
        &id[..8]
    } else {
        id
    }
}

// ── Login Page ──

pub fn render_login(error: Option<&str>) -> String {
    let body = html! {
        div class="login-container" {
            h1 { "nautiloop" }
            @if let Some(err) = error {
                div class="login-error" { (err) }
            }
            form method="post" action="/dashboard/login" {
                label for="api_key" { "API Key" }
                input type="password" name="api_key" id="api_key" required placeholder="Enter API key" autocomplete="current-password";
                label for="engineer_name" { "Engineer Name" }
                input type="text" name="engineer_name" id="engineer_name" required placeholder="e.g. alice" autocomplete="username";
                button type="submit" class="btn btn-primary" { "Sign in" }
            }
        }
    };
    layout("nautiloop \u{00b7} login", "", body, "")
}

// ── Dashboard Card Grid ──

pub fn render_dashboard(data: &DashboardStateResponse, viewer: &str) -> String {
    let body = html! {
        (nav_bar("grid", viewer))

        // Fleet summary (FR-9)
        div class="fleet-summary" {
            span #fleet-summary-content {
                @if let Some(ref fs) = data.fleet_summary {
                    (render_fleet_summary_inline(fs))
                }
            }
        }

        // Filter chips (FR-3e) — state row
        div class="filter-bar" {
            button class="chip active" data-state-filter="active" {
                "Active (" span #chip-active-count { (count_active(&data.aggregates.counts_by_state)) } ")"
            }
            button class="chip" data-state-filter="converged" {
                "Converged (" span #chip-converged-count { (count_converged(&data.aggregates.counts_by_state)) } ")"
            }
            button class="chip" data-state-filter="failed" {
                "Failed (" span #chip-failed-count { (count_failed(&data.aggregates.counts_by_state)) } ")"
            }
            button class="chip" data-state-filter="all" {
                "All (" span #chip-all-count { (data.aggregates.total_loops) } ")"
            }
        }

        // Engineer row
        div class="filter-bar" #engineer-chips {
            button class="chip active" data-eng-filter="mine" { "Mine" }
            button class="chip" data-eng-filter="team" { "Team" }
            @for eng in &data.engineers {
                @if eng != viewer {
                    button class="chip" data-eng-individual=(eng) {
                        (eng)
                    }
                }
            }
        }

        // Card grid
        div #card-grid class="card-grid" {
            @for loop_item in &data.loops {
                @if !is_terminal_state(&loop_item.state) {
                    (render_card(loop_item, viewer))
                }
            }
            @if data.loops.iter().all(|l| is_terminal_state(&l.state)) {
                div class="empty-state" { "No active loops." }
            }
        }
    };
    layout("nautiloop", viewer, body, "")
}

fn render_card(l: &DashboardLoop, viewer: &str) -> Markup {
    let show_engineer = l.engineer != viewer;
    html! {
        a class="card" href=(format!("/dashboard/loops/{}", l.id)) data-id=(l.id) {
            @if show_engineer {
                span class="card-engineer" style=(format!("background:{}", engineer_color(&l.engineer))) {
                    (engineer_initials(&l.engineer))
                }
            }
            div class="card-header" {
                span class=(format!("pulse {}", pulse_class(l.sub_state.as_deref()))) {}
                span class=(badge_class(&l.state)) { (l.state) }
                span style="font-size:0.75rem;color:var(--text-muted)" { (short_id(&l.id)) }
                span style="margin-left:auto;font-size:0.75rem;color:var(--text-muted)" {
                    (fmt_elapsed(l.created_at))
                }
            }
            div class="card-title" { (spec_filename(&l.spec_path)) }
            div class="card-subtitle" { (l.branch) }
            div class="card-progress" {
                @if is_terminal_state(&l.state) {
                    "round " (l.round)
                } @else {
                    "round " (l.round) "/" (l.max_rounds) " \u{00b7} stage: " (l.current_stage.as_deref().unwrap_or("\u{2014}"))
                }
            }
            div class="card-metrics" {
                span { (fmt_tokens(l.total_tokens.input + l.total_tokens.output)) " tok" }
                span { (fmt_cost(l.total_cost)) }
                span { (l.last_verdict.as_deref().unwrap_or("\u{2014}")) }
            }
        }
    }
}

fn render_fleet_summary_inline(fs: &super::aggregate::FleetSummary) -> Markup {
    html! {
        "This week \u{00b7} "
        a href="/dashboard/stats#total-loops" class="fleet-link" {
            (fs.total_loops) " loops"
        }
        @if let Some(cost) = fs.total_cost {
            " \u{00b7} "
            a href="/dashboard/stats#total-cost" class="fleet-link" {
                (format!("${:.2}", cost))
            }
        }
        @if let Some(rate) = fs.converge_rate {
            " \u{00b7} "
            a href="/dashboard/stats#converge-rate" class="fleet-link" {
                (format!("{}%", (rate * 100.0).round() as i32))
                @if let Some(ref trends) = fs.trends {
                    @if let Some(delta) = trends.converge_rate_delta {
                        @let d = (delta * 100.0).round() as i32;
                        @if d > 0 {
                            " " span class="trend-up" { "\u{2191}" (d) "%" }
                        } @else if d < 0 {
                            " " span class="trend-down" { "\u{2193}" (d.unsigned_abs()) "%" }
                        }
                    }
                }
                " converged"
            }
        }
        @if let Some(avg) = fs.avg_rounds {
            " \u{00b7} "
            a href="/dashboard/stats#avg-rounds" class="fleet-link" {
                "avg " (format!("{:.1}", avg)) " rounds"
            }
        }
        @if let Some(ref ts) = fs.top_spender {
            " \u{00b7} "
            a href="/dashboard/stats#per-engineer" class="fleet-link" {
                "top: " (ts.engineer) " (" (format!("${:.2}", ts.cost)) ")"
            }
        }
    }
}

// ── Loop Detail Page ──

pub fn render_loop_detail(
    record: &LoopRecord,
    rounds: &[RoundRecord],
    log_lines: &[String],
    viewer: &str,
    config: &NautiloopConfig,
) -> String {
    let state_str = record.state.to_string();
    let is_terminal = record.state.is_terminal();
    let id_str = record.id.to_string();

    let body = html! {
        (nav_bar("grid", viewer))

        // Hero header
        div class="detail-hero" {
            h2 {
                span class=(badge_class(&state_str)) { (state_str) }
                " "
                a href=(format!("/dashboard/specs?path={}", urlencoding::encode(&record.spec_path))) {
                    (spec_filename(&record.spec_path))
                }
            }
            div class="meta" {
                span { (fmt_elapsed(record.created_at)) }
                span { "round " (record.round) "/" (record.max_rounds) }
                span { (record.branch) }
                @if let Some(ref pr_url) = record.spec_pr_url {
                    a href=(pr_url) target="_blank" { "PR" }
                }
            }
        }

        // Action buttons (FR-4a)
        div class="detail-actions" {
            @if record.state == LoopState::AwaitingApproval {
                button class="btn btn-primary" data-action="approve" data-loop-id=(id_str) { "Approve" }
            }
            @if !is_terminal {
                button class="btn btn-danger" data-action="cancel" data-loop-id=(id_str) { "Cancel" }
            }
            @if matches!(record.state, LoopState::Paused | LoopState::AwaitingReauth) || (record.state == LoopState::Failed && record.failed_from_state.is_some()) {
                button class="btn" data-action="resume" data-loop-id=(id_str) { "Resume" }
            }
            @if record.state == LoopState::Failed && record.failed_from_state.is_some() {
                button class="btn" data-action="extend" data-loop-id=(id_str) { "Extend +10" }
            }
            @if let Some(ref pr_url) = record.spec_pr_url {
                a class="btn" href=(pr_url) target="_blank" { "Open PR" }
            }
        }

        div class="detail-content" {
            div class="detail-split" {
                // Left: rounds table
                div {
                    h3 style="font-size:0.85rem;margin-bottom:0.5rem;color:var(--text-muted)" { "Rounds" }
                    div class="table-wrap" {
                        (render_rounds_table(rounds, record, config))
                    }

                    // Token/cost breakdown
                    h3 style="font-size:0.85rem;margin:0.75rem 0 0.5rem;color:var(--text-muted)" { "Token Breakdown" }
                    (render_token_breakdown(rounds, record, config))
                }

                // Right: log pane
                div {
                    h3 style="font-size:0.85rem;margin-bottom:0.5rem;color:var(--text-muted)" { "Logs" }
                    div #log-pane class="log-pane" {
                        @for line in log_lines {
                            span class="log-line" { (line) "\n" }
                        }
                    }

                    // Pod introspect (FR-5)
                    @if !is_terminal {
                        details #pod-introspect class="pod-introspect" {
                            summary { "Inspect pod" }
                            div #introspect-data class="pod-introspect-data" { "Loading..." }
                        }
                    }
                }
            }
        }
    };

    let extra = format!(
        " data-loop-id=\"{}\" data-loop-terminal=\"{}\"",
        id_str, is_terminal
    );
    layout(
        &format!("nautiloop \u{00b7} {}", spec_filename(&record.spec_path)),
        viewer,
        body,
        &extra,
    )
}

fn render_rounds_table(
    rounds: &[RoundRecord],
    record: &LoopRecord,
    config: &NautiloopConfig,
) -> Markup {
    // Group by round number
    let mut round_groups: std::collections::BTreeMap<i32, Vec<&RoundRecord>> =
        std::collections::BTreeMap::new();
    for r in rounds {
        round_groups.entry(r.round).or_default().push(r);
    }

    html! {
        table class="rounds-table" {
            thead {
                tr {
                    th { "#" }
                    th { "Stage" }
                    th { "Verdict" }
                    th { "Issues" }
                    th { "Confidence" }
                    th { "Tokens" }
                    th { "Cost" }
                    th { "Duration" }
                }
            }
            tbody {
                @for (_round_num, stages) in &round_groups {
                    @for stage_round in stages {
                        (render_round_row(stage_round, record, config))
                    }
                }
            }
        }
    }
}

fn render_round_row(
    r: &RoundRecord,
    record: &LoopRecord,
    config: &NautiloopConfig,
) -> Markup {
    let verdict = extract_round_verdict(r);
    let tokens = extract_round_tokens(r);
    let cost = compute_round_cost(r, record, config);
    let duration = r
        .duration_secs
        .map(|d| format!("{}s", d))
        .unwrap_or_else(|| "\u{2014}".to_string());
    let token_str = tokens
        .as_ref()
        .map(|t| format!("{}+{}", fmt_tokens(t.input), fmt_tokens(t.output)))
        .unwrap_or_else(|| "\u{2014}".to_string());
    let has_judge = check_judge_icon(r);
    let (issues_count, confidence) = extract_review_metrics(r);

    let detail_html = render_round_detail(r);

    html! {
        tr class="round-row expandable" {
            td { (r.round) }
            td { (r.stage) }
            td {
                (verdict)
                @if has_judge {
                    " " span class="judge-icon" title="Judge decision" { "\u{2696}" }
                }
            }
            td { (issues_count) }
            td { (confidence) }
            td { (token_str) }
            td { (fmt_cost(cost)) }
            td { (duration) }
        }
        tr class="round-detail" {
            td colspan="8" {
                (detail_html)
            }
        }
    }
}

/// Extract issues count and confidence score from review/audit round output.
/// Returns ("—", "—") for non-review stages.
fn extract_review_metrics(r: &RoundRecord) -> (String, String) {
    let dash = "\u{2014}".to_string();
    let Some(ref output) = r.output else {
        return (dash.clone(), dash);
    };
    match r.stage.as_str() {
        "review" | "audit" => {
            if let Ok(rd) = serde_json::from_value::<ReviewResultData>(output.clone()) {
                let issues = rd
                    .verdict
                    .get("issues")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len().to_string())
                    .unwrap_or_else(|| dash.clone());
                let conf = rd
                    .verdict
                    .get("confidence")
                    .and_then(|v| v.as_f64())
                    .map(|c| format!("{:.0}%", c * 100.0))
                    .unwrap_or_else(|| dash.clone());
                (issues, conf)
            } else {
                (dash.clone(), dash)
            }
        }
        _ => (dash.clone(), dash),
    }
}

fn extract_round_verdict(r: &RoundRecord) -> String {
    let Some(ref output) = r.output else {
        return "\u{2014}".to_string();
    };
    match r.stage.as_str() {
        "review" | "audit" => {
            if let Ok(rd) = serde_json::from_value::<ReviewResultData>(output.clone())
                && let Some(clean) = rd.verdict.get("clean").and_then(|v| v.as_bool()) {
                    return if clean {
                        "clean".to_string()
                    } else {
                        "not clean".to_string()
                    };
                }
            "\u{2014}".to_string()
        }
        "test" => {
            if let Ok(td) = serde_json::from_value::<TestResultData>(output.clone()) {
                return if td.all_passed {
                    "passed".to_string()
                } else {
                    "failed".to_string()
                };
            }
            "\u{2014}".to_string()
        }
        _ => "\u{2014}".to_string(),
    }
}

fn extract_round_tokens(r: &RoundRecord) -> Option<crate::types::verdict::TokenUsage> {
    let output = r.output.as_ref()?;
    match r.stage.as_str() {
        "implement" => serde_json::from_value::<ImplResultData>(output.clone())
            .ok()
            .map(|d| d.token_usage),
        "test" => serde_json::from_value::<TestResultData>(output.clone())
            .ok()
            .map(|d| d.token_usage),
        "review" | "audit" => serde_json::from_value::<ReviewResultData>(output.clone())
            .ok()
            .map(|d| d.token_usage),
        "revise" => serde_json::from_value::<ReviseResultData>(output.clone())
            .ok()
            .map(|d| d.token_usage),
        _ => None,
    }
}

fn compute_round_cost(
    r: &RoundRecord,
    record: &LoopRecord,
    config: &NautiloopConfig,
) -> Option<f64> {
    let tu = extract_round_tokens(r)?;
    let pricing = config.pricing.as_ref()?;
    let model = match r.stage.as_str() {
        "implement" | "revise" | "test" => record
            .model_implementor
            .clone()
            .unwrap_or_else(|| config.models.implementor.clone()),
        "review" | "audit" => record
            .model_reviewer
            .clone()
            .unwrap_or_else(|| config.models.reviewer.clone()),
        _ => return None,
    };
    super::aggregate::compute_cost(&tu, &model, pricing)
}

fn check_judge_icon(_r: &RoundRecord) -> bool {
    // FR-11b: Only render if judge_decisions exist (post-#128).
    // For now, return false since the judge feature is not yet shipped.
    false
}

fn render_round_detail(r: &RoundRecord) -> Markup {
    let Some(ref output) = r.output else {
        return html! { "\u{2014}" };
    };

    match r.stage.as_str() {
        "implement" => {
            if let Ok(d) = serde_json::from_value::<ImplResultData>(output.clone()) {
                return html! {
                    "SHA: " (d.new_sha) " | exit: " (d.exit_code)
                };
            }
        }
        "revise" => {
            if let Ok(d) = serde_json::from_value::<ReviseResultData>(output.clone()) {
                return html! {
                    "SHA: " (d.new_sha) " | spec: " (d.revised_spec_path) " | exit: " (d.exit_code)
                };
            }
        }
        "test" => {
            if let Ok(d) = serde_json::from_value::<TestResultData>(output.clone()) {
                let status = if d.all_passed { "passed" } else { "failed" };
                return html! {
                    "Status: " (status) " | services: " (d.services.len())
                    @for svc in &d.services {
                        br;
                        "  " (svc.name) ": exit " (svc.exit_code)
                    }
                };
            }
        }
        "review" | "audit" => {
            if let Ok(d) = serde_json::from_value::<ReviewResultData>(output.clone()) {
                let clean = d.verdict.get("clean").and_then(|v| v.as_bool());
                let summary = d
                    .verdict
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let issues_count = d
                    .verdict
                    .get("issues")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                let confidence = d
                    .verdict
                    .get("confidence")
                    .and_then(|v| v.as_f64());

                return html! {
                    @if let Some(c) = clean {
                        "Verdict: " (if c { "clean" } else { "not clean" })
                    }
                    @if let Some(conf) = confidence {
                        " | confidence: " (format!("{:.0}%", conf * 100.0))
                    }
                    " | issues: " (issues_count)
                    @if !summary.is_empty() {
                        br;
                        (summary)
                    }
                };
            }
        }
        _ => {}
    }
    html! { pre { (serde_json::to_string_pretty(output).unwrap_or_default()) } }
}

fn render_token_breakdown(
    rounds: &[RoundRecord],
    record: &LoopRecord,
    config: &NautiloopConfig,
) -> Markup {
    let mut max_total = 0u64;
    let mut rows: Vec<(i32, &str, u64, u64, Option<f64>)> = Vec::new();

    for r in rounds {
        if let Some(tu) = extract_round_tokens(r) {
            let total = tu.input + tu.output;
            if total > max_total {
                max_total = total;
            }
            let cost = compute_round_cost(r, record, config);
            rows.push((r.round, &r.stage, tu.input, tu.output, cost));
        }
    }

    if rows.is_empty() {
        return html! { div class="empty-state" { "No token data available." } };
    }

    html! {
        table class="token-table" {
            thead {
                tr {
                    th { "Round" }
                    th { "Stage" }
                    th { "Input" }
                    th { "Output" }
                    th { "Cost" }
                    th class="bar-cell" { "" }
                }
            }
            tbody {
                @for (round, stage, input, output, cost) in &rows {
                    @let total = input + output;
                    @let pct = if max_total > 0 { (total as f64 / max_total as f64 * 100.0) as u32 } else { 0 };
                    tr {
                        td { (round) }
                        td { (stage) }
                        td { (fmt_tokens(*input)) }
                        td { (fmt_tokens(*output)) }
                        td { (fmt_cost(*cost)) }
                        td class="bar-cell" {
                            div class="bar" style=(format!("width:{}%", pct)) {}
                        }
                    }
                }
            }
        }
    }
}

// ── Feed Page ──

pub fn render_feed(
    data: &FeedResponse,
    viewer: &str,
    state_filter: Option<&str>,
    engineer_filter: Option<&str>,
) -> String {
    let has_filter = state_filter.is_some() || engineer_filter.is_some();
    let body = html! {
        (nav_bar("feed", viewer))

        div class="filter-bar" {
            button class=(if !has_filter { "chip active" } else { "chip" })
                data-feed-filter="" data-feed-filter-type="clear" { "All events" }
            button class=(if state_filter == Some("converged") { "chip active" } else { "chip" })
                data-feed-filter="converged" data-feed-filter-type="state" { "Converged" }
            button class=(if state_filter == Some("failed") { "chip active" } else { "chip" })
                data-feed-filter="failed" data-feed-filter-type="state" { "Failed" }
            @for eng in &data.engineers {
                button class=(if engineer_filter == Some(eng.as_str()) { "chip active" } else { "chip" })
                    data-feed-filter=(eng) data-feed-filter-type="engineer" { (eng) }
            }
        }

        div #feed-list class="feed-list" {
            @if data.events.is_empty() {
                div class="empty-state" {
                    @if has_filter {
                        "No events match this filter. "
                        a href="/dashboard/feed" { "Clear filter" }
                    } @else {
                        "No terminal events yet."
                    }
                }
            }
            @for ev in &data.events {
                (render_feed_item(ev))
            }
        }

        @if data.has_more {
            div class="load-more" {
                button #feed-load-more class="btn"
                    data-cursor=(data.events.last().map(|e| format!("{}|{}", e.updated_at.to_rfc3339(), e.id)).unwrap_or_default())
                    data-state-filter=(state_filter.unwrap_or(""))
                    data-engineer-filter=(engineer_filter.unwrap_or("")) {
                    "Load more"
                }
            }
        }
    };
    layout("nautiloop \u{00b7} feed", viewer, body, "")
}

fn render_feed_item(ev: &super::aggregate::FeedEvent) -> Markup {
    let time = ev.updated_at.format("%H:%M").to_string();
    let ext = if ev.extensions > 0 {
        format!(" [extended \u{00d7}{}]", ev.extensions)
    } else {
        String::new()
    };

    html! {
        a class="feed-item" href=(format!("/dashboard/loops/{}", ev.id)) {
            span class="feed-time" { (time) }
            span { (ev.engineer) }
            span class="feed-spec" { (spec_filename(&ev.spec_path)) }
            span class=(badge_class(&ev.state)) { (ev.state) }
            @if ev.spec_pr_url.is_some() {
                span class="feed-detail" { "PR" }
            }
            span class="feed-detail" { (ev.rounds) " rounds" }
            span class="feed-detail" { (fmt_cost(ev.total_cost)) }
            @if !ext.is_empty() {
                span class="feed-detail" { (ext) }
            }
        }
    }
}

// ── Specs Page ──

pub fn render_specs(data: &SpecsResponse, viewer: &str) -> String {
    let body = html! {
        (nav_bar("grid", viewer))

        div class="spec-header" {
            h2 { (spec_filename(&data.spec_path)) }
            div class="spec-aggregates" {
                span { (data.aggregates.total_runs) " runs" }
                span { "\u{00b7} " (fmt_rate(data.aggregates.converge_rate)) " converge rate" }
                @if let Some(avg) = data.aggregates.avg_rounds {
                    span { "\u{00b7} avg " (format!("{:.1}", avg)) " rounds" }
                }
                span { "\u{00b7} total cost " (fmt_cost(data.aggregates.total_cost)) }
            }
        }

        div class="section" {
            div class="table-wrap" {
                table class="data-table" {
                    thead {
                        tr {
                            th { "Date" }
                            th { "Engineer" }
                            th { "Result" }
                            th { "Rounds" }
                            th { "Cost" }
                            th { "Branch" }
                        }
                    }
                    tbody {
                        @for run in &data.runs {
                            tr {
                                td { (run.created_at.format("%Y-%m-%d %H:%M")) }
                                td { (run.engineer) }
                                td { span class=(badge_class(&run.state)) { (run.state) } }
                                td { (run.rounds) }
                                td { (fmt_cost(run.total_cost)) }
                                td style="font-size:0.7rem;word-break:break-all" { (run.branch) }
                            }
                        }
                    }
                }
            }
        }
    };
    layout(
        &format!("nautiloop \u{00b7} {}", spec_filename(&data.spec_path)),
        viewer,
        body,
        "",
    )
}

pub fn render_specs_empty(viewer: &str) -> String {
    let body = html! {
        (nav_bar("grid", viewer))
        div class="empty-state" { "No spec path specified." }
    };
    layout("nautiloop \u{00b7} specs", viewer, body, "")
}

// ── Stats Page ──

pub fn render_stats(data: &StatsResponse, viewer: &str) -> String {
    let body = html! {
        (nav_bar("stats", viewer))

        // Window toggle
        div class="filter-bar" {
            @for w in &["24h", "7d", "30d"] {
                button class=(if data.window == *w { "chip active" } else { "chip" }) data-window=(w) { (w) }
            }
        }

        // Headline cards
        div class="stats-cards" {
            div class="stat-card" id="total-loops" {
                div class="label" { "Total Loops" }
                div class="value" { (data.headline.total_loops) }
            }
            div class="stat-card" id="total-cost" {
                div class="label" { "Total Cost" }
                div class="value" { (fmt_cost(data.headline.total_cost)) }
            }
            div class="stat-card" id="converge-rate" {
                div class="label" { "Converge Rate" }
                div class="value" { (fmt_rate(data.headline.converge_rate)) }
            }
            div class="stat-card" id="avg-rounds" {
                div class="label" { "Avg Rounds" }
                div class="value" {
                    @if let Some(avg) = data.headline.avg_rounds {
                        (format!("{:.1}", avg))
                    } @else {
                        "\u{2014}"
                    }
                }
            }
        }

        // Per-engineer table
        div class="stats-section" {
            h3 id="per-engineer" { "Per Engineer" }
            div class="table-wrap" {
                table class="data-table" {
                    thead {
                        tr {
                            th { "Engineer" }
                            th { "Loops" }
                            th { "Cost" }
                            th { "Converge Rate" }
                        }
                    }
                    tbody {
                        @for eng in &data.per_engineer {
                            tr {
                                td { (eng.engineer) }
                                td { (eng.loops) }
                                td { (fmt_cost(eng.cost)) }
                                td { (fmt_rate(eng.converge_rate)) }
                            }
                        }
                    }
                }
            }
        }

        // Per-spec table
        div class="stats-section" {
            h3 id="per-spec" { "Top Specs" }
            div class="table-wrap" {
                table class="data-table" {
                    thead {
                        tr {
                            th { "Spec" }
                            th { "Runs" }
                            th { "Cost" }
                            th { "Converge Rate" }
                            th { "Avg Rounds" }
                        }
                    }
                    tbody {
                        @for spec in &data.per_spec {
                            tr {
                                td { (spec_filename(&spec.spec_path)) }
                                td { (spec.runs) }
                                td { (fmt_cost(spec.cost)) }
                                td { (fmt_rate(spec.converge_rate)) }
                                td {
                                    @if let Some(avg) = spec.avg_rounds {
                                        (format!("{:.1}", avg))
                                    } @else {
                                        "\u{2014}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Time series
        div class="stats-section" {
            h3 id="time-series" { "Daily Activity" }
            @for day in &data.time_series {
                @let max_val = day.started.max(1) as f64;
                div class="ts-row" {
                    span class="ts-label" { (day.date) }
                    div class="ts-bars" {
                        div class="ts-bar ts-bar-converged"
                            style=(format!("width:{:.1}%", day.converged as f64 * 100.0 / max_val))
                            title=(format!("{} converged", day.converged)) {}
                        div class="ts-bar ts-bar-failed"
                            style=(format!("width:{:.1}%", day.failed as f64 * 100.0 / max_val))
                            title=(format!("{} failed", day.failed)) {}
                        @let other = day.started.saturating_sub(day.converged.saturating_add(day.failed));
                        @if other > 0 {
                            div class="ts-bar ts-bar-started"
                                style=(format!("width:{:.1}%", other as f64 * 100.0 / max_val))
                                title=(format!("{} other", other)) {}
                        }
                    }
                    span class="ts-count" { (day.started) }
                }
            }
        }
    };
    layout("nautiloop \u{00b7} stats", viewer, body, "")
}

// ── Utility Helpers ──

fn is_terminal_state(state: &str) -> bool {
    matches!(
        state,
        "CONVERGED" | "FAILED" | "CANCELLED" | "HARDENED" | "SHIPPED"
    )
}

fn pulse_class(sub_state: Option<&str>) -> &'static str {
    match sub_state {
        Some("RUNNING") => "pulse-running",
        Some("DISPATCHED") => "pulse-dispatched",
        _ => "pulse-completed",
    }
}

fn engineer_color(name: &str) -> String {
    let mut h: i32 = 0;
    for b in name.bytes() {
        h = ((h << 5).wrapping_sub(h)).wrapping_add(b as i32);
    }
    let hue = h.rem_euclid(360);
    format!("hsl({},55%,45%)", hue)
}

fn engineer_initials(name: &str) -> String {
    name.chars().take(2).collect::<String>().to_uppercase()
}

fn fmt_elapsed(created: chrono::DateTime<chrono::Utc>) -> String {
    let elapsed = chrono::Utc::now() - created;
    let secs = elapsed.num_seconds();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn count_active(counts: &std::collections::HashMap<String, u64>) -> u64 {
    counts
        .iter()
        .filter(|(k, _)| !is_terminal_state(k))
        .map(|(_, v)| v)
        .sum()
}

fn count_converged(counts: &std::collections::HashMap<String, u64>) -> u64 {
    counts.get("CONVERGED").copied().unwrap_or(0)
        + counts.get("HARDENED").copied().unwrap_or(0)
        + counts.get("SHIPPED").copied().unwrap_or(0)
}

fn count_failed(counts: &std::collections::HashMap<String, u64>) -> u64 {
    counts.get("FAILED").copied().unwrap_or(0)
        + counts.get("CANCELLED").copied().unwrap_or(0)
}

// URL encoding helper
mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut result = String::new();
        for b in s.bytes() {
            match b {
                b'A'..=b'Z'
                | b'a'..=b'z'
                | b'0'..=b'9'
                | b'-'
                | b'_'
                | b'.'
                | b'~' => result.push(b as char),
                _ => {
                    result.push('%');
                    result.push_str(&format!("{:02X}", b));
                }
            }
        }
        result
    }
}
