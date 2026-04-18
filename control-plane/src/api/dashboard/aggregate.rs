use std::collections::HashMap;
use std::time::Instant;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::{ModelPricing, NautiloopConfig, PricingConfig};
use crate::state::StateStore;
use crate::types::verdict::{
    ImplResultData, ReviewResultData, ReviseResultData, TestResultData, TokenUsage,
};
use crate::types::{LoopRecord, LoopState, RoundRecord};

// ── Response types ──

#[derive(Debug, Serialize)]
pub struct DashboardStateResponse {
    pub loops: Vec<DashboardLoop>,
    pub aggregates: Aggregates,
    pub fleet_summary: Option<FleetSummary>,
    pub engineers: Vec<String>,
    pub viewer: String,
}

#[derive(Debug, Serialize)]
pub struct DashboardLoop {
    pub id: String,
    pub spec_path: String,
    pub branch: String,
    pub engineer: String,
    pub state: String,
    pub sub_state: Option<String>,
    pub round: i32,
    pub max_rounds: i32,
    pub current_stage: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub spec_pr_url: Option<String>,
    pub failed_from_state: Option<String>,
    pub last_verdict: Option<String>,
    pub total_tokens: TokenSummary,
    pub total_cost: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct TokenSummary {
    pub input: u64,
    pub output: u64,
}

#[derive(Debug, Serialize)]
pub struct Aggregates {
    pub counts_by_state: HashMap<String, u64>,
    pub total_tokens: TokenSummary,
    pub total_cost: Option<f64>,
    pub total_loops: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FleetSummary {
    pub window_days: u32,
    pub total_loops: u64,
    pub total_cost: Option<f64>,
    pub converge_rate: Option<f64>,
    pub avg_rounds: Option<f64>,
    pub top_spender: Option<TopSpender>,
    pub trends: Option<Trends>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopSpender {
    pub engineer: String,
    pub cost: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Trends {
    pub converge_rate_delta: Option<f64>,
    pub avg_rounds_delta: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct FeedResponse {
    pub events: Vec<FeedEvent>,
    pub has_more: bool,
}

#[derive(Debug, Serialize)]
pub struct FeedEvent {
    pub id: String,
    pub spec_path: String,
    pub engineer: String,
    pub state: String,
    pub rounds: i32,
    pub total_tokens: TokenSummary,
    pub total_cost: Option<f64>,
    pub spec_pr_url: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub extensions: i32,
}

#[derive(Debug, Serialize)]
pub struct SpecsResponse {
    pub spec_path: String,
    pub runs: Vec<SpecRun>,
    pub aggregates: SpecAggregates,
}

#[derive(Debug, Serialize)]
pub struct SpecRun {
    pub id: String,
    pub engineer: String,
    pub state: String,
    pub rounds: i32,
    pub total_cost: Option<f64>,
    pub branch: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct SpecAggregates {
    pub total_runs: u64,
    pub converge_rate: Option<f64>,
    pub avg_rounds: Option<f64>,
    pub total_cost: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub window: String,
    pub headline: StatsHeadline,
    pub per_engineer: Vec<EngineerStats>,
    pub per_spec: Vec<SpecStats>,
    pub time_series: Vec<DayStats>,
}

#[derive(Debug, Serialize)]
pub struct StatsHeadline {
    pub total_loops: u64,
    pub total_cost: Option<f64>,
    pub converge_rate: Option<f64>,
    pub avg_rounds: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct EngineerStats {
    pub engineer: String,
    pub loops: u64,
    pub cost: Option<f64>,
    pub converge_rate: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct SpecStats {
    pub spec_path: String,
    pub runs: u64,
    pub cost: Option<f64>,
    pub converge_rate: Option<f64>,
    pub avg_rounds: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct DayStats {
    pub date: String,
    pub started: u64,
    pub converged: u64,
    pub failed: u64,
}

// ── Fleet Summary Cache ──

pub struct FleetSummaryCache {
    inner: RwLock<Option<(FleetSummary, Instant)>>,
}

impl Default for FleetSummaryCache {
    fn default() -> Self {
        Self {
            inner: RwLock::new(None),
        }
    }
}

impl FleetSummaryCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn get(&self) -> Option<FleetSummary> {
        let guard = self.inner.read().await;
        guard.as_ref().and_then(|(summary, ts)| {
            if ts.elapsed().as_secs() < 60 {
                Some(summary.clone())
            } else {
                None
            }
        })
    }

    pub async fn set(&self, summary: FleetSummary) {
        let mut guard = self.inner.write().await;
        *guard = Some((summary, Instant::now()));
    }
}

// ── Stats Cache ──

pub struct StatsCache {
    inner: RwLock<HashMap<String, (StatsResponse, Instant)>>,
}

impl Default for StatsCache {
    fn default() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }
}

impl StatsCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn get(&self, window: &str) -> Option<StatsResponse> {
        let guard = self.inner.read().await;
        guard.get(window).and_then(|(resp, ts)| {
            if ts.elapsed().as_secs() < 60 {
                // StatsResponse doesn't implement Clone, so we serialize/deserialize
                serde_json::to_value(resp)
                    .ok()
                    .and_then(|v| serde_json::from_value(v).ok())
            } else {
                None
            }
        })
    }

    pub async fn set(&self, window: String, resp: &StatsResponse) {
        if let Ok(cloned) = serde_json::to_value(resp)
            .and_then(serde_json::from_value::<StatsResponse>)
        {
            let mut guard = self.inner.write().await;
            guard.insert(window, (cloned, Instant::now()));
        }
    }
}

// ── Pricing ──

/// Compute cost in USD for a token usage given model name and pricing config (FR-15b).
pub fn compute_cost(
    token_usage: &TokenUsage,
    model: &str,
    pricing: &PricingConfig,
) -> Option<f64> {
    let model_pricing = pricing.models.get(model)?;
    Some(compute_cost_with_pricing(token_usage, model_pricing))
}

fn compute_cost_with_pricing(token_usage: &TokenUsage, pricing: &ModelPricing) -> f64 {
    let input_cost = (token_usage.input as f64 / 1_000_000.0) * pricing.input_per_million;
    let output_cost = (token_usage.output as f64 / 1_000_000.0) * pricing.output_per_million;
    input_cost + output_cost
}

/// Resolve the model for a given stage using the loop's model fields (FR-15b).
fn resolve_model_for_stage(
    stage: &str,
    loop_record: &LoopRecord,
    config: &NautiloopConfig,
) -> Option<String> {
    let model = match stage {
        "implement" | "revise" | "test" => loop_record
            .model_implementor
            .clone()
            .unwrap_or_else(|| config.models.implementor.clone()),
        "review" | "audit" => loop_record
            .model_reviewer
            .clone()
            .unwrap_or_else(|| config.models.reviewer.clone()),
        _ => return None,
    };
    Some(model)
}

/// Extract token usage from a round's output JSON.
fn extract_token_usage(round: &RoundRecord) -> Option<TokenUsage> {
    let output = round.output.as_ref()?;
    // Try each stage-specific type
    match round.stage.as_str() {
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

/// Extract the last review/audit verdict from rounds.
fn extract_last_verdict(rounds: &[RoundRecord]) -> Option<String> {
    rounds
        .iter()
        .rev()
        .find(|r| r.stage == "review" || r.stage == "audit")
        .and_then(|r| r.output.as_ref())
        .and_then(|output| {
            serde_json::from_value::<ReviewResultData>(output.clone())
                .ok()
                .and_then(|rd| {
                    rd.verdict.get("clean").and_then(|v| v.as_bool()).map(|clean| {
                        if clean {
                            "clean".to_string()
                        } else {
                            "not clean".to_string()
                        }
                    })
                })
        })
}

/// Compute total tokens and cost for a loop given its rounds.
fn compute_loop_totals(
    rounds: &[RoundRecord],
    loop_record: &LoopRecord,
    config: &NautiloopConfig,
) -> (TokenSummary, Option<f64>) {
    let mut total_input = 0u64;
    let mut total_output = 0u64;
    let mut total_cost = 0.0f64;
    let mut has_pricing = false;

    let pricing = config.pricing.as_ref();

    for round in rounds {
        if let Some(tu) = extract_token_usage(round) {
            total_input += tu.input;
            total_output += tu.output;
            if let Some(pc) = pricing
                && let Some(model) = resolve_model_for_stage(&round.stage, loop_record, config)
                    && let Some(cost) = compute_cost(&tu, &model, pc) {
                        total_cost += cost;
                        has_pricing = true;
                    }
        }
    }

    // Only return Some(cost) when at least one round had computable cost.
    // When pricing is configured but no rounds have token data yet, return None
    // so the UI shows "—" instead of "$0.00".
    let cost = if has_pricing {
        Some(total_cost)
    } else {
        None
    };

    (
        TokenSummary {
            input: total_input,
            output: total_output,
        },
        cost,
    )
}

// ── Current Stage Resolution (mirrors handlers.rs logic) ──

fn current_stage_for_record(record: &LoopRecord, rounds: &[RoundRecord]) -> Option<String> {
    let source_state = if record.state.is_active_stage() {
        Some(record.state)
    } else {
        match record.state {
            LoopState::Paused => record.paused_from_state,
            LoopState::AwaitingReauth => record.reauth_from_state,
            LoopState::Failed => record.failed_from_state,
            _ => None,
        }
    };

    let source_state = source_state?;

    match source_state {
        LoopState::Implementing => Some("implement".to_string()),
        LoopState::Testing => Some("test".to_string()),
        LoopState::Reviewing => Some("review".to_string()),
        LoopState::Hardening => {
            let stage = rounds
                .iter()
                .rev()
                .find(|r| r.round == record.round)
                .map(|r| r.stage.clone())
                .unwrap_or_else(|| "audit".to_string());
            Some(stage)
        }
        _ => None,
    }
}

// ── Build dashboard state response ──

pub async fn build_dashboard_state(
    store: &dyn StateStore,
    config: &NautiloopConfig,
    team: bool,
    include_all_terminal: bool,
    viewer_engineer: &str,
    fleet_cache: &FleetSummaryCache,
) -> crate::error::Result<DashboardStateResponse> {
    let engineer_filter = if team {
        None
    } else {
        Some(viewer_engineer)
    };

    // Get all loops (active + terminal for card grid)
    let loops = store
        .get_loops_for_engineer(engineer_filter, team, true)
        .await?;

    let now = Utc::now();
    let cutoff = now - Duration::hours(24);

    // Filter: active + recently-terminal (or all terminal if requested)
    let filtered: Vec<&LoopRecord> = loops
        .iter()
        .filter(|l| {
            !l.state.is_terminal() || include_all_terminal || l.updated_at > cutoff
        })
        .collect();

    let mut dashboard_loops = Vec::with_capacity(filtered.len());
    let mut counts_by_state: HashMap<String, u64> = HashMap::new();
    let mut agg_input = 0u64;
    let mut agg_output = 0u64;
    let mut agg_cost = 0.0f64;
    let mut has_cost = false;
    let mut engineers_set: std::collections::HashSet<String> = std::collections::HashSet::new();

    for record in &filtered {
        let rounds = store.get_rounds(record.id).await?;
        let current_stage = current_stage_for_record(record, &rounds);
        let last_verdict = extract_last_verdict(&rounds);
        let (tokens, cost) = compute_loop_totals(&rounds, record, config);

        *counts_by_state
            .entry(record.state.to_string())
            .or_insert(0) += 1;
        agg_input += tokens.input;
        agg_output += tokens.output;
        if let Some(c) = cost {
            agg_cost += c;
            has_cost = true;
        }
        engineers_set.insert(record.engineer.clone());

        dashboard_loops.push(DashboardLoop {
            id: record.id.to_string(),
            spec_path: record.spec_path.clone(),
            branch: record.branch.clone(),
            engineer: record.engineer.clone(),
            state: record.state.to_string(),
            sub_state: record.sub_state.map(|s| s.to_string()),
            round: record.round,
            max_rounds: record.max_rounds,
            current_stage,
            created_at: record.created_at,
            updated_at: record.updated_at,
            spec_pr_url: record.spec_pr_url.clone(),
            failed_from_state: record.failed_from_state.map(|s| s.to_string()),
            last_verdict,
            total_tokens: tokens,
            total_cost: cost,
        });
    }

    let mut engineers: Vec<String> = engineers_set.into_iter().collect();
    engineers.sort();

    // Fleet summary (cached 60s)
    let fleet_summary = if let Some(cached) = fleet_cache.get().await {
        Some(cached)
    } else {
        let summary =
            build_fleet_summary(store, config, now).await.ok();
        if let Some(ref s) = summary {
            fleet_cache.set(s.clone()).await;
        }
        summary
    };

    Ok(DashboardStateResponse {
        loops: dashboard_loops,
        aggregates: Aggregates {
            counts_by_state,
            total_tokens: TokenSummary {
                input: agg_input,
                output: agg_output,
            },
            total_cost: if has_cost { Some(agg_cost) } else { None },
            total_loops: filtered.len() as u64,
        },
        fleet_summary,
        engineers,
        viewer: viewer_engineer.to_string(),
    })
}

/// Build the fleet summary for the last 7 days (FR-9).
async fn build_fleet_summary(
    store: &dyn StateStore,
    config: &NautiloopConfig,
    now: DateTime<Utc>,
) -> crate::error::Result<FleetSummary> {
    let window = Duration::days(7);
    let cutoff = now - window;
    let prev_cutoff = cutoff - window;

    // Get all loops (including terminal) for the window
    let all_loops = store
        .get_loops_for_engineer(None, true, true)
        .await?;

    let current_window: Vec<&LoopRecord> = all_loops
        .iter()
        .filter(|l| l.created_at > cutoff)
        .collect();
    let prev_window: Vec<&LoopRecord> = all_loops
        .iter()
        .filter(|l| l.created_at > prev_cutoff && l.created_at <= cutoff)
        .collect();

    let (summary, _cost) =
        compute_window_summary(&current_window, store, config).await?;
    let (prev_summary, _) =
        compute_window_summary(&prev_window, store, config).await?;

    let trends = if !prev_window.is_empty() {
        Some(Trends {
            converge_rate_delta: match (summary.converge_rate, prev_summary.converge_rate) {
                (Some(a), Some(b)) => Some(a - b),
                _ => None,
            },
            avg_rounds_delta: match (summary.avg_rounds, prev_summary.avg_rounds) {
                (Some(a), Some(b)) => Some(a - b),
                _ => None,
            },
        })
    } else {
        None
    };

    Ok(FleetSummary {
        window_days: 7,
        total_loops: summary.total_loops,
        total_cost: summary.total_cost,
        converge_rate: summary.converge_rate,
        avg_rounds: summary.avg_rounds,
        top_spender: summary.top_spender,
        trends,
    })
}

struct WindowSummary {
    total_loops: u64,
    total_cost: Option<f64>,
    converge_rate: Option<f64>,
    avg_rounds: Option<f64>,
    top_spender: Option<TopSpender>,
}

async fn compute_window_summary(
    loops: &[&LoopRecord],
    store: &dyn StateStore,
    config: &NautiloopConfig,
) -> crate::error::Result<(WindowSummary, f64)> {
    let total_loops = loops.len() as u64;
    let mut total_cost = 0.0f64;
    let mut has_cost = false;
    let mut terminal_count = 0u64;
    let mut converged_count = 0u64;
    let mut rounds_sum = 0i64;
    let mut cost_by_engineer: HashMap<String, f64> = HashMap::new();

    for record in loops {
        let rounds = store.get_rounds(record.id).await?;
        let (_, cost) = compute_loop_totals(&rounds, record, config);
        if let Some(c) = cost {
            total_cost += c;
            has_cost = true;
            *cost_by_engineer.entry(record.engineer.clone()).or_insert(0.0) += c;
        }
        if record.state.is_terminal() {
            terminal_count += 1;
            rounds_sum += record.round as i64;
            if matches!(
                record.state,
                LoopState::Converged | LoopState::Hardened | LoopState::Shipped
            ) {
                converged_count += 1;
            }
        }
    }

    let converge_rate = if terminal_count > 0 {
        Some(converged_count as f64 / terminal_count as f64)
    } else {
        None
    };
    let avg_rounds = if terminal_count > 0 {
        Some(rounds_sum as f64 / terminal_count as f64)
    } else {
        None
    };
    let top_spender = cost_by_engineer
        .into_iter()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(engineer, cost)| TopSpender { engineer, cost });

    Ok((
        WindowSummary {
            total_loops,
            total_cost: if has_cost { Some(total_cost) } else { None },
            converge_rate,
            avg_rounds,
            top_spender,
        },
        total_cost,
    ))
}

// ── Build feed response (FR-12) ──

pub async fn build_feed_response(
    store: &dyn StateStore,
    config: &NautiloopConfig,
    cursor: Option<DateTime<Utc>>,
    limit: usize,
    filter: Option<&str>,
) -> crate::error::Result<FeedResponse> {
    let all_loops = store
        .get_loops_for_engineer(None, true, true)
        .await?;

    let mut terminal: Vec<&LoopRecord> = all_loops
        .iter()
        .filter(|l| {
            if !l.state.is_terminal() {
                return false;
            }
            // Cursor-based pagination: exclude loops strictly after the cursor.
            // Use `>` so that items sharing the cursor timestamp are included
            // (potential minor duplication is preferable to losing events).
            if let Some(c) = cursor
                && l.updated_at > c {
                    return false;
                }
            match filter {
                Some("converged") => matches!(
                    l.state,
                    LoopState::Converged | LoopState::Hardened | LoopState::Shipped
                ),
                Some("failed") => matches!(l.state, LoopState::Failed | LoopState::Cancelled),
                Some(eng) => l.engineer == eng,
                None => true,
            }
        })
        .collect();

    terminal.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    let has_more = terminal.len() > limit;
    let events_slice = &terminal[..terminal.len().min(limit)];

    let mut events = Vec::with_capacity(events_slice.len());
    for record in events_slice {
        let rounds = store.get_rounds(record.id).await?;
        let (tokens, cost) = compute_loop_totals(&rounds, record, config);
        // Count extensions: max_rounds above the configured default
        let default_max = if record.kind == crate::types::LoopKind::Harden {
            config.limits.max_rounds_harden as i32
        } else {
            config.limits.max_rounds_implement as i32
        };
        let extensions = ((record.max_rounds - default_max) / 10).max(0);

        events.push(FeedEvent {
            id: record.id.to_string(),
            spec_path: record.spec_path.clone(),
            engineer: record.engineer.clone(),
            state: record.state.to_string(),
            rounds: record.round,
            total_tokens: tokens,
            total_cost: cost,
            spec_pr_url: record.spec_pr_url.clone(),
            updated_at: record.updated_at,
            extensions,
        });
    }

    Ok(FeedResponse { events, has_more })
}

// ── Build specs response (FR-13) ──

pub async fn build_specs_response(
    store: &dyn StateStore,
    config: &NautiloopConfig,
    spec_path: &str,
    limit: usize,
) -> crate::error::Result<SpecsResponse> {
    let all_loops = store
        .get_loops_for_engineer(None, true, true)
        .await?;

    let mut matching: Vec<&LoopRecord> = all_loops
        .iter()
        .filter(|l| l.spec_path == spec_path)
        .collect();
    matching.sort_by_key(|r| std::cmp::Reverse(r.created_at));

    let total_runs = matching.len() as u64;
    let mut converged = 0u64;
    let mut terminal = 0u64;
    let mut rounds_sum = 0i64;
    let mut total_cost_sum = 0.0f64;
    let mut has_cost = false;

    // Single pass: compute rounds/cost once per loop, build both runs slice and aggregates
    let mut runs = Vec::with_capacity(matching.len().min(limit));
    for (i, record) in matching.iter().enumerate() {
        let rounds = store.get_rounds(record.id).await?;
        let (_, cost) = compute_loop_totals(&rounds, record, config);

        // Build the runs response slice for the first `limit` items
        if i < limit {
            runs.push(SpecRun {
                id: record.id.to_string(),
                engineer: record.engineer.clone(),
                state: record.state.to_string(),
                rounds: record.round,
                total_cost: cost,
                branch: record.branch.clone(),
                created_at: record.created_at,
            });
        }

        // Aggregate across all matching loops
        if record.state.is_terminal() {
            terminal += 1;
            rounds_sum += record.round as i64;
            if matches!(
                record.state,
                LoopState::Converged | LoopState::Hardened | LoopState::Shipped
            ) {
                converged += 1;
            }
        }
        if let Some(c) = cost {
            total_cost_sum += c;
            has_cost = true;
        }
    }

    Ok(SpecsResponse {
        spec_path: spec_path.to_string(),
        runs,
        aggregates: SpecAggregates {
            total_runs,
            converge_rate: if terminal > 0 {
                Some(converged as f64 / terminal as f64)
            } else {
                None
            },
            avg_rounds: if terminal > 0 {
                Some(rounds_sum as f64 / terminal as f64)
            } else {
                None
            },
            total_cost: if has_cost {
                Some(total_cost_sum)
            } else {
                None
            },
        },
    })
}

// ── Build stats response (FR-14) ──

pub async fn build_stats_response(
    store: &dyn StateStore,
    config: &NautiloopConfig,
    window_str: &str,
) -> crate::error::Result<StatsResponse> {
    let now = Utc::now();
    let window_days: i64 = match window_str {
        "24h" => 1,
        "30d" => 30,
        _ => 7, // default 7d
    };
    let cutoff = now - Duration::days(window_days);

    let all_loops = store
        .get_loops_for_engineer(None, true, true)
        .await?;

    let window_loops: Vec<&LoopRecord> = all_loops
        .iter()
        .filter(|l| l.created_at > cutoff)
        .collect();

    // Headline
    let mut total_cost = 0.0f64;
    let mut has_cost = false;
    let mut terminal_count = 0u64;
    let mut converged_count = 0u64;
    let mut rounds_sum = 0i64;

    // Per-engineer
    let mut eng_map: HashMap<String, (u64, f64, u64, u64)> = HashMap::new(); // (loops, cost, converged, terminal)
    // Per-spec
    let mut spec_map: HashMap<String, (u64, f64, u64, u64, i64)> = HashMap::new(); // (runs, cost, converged, terminal, rounds_sum)
    // Time series
    let mut day_map: HashMap<String, (u64, u64, u64)> = HashMap::new(); // (started, converged, failed)

    for record in &window_loops {
        let rounds = store.get_rounds(record.id).await?;
        let (_, cost) = compute_loop_totals(&rounds, record, config);
        let cost_val = cost.unwrap_or(0.0);
        if cost.is_some() {
            has_cost = true;
        }
        total_cost += cost_val;

        if record.state.is_terminal() {
            terminal_count += 1;
            rounds_sum += record.round as i64;
            if matches!(
                record.state,
                LoopState::Converged | LoopState::Hardened | LoopState::Shipped
            ) {
                converged_count += 1;
            }
        }

        // Per-engineer
        let eng = eng_map.entry(record.engineer.clone()).or_default();
        eng.0 += 1;
        eng.1 += cost_val;
        if matches!(
            record.state,
            LoopState::Converged | LoopState::Hardened | LoopState::Shipped
        ) {
            eng.2 += 1;
        }
        if record.state.is_terminal() {
            eng.3 += 1;
        }

        // Per-spec
        let spec = spec_map.entry(record.spec_path.clone()).or_default();
        spec.0 += 1;
        spec.1 += cost_val;
        if matches!(
            record.state,
            LoopState::Converged | LoopState::Hardened | LoopState::Shipped
        ) {
            spec.2 += 1;
        }
        if record.state.is_terminal() {
            spec.3 += 1;
            spec.4 += record.round as i64;
        }

        // Time series
        let day = record.created_at.format("%Y-%m-%d").to_string();
        let ds = day_map.entry(day).or_default();
        ds.0 += 1;
        if matches!(
            record.state,
            LoopState::Converged | LoopState::Hardened | LoopState::Shipped
        ) {
            ds.1 += 1;
        }
        if matches!(record.state, LoopState::Failed | LoopState::Cancelled) {
            ds.2 += 1;
        }
    }

    let mut per_engineer: Vec<EngineerStats> = eng_map
        .into_iter()
        .map(|(engineer, (loops, cost, converged, terminal))| EngineerStats {
            engineer,
            loops,
            cost: if has_cost { Some(cost) } else { None },
            converge_rate: if terminal > 0 {
                Some(converged as f64 / terminal as f64)
            } else {
                None
            },
        })
        .collect();
    per_engineer.sort_by_key(|e| std::cmp::Reverse(e.loops));

    let mut per_spec: Vec<SpecStats> = spec_map
        .into_iter()
        .map(
            |(spec_path, (runs, cost, converged, terminal, rsum))| SpecStats {
                spec_path,
                runs,
                cost: if has_cost { Some(cost) } else { None },
                converge_rate: if terminal > 0 {
                    Some(converged as f64 / terminal as f64)
                } else {
                    None
                },
                avg_rounds: if terminal > 0 {
                    Some(rsum as f64 / terminal as f64)
                } else {
                    None
                },
            },
        )
        .collect();
    per_spec.sort_by_key(|s| std::cmp::Reverse(s.runs));
    per_spec.truncate(10);

    let mut time_series: Vec<DayStats> = day_map
        .into_iter()
        .map(|(date, (started, converged, failed))| DayStats {
            date,
            started,
            converged,
            failed,
        })
        .collect();
    time_series.sort_by(|a, b| a.date.cmp(&b.date));

    Ok(StatsResponse {
        window: window_str.to_string(),
        headline: StatsHeadline {
            total_loops: window_loops.len() as u64,
            total_cost: if has_cost { Some(total_cost) } else { None },
            converge_rate: if terminal_count > 0 {
                Some(converged_count as f64 / terminal_count as f64)
            } else {
                None
            },
            avg_rounds: if terminal_count > 0 {
                Some(rounds_sum as f64 / terminal_count as f64)
            } else {
                None
            },
        },
        per_engineer,
        per_spec,
        time_series,
    })
}

// Need Deserialize for cache clone via serde
impl<'de> Deserialize<'de> for StatsResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Inner {
            window: String,
            headline: InnerHeadline,
            per_engineer: Vec<InnerEngineerStats>,
            per_spec: Vec<InnerSpecStats>,
            time_series: Vec<InnerDayStats>,
        }
        #[derive(Deserialize)]
        struct InnerHeadline {
            total_loops: u64,
            total_cost: Option<f64>,
            converge_rate: Option<f64>,
            avg_rounds: Option<f64>,
        }
        #[derive(Deserialize)]
        struct InnerEngineerStats {
            engineer: String,
            loops: u64,
            cost: Option<f64>,
            converge_rate: Option<f64>,
        }
        #[derive(Deserialize)]
        struct InnerSpecStats {
            spec_path: String,
            runs: u64,
            cost: Option<f64>,
            converge_rate: Option<f64>,
            avg_rounds: Option<f64>,
        }
        #[derive(Deserialize)]
        struct InnerDayStats {
            date: String,
            started: u64,
            converged: u64,
            failed: u64,
        }

        let inner = Inner::deserialize(deserializer)?;
        Ok(StatsResponse {
            window: inner.window,
            headline: StatsHeadline {
                total_loops: inner.headline.total_loops,
                total_cost: inner.headline.total_cost,
                converge_rate: inner.headline.converge_rate,
                avg_rounds: inner.headline.avg_rounds,
            },
            per_engineer: inner
                .per_engineer
                .into_iter()
                .map(|e| EngineerStats {
                    engineer: e.engineer,
                    loops: e.loops,
                    cost: e.cost,
                    converge_rate: e.converge_rate,
                })
                .collect(),
            per_spec: inner
                .per_spec
                .into_iter()
                .map(|s| SpecStats {
                    spec_path: s.spec_path,
                    runs: s.runs,
                    cost: s.cost,
                    converge_rate: s.converge_rate,
                    avg_rounds: s.avg_rounds,
                })
                .collect(),
            time_series: inner
                .time_series
                .into_iter()
                .map(|d| DayStats {
                    date: d.date,
                    started: d.started,
                    converged: d.converged,
                    failed: d.failed,
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_cost() {
        let tu = TokenUsage {
            input: 1_000_000,
            output: 500_000,
        };
        let pricing = PricingConfig {
            models: HashMap::from([(
                "claude-sonnet-4-20250514".to_string(),
                ModelPricing {
                    input_per_million: 3.0,
                    output_per_million: 15.0,
                },
            )]),
        };
        let cost = compute_cost(&tu, "claude-sonnet-4-20250514", &pricing).unwrap();
        assert!((cost - 10.5).abs() < 0.001); // 3.0 + 7.5
    }

    #[test]
    fn test_compute_cost_unknown_model() {
        let tu = TokenUsage {
            input: 1000,
            output: 500,
        };
        let pricing = PricingConfig {
            models: HashMap::new(),
        };
        assert!(compute_cost(&tu, "unknown-model", &pricing).is_none());
    }
}
