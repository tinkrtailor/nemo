use chrono::{DateTime, Utc};
use maud::{DOCTYPE, Markup, html};

use crate::types::{JudgeDecisionRecord, LoopRecord, LoopState};

/// Render the base layout shell.
/// CSS and JS are loaded via external links (cached by browser) rather than inlined.
fn layout(title: &str, nav_active: &str, show_team: bool, csrf_token: &str, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                link rel="stylesheet" href="/dashboard/static/dashboard.css";
            }
            body {
                (render_header(nav_active, show_team, csrf_token))
                main { (content) }
                div #status-bar class="status-bar" {}
                script src="/dashboard/static/dashboard.js" defer {}
            }
        }
    }
}

fn render_header(active: &str, show_team: bool, csrf_token: &str) -> Markup {
    html! {
        header class="header" {
            a href="/dashboard" class="header-brand" { "NAUTILOOP" }
            nav class="header-nav" {
                a href="/dashboard" class=(if active == "grid" { "active" } else { "" }) { "Loops" }
                a href="/dashboard/feed" class=(if active == "feed" { "active" } else { "" }) { "Feed" }
                a href="/dashboard/stats" class=(if active == "stats" { "active" } else { "" }) { "Stats" }
                div style="position:relative" {
                        button #menu-toggle class="menu-btn" { "\u{22EF}" }
                        div #menu-dropdown class="menu-dropdown hidden" {
                            button #cancel-all-btn data-action="cancel-all" class=(if show_team { "danger" } else { "danger hidden" }) { "Cancel all active loops" }
                            form action="/dashboard/logout" method="post" {
                                input type="hidden" name="csrf_token" value=(csrf_token);
                                button type="submit" { "Logout" }
                            }
                        }
                    }
            }
        }
    }
}

/// Format a duration from a DateTime to now into a human-readable string.
pub fn format_elapsed(dt: DateTime<Utc>) -> String {
    let secs = (Utc::now() - dt).num_seconds().max(0);
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Format a token count into a human-readable string (e.g., "52K").
pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}

/// CSS class for a state badge.
fn badge_class(state: LoopState) -> &'static str {
    match state {
        LoopState::Implementing => "badge badge-implementing",
        LoopState::Hardening => "badge badge-hardening",
        LoopState::Testing => "badge badge-testing",
        LoopState::Reviewing => "badge badge-reviewing",
        LoopState::Converged => "badge badge-converged",
        LoopState::Hardened => "badge badge-hardened",
        LoopState::Shipped => "badge badge-shipped",
        LoopState::Failed => "badge badge-failed",
        LoopState::Cancelled => "badge badge-cancelled",
        LoopState::Pending => "badge badge-pending",
        LoopState::AwaitingApproval => "badge badge-awaiting-approval",
        LoopState::Paused => "badge badge-paused",
        LoopState::AwaitingReauth => "badge badge-awaiting-reauth",
    }
}

/// Compute a stable color for an engineer name.
fn engineer_color(name: &str) -> &'static str {
    let hash: u32 = name.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    let colors = [
        "#1B6B5A", "#3B7BC0", "#C4841D", "#8B5CF6", "#C4392D",
        "#2D7A4F", "#E8A838", "#6366F1", "#0EA5E9", "#D946EF",
    ];
    colors[(hash as usize) % colors.len()]
}

/// Extract the spec filename from a full path.
fn spec_filename(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(path)
}

/// Short loop ID (first 8 chars).
fn short_id(id: &uuid::Uuid) -> String {
    id.to_string()[..8].to_string()
}

// ── Login Page ──

pub fn render_login(error: Option<&str>, csrf_token: &str) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "nautiloop — login" }
                link rel="stylesheet" href="/dashboard/static/dashboard.css";
            }
            body {
                div class="login-page" {
                    div class="login-card" {
                        h1 { "NAUTILOOP" }
                        @if let Some(err) = error {
                            p class="login-error" { (err) }
                        }
                        form method="post" action="/dashboard/login" {
                            input type="hidden" name="csrf_token" value=(csrf_token);
                            input class="login-input" type="text" name="engineer_name"
                                  placeholder="Your name (e.g. alice)" autocomplete="username" required;
                            input class="login-input" type="password" name="api_key"
                                  placeholder="API key" autocomplete="current-password" required;
                            button class="login-submit" type="submit" { "Sign in" }
                        }
                    }
                }
            }
        }
    }
}

// ── Card Grid Page ──

pub struct CardData {
    pub record: LoopRecord,
    pub current_stage: Option<String>,
    pub total_tokens: u64,
    pub total_cost: f64,
    pub last_verdict: Option<String>,
}

pub struct FleetSummary {
    pub total_loops: usize,
    pub total_cost: f64,
    pub converge_rate: Option<f64>,
    pub avg_rounds: Option<f64>,
    pub top_spender: Option<(String, f64)>,
    /// Delta vs prior 7-day window (FR-9b)
    pub converge_rate_trend: Option<f64>,
    pub avg_rounds_trend: Option<f64>,
    pub cost_trend: Option<f64>,
}

#[allow(clippy::too_many_arguments)]
pub fn render_grid(
    cards: &[CardData],
    fleet: &FleetSummary,
    engineers: &[String],
    active_state_filter: &str,
    active_engineer_filter: &str,
    show_team: bool,
    counts: &StateCounts,
    csrf_token: &str,
) -> Markup {
    layout("nautiloop", "grid", show_team, csrf_token, html! {
        // Fleet summary (FR-9) with per-field links (FR-9c) and trends (FR-9b)
        div #fleet-summary class="fleet-summary" {
            span class="fleet-field" { "This week" }
            span class="fleet-sep" { " \u{00B7} " }
            a href="/dashboard/stats?focus=loops" class="fleet-field" {
                (fleet.total_loops) " loops"
            }
            span class="fleet-sep" { " \u{00B7} " }
            a href="/dashboard/stats?focus=cost" class="fleet-field" {
                "$" (format!("{:.2}", fleet.total_cost))
                @if let Some(delta) = fleet.cost_trend {
                    // Cost down = favorable (trend-up green), cost up = unfavorable (trend-down red)
                    span class=(if delta <= 0.0 { "trend trend-up" } else { "trend trend-down" })
                         title=(if delta > 0.0 {
                             format!("Cost increased by ${:.2} vs prior week", delta.abs())
                         } else {
                             format!("Cost decreased by ${:.2} vs prior week", delta.abs())
                         }) {
                        @if delta > 0.0 {
                            " \u{2191}$" (format!("{:.2}", delta.abs()))
                        } @else {
                            " \u{2193}$" (format!("{:.2}", delta.abs()))
                        }
                    }
                }
            }
            @if let Some(rate) = fleet.converge_rate {
                span class="fleet-sep" { " \u{00B7} " }
                a href="/dashboard/stats?focus=converge" class="fleet-field" {
                    (format!("{:.0}%", rate * 100.0))
                    @if let Some(delta) = fleet.converge_rate_trend {
                        // Converge rate up = favorable (trend-up green)
                        span class=(if delta >= 0.0 { "trend trend-up" } else { "trend trend-down" })
                             title=(if delta > 0.0 {
                                 format!("Converge rate increased by {:.0}% vs prior week", (delta * 100.0).abs())
                             } else {
                                 format!("Converge rate decreased by {:.0}% vs prior week", (delta * 100.0).abs())
                             }) {
                            @if delta > 0.0 {
                                " \u{2191}" (format!("{:.0}%", (delta * 100.0).abs()))
                            } @else {
                                " \u{2193}" (format!("{:.0}%", (delta * 100.0).abs()))
                            }
                        }
                    }
                    " converged"
                }
            }
            @if let Some(avg) = fleet.avg_rounds {
                span class="fleet-sep" { " \u{00B7} " }
                a href="/dashboard/stats?focus=rounds" class="fleet-field" {
                    "avg " (format!("{:.1}", avg))
                    @if let Some(delta) = fleet.avg_rounds_trend {
                        // Consistent arrow semantics: ↑ = increased, ↓ = decreased.
                        // Color indicates favorability: fewer rounds (↓) = green, more (↑) = red.
                        span class=(if delta <= 0.0 { "trend trend-up" } else { "trend trend-down" })
                             title=(if delta > 0.0 {
                                 format!("Avg rounds increased by {:.1} vs prior week", delta.abs())
                             } else {
                                 format!("Avg rounds decreased by {:.1} vs prior week", delta.abs())
                             }) {
                            @if delta > 0.0 {
                                " \u{2191}" (format!("{:.1}", delta.abs()))
                            } @else {
                                " \u{2193}" (format!("{:.1}", delta.abs()))
                            }
                        }
                    }
                    " rounds"
                }
            }
            @if let Some((ref name, cost)) = fleet.top_spender {
                span class="fleet-sep" { " \u{00B7} " }
                a href="/dashboard/stats?focus=engineer" class="fleet-field" {
                    "top: " (name) " ($" (format!("{:.2}", cost)) ")"
                }
            }
        }

        // State filter chips (FR-3e)
        div class="chip-bar" {
            button class=(chip_active("active", active_state_filter))
                   data-filter="active" data-group="state" {
                "Active (" (counts.active) ")"
            }
            button class=(chip_active("converged", active_state_filter))
                   data-filter="converged" data-group="state" {
                "Converged (" (counts.converged) ")"
            }
            button class=(chip_active("failed", active_state_filter))
                   data-filter="failed" data-group="state" {
                "Failed (" (counts.failed) ")"
            }
            button class=(chip_active("all", active_state_filter))
                   data-filter="all" data-group="state" {
                "All"
            }
        }

        // Engineer filter chips (FR-3e)
        div class="chip-bar" {
            button class=(chip_active("mine", active_engineer_filter))
                   data-filter="mine" data-group="engineer" {
                "Mine"
            }
            button class=(chip_active("team", active_engineer_filter))
                   data-filter="team" data-group="engineer" {
                "Team"
            }
            @for eng in engineers {
                button class=(chip_active(eng, active_engineer_filter))
                       data-filter=(eng) data-group="engineer" {
                    (eng)
                }
            }
        }

        // Card grid (FR-3a)
        div #card-grid class="card-grid" {
            @for card in cards {
                (render_card(card, show_team || active_engineer_filter == "team"))
            }
            @if cards.is_empty() {
                p class="text-muted" style="padding: 32px; text-align: center;" {
                    "No loops match the current filters."
                }
            }
        }
    })
}

fn chip_active(value: &str, active: &str) -> String {
    if value == active {
        "chip active".to_string()
    } else {
        "chip".to_string()
    }
}

pub struct StateCounts {
    pub active: usize,
    pub converged: usize,
    pub failed: usize,
}

fn render_card(card: &CardData, show_engineer: bool) -> Markup {
    let r = &card.record;
    let is_active = !r.state.is_terminal();

    html! {
        a class="card" href=(format!("/dashboard/loops/{}", r.id))
          data-loop-id=(r.id.to_string()) {
            div class="card-header" {
                @if show_engineer {
                    span class="engineer-badge"
                         style=(format!("background:{}", engineer_color(&r.engineer))) {
                        (engineer_initials(&r.engineer))
                    }
                }
                span class=(badge_class(r.state)) { (r.state) }
                span class="card-id" { (short_id(&r.id)) }
                span class="card-elapsed" { (format_elapsed(r.created_at)) }
                span class=(if is_active { "pulse active" } else { "pulse" }) {}
            }
            div class="card-title" { (spec_filename(&r.spec_path)) }
            div class="card-branch" { (r.branch) }
            div class="card-progress" {
                @if is_active {
                    "round " (r.round) "/" (r.max_rounds)
                    @if let Some(ref stage) = card.current_stage {
                        " \u{00B7} stage: " (stage)
                    }
                } @else {
                    "round " (r.round)
                }
            }
            div class="card-metrics" {
                (format_tokens(card.total_tokens)) " tokens"
                @if card.total_cost > 0.0 {
                    " \u{00B7} $" (format!("{:.2}", card.total_cost))
                }
                @if let Some(ref v) = card.last_verdict {
                    " \u{00B7} " (v)
                }
            }
        }
    }
}

fn engineer_initials(name: &str) -> String {
    let parts: Vec<&str> = name.split('-').collect();
    if parts.len() >= 2 {
        format!(
            "{}{}",
            parts[0].chars().next().unwrap_or('?'),
            parts[1].chars().next().unwrap_or('?')
        )
        .to_uppercase()
    } else {
        name.chars().take(2).collect::<String>().to_uppercase()
    }
}

// ── Detail Page ──

pub struct DetailData {
    pub record: LoopRecord,
    pub rounds: Vec<RoundData>,
    pub logs: Vec<String>,
    pub judge_decisions: Vec<JudgeDecisionRecord>,
    pub total_tokens: u64,
    pub total_cost: f64,
    pub token_breakdown: Vec<TokenBreakdownRow>,
}

pub struct RoundData {
    pub round: i32,
    pub stages: Vec<StageData>,
}

pub struct StageData {
    pub stage: String,
    pub verdict_clean: Option<bool>,
    pub issues_count: usize,
    pub confidence: Option<f64>,
    pub tokens: u64,
    pub cost: f64,
    pub duration_secs: Option<i64>,
    pub has_judge: bool,
    pub judge_decision: Option<JudgeDecisionRecord>,
}

pub struct TokenBreakdownRow {
    pub label: String,
    pub tokens: u64,
    pub cost: f64,
    pub fraction: f64,
}

pub fn render_detail(data: &DetailData, csrf_token: &str) -> Markup {
    let r = &data.record;
    let is_terminal = r.state.is_terminal();

    layout(&format!("{} — nautiloop", spec_filename(&r.spec_path)), "grid", false, csrf_token, html! {
        div class="detail" {
            a href="/dashboard" class="back-link" { "\u{2190} Back to loops" }

            // Hero header (FR-4a)
            div class="hero" {
                span class=(badge_class(r.state)) { (r.state) }
                h1 class="hero-title" { (spec_filename(&r.spec_path)) }
                span class="hero-elapsed" { (format_elapsed(r.created_at)) }
                @if let Some(ref url) = r.spec_pr_url {
                    span class="hero-pr" {
                        a href=(url) target="_blank" rel="noopener" { "Open PR \u{2197}" }
                    }
                }
            }

            // Action buttons (FR-4a)
            div class="actions" {
                @if r.state == LoopState::AwaitingApproval {
                    button class="btn btn-primary" data-action="approve"
                           data-loop-id=(r.id) { "Approve" }
                }
                @if !is_terminal {
                    button class="btn btn-danger" data-action="cancel"
                           data-loop-id=(r.id) { "Cancel" }
                }
                @if matches!(r.state, LoopState::Paused | LoopState::AwaitingReauth)
                    || (r.state == LoopState::Failed && r.failed_from_state.is_some()) {
                    button class="btn" data-action="resume"
                           data-loop-id=(r.id) { "Resume" }
                }
                @if r.state == LoopState::Failed && r.failed_from_state.is_some() {
                    button class="btn" data-action="extend"
                           data-loop-id=(r.id) { "Extend +10" }
                }
                @if let Some(ref url) = r.spec_pr_url {
                    a class="btn" href=(url) target="_blank" rel="noopener" { "Open PR" }
                }
            }

            // Spec filename link (FR-13)
            p class="text-sm text-muted mb-md" {
                "Spec: "
                a href=(format!("/dashboard/specs/{}", r.spec_path)) { (r.spec_path) }
                " \u{00B7} Branch: " code { (r.branch) }
                " \u{00B7} Engineer: " (r.engineer)
            }

            div class="detail-columns" {
                div {
                    // Rounds table (FR-4a)
                    h3 class="text-sm mb-md" style="color: var(--text-secondary); text-transform: uppercase; letter-spacing: 0.05em;" {
                        "Rounds"
                    }
                    div class="rounds-table-wrap" {
                        table class="rounds-table" {
                            thead {
                                tr {
                                    th { "Round" }
                                    th { "Stage" }
                                    th { "Verdict" }
                                    th { "Issues" }
                                    th { "Conf." }
                                    th { "Tokens" }
                                    th { "Cost" }
                                    th { "Duration" }
                                }
                            }
                            tbody {
                                @for rd in &data.rounds {
                                    @for sd in &rd.stages {
                                        tr {
                                            td { (rd.round) }
                                            td { (sd.stage) }
                                            td {
                                                @if let Some(clean) = sd.verdict_clean {
                                                    @if clean {
                                                        span class="verdict-clean" { "clean" }
                                                    } @else {
                                                        span class="verdict-not-clean" { "not clean" }
                                                    }
                                                } @else {
                                                    span class="verdict-dash" { "\u{2014}" }
                                                }
                                                @if sd.has_judge {
                                                    span class="judge-icon" title="Judge fired" { "\u{2696}" }
                                                }
                                            }
                                            td { (sd.issues_count) }
                                            td {
                                                @if let Some(c) = sd.confidence {
                                                    (format!("{:.0}%", c * 100.0))
                                                } @else {
                                                    "\u{2014}"
                                                }
                                            }
                                            td { (format_tokens(sd.tokens)) }
                                            td { "$" (format!("{:.2}", sd.cost)) }
                                            td {
                                                @if let Some(d) = sd.duration_secs {
                                                    (format_duration(d))
                                                } @else {
                                                    "\u{2014}"
                                                }
                                            }
                                        }
                                        // Judge detail row (FR-11a)
                                        @if let Some(ref jd) = sd.judge_decision {
                                            tr class="judge-detail-row hidden" {
                                                td colspan="8" {
                                                    div class="judge-detail" {
                                                        dl {
                                                            dt { "Decision" }
                                                            dd { (jd.decision) }
                                                            @if let Some(ref conf) = jd.confidence {
                                                                dt { "Confidence" }
                                                                dd { (format!("{:.0}%", conf * 100.0)) }
                                                            }
                                                            @if let Some(ref reasoning) = jd.reasoning {
                                                                dt { "Reasoning" }
                                                                dd { (reasoning) }
                                                            }
                                                            @if let Some(ref hint) = jd.hint {
                                                                dt { "Hint" }
                                                                dd { (hint) }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Token/cost breakdown
                    h3 class="text-sm mb-md mt-md" style="color: var(--text-secondary); text-transform: uppercase; letter-spacing: 0.05em;" {
                        "Token Breakdown"
                    }
                    @if !data.token_breakdown.is_empty() {
                        @for row in &data.token_breakdown {
                            div class="bar-row" {
                                span class="bar-label" { (row.label) }
                                div class="bar-track" {
                                    div class="bar-fill" style=(format!("width:{}%", (row.fraction * 100.0).min(100.0))) {}
                                }
                                span class="bar-value" { (format_tokens(row.tokens)) " / $" (format!("{:.2}", row.cost)) }
                            }
                        }
                    }
                }

                div {
                    // Log pane (FR-4a)
                    h3 class="text-sm mb-md" style="color: var(--text-secondary); text-transform: uppercase; letter-spacing: 0.05em;" {
                        "Logs"
                    }
                    div #log-pane class="log-pane"
                        data-loop-id=(r.id)
                        data-terminal=(if is_terminal { "true" } else { "false" }) {
                        @for line in &data.logs {
                            span class="log-line" { (line) "\n" }
                        }
                    }

                    // Pod introspect (FR-5)
                    @if !is_terminal {
                        details #pod-introspect class="disclosure" data-loop-id=(r.id) {
                            summary { "Inspect pod" }
                            div class="disclosure-body" {
                                p class="text-sm text-muted" { "Loading..." }
                            }
                        }
                    }

                    // Judge decisions tab (FR-11b)
                    @if !data.judge_decisions.is_empty() {
                        h3 class="text-sm mb-md mt-md" style="color: var(--text-secondary); text-transform: uppercase; letter-spacing: 0.05em;" {
                            "Judge Decisions"
                        }
                        @for jd in &data.judge_decisions {
                            div class="judge-detail mb-md" {
                                dl {
                                    dt { "Round " (jd.round) " \u{00B7} " (jd.phase) " \u{00B7} " (jd.trigger) }
                                    dd {
                                        span class=(if jd.decision == "exit_clean" { "verdict-clean" } else if jd.decision == "exit_fail" { "verdict-not-clean" } else { "" }) {
                                            (jd.decision)
                                        }
                                        @if let Some(ref conf) = jd.confidence {
                                            " (" (format!("{:.0}%", conf * 100.0)) " confidence)"
                                        }
                                    }
                                    @if let Some(ref reasoning) = jd.reasoning {
                                        dt { "Reasoning" }
                                        dd { (reasoning) }
                                    }
                                    @if let Some(ref hint) = jd.hint {
                                        dt { "Hint" }
                                        dd { (hint) }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

fn format_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

// ── Feed Page (FR-12) ──

pub struct FeedItem {
    pub loop_id: uuid::Uuid,
    pub engineer: String,
    pub spec_path: String,
    pub state: LoopState,
    pub round: i32,
    pub total_cost: f64,
    pub spec_pr_url: Option<String>,
    pub updated_at: DateTime<Utc>,
}

pub fn render_feed(
    items: &[FeedItem],
    next_cursor: Option<&str>,
    filter: &str,
    engineers: &[String],
    active_engineer: Option<&str>,
    csrf_token: &str,
) -> Markup {
    layout("Feed — nautiloop", "feed", false, csrf_token, html! {
        div style="padding: var(--sp-md);" {
            h2 style="font-size: 1.125rem; font-family: var(--font-display); font-weight: 700; margin-bottom: var(--sp-md);" {
                "Notification Feed"
            }

            // State filter chips (FR-12b)
            div class="chip-bar" {
                a href="/dashboard/feed" class=(if filter == "all" && active_engineer.is_none() { "chip active" } else { "chip" }) { "All events" }
                a href="/dashboard/feed?filter=converged" class=(if filter == "converged" { "chip active" } else { "chip" }) { "Converged only" }
                a href="/dashboard/feed?filter=failed" class=(if filter == "failed" { "chip active" } else { "chip" }) { "Failed only" }
            }

            // Per-engineer filter chips (FR-12b)
            @if !engineers.is_empty() {
                div class="chip-bar" {
                    @for eng in engineers {
                        a href=(format!("/dashboard/feed?filter={}&engineer={}", filter, eng))
                          class=(if active_engineer == Some(eng.as_str()) { "chip active" } else { "chip" }) {
                            (eng)
                        }
                    }
                }
            }

            div #feed-list class="feed-list" {
                @for item in items {
                    a class="feed-item" href=(format!("/dashboard/loops/{}", item.loop_id)) {
                        span class="feed-time" { (item.updated_at.format("%H:%M")) }
                        span class="feed-engineer" { (item.engineer) }
                        span class="feed-spec" { (spec_filename(&item.spec_path)) }
                        span class=(badge_class(item.state)) { (item.state) }
                        @if let Some(ref url) = item.spec_pr_url {
                            span { a href=(url) target="_blank" rel="noopener" style="font-size:0.75rem" { "PR" } }
                        }
                        span class="feed-cost" {
                            (item.round) " rounds \u{00B7} $" (format!("{:.2}", item.total_cost))
                        }
                    }
                }
                @if items.is_empty() {
                    p class="text-muted" style="padding: 32px; text-align: center;" {
                        "No terminal events yet."
                    }
                }
            }

            @if let Some(cursor) = next_cursor {
                button #load-more-btn class="load-more" data-cursor=(cursor) { "Load more" }
            }
        }
    })
}

// ── Specs History Page (FR-13) ──

pub struct SpecHistoryItem {
    pub loop_id: uuid::Uuid,
    pub engineer: String,
    pub state: LoopState,
    pub round: i32,
    pub total_cost: f64,
    pub branch: String,
    pub created_at: DateTime<Utc>,
}

pub struct SpecAggregate {
    pub total_runs: usize,
    pub converge_rate: f64,
    pub avg_rounds: f64,
    pub total_cost: f64,
}

pub fn render_spec_history(
    spec_path: &str,
    items: &[SpecHistoryItem],
    aggregate: &SpecAggregate,
    csrf_token: &str,
) -> Markup {
    layout(&format!("{} — nautiloop", spec_filename(spec_path)), "grid", false, csrf_token, html! {
        div class="detail" {
            a href="/dashboard" class="back-link" { "\u{2190} Back to loops" }

            div class="spec-history-header" {
                h2 { (spec_path) }
                p class="spec-aggregate" {
                    (aggregate.total_runs) " runs \u{00B7} "
                    (format!("{:.0}%", aggregate.converge_rate * 100.0)) " converge rate \u{00B7} "
                    "avg " (format!("{:.1}", aggregate.avg_rounds)) " rounds \u{00B7} "
                    "total cost $" (format!("{:.2}", aggregate.total_cost))
                }
            }

            div class="rounds-table-wrap" {
                table class="rounds-table" {
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
                        @for item in items {
                            tr {
                                td {
                                    a href=(format!("/dashboard/loops/{}", item.loop_id)) {
                                        (item.created_at.format("%Y-%m-%d %H:%M"))
                                    }
                                }
                                td { (item.engineer) }
                                td { span class=(badge_class(item.state)) { (item.state) } }
                                td { (item.round) }
                                td { "$" (format!("{:.2}", item.total_cost)) }
                                td style="font-family: var(--font-code); font-size: 0.75rem;" { (item.branch) }
                            }
                        }
                    }
                }
            }
        }
    })
}

// ── Stats Page (FR-14) ──

#[derive(Clone)]
pub struct StatsData {
    pub window: String,
    pub total_loops: usize,
    pub total_cost: f64,
    pub converge_rate: f64,
    pub avg_rounds: f64,
    pub per_engineer: Vec<EngineerStats>,
    pub per_spec: Vec<SpecStats>,
    pub daily_series: Vec<DayStats>,
}

#[derive(Clone)]
pub struct EngineerStats {
    pub engineer: String,
    pub loops: usize,
    pub cost: f64,
    pub converge_rate: f64,
}

#[derive(Clone)]
pub struct SpecStats {
    pub spec_path: String,
    pub runs: usize,
    pub cost: f64,
    pub converge_rate: f64,
}

#[derive(Clone)]
pub struct DayStats {
    pub date: String,
    pub started: usize,
    pub converged: usize,
    pub failed: usize,
}

pub fn render_stats(data: &StatsData, csrf_token: &str) -> Markup {
    let max_daily = data.daily_series.iter().map(|d| d.started.max(d.converged).max(d.failed)).max().unwrap_or(1).max(1);

    layout("Stats — nautiloop", "stats", false, csrf_token, html! {
        div class="detail" {
            div class="flex items-center justify-between mb-md" {
                h2 style="font-size: 1.125rem; font-family: var(--font-display); font-weight: 700;" { "Stats" }
                div class="window-toggle" {
                    button class=(if data.window == "24h" { "active" } else { "" }) data-window="24h" { "24h" }
                    button class=(if data.window == "7d" { "active" } else { "" }) data-window="7d" { "7d" }
                    button class=(if data.window == "30d" { "active" } else { "" }) data-window="30d" { "30d" }
                }
            }

            // Headline cards (FR-9c: id anchors for focus param)
            div class="stat-cards" {
                div #stat-loops class="stat-card" {
                    div class="stat-card-label" { "Total Loops" }
                    div class="stat-card-value" { (data.total_loops) }
                }
                div #stat-cost class="stat-card" {
                    div class="stat-card-label" { "Total Cost" }
                    div class="stat-card-value" { "$" (format!("{:.2}", data.total_cost)) }
                }
                div #stat-converge class="stat-card" {
                    div class="stat-card-label" { "Converge Rate" }
                    div class="stat-card-value" { (format!("{:.0}%", data.converge_rate * 100.0)) }
                }
                div #stat-rounds class="stat-card" {
                    div class="stat-card-label" { "Avg Rounds" }
                    div class="stat-card-value" { (format!("{:.1}", data.avg_rounds)) }
                }
            }

            // Per-engineer table
            h3 #stat-engineer class="text-sm mb-md mt-md" style="color: var(--text-secondary); text-transform: uppercase; letter-spacing: 0.05em;" {
                "Per Engineer"
            }
            div class="rounds-table-wrap" {
                table class="rounds-table" {
                    thead { tr { th { "Engineer" } th { "Loops" } th { "Cost" } th { "Converge %" } } }
                    tbody {
                        @for e in &data.per_engineer {
                            tr {
                                td { (e.engineer) }
                                td { (e.loops) }
                                td { "$" (format!("{:.2}", e.cost)) }
                                td { (format!("{:.0}%", e.converge_rate * 100.0)) }
                            }
                        }
                    }
                }
            }

            // Per-spec table
            h3 class="text-sm mb-md mt-md" style="color: var(--text-secondary); text-transform: uppercase; letter-spacing: 0.05em;" {
                "Top Specs"
            }
            div class="rounds-table-wrap" {
                table class="rounds-table" {
                    thead { tr { th { "Spec" } th { "Runs" } th { "Cost" } th { "Converge %" } } }
                    tbody {
                        @for s in &data.per_spec {
                            tr {
                                td {
                                    a href=(format!("/dashboard/specs/{}", s.spec_path)) {
                                        (spec_filename(&s.spec_path))
                                    }
                                }
                                td { (s.runs) }
                                td { "$" (format!("{:.2}", s.cost)) }
                                td { (format!("{:.0}%", s.converge_rate * 100.0)) }
                            }
                        }
                    }
                }
            }

            // Daily time series (CSS bars)
            h3 class="text-sm mb-md mt-md" style="color: var(--text-secondary); text-transform: uppercase; letter-spacing: 0.05em;" {
                "Daily Activity"
            }
            @for day in &data.daily_series {
                div class="bar-row" {
                    span class="bar-label" { (day.date) }
                    div class="bar-track" {
                        div class="bar-fill" style=(format!("width:{}%; background: var(--primary);", (day.started as f64 / max_daily as f64 * 100.0).min(100.0))) {}
                    }
                    span class="bar-value" { (day.started) " started" }
                }
                div class="bar-row" {
                    span class="bar-label" {}
                    div class="bar-track" {
                        div class="bar-fill" style=(format!("width:{}%; background: var(--success);", (day.converged as f64 / max_daily as f64 * 100.0).min(100.0))) {}
                    }
                    span class="bar-value" { (day.converged) " conv." }
                }
                div class="bar-row" {
                    span class="bar-label" {}
                    div class="bar-track" {
                        div class="bar-fill" style=(format!("width:{}%; background: var(--error);", (day.failed as f64 / max_daily as f64 * 100.0).min(100.0))) {}
                    }
                    span class="bar-value" { (day.failed) " failed" }
                }
            }
        }
    })
}
