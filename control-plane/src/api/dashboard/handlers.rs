use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::{Duration, Utc};
use std::collections::HashMap;
use uuid::Uuid;

use super::auth::CsrfToken;
use super::render;
use crate::api::AppState;
use crate::error::NautiloopError;
use crate::state::LoopFlag;
use crate::types::LoopState;

// Stats cache moved to AppState to avoid global state and cross-test contamination.

/// Extract the CSRF token string from the optional extension (set by auth middleware).
fn csrf_from(ext: Option<axum::Extension<CsrfToken>>) -> String {
    ext.map(|e| e.0 .0.clone()).unwrap_or_default()
}


/// GET /dashboard/login — render login form with CSRF token.
pub async fn login_page(Query(params): Query<HashMap<String, String>>) -> Response {
    let error = params.get("error").map(|s| s.as_str());
    let csrf_token = super::auth::generate_csrf_token();
    let html = render::render_login(error, &csrf_token).into_string();

    let csrf_cookie = format!(
        "nautiloop_csrf={}; HttpOnly; SameSite=Strict; Path=/dashboard; Max-Age=86400",
        csrf_token
    );
    let mut response = Html(html).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        csrf_cookie.parse().unwrap(),
    );
    response
}

/// POST /dashboard/login — validate CSRF token + API key, set cookie, redirect.
pub async fn login_submit(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::extract::Form(form): axum::extract::Form<HashMap<String, String>>,
) -> Response {
    // Validate CSRF token (double-submit cookie pattern)
    let csrf_form = form.get("csrf_token").map(|s| s.as_str()).unwrap_or("");
    let csrf_cookie = super::auth::extract_cookie_value(&headers, "nautiloop_csrf")
        .unwrap_or("");
    if !super::auth::validate_csrf_token(csrf_form, csrf_cookie) {
        return Redirect::to("/dashboard/login?error=Invalid+request,+please+try+again").into_response();
    }

    let api_key = form.get("api_key").map(|s| s.as_str()).unwrap_or("");
    let engineer_name = form.get("engineer_name").map(|s| s.as_str()).unwrap_or("");

    // Validate engineer name: non-empty, safe ASCII (alphanumeric, dash, underscore, dot).
    if engineer_name.is_empty()
        || !engineer_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Redirect::to("/dashboard/login?error=Invalid+engineer+name+format").into_response();
    }

    // Validate API key contains only safe ASCII characters (prevent cookie/header injection)
    // Must happen before comparison to reject malformed keys before any other processing.
    if api_key.is_empty()
        || !api_key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Redirect::to("/dashboard/login?error=Invalid+API+key+format").into_response();
    }

    let env_key = std::env::var("NAUTILOOP_API_KEY").ok();
    let expected = state.api_key.as_deref()
        .or(env_key.as_deref())
        .unwrap_or("");
    if !super::auth::validate_api_key_against(api_key, expected) {
        return Redirect::to("/dashboard/login?error=Invalid+API+key").into_response();
    }

    // Set HttpOnly, SameSite=Strict cookies with 7-day expiry.
    // Secure flag controlled by explicit config (defaults to auto-detect from bind_addr).
    let secure_flag = if state.config.dashboard_secure_cookie() { "; Secure" } else { "" };
    let api_cookie = format!(
        "nautiloop_api_key={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=604800{}",
        api_key, secure_flag
    );
    let engineer_cookie = format!(
        "nautiloop_engineer={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=604800{}",
        engineer_name, secure_flag
    );

    let mut response = Redirect::to("/dashboard").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        api_cookie.parse().unwrap(),
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        engineer_cookie.parse().unwrap(),
    );
    response
}

/// POST /dashboard/logout — validate CSRF, clear cookie and redirect to login.
pub async fn logout(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::extract::Form(form): axum::extract::Form<HashMap<String, String>>,
) -> Response {
    // Validate CSRF token (double-submit cookie pattern)
    let csrf_form = form.get("csrf_token").map(|s| s.as_str()).unwrap_or("");
    let csrf_cookie = super::auth::extract_cookie_value(&headers, "nautiloop_csrf")
        .unwrap_or("");
    if !super::auth::validate_csrf_token(csrf_form, csrf_cookie) {
        return Redirect::to("/dashboard/login?error=Invalid+request,+please+try+again").into_response();
    }

    let secure_flag = if state.config.dashboard_secure_cookie() { "; Secure" } else { "" };
    let api_clear = format!(
        "nautiloop_api_key=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{}",
        secure_flag
    );
    let engineer_clear = format!(
        "nautiloop_engineer=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{}",
        secure_flag
    );
    let csrf_clear = "nautiloop_csrf=; HttpOnly; SameSite=Strict; Path=/dashboard; Max-Age=0";
    let mut response = Redirect::to("/dashboard/login").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        api_clear.parse().unwrap(),
    );
    if let Ok(val) = engineer_clear.parse() {
        response.headers_mut().append(header::SET_COOKIE, val);
    }
    if let Ok(val) = csrf_clear.parse() {
        response.headers_mut().append(header::SET_COOKIE, val);
    }
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
    csrf_ext: Option<axum::Extension<CsrfToken>>,
    engineer_ext: Option<axum::Extension<super::auth::EngineerName>>,
) -> Result<Html<String>, NautiloopError> {
    let csrf_token = csrf_from(csrf_ext);
    let viewer_engineer = engineer_ext.map(|e| e.0 .0.clone());
    let show_team = query.team.unwrap_or(false);

    // Resolve engineer filter: explicit query param > 'mine' (default, uses viewer cookie) > all
    let effective_engineer = if let Some(ref eng) = query.engineer {
        Some(eng.clone())
    } else if !show_team {
        // 'Mine' mode: scope to the viewer's own loops (FR-3e)
        viewer_engineer.clone()
    } else {
        None
    };

    let loops = state
        .store
        .get_loops_for_engineer(
            effective_engineer.as_deref(),
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

    // Compute counts from the full set BEFORE filtering (no second DB query)
    let counts = render::StateCounts {
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

    // Fetch rounds for all loops in a single query (avoids N+1)
    let loop_ids: Vec<Uuid> = loops.iter().map(|l| l.id).collect();
    let all_rounds = state.store.get_rounds_for_loops(&loop_ids).await?;

    let mut cards = Vec::with_capacity(loops.len());
    let mut loop_costs: HashMap<Uuid, f64> = HashMap::new();
    for record in &loops {
        let rounds = all_rounds.get(&record.id).map(|v| v.as_slice()).unwrap_or(&[]);
        let (total_tokens, total_cost, last_verdict) = compute_round_metrics(rounds);
        let current_stage = resolve_current_stage_with_rounds(record, rounds);
        loop_costs.insert(record.id, total_cost);

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

    // Fleet summary (FR-9) — always uses ALL loops regardless of current view filter.
    // When show_team=true and no engineer filter, the initial query already fetched
    // all loops — reuse them instead of issuing a second DB query.
    let has_all_loops = show_team && query.engineer.is_none();
    let fleet = if has_all_loops {
        compute_fleet_summary(&loops, &loop_costs)
    } else {
        let all_loops_for_fleet = state
            .store
            .get_loops_for_engineer(None, true, true)
            .await?;
        let mut fleet_costs: HashMap<Uuid, f64> = loop_costs.clone();
        let fleet_loop_ids_needed: Vec<Uuid> = all_loops_for_fleet
            .iter()
            .filter(|l| !fleet_costs.contains_key(&l.id))
            .map(|l| l.id)
            .collect();
        if !fleet_loop_ids_needed.is_empty() {
            let extra_rounds = state.store.get_rounds_for_loops(&fleet_loop_ids_needed).await?;
            for l in &all_loops_for_fleet {
                fleet_costs.entry(l.id).or_insert_with(|| {
                    let rounds = extra_rounds.get(&l.id).map(|v| v.as_slice()).unwrap_or(&[]);
                    let (_, cost, _) = compute_round_metrics(rounds);
                    cost
                });
            }
        }
        compute_fleet_summary(&all_loops_for_fleet, &fleet_costs)
    };

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
            &csrf_token,
        )
        .into_string(),
    ))
}

/// GET /dashboard/loops/:id — render detail page.
pub async fn detail_page(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    csrf_ext: Option<axum::Extension<CsrfToken>>,
) -> Result<Html<String>, NautiloopError> {
    let csrf_token = csrf_from(csrf_ext);
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

    Ok(Html(render::render_detail(&detail_data, &csrf_token).into_string()))
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

// ── Dashboard-namespaced action proxies ──
//
// The main API endpoints (/approve/:id, /cancel/:id, etc.) are protected by the
// main API auth middleware which only accepts Bearer headers. The dashboard JS
// cannot construct Bearer headers because the API key cookie is HttpOnly. These
// proxy routes go through the dashboard auth middleware (which accepts cookies)
// and call the same state-store logic as the main API handlers.

/// POST /dashboard/api/approve/:id — approve a loop (cookie-authed proxy).
pub async fn proxy_approve(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, NautiloopError> {
    let record = state
        .store
        .get_loop(id)
        .await?
        .ok_or(NautiloopError::LoopNotFound { id })?;

    if record.state != LoopState::AwaitingApproval {
        return Err(NautiloopError::InvalidStateTransition {
            action: "approve".to_string(),
            state: record.state.to_string(),
            expected: "AWAITING_APPROVAL".to_string(),
        });
    }

    state
        .store
        .set_loop_flag(id, LoopFlag::Approve, true)
        .await?;

    Ok(Json(serde_json::json!({
        "loop_id": id,
        "state": record.state.to_string(),
        "approve_requested": true
    })))
}

/// DELETE /dashboard/api/cancel/:id — cancel a loop (cookie-authed proxy).
pub async fn proxy_cancel(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, NautiloopError> {
    let record = state
        .store
        .get_loop(id)
        .await?
        .ok_or(NautiloopError::LoopNotFound { id })?;

    if record.state.is_terminal() {
        return Err(NautiloopError::InvalidStateTransition {
            action: "cancel".to_string(),
            state: record.state.to_string(),
            expected: "non-terminal state".to_string(),
        });
    }

    state
        .store
        .set_loop_flag(id, LoopFlag::Cancel, true)
        .await?;

    Ok(Json(serde_json::json!({
        "loop_id": id,
        "state": record.state.to_string(),
        "cancel_requested": true
    })))
}

/// POST /dashboard/api/resume/:id — resume a loop (cookie-authed proxy).
pub async fn proxy_resume(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, NautiloopError> {
    let record = state
        .store
        .get_loop(id)
        .await?
        .ok_or(NautiloopError::LoopNotFound { id })?;

    let resumable = match record.state {
        LoopState::Paused | LoopState::AwaitingReauth => true,
        LoopState::Failed => record.failed_from_state.is_some(),
        _ => false,
    };

    if !resumable {
        let expected = if record.state == LoopState::Failed {
            "FAILED loop has no resumable stage (max rounds exhausted or logical failure)"
                .to_string()
        } else {
            "PAUSED, AWAITING_REAUTH, or transient-FAILED".to_string()
        };
        return Err(NautiloopError::InvalidStateTransition {
            action: "resume".to_string(),
            state: record.state.to_string(),
            expected,
        });
    }

    if record.state == LoopState::Failed
        && let Some(other) = state.store.get_loop_by_branch_any(&record.branch).await?
        && other.id != record.id
        && other.updated_at > record.updated_at
    {
        return Err(NautiloopError::InvalidStateTransition {
            action: "resume".to_string(),
            state: record.state.to_string(),
            expected: format!(
                "branch {} was taken over by a newer loop {} (state {}) — start a fresh loop instead",
                record.branch, other.id, other.state
            ),
        });
    }

    state
        .store
        .set_loop_flag(id, LoopFlag::Resume, true)
        .await?;

    Ok(Json(serde_json::json!({
        "loop_id": id,
        "state": record.state.to_string(),
        "resume_requested": true
    })))
}

/// POST /dashboard/api/extend/:id — extend a failed loop's max_rounds (cookie-authed proxy).
#[derive(Debug, serde::Deserialize)]
pub struct DashboardExtendRequest {
    pub add_rounds: u32,
}

pub async fn proxy_extend(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<DashboardExtendRequest>,
) -> Result<Json<serde_json::Value>, NautiloopError> {
    if req.add_rounds == 0 {
        return Err(NautiloopError::BadRequest(
            "add_rounds must be > 0".to_string(),
        ));
    }

    let record = state
        .store
        .get_loop(id)
        .await?
        .ok_or(NautiloopError::LoopNotFound { id })?;

    if record.state != LoopState::Failed {
        return Err(NautiloopError::InvalidStateTransition {
            action: "extend".to_string(),
            state: record.state.to_string(),
            expected: "FAILED".to_string(),
        });
    }

    let Some(resume_state) = record.failed_from_state else {
        return Err(NautiloopError::InvalidStateTransition {
            action: "extend".to_string(),
            state: record.state.to_string(),
            expected:
                "FAILED loop with a preserved failed_from_state (not extendable — start a fresh loop)"
                    .to_string(),
        });
    };

    if let Some(other) = state.store.get_loop_by_branch_any(&record.branch).await?
        && other.id != record.id
        && other.updated_at > record.updated_at
    {
        return Err(NautiloopError::InvalidStateTransition {
            action: "extend".to_string(),
            state: record.state.to_string(),
            expected: format!(
                "branch {} was taken over by a newer loop {} (state {}) — start a fresh loop instead",
                record.branch, other.id, other.state
            ),
        });
    }

    let prior_max = record.max_rounds as u32;
    let new_max = prior_max + req.add_rounds;

    let mut updated = record.clone();
    updated.max_rounds = new_max as i32;
    updated.failure_reason = None;
    state.store.update_loop(&updated).await?;
    state
        .store
        .set_loop_flag(id, LoopFlag::Resume, true)
        .await?;

    Ok(Json(serde_json::json!({
        "loop_id": id,
        "prior_max_rounds": prior_max,
        "new_max_rounds": new_max,
        "resumed_to_state": resume_state.to_string()
    })))
}

/// GET /dashboard/api/pod-introspect/:id — pod introspection proxy (cookie-authed, FR-5a).
/// Proxies to the same pod introspection logic as the main API endpoint, but
/// goes through the dashboard auth middleware (which accepts cookies) instead of
/// the main API auth middleware (which only accepts Bearer headers).
pub async fn proxy_pod_introspect(
    State(state): State<AppState>,
    Path(loop_id): Path<Uuid>,
) -> Result<Response, NautiloopError> {
    // Delegate to the main pod_introspect handler logic.
    // We call it directly rather than re-implementing to keep the proxy thin.
    let response = super::super::introspect::pod_introspect(
        State(state),
        Path(loop_id),
    )
    .await?;
    Ok(response.into_response())
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FleetSummaryJson {
    pub text: String,
    pub total_loops: usize,
    pub total_cost: f64,
    pub converge_rate: Option<f64>,
    pub avg_rounds: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CountsJson {
    pub active: usize,
    pub converged: usize,
    pub failed: usize,
}

pub async fn dashboard_state(
    State(state): State<AppState>,
    Query(query): Query<GridQuery>,
    engineer_ext: Option<axum::Extension<super::auth::EngineerName>>,
) -> Result<Json<DashboardStateResponse>, NautiloopError> {
    let viewer_engineer = engineer_ext.map(|e| e.0 .0.clone());
    let show_team = query.team.unwrap_or(false);

    // Resolve engineer filter same as grid_page (FR-3e)
    let effective_engineer = if let Some(ref eng) = query.engineer {
        Some(eng.clone())
    } else if !show_team {
        viewer_engineer
    } else {
        None
    };

    let state_filter = query.state_filter.as_deref().unwrap_or("active");

    // Optimization: for the default "active" filter, only fetch non-terminal loops
    // from the DB instead of fetching all (potentially 10,000+) historical loops.
    // The "all" filter still requires include_terminal=true.
    let include_terminal = !matches!(state_filter, "active");
    let loops = state
        .store
        .get_loops_for_engineer(effective_engineer.as_deref(), show_team, include_terminal)
        .await?;

    // Fetch rounds for the view-filtered loops only
    let loop_ids: Vec<Uuid> = loops.iter().map(|l| l.id).collect();
    let all_rounds = state.store.get_rounds_for_loops(&loop_ids).await?;

    let mut summaries = Vec::with_capacity(loops.len());
    for record in &loops {
        let rounds = all_rounds.get(&record.id).map(|v| v.as_slice()).unwrap_or(&[]);
        let (total_tokens, total_cost, last_verdict) = compute_round_metrics(rounds);
        let current_stage = resolve_current_stage_with_rounds(record, rounds);

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

    // Fleet summary + counts use ALL loops (FR-9a). Cache with 10s TTL to
    // avoid fetching unbounded historical data on every 5s poll cycle.
    let (fleet_json, counts) = compute_fleet_cached(&state).await?;

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
    #[serde(default)]
    pub engineer: Option<String>,
}

pub async fn feed_page(
    State(state): State<AppState>,
    Query(query): Query<FeedQuery>,
    csrf_ext: Option<axum::Extension<CsrfToken>>,
) -> Result<Html<String>, NautiloopError> {
    let csrf_token = csrf_from(csrf_ext);
    let filter = query.filter.as_deref().unwrap_or("all");
    let engineer_filter = query.engineer.as_deref();
    let (items, engineers) = fetch_feed_items_with_engineers(&state, filter, engineer_filter, query.cursor.as_deref(), 50).await?;

    let next_cursor = if items.len() >= 50 {
        items.last().map(|i| i.updated_at.to_rfc3339())
    } else {
        None
    };

    Ok(Html(
        render::render_feed(&items, next_cursor.as_deref(), filter, &engineers, engineer_filter, &csrf_token).into_string(),
    ))
}

/// GET /dashboard/feed (JSON) — for AJAX load-more.
pub async fn feed_json(
    State(state): State<AppState>,
    Query(query): Query<FeedQuery>,
) -> Result<Json<FeedJsonResponse>, NautiloopError> {
    let filter = query.filter.as_deref().unwrap_or("all");
    let engineer_filter = query.engineer.as_deref();
    let (items, _) = fetch_feed_items_with_engineers(&state, filter, engineer_filter, query.cursor.as_deref(), 50).await?;

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

/// Fetch feed items and the set of unique engineers (from terminal loops only).
/// Uses `get_terminal_loops` for DB-level filtering (no LIMIT 100 cap).
async fn fetch_feed_items_with_engineers(
    state: &AppState,
    filter: &str,
    engineer: Option<&str>,
    cursor: Option<&str>,
    limit: usize,
) -> Result<(Vec<render::FeedItem>, Vec<String>), NautiloopError> {
    let cursor_dt = cursor
        .and_then(|c| chrono::DateTime::parse_from_rfc3339(c).ok())
        .map(|dt| dt.with_timezone(&Utc));

    // Fetch distinct engineer names via a lightweight query (no full row scan).
    let engineers = state.store.get_distinct_engineers().await?;

    // Fetch terminal loops with state sub-filter applied via a fetch loop.
    // We fetch in batches and filter in memory because the state sub-filter
    // (converged/failed) maps to multiple DB states. The loop ensures we
    // collect `limit` matching items even when many rows don't match.
    let mut terminal_loops = Vec::with_capacity(limit);
    let mut current_cursor = cursor_dt;
    let batch_size = limit + 50;
    let max_fetches = 20; // Safety bound to prevent runaway queries
    for _ in 0..max_fetches {
        let batch = state
            .store
            .get_terminal_loops(engineer, None, None, current_cursor, batch_size)
            .await?;
        let batch_len = batch.len();
        for l in batch {
            let matches = match filter {
                "converged" => matches!(
                    l.state,
                    LoopState::Converged | LoopState::Hardened | LoopState::Shipped
                ),
                "failed" => l.state == LoopState::Failed,
                _ => true,
            };
            if matches {
                current_cursor = Some(l.updated_at);
                terminal_loops.push(l);
                if terminal_loops.len() >= limit {
                    break;
                }
            } else {
                current_cursor = Some(l.updated_at);
            }
        }
        if terminal_loops.len() >= limit || batch_len < batch_size {
            break;
        }
    }

    // Fetch rounds for all terminal loops in one query
    let feed_loop_ids: Vec<Uuid> = terminal_loops.iter().map(|l| l.id).collect();
    let feed_rounds = state.store.get_rounds_for_loops(&feed_loop_ids).await?;

    let mut items = Vec::with_capacity(terminal_loops.len());
    for l in terminal_loops {
        let rounds = feed_rounds.get(&l.id).map(|v| v.as_slice()).unwrap_or(&[]);
        let (_, total_cost, _) = compute_round_metrics(rounds);
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

    Ok((items, engineers))
}

/// GET /dashboard/specs/:path — per-spec history (FR-13).
/// Shows ALL past loops for the spec (including active ones), not just terminal.
pub async fn specs_page(
    State(state): State<AppState>,
    Path(spec_path): Path<String>,
    csrf_ext: Option<axum::Extension<CsrfToken>>,
) -> Result<Html<String>, NautiloopError> {
    let csrf_token = csrf_from(csrf_ext);

    // Fetch loops matching this spec_path efficiently:
    // - Terminal loops: use get_terminal_loops with spec_path filter (DB-level)
    // - Active loops: use get_active_loops_for_spec with spec_path filter (DB-level)
    // Both queries filter at the database level to avoid fetching unrelated loops.
    let (terminal_loops, active_matching) = tokio::join!(
        state.store.get_terminal_loops(None, Some(&spec_path), None, None, 500),
        state.store.get_active_loops_for_spec(&spec_path),
    );
    let terminal_loops = terminal_loops?;
    let active_matching = active_matching?;
    let mut matching = terminal_loops;
    matching.extend(active_matching);

    // Fetch rounds for all matching loops in one query
    let spec_loop_ids: Vec<Uuid> = matching.iter().map(|l| l.id).collect();
    let spec_rounds = state.store.get_rounds_for_loops(&spec_loop_ids).await?;

    let mut items = Vec::new();
    let mut total_cost = 0.0;
    let mut converged_count = 0usize;
    let mut terminal_count = 0usize;
    let mut terminal_rounds_sum = 0f64;

    for l in &matching {
        let rounds = spec_rounds.get(&l.id).map(|v| v.as_slice()).unwrap_or(&[]);
        let (_, cost, _) = compute_round_metrics(rounds);
        total_cost += cost;
        if l.state.is_terminal() {
            terminal_count += 1;
            terminal_rounds_sum += l.round as f64;
        }
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
        avg_rounds: if terminal_count > 0 {
            terminal_rounds_sum / terminal_count as f64
        } else {
            0.0
        },
        total_cost,
    };

    Ok(Html(
        render::render_spec_history(&spec_path, &items, &aggregate, &csrf_token).into_string(),
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
    csrf_ext: Option<axum::Extension<CsrfToken>>,
) -> Result<Html<String>, NautiloopError> {
    let csrf_token = csrf_from(csrf_ext);
    let stats = compute_stats_cached(&state, &query.window).await?;
    Ok(Html(render::render_stats(&stats, &csrf_token).into_string()))
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
/// Cache is per-AppState instance (avoids global static and cross-test contamination).
async fn compute_stats_cached(
    state: &AppState,
    window: &str,
) -> Result<render::StatsData, NautiloopError> {
    // Check cache under read lock
    {
        let guard = state.stats_cache.read().await;
        if let Some((ref cached_window, ref data, ref cached_at)) = *guard
            && cached_window == window
            && Utc::now() - *cached_at < Duration::seconds(60)
        {
            return Ok(data.clone());
        }
    }
    // Cache miss — compute and store under write lock
    let stats = compute_stats(state, window).await?;
    {
        let mut guard = state.stats_cache.write().await;
        *guard = Some((window.to_string(), stats.clone(), Utc::now()));
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

/// Compute fleet summary + counts with a 10-second cache (FR-9a performance).
/// The fleet summary aggregates ALL loops regardless of the current view filter,
/// so it's the same for every poll. Caching avoids fetching unbounded historical
/// data on every 5s card-grid poll cycle.
///
/// Counts (active/converged/failed) are computed from ALL loops regardless of
/// age, not bounded by the 14-day fleet summary window. This ensures active
/// loops older than 14 days still appear in filter chip badges.
async fn compute_fleet_cached(
    state: &AppState,
) -> Result<(FleetSummaryJson, CountsJson), NautiloopError> {
    // Check cache under read lock
    {
        let guard = state.fleet_cache.read().await;
        if let Some((ref fleet, ref counts, ref cached_at)) = *guard
            && Utc::now() - *cached_at < Duration::seconds(10)
        {
            return Ok((fleet.clone(), counts.clone()));
        }
    }
    // Fleet summary: 14-day window (current + prior week for FR-9b trend).
    // Counts use a lightweight GROUP BY query (O(1) result size) instead of
    // fetching all loop records — accurate regardless of total loop count.
    let since = Utc::now() - Duration::days(14);
    let (agg_loops, state_counts) = tokio::join!(
        state.store.get_loops_for_aggregation(since),
        state.store.get_loop_state_counts(),
    );
    let agg_loops = agg_loops?;
    let state_counts = state_counts?;

    let agg_ids: Vec<Uuid> = agg_loops.iter().map(|l| l.id).collect();
    let all_rounds = state.store.get_rounds_for_loops(&agg_ids).await?;
    let mut loop_costs: HashMap<Uuid, f64> = HashMap::new();
    for l in &agg_loops {
        let rounds = all_rounds.get(&l.id).map(|v| v.as_slice()).unwrap_or(&[]);
        let (_, cost, _) = compute_round_metrics(rounds);
        loop_costs.insert(l.id, cost);
    }
    let fleet = compute_fleet_summary(&agg_loops, &loop_costs);
    let fleet_json = FleetSummaryJson {
        text: format_fleet_text(&fleet),
        total_loops: fleet.total_loops,
        total_cost: fleet.total_cost,
        converge_rate: fleet.converge_rate,
        avg_rounds: fleet.avg_rounds,
    };
    // Counts from lightweight GROUP BY query — exact regardless of total loops.
    let get_count = |s: &LoopState| *state_counts.get(s).unwrap_or(&0);
    let counts = CountsJson {
        active: state_counts
            .iter()
            .filter(|(s, _)| !s.is_terminal())
            .map(|(_, c)| c)
            .sum(),
        converged: get_count(&LoopState::Converged)
            + get_count(&LoopState::Hardened)
            + get_count(&LoopState::Shipped),
        failed: get_count(&LoopState::Failed),
    };
    {
        let mut guard = state.fleet_cache.write().await;
        *guard = Some((fleet_json.clone(), counts.clone(), Utc::now()));
    }
    Ok((fleet_json, counts))
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

/// Resolve the current stage for a loop record using pre-fetched rounds.
/// This avoids additional DB queries for Hardening state resolution.
fn resolve_current_stage_with_rounds(
    record: &crate::types::LoopRecord,
    rounds: &[crate::types::RoundRecord],
) -> Option<String> {
    if record.state.is_active_stage() {
        return match record.state {
            LoopState::Implementing => Some("implement".to_string()),
            LoopState::Testing => Some("test".to_string()),
            LoopState::Reviewing => Some("review".to_string()),
            LoopState::Hardening => Some(
                rounds
                    .iter()
                    .rfind(|r| r.round == record.round)
                    .map(|r| r.stage.clone())
                    .unwrap_or_else(|| "audit".to_string()),
            ),
            _ => None,
        };
    }

    // For paused/failed, derive from the source state
    let source = match record.state {
        LoopState::Paused => record.paused_from_state,
        LoopState::AwaitingReauth => record.reauth_from_state,
        LoopState::Failed => record.failed_from_state,
        _ => None,
    };

    match source {
        Some(LoopState::Implementing) => Some("implement".to_string()),
        Some(LoopState::Testing) => Some("test".to_string()),
        Some(LoopState::Reviewing) => Some("review".to_string()),
        Some(LoopState::Hardening) => Some(
            rounds
                .iter()
                .rfind(|r| r.round == record.round)
                .map(|r| r.stage.clone())
                .unwrap_or_else(|| "audit".to_string()),
        ),
        _ => None,
    }
}

/// Compute fleet summary from loops and pre-fetched per-loop costs.
/// `loop_costs` maps loop_id -> total_cost computed from round data.
fn compute_fleet_summary(
    loops: &[crate::types::LoopRecord],
    loop_costs: &HashMap<Uuid, f64>,
) -> render::FleetSummary {
    let week_ago = Utc::now() - Duration::days(7);
    let prior_week_start = week_ago - Duration::days(7);
    let this_week: Vec<_> = loops
        .iter()
        .filter(|l| l.created_at >= week_ago)
        .collect();
    let prior_week: Vec<_> = loops
        .iter()
        .filter(|l| l.created_at >= prior_week_start && l.created_at < week_ago)
        .collect();

    let total_loops = this_week.len();

    // Current window metrics
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

    // Total cost from actual round data
    let total_cost: f64 = this_week
        .iter()
        .map(|l| loop_costs.get(&l.id).copied().unwrap_or(0.0))
        .sum();

    // Top spender by actual cost
    let mut engineer_cost: HashMap<&str, f64> = HashMap::new();
    for l in &this_week {
        let cost = loop_costs.get(&l.id).copied().unwrap_or(0.0);
        *engineer_cost.entry(&l.engineer).or_insert(0.0) += cost;
    }
    let top_spender = engineer_cost
        .into_iter()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(name, cost)| (name.to_string(), cost));

    // Prior-period trends (FR-9b)
    let prior_terminal: Vec<_> = prior_week
        .iter()
        .filter(|l| l.state.is_terminal())
        .collect();
    let prior_converged = prior_terminal
        .iter()
        .filter(|l| {
            matches!(
                l.state,
                LoopState::Converged | LoopState::Hardened | LoopState::Shipped
            )
        })
        .count();
    let prior_converge_rate = if !prior_terminal.is_empty() {
        Some(prior_converged as f64 / prior_terminal.len() as f64)
    } else {
        None
    };
    let prior_avg_rounds = if !prior_terminal.is_empty() {
        Some(prior_terminal.iter().map(|l| l.round as f64).sum::<f64>() / prior_terminal.len() as f64)
    } else {
        None
    };
    let prior_total_cost: f64 = prior_week
        .iter()
        .map(|l| loop_costs.get(&l.id).copied().unwrap_or(0.0))
        .sum();

    // Compute trend deltas (Some only when prior data exists)
    let has_prior = !prior_week.is_empty();
    let converge_rate_trend = if has_prior {
        match (converge_rate, prior_converge_rate) {
            (Some(cur), Some(prev)) => Some(cur - prev),
            _ => None,
        }
    } else {
        None
    };
    let avg_rounds_trend = if has_prior {
        match (avg_rounds, prior_avg_rounds) {
            (Some(cur), Some(prev)) => Some(cur - prev),
            _ => None,
        }
    } else {
        None
    };
    let cost_trend = if has_prior && prior_total_cost > 0.0 {
        Some(total_cost - prior_total_cost)
    } else {
        None
    };

    render::FleetSummary {
        total_loops,
        total_cost,
        converge_rate,
        avg_rounds,
        top_spender,
        converge_rate_trend,
        avg_rounds_trend,
        cost_trend,
    }
}

fn format_fleet_text(fleet: &render::FleetSummary) -> String {
    let mut parts = vec![
        "This week".to_string(),
        format!("{} loops", fleet.total_loops),
    ];

    let cost_str = format!("${:.2}", fleet.total_cost);
    if let Some(delta) = fleet.cost_trend {
        let arrow = if delta > 0.0 { "\u{2191}" } else { "\u{2193}" };
        parts.push(format!("{} {}${:.2}", cost_str, arrow, delta.abs()));
    } else {
        parts.push(cost_str);
    }

    if let Some(rate) = fleet.converge_rate {
        let base = format!("{:.0}%", rate * 100.0);
        if let Some(delta) = fleet.converge_rate_trend {
            let arrow = if delta > 0.0 { "\u{2191}" } else { "\u{2193}" };
            parts.push(format!("{} {}{:.0}% converged", base, arrow, (delta * 100.0).abs()));
        } else {
            parts.push(format!("{} converged", base));
        }
    }
    if let Some(avg) = fleet.avg_rounds {
        let base = format!("avg {:.1}", avg);
        if let Some(delta) = fleet.avg_rounds_trend {
            let arrow = if delta > 0.0 { "\u{2191}" } else { "\u{2193}" }; // consistent: ↑=increased, ↓=decreased
            parts.push(format!("{} {}{:.1} rounds", base, arrow, delta.abs()));
        } else {
            parts.push(format!("{} rounds", base));
        }
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

    // Use time-bounded aggregation query (no row LIMIT) so stats are
    // accurate on long-running deployments with >10k loops.
    let all_loops = state
        .store
        .get_loops_for_aggregation(cutoff)
        .await?;

    let window_loops: Vec<_> = all_loops.iter().collect();

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

    // Fetch rounds for all window loops in a single query
    let stats_loop_ids: Vec<Uuid> = window_loops.iter().map(|l| l.id).collect();
    let stats_rounds = state.store.get_rounds_for_loops(&stats_loop_ids).await?;

    let mut loop_cost_map: HashMap<Uuid, f64> = HashMap::new();
    let mut total_cost = 0.0;
    for l in &window_loops {
        let rounds = stats_rounds.get(&l.id).map(|v| v.as_slice()).unwrap_or(&[]);
        let (_, cost, _) = compute_round_metrics(rounds);
        loop_cost_map.insert(l.id, cost);
        total_cost += cost;
    }

    // Per-engineer stats (single pass)
    let mut engineer_map: HashMap<&str, (usize, f64, usize, usize)> = HashMap::new();
    // Per-spec stats (same single pass)
    let mut spec_map: HashMap<&str, (usize, f64, usize, usize)> = HashMap::new();

    for l in &window_loops {
        let cost = loop_cost_map.get(&l.id).copied().unwrap_or(0.0);
        let is_converged = matches!(
            l.state,
            LoopState::Converged | LoopState::Hardened | LoopState::Shipped
        );

        // Engineer aggregate
        let eng_entry = engineer_map.entry(&l.engineer).or_insert((0, 0.0, 0, 0));
        eng_entry.0 += 1;
        eng_entry.1 += cost;
        if l.state.is_terminal() {
            eng_entry.3 += 1;
            if is_converged {
                eng_entry.2 += 1;
            }
        }

        // Spec aggregate
        let spec_entry = spec_map.entry(&l.spec_path).or_insert((0, 0.0, 0, 0));
        spec_entry.0 += 1;
        spec_entry.1 += cost;
        if l.state.is_terminal() {
            spec_entry.3 += 1;
            if is_converged {
                spec_entry.2 += 1;
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
    // FR-14a: "daily count of loops started vs terminal outcomes"
    // Started uses created_at; terminal outcomes use updated_at (when they terminated).
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

        // Loops started on this day
        let started = window_loops
            .iter()
            .filter(|l| l.created_at >= day_start && l.created_at < day_end)
            .count();

        // Terminal outcomes that landed on this day (by updated_at)
        let converged = window_loops
            .iter()
            .filter(|l| {
                l.updated_at >= day_start
                    && l.updated_at < day_end
                    && matches!(
                        l.state,
                        LoopState::Converged | LoopState::Hardened | LoopState::Shipped
                    )
            })
            .count();
        let failed = window_loops
            .iter()
            .filter(|l| {
                l.updated_at >= day_start
                    && l.updated_at < day_end
                    && l.state == LoopState::Failed
            })
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
        test_state_with_key("test-api-key")
    }

    fn test_state_with_key(key: &str) -> AppState {
        AppState {
            store: Arc::new(MemoryStateStore::new()),
            git: Arc::new(MockGitOperations::new()),
            config: Arc::new(NautiloopConfig::default()),
            kube_client: None,
            pool: None,
            stats_cache: Arc::new(tokio::sync::RwLock::new(None)),
            fleet_cache: Arc::new(tokio::sync::RwLock::new(None)),
            api_key: Some(key.to_string()),
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
        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
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
        use tower::ServiceExt;
        let state = test_state();

        // Step 1: GET the login page to obtain a valid CSRF token
        let app1 = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state.clone());
        let get_response = app1
            .oneshot(
                Request::builder()
                    .uri("/dashboard/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let csrf_cookie_hdr = get_response.headers().get("set-cookie").unwrap().to_str().unwrap();
        let csrf_token = csrf_cookie_hdr
            .split(';').next().unwrap()
            .strip_prefix("nautiloop_csrf=").unwrap();

        // Step 2: POST with valid CSRF but wrong API key
        let app2 = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);
        let body = format!("engineer_name=alice&api_key=wrong-key&csrf_token={}", csrf_token);
        let response = app2
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dashboard/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("cookie", format!("nautiloop_csrf={}", csrf_token))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should redirect to login with the specific "Invalid+API+key" error
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response.headers().get("location").unwrap().to_str().unwrap();
        assert!(location.contains("/dashboard/login"));
        assert!(location.contains("Invalid+API+key"), "expected 'Invalid+API+key' in redirect, got: {}", location);
    }

    #[tokio::test]
    async fn test_login_submit_valid_key() {
        use tower::ServiceExt;
        let state = test_state();

        // Step 1: GET the login page to obtain CSRF token cookie
        let app1 = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state.clone());
        let get_response = app1
            .oneshot(
                Request::builder()
                    .uri("/dashboard/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let csrf_cookie_hdr = get_response.headers().get("set-cookie").unwrap().to_str().unwrap();
        let csrf_token = csrf_cookie_hdr
            .split(';').next().unwrap()
            .strip_prefix("nautiloop_csrf=").unwrap();

        // Step 2: POST with CSRF token + API key
        let app2 = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);
        let body = format!("engineer_name=alice&api_key=test-api-key&csrf_token={}", csrf_token);
        let response = app2
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dashboard/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("cookie", format!("nautiloop_csrf={}", csrf_token))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response.headers().get("location").unwrap().to_str().unwrap();
        assert_eq!(location, "/dashboard");
        // Should set both api_key and engineer cookies
        let cookies: Vec<_> = response.headers().get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        let api_cookie = cookies.iter().find(|c| c.contains("nautiloop_api_key=test-api-key")).unwrap();
        assert!(api_cookie.contains("HttpOnly"));
        assert!(api_cookie.contains("SameSite=Strict"));
        let eng_cookie = cookies.iter().find(|c| c.contains("nautiloop_engineer=alice")).unwrap();
        assert!(eng_cookie.contains("HttpOnly"));
    }

    #[tokio::test]
    async fn test_unauthenticated_redirect() {
        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
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
        let state = test_state();
        // Insert a loop
        let record = test_loop_record("alice", LoopState::Implementing);
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard?state_filter=all")
                    .header("accept", "text/html")
                    .header("cookie", "nautiloop_api_key=test-api-key")
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

        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(&format!("/dashboard/loops/{}", loop_id))
                    .header("accept", "text/html")
                    .header("cookie", "nautiloop_api_key=test-api-key")
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
        let state = test_state();
        let record = test_loop_record("alice", LoopState::Implementing);
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/state?state_filter=all")
                    .header("authorization", "Bearer test-api-key")
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
        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
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
        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
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
        let csrf_token = "test-csrf-token-12345678";
        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(test_state());

        let body = format!("csrf_token={}", csrf_token);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dashboard/logout")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("cookie", format!("nautiloop_api_key=test-api-key; nautiloop_csrf={}", csrf_token))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookies: Vec<_> = response.headers().get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        let api_cookie = cookies.iter().find(|c| c.contains("nautiloop_api_key=")).unwrap();
        assert!(api_cookie.contains("Max-Age=0"));
    }

    #[tokio::test]
    async fn test_feed_page() {
        let state = test_state();
        let mut record = test_loop_record("alice", LoopState::Converged);
        record.spec_pr_url = Some("https://github.com/test/repo/pull/1".to_string());
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/feed")
                    .header("accept", "text/html")
                    .header("cookie", "nautiloop_api_key=test-api-key")
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
        let state = test_state();
        let record = test_loop_record("alice", LoopState::Converged);
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/stats?window=7d")
                    .header("accept", "text/html")
                    .header("cookie", "nautiloop_api_key=test-api-key")
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
        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(test_state());

        // Should succeed with bearer token
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/state")
                    .header("authorization", "Bearer test-api-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_unauthenticated_json_returns_401() {
        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
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
        let state = test_state();
        let mut record = test_loop_record("alice", LoopState::Converged);
        record.spec_pr_url = Some("https://github.com/test/repo/pull/2".to_string());
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/feed/json")
                    .header("authorization", "Bearer test-api-key")
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
        let state = test_state();
        let record = test_loop_record("bob", LoopState::Converged);
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/stats/json?window=7d")
                    .header("authorization", "Bearer test-api-key")
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
        let state = test_state();
        let record = test_loop_record("alice", LoopState::Converged);
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/specs/specs/test-feature.md")
                    .header("accept", "text/html")
                    .header("cookie", "nautiloop_api_key=test-api-key")
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
        let state = test_state();
        // Create one active and one terminal loop
        let active = test_loop_record("alice", LoopState::Implementing);
        let terminal = test_loop_record("bob", LoopState::Converged);
        state.store.create_loop(&active).await.unwrap();
        state.store.create_loop(&terminal).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/state?state_filter=active&team=true")
                    .header("authorization", "Bearer test-api-key")
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

    #[tokio::test]
    async fn test_proxy_approve_with_cookie_auth() {
        let state = test_state();
        let mut record = test_loop_record("alice", LoopState::AwaitingApproval);
        record.state = LoopState::AwaitingApproval;
        let loop_id = record.id;
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&format!("/dashboard/api/approve/{}", loop_id))
                    .header("cookie", "nautiloop_api_key=test-api-key")
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["approve_requested"], true);
    }

    #[tokio::test]
    async fn test_proxy_cancel_with_cookie_auth() {
        let state = test_state();
        let record = test_loop_record("alice", LoopState::Implementing);
        let loop_id = record.id;
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(&format!("/dashboard/api/cancel/{}", loop_id))
                    .header("cookie", "nautiloop_api_key=test-api-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["cancel_requested"], true);
    }

    #[tokio::test]
    async fn test_proxy_cancel_terminal_loop_rejected() {
        let state = test_state();
        let record = test_loop_record("alice", LoopState::Converged);
        let loop_id = record.id;
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(&format!("/dashboard/api/cancel/{}", loop_id))
                    .header("cookie", "nautiloop_api_key=test-api-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should fail — terminal loops can't be cancelled
        assert_ne!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_proxy_resume_with_cookie_auth() {
        let state = test_state();
        let record = test_loop_record("alice", LoopState::Failed);
        let loop_id = record.id;
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&format!("/dashboard/api/resume/{}", loop_id))
                    .header("cookie", "nautiloop_api_key=test-api-key")
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["resume_requested"], true);
    }

    #[tokio::test]
    async fn test_proxy_extend_with_cookie_auth() {
        let state = test_state();
        let record = test_loop_record("alice", LoopState::Failed);
        let loop_id = record.id;
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&format!("/dashboard/api/extend/{}", loop_id))
                    .header("cookie", "nautiloop_api_key=test-api-key")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"add_rounds":10}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["new_max_rounds"], 25); // 15 default + 10
    }

    #[tokio::test]
    async fn test_proxy_actions_require_auth() {
        let state = test_state();
        let record = test_loop_record("alice", LoopState::Implementing);
        let loop_id = record.id;
        state.store.create_loop(&record).await.unwrap();

        let app = crate::api::dashboard::build_dashboard_router_with_key(Some("test-api-key".to_string()))
            .with_state(state);

        // No cookie or bearer → should be unauthorized
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(&format!("/dashboard/api/cancel/{}", loop_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
