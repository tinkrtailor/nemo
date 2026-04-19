use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use super::aggregate::{self, FleetSummaryCache, StatsCache};
use crate::util::extract_cookie_value;
use super::templates;
use crate::api::AppState;
use crate::error::NautiloopError;

/// Extended app state for dashboard endpoints.
#[derive(Clone)]
pub struct DashboardState {
    pub app: AppState,
    pub fleet_cache: Arc<FleetSummaryCache>,
    pub stats_cache: Arc<StatsCache>,
}

// ── Login ──

pub async fn login_page() -> Html<String> {
    Html(templates::render_login(None))
}

#[derive(Deserialize)]
pub struct LoginForm {
    pub api_key: String,
    pub engineer_name: String,
}

pub async fn login_submit(
    axum::Form(form): axum::Form<LoginForm>,
) -> Response {
    let expected_key = match std::env::var("NAUTILOOP_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            return Html(templates::render_login(Some("Server misconfigured")))
                .into_response();
        }
    };

    if form.api_key.is_empty() || form.engineer_name.is_empty() {
        return Html(templates::render_login(Some("Both fields are required")))
            .into_response();
    }

    if !crate::util::constant_time_eq(form.api_key.as_bytes(), expected_key.as_bytes()) {
        return Html(templates::render_login(Some("Invalid API key")))
            .into_response();
    }

    // Validate engineer name: only allow alphanumeric, hyphens, underscores, dots
    // to prevent cookie header injection via semicolons or CR/LF
    if !form
        .engineer_name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Html(templates::render_login(Some(
            "Engineer name may only contain letters, numbers, hyphens, underscores, and dots",
        )))
        .into_response();
    }

    // Determine if localhost (omit Secure flag for local dev)
    // Default to secure (Secure flag ON) — only omit when bind address is explicitly localhost
    let bind_addr = std::env::var("NAUTILOOP_BIND_ADDR").unwrap_or_default();
    let is_localhost = !bind_addr.is_empty()
        && (bind_addr == "127.0.0.1"
            || bind_addr == "[::1]"
            || bind_addr == "localhost");

    let secure_flag = if is_localhost { "" } else { "; Secure" };

    // Validate the API key contains only cookie-safe characters (RFC 6265 §4.1.1).
    // Cookie-values must not contain semicolons, commas, spaces, backslash,
    // double-quotes, or control characters.
    if !form.api_key.bytes().all(|b| {
        b > 0x20
            && b < 0x7F
            && b != b';'
            && b != b','
            && b != b' '
            && b != b'\\'
            && b != b'"'
    }) {
        return Html(templates::render_login(Some(
            "API key contains characters not safe for cookies",
        )))
        .into_response();
    }

    let key_cookie = format!(
        "nautiloop_api_key={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=604800{}",
        form.api_key, secure_flag
    );
    let engineer_cookie = format!(
        "nautiloop_engineer={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=604800{}",
        form.engineer_name, secure_flag
    );

    let mut response = Redirect::to("/dashboard").into_response();
    let headers = response.headers_mut();
    headers.append("set-cookie", key_cookie.parse().unwrap());
    headers.append("set-cookie", engineer_cookie.parse().unwrap());
    response
}

// ── Logout ──

pub async fn logout() -> Response {
    let mut response = Redirect::to("/dashboard/login").into_response();
    let headers = response.headers_mut();
    headers.append(
        "set-cookie",
        "nautiloop_api_key=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"
            .parse()
            .unwrap(),
    );
    headers.append(
        "set-cookie",
        "nautiloop_engineer=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"
            .parse()
            .unwrap(),
    );
    response
}

// ── Card Grid (main dashboard) ──

pub async fn dashboard_page(
    State(state): State<DashboardState>,
    headers: HeaderMap,
) -> Result<Html<String>, NautiloopError> {
    let viewer = extract_cookie_value(&headers, "nautiloop_engineer")
        .unwrap_or_else(|| "unknown".to_string());

    let data = aggregate::build_dashboard_state(
        state.app.store.as_ref(),
        &state.app.config,
        false, // default: mine
        false, // only recent terminal
        &viewer,
        &state.fleet_cache,
    )
    .await?;

    Ok(Html(templates::render_dashboard(&data, &viewer)))
}

// ── Dashboard State JSON ──

#[derive(Deserialize)]
pub struct StateQuery {
    pub team: Option<bool>,
    pub include_terminal: Option<String>,
}

pub async fn dashboard_state(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Query(query): Query<StateQuery>,
) -> Result<Json<aggregate::DashboardStateResponse>, NautiloopError> {
    let viewer = extract_cookie_value(&headers, "nautiloop_engineer")
        .unwrap_or_else(|| "unknown".to_string());
    let team = query.team.unwrap_or(false);
    let include_all = query.include_terminal.as_deref() == Some("all");

    let data = aggregate::build_dashboard_state(
        state.app.store.as_ref(),
        &state.app.config,
        team,
        include_all,
        &viewer,
        &state.fleet_cache,
    )
    .await?;

    Ok(Json(data))
}

// ── Loop Detail ──

pub async fn loop_detail_page(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Html<String>, NautiloopError> {
    let viewer = extract_cookie_value(&headers, "nautiloop_engineer")
        .unwrap_or_else(|| "unknown".to_string());

    let record = state
        .app
        .store
        .get_loop(id)
        .await?
        .ok_or(NautiloopError::LoopNotFound { id })?;

    let rounds = state.app.store.get_rounds(id).await?;

    // Get last 200 log lines for the log pane (uses LIMIT query, not full load)
    let recent_logs = state.app.store.get_recent_logs(id, 200).await?;
    let log_lines: Vec<String> = recent_logs.iter().map(|l| l.line.clone()).collect();

    Ok(Html(templates::render_loop_detail(
        &record,
        &rounds,
        &log_lines,
        &viewer,
        &state.app.config,
    )))
}

// ── Dashboard Stream (SSE for active, JSON for terminal) ──

pub async fn dashboard_stream(
    State(state): State<DashboardState>,
    Path(id): Path<Uuid>,
) -> Result<Response, NautiloopError> {
    let record = state
        .app
        .store
        .get_loop(id)
        .await?
        .ok_or(NautiloopError::LoopNotFound { id })?;

    if record.state.is_terminal() {
        // Return last 200 lines as JSON (uses LIMIT query, not full load)
        let recent = state.app.store.get_recent_logs(id, 200).await?;
        let lines: Vec<String> = recent.iter().map(|l| l.line.clone()).collect();
        Ok(Json(lines).into_response())
    } else {
        // SSE stream for active loops: send last 200 lines first, then tail new lines.
        // Known limitation: there is a small race window between the get_recent_logs call
        // and the SSE tail starting — logs arriving in that gap may be missed. This is
        // acceptable for a dashboard log tail (not an audit log). The dedup set is bounded
        // by the initial 200 logs so it's not a memory concern.
        // Get recent logs to determine the SSE start offset (uses LIMIT query, not full load).
        let recent_logs = state.app.store.get_recent_logs(id, 200).await?;

        // Determine the cursor timestamp: start SSE from the last recent log's
        // timestamp so we only get new lines going forward (no re-sending).
        let start_ts = recent_logs
            .last()
            .map(|l| l.timestamp)
            .unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH);

        // Collect seen IDs from recent logs for dedup in the tail phase.
        let initial_seen: std::collections::HashSet<uuid::Uuid> =
            recent_logs.iter().map(|l| l.id).collect();

        let store = state.app.store.clone();
        let stream = async_stream::stream! {
            use axum::response::sse::Event;

            // First, emit the recent log lines as SSE events
            for log in &recent_logs {
                let event = crate::types::api::LogEventResponse {
                    timestamp: log.timestamp,
                    stage: log.stage.clone(),
                    round: log.round,
                    line: log.line.clone(),
                };
                if let Ok(json) = serde_json::to_string(&event) {
                    yield Ok::<_, std::convert::Infallible>(Event::default().data(json));
                }
            }

            // Now tail new lines using the same pattern as sse::stream_logs
            let mut cursor_timestamp = start_ts;
            let mut seen_ids = initial_seen.clone();
            let poll_interval = std::time::Duration::from_millis(500);

            loop {
                let new_logs = match store.get_logs_after(id, cursor_timestamp).await {
                    Ok(logs) => logs,
                    Err(e) => {
                        tracing::error!(error = %e, "Dashboard stream: failed to get logs");
                        break;
                    }
                };

                for log in &new_logs {
                    if !seen_ids.insert(log.id) {
                        continue;
                    }
                    if log.timestamp > cursor_timestamp {
                        cursor_timestamp = log.timestamp;
                        seen_ids.clear();
                        seen_ids.insert(log.id);
                    }

                    let event = crate::types::api::LogEventResponse {
                        timestamp: log.timestamp,
                        stage: log.stage.clone(),
                        round: log.round,
                        line: log.line.clone(),
                    };
                    if let Ok(json) = serde_json::to_string(&event) {
                        yield Ok(Event::default().data(json));
                    }
                }

                // Check if loop is terminal
                match store.get_loop(id).await {
                    Ok(Some(rec)) if rec.state.is_terminal() => {
                        yield Ok(Event::default().data(
                            serde_json::json!({
                                "type": "end",
                                "state": rec.state,
                            }).to_string()
                        ));
                        break;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Dashboard stream: failed to check loop state");
                        break;
                    }
                    _ => {}
                }

                tokio::time::sleep(poll_interval).await;
            }
        };

        use axum::response::sse::Sse;
        Ok(Sse::new(stream)
            .keep_alive(
                axum::response::sse::KeepAlive::new()
                    .interval(std::time::Duration::from_secs(15))
            )
            .into_response())
    }
}

// ── Feed (FR-12) ──

#[derive(Deserialize)]
pub struct FeedQuery {
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    /// State filter: "converged" or "failed". Separate from engineer filter
    /// to avoid ambiguity if an engineer is named "converged" or "failed".
    pub state_filter: Option<String>,
    /// Engineer name filter.
    pub engineer_filter: Option<String>,
    /// Legacy combined filter param — maps to state_filter if "converged"/"failed",
    /// otherwise maps to engineer_filter. Kept for backwards compatibility with
    /// bookmarked URLs and localStorage-persisted filters.
    pub filter: Option<String>,
}

pub async fn feed_page(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Query(query): Query<FeedQuery>,
) -> Result<Response, NautiloopError> {
    let accepts_json = wants_json(&headers);
    // Compound cursor: "timestamp|uuid" for deterministic pagination (no boundary duplication).
    let cursor = query.cursor.as_deref().and_then(|c| {
        let parts: Vec<&str> = c.splitn(2, '|').collect();
        if parts.len() == 2 {
            let ts = parts[0].parse::<DateTime<Utc>>().ok()?;
            let id = parts[1].parse::<Uuid>().ok()?;
            Some((ts, id))
        } else {
            // Fallback: legacy timestamp-only cursor (backwards compat during rollout)
            let ts = c.parse::<DateTime<Utc>>().ok()?;
            Some((ts, Uuid::nil()))
        }
    });
    let limit = query.limit.unwrap_or(50).min(100);

    // Resolve filter parameters: prefer explicit state_filter/engineer_filter,
    // fall back to legacy `filter` param for backwards compat.
    let (state_filter, engineer_filter) = resolve_feed_filters(
        query.state_filter.as_deref(),
        query.engineer_filter.as_deref(),
        query.filter.as_deref(),
    );

    let data = aggregate::build_feed_response(
        state.app.store.as_ref(),
        &state.app.config,
        cursor,
        limit,
        state_filter.as_deref(),
        engineer_filter.as_deref(),
    )
    .await?;

    if accepts_json {
        Ok(Json(data).into_response())
    } else {
        let viewer = extract_cookie_value(&headers, "nautiloop_engineer")
            .unwrap_or_else(|| "unknown".to_string());
        Ok(Html(templates::render_feed(
            &data,
            &viewer,
            state_filter.as_deref(),
            engineer_filter.as_deref(),
        )).into_response())
    }
}

/// Resolve feed filter query params into separate state and engineer filters.
/// Prefers explicit `state_filter`/`engineer_filter` over legacy `filter`.
fn resolve_feed_filters(
    state_filter: Option<&str>,
    engineer_filter: Option<&str>,
    legacy_filter: Option<&str>,
) -> (Option<String>, Option<String>) {
    if state_filter.is_some() || engineer_filter.is_some() {
        return (
            state_filter.map(|s| s.to_string()),
            engineer_filter.map(|s| s.to_string()),
        );
    }
    // Legacy: "converged"/"failed" → state filter; anything else → engineer filter.
    match legacy_filter {
        Some("converged") | Some("failed") => (legacy_filter.map(|s| s.to_string()), None),
        Some(eng) if !eng.is_empty() => (None, Some(eng.to_string())),
        _ => (None, None),
    }
}

// ── Specs (FR-13) ──

#[derive(Deserialize)]
pub struct SpecsQuery {
    pub path: Option<String>,
    pub limit: Option<usize>,
}

pub async fn specs_page(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Query(query): Query<SpecsQuery>,
) -> Result<Response, NautiloopError> {
    let accepts_json = wants_json(&headers);
    let spec_path = query.path.as_deref().unwrap_or("");

    if spec_path.is_empty() {
        if accepts_json {
            return Err(NautiloopError::BadRequest(
                "path query parameter required".to_string(),
            ));
        }
        let viewer = extract_cookie_value(&headers, "nautiloop_engineer")
            .unwrap_or_else(|| "unknown".to_string());
        return Ok(Html(templates::render_specs_empty(&viewer)).into_response());
    }

    let data = aggregate::build_specs_response(
        state.app.store.as_ref(),
        &state.app.config,
        spec_path,
        query.limit.unwrap_or(50),
    )
    .await?;

    if accepts_json {
        Ok(Json(data).into_response())
    } else {
        let viewer = extract_cookie_value(&headers, "nautiloop_engineer")
            .unwrap_or_else(|| "unknown".to_string());
        Ok(Html(templates::render_specs(&data, &viewer)).into_response())
    }
}

// ── Stats (FR-14) ──

#[derive(Deserialize)]
pub struct StatsQuery {
    pub window: Option<String>,
}

pub async fn stats_page(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Query(query): Query<StatsQuery>,
) -> Result<Response, NautiloopError> {
    let accepts_json = wants_json(&headers);
    let window = query.window.as_deref().unwrap_or("7d");
    let data = get_or_compute_stats(&state, window).await?;

    if accepts_json {
        Ok(Json(data).into_response())
    } else {
        let viewer = extract_cookie_value(&headers, "nautiloop_engineer")
            .unwrap_or_else(|| "unknown".to_string());
        Ok(Html(templates::render_stats(&data, &viewer)).into_response())
    }
}

async fn get_or_compute_stats(
    state: &DashboardState,
    window: &str,
) -> Result<aggregate::StatsResponse, NautiloopError> {
    // Check cache
    if let Some(cached) = state.stats_cache.get(window).await {
        return Ok(cached);
    }

    let data = aggregate::build_stats_response(
        state.app.store.as_ref(),
        &state.app.config,
        window,
    )
    .await?;

    state
        .stats_cache
        .set(window.to_string(), &data)
        .await;

    Ok(data)
}

// ── Static Assets ──

pub async fn static_css() -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            ("content-type", "text/css; charset=utf-8"),
            ("cache-control", "public, max-age=3600"),
        ],
        include_str!("../../../assets/dashboard.css"),
    )
}

pub async fn static_js() -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            ("content-type", "application/javascript; charset=utf-8"),
            ("cache-control", "public, max-age=3600"),
        ],
        include_str!("../../../assets/dashboard.js"),
    )
}

// ── Helpers ──

fn wants_json(headers: &HeaderMap) -> bool {
    headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::dashboard;
    use crate::config::NautiloopConfig;
    use crate::git::mock::MockGitOperations;
    use crate::state::memory::MemoryStateStore;
    use crate::state::StateStore;
    use crate::types::{LoopKind, LoopRecord, LoopState, RoundRecord, SubState};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    /// Mutex to serialize tests that modify the NAUTILOOP_API_KEY env var,
    /// preventing races when cargo test runs them in parallel.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Set the API key env var under the mutex lock. Returns the guard
    /// which must be held for the duration of the test.
    fn set_test_api_key(key: &str) -> std::sync::MutexGuard<'static, ()> {
        let guard = ENV_MUTEX.lock().unwrap();
        unsafe { std::env::set_var("NAUTILOOP_API_KEY", key) };
        guard
    }

    fn make_test_loop(engineer: &str, state: LoopState) -> LoopRecord {
        LoopRecord {
            id: Uuid::new_v4(),
            engineer: engineer.to_string(),
            spec_path: "specs/test-feature.md".to_string(),
            spec_content_hash: "abcd1234".to_string(),
            branch: format!("agent/{}/test-feature-abcd1234", engineer),
            kind: LoopKind::Implement,
            state,
            sub_state: if state.is_active_stage() {
                Some(SubState::Running)
            } else {
                None
            },
            round: 3,
            max_rounds: 15,
            harden: false,
            harden_only: false,
            auto_approve: false,
            cancel_requested: false,
            approve_requested: false,
            resume_requested: false,
            paused_from_state: None,
            reauth_from_state: None,
            failed_from_state: None,
            failure_reason: None,
            current_sha: Some("abc123".to_string()),
            opencode_session_id: None,
            claude_session_id: None,
            active_job_name: None,
            retry_count: 0,
            ship_mode: false,
            model_implementor: Some("claude-sonnet-4-20250514".to_string()),
            model_reviewer: Some("claude-opus-4-20250514".to_string()),
            merge_sha: None,
            merged_at: None,
            hardened_spec_path: None,
            spec_pr_url: None,
            resolved_default_branch: Some("main".to_string()),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn make_test_round(loop_id: Uuid, round: i32, stage: &str) -> RoundRecord {
        RoundRecord {
            id: Uuid::new_v4(),
            loop_id,
            round,
            stage: stage.to_string(),
            input: None,
            output: Some(serde_json::json!({
                "new_sha": "abc123",
                "token_usage": {"input": 10000, "output": 2000},
                "exit_code": 0,
                "session_id": "test-session"
            })),
            started_at: Some(chrono::Utc::now()),
            completed_at: Some(chrono::Utc::now()),
            duration_secs: Some(120),
            job_name: None,
        }
    }

    fn build_test_app() -> (axum::Router, Arc<MemoryStateStore>) {
        let store = Arc::new(MemoryStateStore::new());
        let git = Arc::new(MockGitOperations::new());
        let app_state = crate::api::AppState {
            store: store.clone(),
            git,
            config: Arc::new(NautiloopConfig::default()),
            kube_client: None,
            pool: None,
        };
        let router = dashboard::build_dashboard_router(app_state.clone())
            .with_state(app_state);
        (router, store)
    }

    #[tokio::test]
    async fn test_login_page_renders() {
        let (app, _) = build_test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/login")
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
        assert!(html.contains("nautiloop"));
        assert!(html.contains("api_key"));
        assert!(html.contains("engineer_name"));
    }

    #[tokio::test]
    async fn test_login_submit_invalid_key() {
        let _guard = set_test_api_key("test-secret-key");
        let (app, _) = build_test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dashboard/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("api_key=wrong-key&engineer_name=alice"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Should re-render login with error
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Invalid API key"));
    }

    #[tokio::test]
    async fn test_login_submit_valid_key() {
        let _guard = set_test_api_key("test-secret-key");
        let (app, _) = build_test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dashboard/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("api_key=test-secret-key&engineer_name=alice"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Should redirect to /dashboard
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(location, "/dashboard");
        // Should set cookies
        let cookies: Vec<&str> = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert!(cookies.iter().any(|c| c.starts_with("nautiloop_api_key=")));
        assert!(cookies.iter().any(|c| c.starts_with("nautiloop_engineer=")));
    }

    #[tokio::test]
    async fn test_dashboard_redirects_without_auth() {
        let _guard = set_test_api_key("test-secret-key");
        let (app, _) = build_test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Should redirect to login
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn test_dashboard_renders_with_cookie() {
        let _guard = set_test_api_key("test-secret-key");
        let (app, store) = build_test_app();

        // Add a test loop
        let loop_record = make_test_loop("alice", LoopState::Implementing);
        store.create_loop(&loop_record).await.unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard")
                    .header("cookie", "nautiloop_api_key=test-secret-key; nautiloop_engineer=alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 131072)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("nautiloop"));
        assert!(html.contains("card-grid"));
    }

    #[tokio::test]
    async fn test_dashboard_state_json() {
        let _guard = set_test_api_key("test-secret-key");
        let (app, store) = build_test_app();

        let loop_record = make_test_loop("alice", LoopState::Implementing);
        let loop_id = loop_record.id;
        store.create_loop(&loop_record).await.unwrap();
        store
            .create_round(&make_test_round(loop_id, 1, "implement"))
            .await
            .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/state?team=true&include_terminal=all")
                    .header("cookie", "nautiloop_api_key=test-secret-key; nautiloop_engineer=alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 131072)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["loops"].is_array());
        assert_eq!(json["loops"].as_array().unwrap().len(), 1);
        assert_eq!(json["viewer"], "alice");
        assert!(json["aggregates"]["total_loops"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn test_loop_detail_page() {
        let _guard = set_test_api_key("test-secret-key");
        let (app, store) = build_test_app();

        let loop_record = make_test_loop("alice", LoopState::Implementing);
        let loop_id = loop_record.id;
        store.create_loop(&loop_record).await.unwrap();
        store
            .create_round(&make_test_round(loop_id, 1, "implement"))
            .await
            .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri(&format!("/dashboard/loops/{}", loop_id))
                    .header("cookie", "nautiloop_api_key=test-secret-key; nautiloop_engineer=alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 131072)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("test-feature.md"));
        assert!(html.contains("Rounds"));
    }

    #[tokio::test]
    async fn test_feed_page() {
        let _guard = set_test_api_key("test-secret-key");
        let (app, store) = build_test_app();

        let mut loop_record = make_test_loop("alice", LoopState::Converged);
        loop_record.state = LoopState::Converged;
        store.create_loop(&loop_record).await.unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/feed")
                    .header("cookie", "nautiloop_api_key=test-secret-key; nautiloop_engineer=alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_feed_json() {
        let _guard = set_test_api_key("test-secret-key");
        let (app, store) = build_test_app();

        let mut loop_record = make_test_loop("alice", LoopState::Converged);
        loop_record.state = LoopState::Converged;
        store.create_loop(&loop_record).await.unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/feed")
                    .header("cookie", "nautiloop_api_key=test-secret-key; nautiloop_engineer=alice")
                    .header("accept", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 131072)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["events"].is_array());
    }

    #[tokio::test]
    async fn test_specs_page_no_path() {
        let _guard = set_test_api_key("test-secret-key");
        let (app, _) = build_test_app();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/specs")
                    .header("cookie", "nautiloop_api_key=test-secret-key; nautiloop_engineer=alice")
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
        assert!(html.contains("No spec path"));
    }

    #[tokio::test]
    async fn test_stats_page() {
        let _guard = set_test_api_key("test-secret-key");
        let (app, _) = build_test_app();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/stats?window=7d")
                    .header("cookie", "nautiloop_api_key=test-secret-key; nautiloop_engineer=alice")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_stats_json() {
        let _guard = set_test_api_key("test-secret-key");
        let (app, _) = build_test_app();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/stats?window=7d")
                    .header("cookie", "nautiloop_api_key=test-secret-key; nautiloop_engineer=alice")
                    .header("accept", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 131072)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["headline"].is_object());
        assert_eq!(json["window"], "7d");
    }

    #[tokio::test]
    async fn test_static_css() {
        let (app, _) = build_test_app();
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
        let ct = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.starts_with("text/css"));
    }

    #[tokio::test]
    async fn test_static_js() {
        let (app, _) = build_test_app();
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
        let ct = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.starts_with("application/javascript"));
    }

    #[tokio::test]
    async fn test_logout() {
        let (app, _) = build_test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dashboard/logout")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookies: Vec<&str> = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert!(cookies.iter().any(|c| c.contains("Max-Age=0")));
    }

    #[tokio::test]
    async fn test_dashboard_state_bearer_auth() {
        let _guard = set_test_api_key("test-secret-key");
        let (app, _) = build_test_app();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/state?team=true")
                    .header("authorization", "Bearer test-secret-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn test_wants_json() {
        let mut headers = HeaderMap::new();
        headers.insert("accept", "application/json".parse().unwrap());
        assert!(wants_json(&headers));

        let headers = HeaderMap::new();
        assert!(!wants_json(&headers));

        let mut headers = HeaderMap::new();
        headers.insert("accept", "text/html".parse().unwrap());
        assert!(!wants_json(&headers));
    }

    #[tokio::test]
    async fn test_stream_terminal_loop_returns_json() {
        let _guard = set_test_api_key("test-secret-key");
        let (app, store) = build_test_app();

        // Create a terminal loop with some logs
        let mut loop_rec = make_test_loop("alice", LoopState::Converged);
        let loop_id = loop_rec.id;
        loop_rec.state = LoopState::Converged;
        loop_rec.sub_state = None;
        store.create_loop(&loop_rec).await.unwrap();

        // Add some log lines
        for i in 0..5 {
            let log = crate::types::LogEvent {
                id: Uuid::new_v4(),
                loop_id,
                round: 1,
                stage: "implement".to_string(),
                timestamp: chrono::Utc::now() + chrono::Duration::milliseconds(i),
                line: format!("log line {}", i),
            };
            store.append_log(&log).await.unwrap();
        }

        let response = app
            .oneshot(
                Request::builder()
                    .uri(&format!("/dashboard/stream/{}", loop_id))
                    .header("cookie", "nautiloop_api_key=test-secret-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let ct = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("application/json"), "terminal loops return JSON, got: {}", ct);

        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let lines: Vec<String> = serde_json::from_slice(&body).unwrap();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], "log line 0");
        assert_eq!(lines[4], "log line 4");
    }

    #[tokio::test]
    async fn test_stream_active_loop_returns_sse() {
        let _guard = set_test_api_key("test-secret-key");
        let (app, store) = build_test_app();

        // Create an active loop with some logs
        let loop_rec = make_test_loop("alice", LoopState::Implementing);
        let loop_id = loop_rec.id;
        store.create_loop(&loop_rec).await.unwrap();

        for i in 0..3 {
            let log = crate::types::LogEvent {
                id: Uuid::new_v4(),
                loop_id,
                round: 1,
                stage: "implement".to_string(),
                timestamp: chrono::Utc::now() + chrono::Duration::milliseconds(i),
                line: format!("active line {}", i),
            };
            store.append_log(&log).await.unwrap();
        }

        let response = app
            .oneshot(
                Request::builder()
                    .uri(&format!("/dashboard/stream/{}", loop_id))
                    .header("cookie", "nautiloop_api_key=test-secret-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let ct = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("text/event-stream"),
            "active loops return SSE, got: {}",
            ct
        );
    }

    #[tokio::test]
    async fn test_stream_terminal_loop_caps_at_200_lines() {
        let _guard = set_test_api_key("test-secret-key");
        let (app, store) = build_test_app();

        let mut loop_rec = make_test_loop("alice", LoopState::Failed);
        loop_rec.sub_state = None;
        let loop_id = loop_rec.id;
        store.create_loop(&loop_rec).await.unwrap();

        // Add 250 log lines
        for i in 0..250 {
            let log = crate::types::LogEvent {
                id: Uuid::new_v4(),
                loop_id,
                round: 1,
                stage: "implement".to_string(),
                timestamp: chrono::Utc::now() + chrono::Duration::milliseconds(i),
                line: format!("line {}", i),
            };
            store.append_log(&log).await.unwrap();
        }

        let response = app
            .oneshot(
                Request::builder()
                    .uri(&format!("/dashboard/stream/{}", loop_id))
                    .header("cookie", "nautiloop_api_key=test-secret-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let lines: Vec<String> = serde_json::from_slice(&body).unwrap();
        // Should cap at 200 lines, showing the last 200
        assert_eq!(lines.len(), 200);
        assert_eq!(lines[0], "line 50");
        assert_eq!(lines[199], "line 249");
    }
}
