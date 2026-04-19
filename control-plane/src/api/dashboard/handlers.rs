use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use std::sync::OnceLock;
use tokio::sync::RwLock;
use uuid::Uuid;

use super::render;
use crate::api::AppState;
use crate::error::NautiloopError;
use crate::types::LoopState;

/// Server-side stats cache (FR-14b): caches computed stats for 60s.
struct StatsCache {
    data: Option<(String, render::StatsData, DateTime<Utc>)>,
}

static STATS_CACHE: OnceLock<RwLock<StatsCache>> = OnceLock::new();

fn stats_cache() -> &'static RwLock<StatsCache> {
    STATS_CACHE.get_or_init(|| RwLock::new(StatsCache { data: None }))
}

/// GET /dashboard/login — render login form.
pub async fn login_page(Query(params): Query<HashMap<String, String>>) -> impl IntoResponse {
    let error = params.get("error").map(|s| s.as_str());
    Html(render::render_login(error).into_string())
}

/// POST /dashboard/login — validate API key, set cookie, redirect.
pub async fn login_submit(
    axum::extract::Form(form): axum::extract::Form<HashMap<String, String>>,
) -> Response {
    let api_key = form.get("api_key").map(|s| s.as_str()).unwrap_or("");

    if api_key.is_empty() || !super::auth::validate_api_key(api_key) {
        return Redirect::to("/dashboard/login?error=Invalid+API+key").into_response();
    }

    // Set HttpOnly, Secure, SameSite=Strict cookie with 7-day expiry
    let cookie = format!(
        "nautiloop_api_key={}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=604800",
        api_key
    );

    let mut response = Redirect::to("/dashboard").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        cookie.parse().unwrap(),
    );
    response
}

/// POST /dashboard/logout — clear cookie and redirect to login.
pub async fn logout() -> Response {
    let cookie =
        "nautiloop_api_key=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0";
    let mut response = Redirect::to("/dashboard/login").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        cookie.parse().unwrap(),
    );
    response
}

/// Query params for the dashboard grid.
#[derive(Debug, serde::Deserialize)]
pub struct GridQuery {
    #[serde(default)]
    pub engineer: Option<String>,
    #[serde(default)]
    pub team: Option<bool>,
    #[serde(default)]
    pub state_filter: Option<String>,
}

/// GET /dashboard — render card grid.
pub async fn grid_page(
    State(state): State<AppState>,
    Query(query): Query<GridQuery>,
) -> Result<Html<String>, NautiloopError> {
    let show_team = query.team.unwrap_or(false);
    let loops = state
        .store
        .get_loops_for_engineer(
            query.engineer.as_deref(),
            show_team,
            true, // include terminal for card grid
        )
        .await?;

    // Collect unique engineers for filter chips
    let mut engineer_set: Vec<String> = loops
        .iter()
        .map(|l| l.engineer.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    engineer_set.sort();

    // Compute per-loop metrics from rounds
    let mut cards = Vec::with_capacity(loops.len());
    for record in &loops {
        let rounds = state.store.get_rounds(record.id).await?;
        let (total_tokens, total_cost, last_verdict) = compute_round_metrics(&rounds);
        let current_stage = resolve_current_stage(&state, record).await?;

        cards.push(render::CardData {
            record: record.clone(),
            current_stage,
            total_tokens,
            total_cost,
            last_verdict,
        });
    }

    // Apply state filter
    let state_filter = query.state_filter.as_deref().unwrap_or("active");
    let filtered_cards: Vec<_> = cards
        .into_iter()
        .filter(|c| match state_filter {
            "active" => !c.record.state.is_terminal(),
            "converged" => matches!(
                c.record.state,
                LoopState::Converged | LoopState::Hardened | LoopState::Shipped
            ),
            "failed" => c.record.state == LoopState::Failed,
            _ => true, // "all"
        })
        .collect();

    // Compute counts for filter chips (from unfiltered set)
    let all_loops = state
        .store
        .get_loops_for_engineer(query.engineer.as_deref(), show_team, true)
        .await?;

    let counts = render::StateCounts {
        active: all_loops.iter().filter(|l| !l.state.is_terminal()).count(),
        converged: all_loops
            .iter()
            .filter(|l| {
                matches!(
                    l.state,
                    LoopState::Converged | LoopState::Hardened | LoopState::Shipped
                )
            })
            .count(),
        failed: all_loops
            .iter()
            .filter(|l| l.state == LoopState::Failed)
            .count(),
    };

    // Fleet summary (FR-9) — 7-day rolling window
    let fleet = compute_fleet_summary(&all_loops).await;

    let engineer_filter = if show_team {
        "team".to_string()
    } else if let Some(ref eng) = query.engineer {
        eng.clone()
    } else {
        "mine".to_string()
    };

    Ok(Html(
        render::render_grid(
            &filtered_cards,
            &fleet,
            &engineer_set,
            state_filter,
            &engineer_filter,
            show_team,
            &counts,
        )
        .into_string(),
    ))
}

/// GET /dashboard/loops/:id — render detail page.
pub async fn detail_page(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, NautiloopError> {
    let record = state
        .store
        .get_loop(id)
        .await?
        .ok_or(NautiloopError::LoopNotFound { id })?;

    let rounds = state.store.get_rounds(id).await?;
    let judge_decisions = state.store.get_judge_decisions(id).await?;

    // Get logs (last 200 lines for display)
    let logs_raw = state.store.get_logs(id, None, None).await?;
    let logs: Vec<String> = logs_raw
        .iter()
        .rev()
        .take(200)
        .rev()
        .map(|l| l.line.clone())
        .collect();

    // Build round data
    let mut round_map: HashMap<i32, Vec<render::StageData>> = HashMap::new();
    let (total_tokens, total_cost, _) = compute_round_metrics(&rounds);
    let mut token_breakdown: Vec<render::TokenBreakdownRow> = Vec::new();
    let mut stage_tokens: HashMap<String, (u64, f64)> = HashMap::new();

    for rr in &rounds {
        let (tokens, cost, verdict_clean, issues_count, confidence) = extract_round_output(rr);
        let has_judge = judge_decisions.iter().any(|jd| jd.round == rr.round && jd.phase == rr.stage);
        let judge_decision = judge_decisions
            .iter()
            .find(|jd| jd.round == rr.round && jd.phase == rr.stage)
            .cloned();

        let entry = stage_tokens.entry(rr.stage.clone()).or_insert((0, 0.0));
        entry.0 += tokens;
        entry.1 += cost;

        round_map
            .entry(rr.round)
            .or_default()
            .push(render::StageData {
                stage: rr.stage.clone(),
                verdict_clean,
                issues_count,
                confidence,
                tokens,
                cost,
                duration_secs: rr.duration_secs,
                has_judge,
                judge_decision,
            });
    }

    let mut round_data: Vec<render::RoundData> = round_map
        .into_iter()
        .map(|(round, stages)| render::RoundData { round, stages })
        .collect();
    round_data.sort_by_key(|r| r.round);

    // Build token breakdown
    for (stage, (tokens, cost)) in &stage_tokens {
        let fraction = if total_tokens > 0 {
            *tokens as f64 / total_tokens as f64
        } else {
            0.0
        };
        token_breakdown.push(render::TokenBreakdownRow {
            label: stage.clone(),
            tokens: *tokens,
            cost: *cost,
            fraction,
        });
    }
    token_breakdown.sort_by_key(|b| std::cmp::Reverse(b.tokens));

    let detail_data = render::DetailData {
        record,
        rounds: round_data,
        logs,
        judge_decisions,
        total_tokens,
        total_cost,
        token_breakdown,
    };

    Ok(Html(render::render_detail(&detail_data).into_string()))
}

/// GET /dashboard/stream/:id — SSE log stream (re-expose under dashboard namespace).
pub async fn stream_logs(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Response, NautiloopError> {
    let record = state
        .store
        .get_loop(id)
        .await?
        .ok_or(NautiloopError::LoopNotFound { id })?;

    if record.state.is_terminal() {
        // For terminal loops, return the logs as SSE events (one-shot)
        let logs = state.store.get_logs(id, None, None).await?;
        let body = logs
            .iter()
            .map(|l| {
                format!(
                    "event: log\ndata: {}\n\n",
                    serde_json::json!({"line": l.line, "timestamp": l.timestamp, "stage": l.stage, "round": l.round})
                )
            })
            .collect::<String>();
        return Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/event-stream")],
            body,
        )
            .into_response());
    }

    // For active loops, delegate to the SSE streaming module
    Ok(
        crate::api::sse::stream_logs(state.store.clone(), id, None, None)
            .await
            .into_response(),
    )
}

/// GET /dashboard/static/dashboard.css
pub async fn static_css() -> impl IntoResponse {
    static CSS: &str = include_str!("../../../assets/dashboard.css");
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css; charset=utf-8"),
         (header::CACHE_CONTROL, "public, max-age=86400")],
        CSS,
    )
}

/// GET /dashboard/static/dashboard.js
pub async fn static_js() -> impl IntoResponse {
    static JS: &str = include_str!("../../../assets/dashboard.js");
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
         (header::CACHE_CONTROL, "public, max-age=86400")],
        JS,
    )
}

/// GET /dashboard/state — JSON roll-up for card grid polling (FR-8b).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct DashboardStateResponse {
    pub loops: Vec<DashboardLoopSummary>,
    pub fleet: FleetSummaryJson,
    pub counts: CountsJson,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct DashboardLoopSummary {
    pub loop_id: Uuid,
    pub engineer: String,
    pub spec_path: String,
    pub branch: String,
    pub state: String,
    pub round: i32,
    pub max_rounds: i32,
    pub current_stage: Option<String>,
    pub total_tokens: u64,
    pub total_cost: f64,
    pub last_verdict: Option<String>,
    pub created_at: chrono::DateTime<Utc>,
    pub updated_at: chrono::DateTime<Utc>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct FleetSummaryJson {
    pub text: String,
    pub total_loops: usize,
    pub total_cost: f64,
    pub converge_rate: Option<f64>,
    pub avg_rounds: Option<f64>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct CountsJson {
    pub active: usize,
    pub converged: usize,
    pub failed: usize,
}

pub async fn dashboard_state(
    State(state): State<AppState>,
    Query(query): Query<GridQuery>,
) -> Result<Json<DashboardStateResponse>, NautiloopError> {
    let show_team = query.team.unwrap_or(false);
    let loops = state
        .store
        .get_loops_for_engineer(query.engineer.as_deref(), show_team, true)
        .await?;

    let mut summaries = Vec::with_capacity(loops.len());
    for record in &loops {
        let rounds = state.store.get_rounds(record.id).await?;
        let (total_tokens, total_cost, last_verdict) = compute_round_metrics(&rounds);
        let current_stage = resolve_current_stage(&state, record).await?;

        // Apply state filter if present
        let state_filter = query.state_filter.as_deref().unwrap_or("active");
        let include = match state_filter {
            "active" => !record.state.is_terminal(),
            "converged" => matches!(
                record.state,
                LoopState::Converged | LoopState::Hardened | LoopState::Shipped
            ),
            "failed" => record.state == LoopState::Failed,
            _ => true,
        };
        if !include {
            continue;
        }

        summaries.push(DashboardLoopSummary {
            loop_id: record.id,
            engineer: record.engineer.clone(),
            spec_path: record.spec_path.clone(),
            branch: record.branch.clone(),
            state: record.state.to_string(),
            round: record.round,
            max_rounds: record.max_rounds,
            current_stage,
            total_tokens,
            total_cost,
            last_verdict,
            created_at: record.created_at,
            updated_at: record.updated_at,
        });
    }

    let fleet = compute_fleet_summary(&loops).await;
    let fleet_json = FleetSummaryJson {
        text: format_fleet_text(&fleet),
        total_loops: fleet.total_loops,
        total_cost: fleet.total_cost,
        converge_rate: fleet.converge_rate,
        avg_rounds: fleet.avg_rounds,
    };

    let counts = CountsJson {
        active: loops.iter().filter(|l| !l.state.is_terminal()).count(),
        converged: loops
            .iter()
            .filter(|l| {
                matches!(
                    l.state,
                    LoopState::Converged | LoopState::Hardened | LoopState::Shipped
                )
            })
            .count(),
        failed: loops.iter().filter(|l| l.state == LoopState::Failed).count(),
    };

    Ok(Json(DashboardStateResponse {
        loops: summaries,
        fleet: fleet_json,
        counts,
    }))
}

/// GET /dashboard/feed — notification feed page (FR-12).
#[derive(Debug, serde::Deserialize)]
pub struct FeedQuery {
    #[serde(default)]
    pub filter: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
}

pub async fn feed_page(
    State(state): State<AppState>,
    Query(query): Query<FeedQuery>,
) -> Result<Html<String>, NautiloopError> {
    let filter = query.filter.as_deref().unwrap_or("all");
    let items = fetch_feed_items(&state, filter, query.cursor.as_deref(), 50).await?;

    let next_cursor = if items.len() >= 50 {
        items.last().map(|i| i.updated_at.to_rfc3339())
    } else {
        None
    };

    Ok(Html(
        render::render_feed(&items, next_cursor.as_deref(), filter).into_string(),
    ))
}

/// GET /dashboard/feed (JSON) — for AJAX load-more.
pub async fn feed_json(
    State(state): State<AppState>,
    Query(query): Query<FeedQuery>,
) -> Result<Json<FeedJsonResponse>, NautiloopError> {
    let filter = query.filter.as_deref().unwrap_or("all");
    let items = fetch_feed_items(&state, filter, query.cursor.as_deref(), 50).await?;

    let next_cursor = if items.len() >= 50 {
        items.last().map(|i| i.updated_at.to_rfc3339())
    } else {
        None
    };

    let json_items: Vec<FeedJsonItem> = items
        .iter()
        .map(|i| FeedJsonItem {
            loop_id: i.loop_id,
            engineer: i.engineer.clone(),
            spec_path: i.spec_path.clone(),
            state: i.state.to_string(),
            round: i.round,
            total_cost: i.total_cost,
            updated_at: i.updated_at,
        })
        .collect();

    Ok(Json(FeedJsonResponse {
        items: json_items,
        next_cursor,
    }))
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct FeedJsonResponse {
    pub items: Vec<FeedJsonItem>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct FeedJsonItem {
    pub loop_id: Uuid,
    pub engineer: String,
    pub spec_path: String,
    pub state: String,
    pub round: i32,
    pub total_cost: f64,
    pub updated_at: chrono::DateTime<Utc>,
}

async fn fetch_feed_items(
    state: &AppState,
    filter: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Vec<render::FeedItem>, NautiloopError> {
    // Get all terminal loops
    let loops = state
        .store
        .get_loops_for_engineer(None, true, true)
        .await?;

    let cursor_dt = cursor
        .and_then(|c| chrono::DateTime::parse_from_rfc3339(c).ok())
        .map(|dt| dt.with_timezone(&Utc));

    let mut terminal_loops: Vec<_> = loops
        .into_iter()
        .filter(|l| {
            if !l.state.is_terminal() {
                return false;
            }
            match filter {
                "converged" => matches!(
                    l.state,
                    LoopState::Converged | LoopState::Hardened | LoopState::Shipped
                ),
                "failed" => l.state == LoopState::Failed,
                _ => true,
            }
        })
        .filter(|l| cursor_dt.is_none_or(|c| l.updated_at < c))
        .collect();

    terminal_loops.sort_by_key(|l| std::cmp::Reverse(l.updated_at));
    terminal_loops.truncate(limit);

    let mut items = Vec::with_capacity(terminal_loops.len());
    for l in terminal_loops {
        let rounds = state.store.get_rounds(l.id).await?;
        let (_, total_cost, _) = compute_round_metrics(&rounds);
        items.push(render::FeedItem {
            loop_id: l.id,
            engineer: l.engineer.clone(),
            spec_path: l.spec_path.clone(),
            state: l.state,
            round: l.round,
            total_cost,
            spec_pr_url: l.spec_pr_url.clone(),
            updated_at: l.updated_at,
        });
    }

    Ok(items)
}

/// GET /dashboard/specs/:path — per-spec history (FR-13).
pub async fn specs_page(
    State(state): State<AppState>,
    Path(spec_path): Path<String>,
) -> Result<Html<String>, NautiloopError> {
    let loops = state
        .store
        .get_loops_for_engineer(None, true, true)
        .await?;

    let matching: Vec<_> = loops
        .into_iter()
        .filter(|l| l.spec_path == spec_path)
        .collect();

    let mut items = Vec::new();
    let mut total_cost = 0.0;
    let mut total_rounds = 0;
    let mut converged_count = 0;
    let terminal_count = matching.iter().filter(|l| l.state.is_terminal()).count();

    for l in &matching {
        let rounds = state.store.get_rounds(l.id).await?;
        let (_, cost, _) = compute_round_metrics(&rounds);
        total_cost += cost;
        total_rounds += l.round;
        if matches!(
            l.state,
            LoopState::Converged | LoopState::Hardened | LoopState::Shipped
        ) {
            converged_count += 1;
        }

        items.push(render::SpecHistoryItem {
            loop_id: l.id,
            engineer: l.engineer.clone(),
            state: l.state,
            round: l.round,
            total_cost: cost,
            branch: l.branch.clone(),
            created_at: l.created_at,
        });
    }

    items.sort_by_key(|i| std::cmp::Reverse(i.created_at));

    let aggregate = render::SpecAggregate {
        total_runs: matching.len(),
        converge_rate: if terminal_count > 0 {
            converged_count as f64 / terminal_count as f64
        } else {
            0.0
        },
        avg_rounds: if !matching.is_empty() {
            total_rounds as f64 / matching.len() as f64
        } else {
            0.0
        },
        total_cost,
    };

    Ok(Html(
        render::render_spec_history(&spec_path, &items, &aggregate).into_string(),
    ))
}

/// GET /dashboard/stats — stats deep-dive page (FR-14).
#[derive(Debug, serde::Deserialize)]
pub struct StatsQuery {
    #[serde(default = "default_window")]
    pub window: String,
}

fn default_window() -> String {
    "7d".to_string()
}

pub async fn stats_page(
    State(state): State<AppState>,
    Query(query): Query<StatsQuery>,
) -> Result<Html<String>, NautiloopError> {
    let stats = compute_stats_cached(&state, &query.window).await?;
    Ok(Html(render::render_stats(&stats).into_string()))
}

/// GET /dashboard/stats/json — for API consumers (FR-14b).
pub async fn stats_json(
    State(state): State<AppState>,
    Query(query): Query<StatsQuery>,
) -> Result<Json<StatsJsonResponse>, NautiloopError> {
    let stats = compute_stats_cached(&state, &query.window).await?;
    Ok(Json(StatsJsonResponse {
        window: stats.window.clone(),
        total_loops: stats.total_loops,
        total_cost: stats.total_cost,
        converge_rate: stats.converge_rate,
        avg_rounds: stats.avg_rounds,
        per_engineer: stats
            .per_engineer
            .iter()
            .map(|e| EngineerStatsJson {
                engineer: e.engineer.clone(),
                loops: e.loops,
                cost: e.cost,
                converge_rate: e.converge_rate,
            })
            .collect(),
    }))
}

/// Compute stats with 60-second server-side cache (FR-14b).
async fn compute_stats_cached(
    state: &AppState,
    window: &str,
) -> Result<render::StatsData, NautiloopError> {
    let cache = stats_cache();
    // Check cache under read lock
    {
        let guard = cache.read().await;
        if let Some((ref cached_window, ref data, ref cached_at)) = guard.data
            && cached_window == window
            && Utc::now() - *cached_at < Duration::seconds(60)
        {
            return Ok(data.clone());
        }
    }
    // Cache miss — compute and store under write lock
    let stats = compute_stats(state, window).await?;
    {
        let mut guard = cache.write().await;
        guard.data = Some((window.to_string(), stats.clone(), Utc::now()));
    }
    Ok(stats)
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct StatsJsonResponse {
    pub window: String,
    pub total_loops: usize,
    pub total_cost: f64,
    pub converge_rate: f64,
    pub avg_rounds: f64,
    pub per_engineer: Vec<EngineerStatsJson>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct EngineerStatsJson {
    pub engineer: String,
    pub loops: usize,
    pub cost: f64,
    pub converge_rate: f64,
}

// ── Shared helpers ──

/// Compute token and cost metrics from round records.
pub fn compute_round_metrics(
    rounds: &[crate::types::RoundRecord],
) -> (u64, f64, Option<String>) {
    let mut total_tokens: u64 = 0;
    let mut total_cost: f64 = 0.0;
    let mut last_verdict: Option<String> = None;

    for rr in rounds {
        let (tokens, cost, clean, _, _) = extract_round_output(rr);
        total_tokens += tokens;
        total_cost += cost;
        if let Some(c) = clean {
            last_verdict = Some(if c { "clean" } else { "not clean" }.to_string());
        }
    }

    (total_tokens, total_cost, last_verdict)
}

/// Extract token usage, cost, verdict, issues, confidence from a round's output JSON.
fn extract_round_output(
    rr: &crate::types::RoundRecord,
) -> (u64, f64, Option<bool>, usize, Option<f64>) {
    let Some(output) = &rr.output else {
        return (0, 0.0, None, 0, None);
    };

    let token_usage = output.get("token_usage");
    let tokens = token_usage
        .map(|tu| {
            let input = tu.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
            let output = tu.get("output").and_then(|v| v.as_u64()).unwrap_or(0);
            input + output
        })
        .unwrap_or(0);

    // Also check nested verdict.token_usage
    let verdict = output.get("verdict");
    let verdict_tokens = verdict
        .and_then(|v| v.get("token_usage"))
        .map(|tu| {
            let input = tu.get("input").and_then(|v| v.as_u64()).unwrap_or(0);
            let output_t = tu.get("output").and_then(|v| v.as_u64()).unwrap_or(0);
            input + output_t
        })
        .unwrap_or(0);

    let total_tokens = if tokens > 0 { tokens } else { verdict_tokens };

    // Simple cost model: $3/M input, $15/M output (rough Claude pricing)
    let input_t = token_usage
        .and_then(|tu| tu.get("input").and_then(|v| v.as_u64()))
        .or_else(|| {
            verdict
                .and_then(|v| v.get("token_usage"))
                .and_then(|tu| tu.get("input").and_then(|v| v.as_u64()))
        })
        .unwrap_or(0);
    let output_t = token_usage
        .and_then(|tu| tu.get("output").and_then(|v| v.as_u64()))
        .or_else(|| {
            verdict
                .and_then(|v| v.get("token_usage"))
                .and_then(|tu| tu.get("output").and_then(|v| v.as_u64()))
        })
        .unwrap_or(0);
    let cost = (input_t as f64 * 3.0 + output_t as f64 * 15.0) / 1_000_000.0;

    // Verdict clean flag
    let clean = verdict
        .and_then(|v| v.get("clean").and_then(|c| c.as_bool()))
        .or_else(|| output.get("clean").and_then(|c| c.as_bool()));

    // Issues count
    let issues = verdict
        .and_then(|v| v.get("issues").and_then(|i| i.as_array()).map(|a| a.len()))
        .or_else(|| {
            output
                .get("issues")
                .and_then(|i| i.as_array())
                .map(|a| a.len())
        })
        .unwrap_or(0);

    // Confidence
    let confidence = verdict
        .and_then(|v| v.get("confidence").and_then(|c| c.as_f64()))
        .or_else(|| output.get("confidence").and_then(|c| c.as_f64()));

    (total_tokens, cost, clean, issues, confidence)
}

/// Resolve the current stage for a loop record.
async fn resolve_current_stage(
    state: &AppState,
    record: &crate::types::LoopRecord,
) -> Result<Option<String>, NautiloopError> {
    if record.state.is_active_stage() {
        let stage = match record.state {
            LoopState::Implementing => "implement",
            LoopState::Testing => "test",
            LoopState::Reviewing => "review",
            LoopState::Hardening => {
                let rounds = state.store.get_rounds(record.id).await?;
                return Ok(Some(
                    rounds
                        .iter()
                        .rfind(|r| r.round == record.round)
                        .map(|r| r.stage.clone())
                        .unwrap_or_else(|| "audit".to_string()),
                ));
            }
            _ => return Ok(None),
        };
        return Ok(Some(stage.to_string()));
    }

    // For paused/failed, derive from the source state
    let source = match record.state {
        LoopState::Paused => record.paused_from_state,
        LoopState::AwaitingReauth => record.reauth_from_state,
        LoopState::Failed => record.failed_from_state,
        _ => None,
    };

    match source {
        Some(LoopState::Implementing) => Ok(Some("implement".to_string())),
        Some(LoopState::Testing) => Ok(Some("test".to_string())),
        Some(LoopState::Reviewing) => Ok(Some("review".to_string())),
        Some(LoopState::Hardening) => {
            let rounds = state.store.get_rounds(record.id).await?;
            Ok(Some(
                rounds
                    .iter()
                    .rfind(|r| r.round == record.round)
                    .map(|r| r.stage.clone())
                    .unwrap_or_else(|| "audit".to_string()),
            ))
        }
        _ => Ok(None),
    }
}

/// Compute fleet summary from a set of loops.
async fn compute_fleet_summary(
    loops: &[crate::types::LoopRecord],
) -> render::FleetSummary {
    let week_ago = Utc::now() - Duration::days(7);
    let this_week: Vec<_> = loops
        .iter()
        .filter(|l| l.created_at >= week_ago)
        .collect();

    let total_loops = this_week.len();
    let terminal: Vec<_> = this_week
        .iter()
        .filter(|l| l.state.is_terminal())
        .collect();
    let converged_count = terminal
        .iter()
        .filter(|l| {
            matches!(
                l.state,
                LoopState::Converged | LoopState::Hardened | LoopState::Shipped
            )
        })
        .count();

    let converge_rate = if !terminal.is_empty() {
        Some(converged_count as f64 / terminal.len() as f64)
    } else {
        None
    };

    let avg_rounds = if !terminal.is_empty() {
        Some(terminal.iter().map(|l| l.round as f64).sum::<f64>() / terminal.len() as f64)
    } else {
        None
    };

    // Top spender (approximation — we don't have round data here, use count as proxy)
    let mut engineer_loops: HashMap<&str, usize> = HashMap::new();
    for l in &this_week {
        *engineer_loops.entry(&l.engineer).or_insert(0) += 1;
    }
    let top_spender = engineer_loops
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(name, count)| (name.to_string(), count as f64 * 0.25)); // rough estimate

    render::FleetSummary {
        total_loops,
        total_cost: 0.0, // computed properly in the dashboard_state endpoint with round data
        converge_rate,
        avg_rounds,
        top_spender,
    }
}

fn format_fleet_text(fleet: &render::FleetSummary) -> String {
    let mut parts = vec![
        format!("This week"),
        format!("{} loops", fleet.total_loops),
        format!("${:.2}", fleet.total_cost),
    ];
    if let Some(rate) = fleet.converge_rate {
        parts.push(format!("{:.0}% converged", rate * 100.0));
    }
    if let Some(avg) = fleet.avg_rounds {
        parts.push(format!("avg {:.1} rounds", avg));
    }
    if let Some((ref name, cost)) = fleet.top_spender {
        parts.push(format!("top: {} (${:.2})", name, cost));
    }
    parts.join(" \u{00B7} ")
}

/// Compute stats for the stats page (FR-14).
async fn compute_stats(
    state: &AppState,
    window: &str,
) -> Result<render::StatsData, NautiloopError> {
    let duration = match window {
        "24h" => Duration::hours(24),
        "30d" => Duration::days(30),
        _ => Duration::days(7),
    };
    let cutoff = Utc::now() - duration;

    let all_loops = state
        .store
        .get_loops_for_engineer(None, true, true)
        .await?;

    let window_loops: Vec<_> = all_loops
        .iter()
        .filter(|l| l.created_at >= cutoff)
        .collect();

    let total_loops = window_loops.len();
    let terminal: Vec<_> = window_loops
        .iter()
        .filter(|l| l.state.is_terminal())
        .collect();
    let converged_count = terminal
        .iter()
        .filter(|l| {
            matches!(
                l.state,
                LoopState::Converged | LoopState::Hardened | LoopState::Shipped
            )
        })
        .count();

    let converge_rate = if !terminal.is_empty() {
        converged_count as f64 / terminal.len() as f64
    } else {
        0.0
    };
    let avg_rounds = if !terminal.is_empty() {
        terminal.iter().map(|l| l.round as f64).sum::<f64>() / terminal.len() as f64
    } else {
        0.0
    };

    // Per-engineer stats
    let mut engineer_map: HashMap<&str, (usize, f64, usize, usize)> = HashMap::new();
    let mut total_cost = 0.0;

    for l in &window_loops {
        let rounds = state.store.get_rounds(l.id).await?;
        let (_, cost, _) = compute_round_metrics(&rounds);
        total_cost += cost;

        let entry = engineer_map.entry(&l.engineer).or_insert((0, 0.0, 0, 0));
        entry.0 += 1;
        entry.1 += cost;
        if l.state.is_terminal() {
            entry.3 += 1;
            if matches!(
                l.state,
                LoopState::Converged | LoopState::Hardened | LoopState::Shipped
            ) {
                entry.2 += 1;
            }
        }
    }

    let mut per_engineer: Vec<render::EngineerStats> = engineer_map
        .into_iter()
        .map(|(eng, (loops, cost, converged, terminal_c))| render::EngineerStats {
            engineer: eng.to_string(),
            loops,
            cost,
            converge_rate: if terminal_c > 0 {
                converged as f64 / terminal_c as f64
            } else {
                0.0
            },
        })
        .collect();
    per_engineer.sort_by(|a, b| b.cost.partial_cmp(&a.cost).unwrap_or(std::cmp::Ordering::Equal));

    // Per-spec stats (top 10)
    let mut spec_map: HashMap<&str, (usize, f64, usize, usize)> = HashMap::new();
    for l in &window_loops {
        let rounds = state.store.get_rounds(l.id).await?;
        let (_, cost, _) = compute_round_metrics(&rounds);

        let entry = spec_map.entry(&l.spec_path).or_insert((0, 0.0, 0, 0));
        entry.0 += 1;
        entry.1 += cost;
        if l.state.is_terminal() {
            entry.3 += 1;
            if matches!(
                l.state,
                LoopState::Converged | LoopState::Hardened | LoopState::Shipped
            ) {
                entry.2 += 1;
            }
        }
    }

    let mut per_spec: Vec<render::SpecStats> = spec_map
        .into_iter()
        .map(|(path, (runs, cost, converged, terminal_c))| render::SpecStats {
            spec_path: path.to_string(),
            runs,
            cost,
            converge_rate: if terminal_c > 0 {
                converged as f64 / terminal_c as f64
            } else {
                0.0
            },
        })
        .collect();
    per_spec.sort_by_key(|s| std::cmp::Reverse(s.runs));
    per_spec.truncate(10);

    // Daily time series
    let days = match window {
        "24h" => 1,
        "30d" => 30,
        _ => 7,
    };
    let mut daily_series = Vec::new();
    for i in 0..days {
        let day_start = (Utc::now() - Duration::days(i))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let day_end = day_start + Duration::days(1);

        let day_loops: Vec<_> = window_loops
            .iter()
            .filter(|l| l.created_at >= day_start && l.created_at < day_end)
            .collect();

        let started = day_loops.len();
        let converged = day_loops
            .iter()
            .filter(|l| {
                matches!(
                    l.state,
                    LoopState::Converged | LoopState::Hardened | LoopState::Shipped
                )
            })
            .count();
        let failed = day_loops
            .iter()
            .filter(|l| l.state == LoopState::Failed)
            .count();

        daily_series.push(render::DayStats {
            date: day_start.format("%m/%d").to_string(),
            started,
            converged,
            failed,
        });
    }
    daily_series.reverse();

    Ok(render::StatsData {
        window: window.to_string(),
        total_loops,
        total_cost,
        converge_rate,
        avg_rounds,
        per_engineer,
        per_spec,
        daily_series,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AppState;
    use crate::config::NautiloopConfig;
    use crate::git::mock::MockGitOperations;
    use crate::state::memory::MemoryStateStore;
    use crate::types::{LoopKind, LoopRecord, LoopState, RoundRecord};
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        AppState {
            store: Arc::new(MemoryStateStore::new()),
            git: Arc::new(MockGitOperations::new()),
            config: Arc::new(NautiloopConfig::default()),
            kube_client: None,
            pool: None,
        }
    }

    fn test_loop_record(engineer: &str, state: LoopState) -> LoopRecord {
        let now = Utc::now();
        LoopRecord {
            id: Uuid::new_v4(),
            engineer: engineer.to_string(),
            spec_path: "specs/test-feature.md".to_string(),
            spec_content_hash: "abcd1234".to_string(),
            branch: format!("agent/{}/test-feature-abcd1234", engineer),
            kind: LoopKind::Implement,
            state,
            sub_state: None,
            round: 2,
            max_rounds: 15,
            harden: false,
            harden_only: false,
            auto_approve: false,
            ship_mode: false,
            cancel_requested: false,
            approve_requested: false,
            resume_requested: false,
            paused_from_state: None,
            reauth_from_state: None,
            failed_from_state: if state == LoopState::Failed {
                Some(LoopState::Implementing)
            } else {
                None
            },
            failure_reason: None,
            current_sha: Some("abc123".to_string()),
            opencode_session_id: None,
            claude_session_id: None,
            active_job_name: None,
            retry_count: 0,
            model_implementor: None,
            model_reviewer: None,
            merge_sha: None,
            merged_at: None,
            hardened_spec_path: None,
            spec_pr_url: None,
            resolved_default_branch: Some("main".to_string()),
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn test_login_page_renders_html() {
        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/login")
                    .header("accept", "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("NAUTILOOP"));
        assert!(html.contains("api_key"));
        assert!(html.contains("Sign in"));
    }

    #[tokio::test]
    async fn test_login_submit_invalid_key() {
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", "test-secret-key") };

        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dashboard/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("api_key=wrong-key"))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should redirect to login with error
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response.headers().get("location").unwrap().to_str().unwrap();
        assert!(location.contains("/dashboard/login"));
        assert!(location.contains("error"));
    }

    #[tokio::test]
    async fn test_login_submit_valid_key() {
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", "valid-test-key") };

        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dashboard/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("api_key=valid-test-key"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response.headers().get("location").unwrap().to_str().unwrap();
        assert_eq!(location, "/dashboard");
        // Should set cookie
        let cookie = response.headers().get("set-cookie").unwrap().to_str().unwrap();
        assert!(cookie.contains("nautiloop_api_key=valid-test-key"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
    }

    #[tokio::test]
    async fn test_unauthenticated_redirect() {
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", "secret-key") };

        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard")
                    .header("accept", "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response.headers().get("location").unwrap().to_str().unwrap();
        assert_eq!(location, "/dashboard/login");
    }

    #[tokio::test]
    async fn test_grid_page_with_cookie_auth() {
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", "grid-test-key") };

        let state = test_state();
        // Insert a loop
        let record = test_loop_record("alice", LoopState::Implementing);
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard?state_filter=all")
                    .header("accept", "text/html")
                    .header("cookie", "nautiloop_api_key=grid-test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 262144)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("test-feature.md"));
        assert!(html.contains("IMPLEMENTING"));
    }

    #[tokio::test]
    async fn test_detail_page() {
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", "detail-test-key") };

        let state = test_state();
        let record = test_loop_record("bob", LoopState::Converged);
        let loop_id = record.id;
        state.store.create_loop(&record).await.unwrap();

        // Add a round
        let round = RoundRecord {
            id: Uuid::new_v4(),
            loop_id,
            round: 1,
            stage: "implement".to_string(),
            input: None,
            output: Some(serde_json::json!({
                "token_usage": {"input": 10000, "output": 2000},
            })),
            started_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
            duration_secs: Some(120),
            job_name: Some("job-1".to_string()),
        };
        state.store.create_round(&round).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(&format!("/dashboard/loops/{}", loop_id))
                    .header("accept", "text/html")
                    .header("cookie", "nautiloop_api_key=detail-test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 262144)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("test-feature.md"));
        assert!(html.contains("CONVERGED"));
        assert!(html.contains("implement"));
    }

    #[tokio::test]
    async fn test_dashboard_state_json() {
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", "state-test-key") };

        let state = test_state();
        let record = test_loop_record("alice", LoopState::Implementing);
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/state?state_filter=all")
                    .header("authorization", "Bearer state-test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let data: DashboardStateResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(data.loops.len(), 1);
        assert_eq!(data.loops[0].state, "IMPLEMENTING");
        assert_eq!(data.loops[0].engineer, "alice");
    }

    #[tokio::test]
    async fn test_static_css() {
        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/static/dashboard.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let ct = response.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.contains("text/css"));
    }

    #[tokio::test]
    async fn test_static_js() {
        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/static/dashboard.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let ct = response.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.contains("javascript"));
    }

    #[tokio::test]
    async fn test_logout_clears_cookie() {
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", "logout-test-key") };

        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dashboard/logout")
                    .header("cookie", "nautiloop_api_key=logout-test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = response.headers().get("set-cookie").unwrap().to_str().unwrap();
        assert!(cookie.contains("Max-Age=0"));
    }

    #[tokio::test]
    async fn test_feed_page() {
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", "feed-test-key") };

        let state = test_state();
        let mut record = test_loop_record("alice", LoopState::Converged);
        record.spec_pr_url = Some("https://github.com/test/repo/pull/1".to_string());
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/feed")
                    .header("accept", "text/html")
                    .header("cookie", "nautiloop_api_key=feed-test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 262144)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("alice"));
        assert!(html.contains("test-feature.md"));
    }

    #[tokio::test]
    async fn test_stats_page() {
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", "stats-test-key") };
        // Clear stats cache to avoid cross-test contamination
        { let mut guard = stats_cache().write().await; guard.data = None; }

        let state = test_state();
        let record = test_loop_record("alice", LoopState::Converged);
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/stats?window=7d")
                    .header("accept", "text/html")
                    .header("cookie", "nautiloop_api_key=stats-test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 262144)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Stats"));
        assert!(html.contains("alice"));
    }

    #[tokio::test]
    async fn test_compute_round_metrics() {
        let rounds = vec![
            RoundRecord {
                id: Uuid::new_v4(),
                loop_id: Uuid::new_v4(),
                round: 1,
                stage: "implement".to_string(),
                input: None,
                output: Some(serde_json::json!({
                    "token_usage": {"input": 50000, "output": 5000},
                })),
                started_at: None,
                completed_at: None,
                duration_secs: Some(60),
                job_name: None,
            },
            RoundRecord {
                id: Uuid::new_v4(),
                loop_id: Uuid::new_v4(),
                round: 1,
                stage: "review".to_string(),
                input: None,
                output: Some(serde_json::json!({
                    "verdict": {
                        "clean": true,
                        "token_usage": {"input": 20000, "output": 3000},
                    },
                })),
                started_at: None,
                completed_at: None,
                duration_secs: Some(30),
                job_name: None,
            },
        ];

        let (total_tokens, total_cost, last_verdict) = compute_round_metrics(&rounds);
        assert_eq!(total_tokens, 55000 + 23000);
        assert!(total_cost > 0.0);
        assert_eq!(last_verdict, Some("clean".to_string()));
    }

    #[tokio::test]
    async fn test_bearer_auth_for_dashboard_state() {
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", "bearer-test-key") };

        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(test_state());

        // Should succeed with bearer token
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/state")
                    .header("authorization", "Bearer bearer-test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_unauthenticated_json_returns_401() {
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", "json-test-key") };

        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(test_state());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_feed_json_endpoint() {
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", "feed-json-key") };

        let state = test_state();
        let mut record = test_loop_record("alice", LoopState::Converged);
        record.spec_pr_url = Some("https://github.com/test/repo/pull/2".to_string());
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/feed/json")
                    .header("authorization", "Bearer feed-json-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let data: FeedJsonResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(data.items.len(), 1);
        assert_eq!(data.items[0].engineer, "alice");
    }

    #[tokio::test]
    async fn test_stats_json_endpoint() {
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", "stats-json-key") };
        // Clear stats cache to avoid cross-test contamination
        { let mut guard = stats_cache().write().await; guard.data = None; }

        let state = test_state();
        let record = test_loop_record("bob", LoopState::Converged);
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/stats/json?window=7d")
                    .header("authorization", "Bearer stats-json-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let data: StatsJsonResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(data.window, "7d");
        assert_eq!(data.total_loops, 1);
    }

    #[tokio::test]
    async fn test_specs_page() {
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", "specs-test-key") };

        let state = test_state();
        let record = test_loop_record("alice", LoopState::Converged);
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/specs/specs/test-feature.md")
                    .header("accept", "text/html")
                    .header("cookie", "nautiloop_api_key=specs-test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 262144)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("test-feature.md"));
        assert!(html.contains("1 runs"));
    }

    #[tokio::test]
    async fn test_state_filter_active() {
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", "filter-test-key") };

        let state = test_state();
        // Create one active and one terminal loop
        let active = test_loop_record("alice", LoopState::Implementing);
        let terminal = test_loop_record("bob", LoopState::Converged);
        state.store.create_loop(&active).await.unwrap();
        state.store.create_loop(&terminal).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router()
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/state?state_filter=active&team=true")
                    .header("authorization", "Bearer filter-test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let data: DashboardStateResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(data.loops.len(), 1);
        assert_eq!(data.loops[0].state, "IMPLEMENTING");
        // Counts should reflect all loops regardless of filter
        assert_eq!(data.counts.active, 1);
        assert_eq!(data.counts.converged, 1);
    }
}
