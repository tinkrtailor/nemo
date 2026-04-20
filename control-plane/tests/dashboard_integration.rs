//! Integration tests for the dashboard web UI (NFR-6).
//!
//! These tests exercise the full middleware chain with a real in-memory state
//! store, simulating a user session across multiple sequential requests:
//! login, card grid, detail page, action buttons, and auth redirects.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use tower::ServiceExt;
use uuid::Uuid;

use nautiloop_control_plane::api::dashboard::build_dashboard_router_with_key;
use nautiloop_control_plane::api::AppState;
use nautiloop_control_plane::config::NautiloopConfig;
use nautiloop_control_plane::git::mock::MockGitOperations;
use nautiloop_control_plane::state::memory::MemoryStateStore;
use nautiloop_control_plane::types::{LoopKind, LoopRecord, LoopState, RoundRecord};

const API_KEY: &str = "integration-test-key";

fn test_state() -> AppState {
    AppState {
        store: Arc::new(MemoryStateStore::new()),
        git: Arc::new(MockGitOperations::new()),
        config: Arc::new(NautiloopConfig::default()),
        kube_client: None,
        pool: None,
        stats_cache: Arc::new(tokio::sync::RwLock::new(None)),
        fleet_cache: Arc::new(tokio::sync::RwLock::new(None)),
        api_key: Some(API_KEY.to_string()),
    }
}

fn make_loop(engineer: &str, state: LoopState) -> LoopRecord {
    let now = Utc::now();
    let id = Uuid::new_v4();
    let short = &id.to_string()[..8];
    LoopRecord {
        id,
        engineer: engineer.to_string(),
        spec_path: "specs/dashboard-test.md".to_string(),
        spec_content_hash: "hash1234".to_string(),
        branch: format!("agent/{}/dashboard-test-{}", engineer, short),
        kind: LoopKind::Implement,
        state,
        sub_state: None,
        round: 3,
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

fn make_round(loop_id: Uuid, round: i32, stage: &str) -> RoundRecord {
    RoundRecord {
        id: Uuid::new_v4(),
        loop_id,
        round,
        stage: stage.to_string(),
        input: None,
        output: Some(serde_json::json!({
            "token_usage": {"input": 25000, "output": 5000},
            "verdict": {"clean": true},
        })),
        started_at: Some(Utc::now()),
        completed_at: Some(Utc::now()),
        duration_secs: Some(60),
        job_name: Some(format!("job-{}", round)),
    }
}

/// Extract Set-Cookie headers from a response as a Vec<String>.
fn extract_cookies(response: &axum::http::Response<Body>) -> Vec<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .collect()
}

/// Build a cookie header string from Set-Cookie values (extract name=value parts).
fn cookies_to_header(cookies: &[String]) -> String {
    cookies
        .iter()
        .filter_map(|c| c.split(';').next())
        .collect::<Vec<_>>()
        .join("; ")
}

// ── Test: Full login flow with cookie propagation ──

#[tokio::test]
async fn test_login_flow_with_cookie_propagation() {
    let state = test_state();
    let record = make_loop("alice", LoopState::Implementing);
    state.store.create_loop(&record).await.unwrap();

    // Step 1: GET /dashboard without auth → redirect to login
    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dashboard")
                .header("accept", "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        "/dashboard/login"
    );

    // Step 2: GET /dashboard/login → obtain CSRF token
    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dashboard/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let login_cookies = extract_cookies(&resp);
    let csrf_cookie = login_cookies
        .iter()
        .find(|c| c.starts_with("nautiloop_csrf="))
        .expect("CSRF cookie missing");
    let csrf_token = csrf_cookie.split(';').next().unwrap()
        .strip_prefix("nautiloop_csrf=").unwrap();

    // Step 3: POST /dashboard/login with valid credentials
    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state.clone());
    let body = format!(
        "engineer_name=alice&api_key={}&csrf_token={}",
        API_KEY, csrf_token
    );
    let resp = app
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
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        "/dashboard"
    );
    let auth_cookies = extract_cookies(&resp);
    assert!(auth_cookies.iter().any(|c| c.contains("nautiloop_api_key=")));
    assert!(auth_cookies.iter().any(|c| c.contains("nautiloop_engineer=alice")));

    // Step 4: GET /dashboard with auth cookies → 200 with card grid
    let cookie_header = cookies_to_header(&auth_cookies);
    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dashboard?state_filter=all")
                .header("accept", "text/html")
                .header("cookie", &cookie_header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 262144).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("dashboard-test.md"), "Card grid should show spec name");
    assert!(html.contains("IMPLEMENTING"), "Card grid should show loop state");
}

// ── Test: Authenticated card grid with loop data ──

#[tokio::test]
async fn test_card_grid_with_loop_data() {
    let state = test_state();
    let active = make_loop("alice", LoopState::Implementing);
    let converged = make_loop("bob", LoopState::Converged);
    state.store.create_loop(&active).await.unwrap();
    state.store.create_loop(&converged).await.unwrap();

    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dashboard?state_filter=all&team=true")
                .header("cookie", format!("nautiloop_api_key={}", API_KEY))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 262144).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("IMPLEMENTING"));
    assert!(html.contains("CONVERGED"));
    assert!(html.contains("alice"));
    assert!(html.contains("bob"));
}

// ── Test: Detail page with rounds and logs ──

#[tokio::test]
async fn test_detail_page_with_rounds() {
    let state = test_state();
    let record = make_loop("alice", LoopState::Converged);
    let loop_id = record.id;
    state.store.create_loop(&record).await.unwrap();

    // Add two rounds
    let r1 = make_round(loop_id, 1, "implement");
    let r2 = make_round(loop_id, 1, "review");
    state.store.create_round(&r1).await.unwrap();
    state.store.create_round(&r2).await.unwrap();

    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri(&format!("/dashboard/loops/{}", loop_id))
                .header("cookie", format!("nautiloop_api_key={}", API_KEY))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 262144).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("dashboard-test.md"), "Detail page should show spec name");
    assert!(html.contains("CONVERGED"), "Detail page should show state");
    assert!(html.contains("implement"), "Rounds table should show implement stage");
    assert!(html.contains("review"), "Rounds table should show review stage");
}

// ── Test: Approve action button side effect ──

#[tokio::test]
async fn test_approve_action_side_effect() {
    let state = test_state();
    let mut record = make_loop("alice", LoopState::AwaitingApproval);
    record.state = LoopState::AwaitingApproval;
    let loop_id = record.id;
    state.store.create_loop(&record).await.unwrap();

    // Issue approve via dashboard API proxy
    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/dashboard/api/approve/{}", loop_id))
                .header("cookie", format!("nautiloop_api_key={}", API_KEY))
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify side effect: approve_requested flag set in state store
    let updated = state.store.get_loop(loop_id).await.unwrap().unwrap();
    assert!(updated.approve_requested, "approve_requested should be true after approve action");
}

// ── Test: Cancel action button side effect ──

#[tokio::test]
async fn test_cancel_action_side_effect() {
    let state = test_state();
    let record = make_loop("alice", LoopState::Implementing);
    let loop_id = record.id;
    state.store.create_loop(&record).await.unwrap();

    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(&format!("/dashboard/api/cancel/{}", loop_id))
                .header("cookie", format!("nautiloop_api_key={}", API_KEY))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let updated = state.store.get_loop(loop_id).await.unwrap().unwrap();
    assert!(updated.cancel_requested, "cancel_requested should be true after cancel action");
}

// ── Test: Extend action side effect ──

#[tokio::test]
async fn test_extend_action_side_effect() {
    let state = test_state();
    let record = make_loop("alice", LoopState::Failed);
    let loop_id = record.id;
    state.store.create_loop(&record).await.unwrap();

    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/dashboard/api/extend/{}", loop_id))
                .header("cookie", format!("nautiloop_api_key={}", API_KEY))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"add_rounds":10}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let updated = state.store.get_loop(loop_id).await.unwrap().unwrap();
    assert_eq!(updated.max_rounds, 25, "max_rounds should be 15 + 10 = 25 after extend");
    assert!(updated.resume_requested, "resume_requested should be true after extend");
}

// ── Test: Unauthenticated redirect chain ──

#[tokio::test]
async fn test_unauthenticated_redirects() {
    let state = test_state();
    let record = make_loop("alice", LoopState::Implementing);
    let loop_id = record.id;
    state.store.create_loop(&record).await.unwrap();

    // All authed routes should redirect to login when no cookie/bearer is provided
    let detail_route = format!("/dashboard/loops/{}", loop_id);
    let routes = vec![
        "/dashboard",
        &detail_route,
        "/dashboard/feed",
        "/dashboard/stats",
    ];

    for route in routes {
        let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(route)
                    .header("accept", "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SEE_OTHER,
            "Route {} should redirect when unauthenticated",
            route
        );
        assert_eq!(
            resp.headers().get("location").unwrap().to_str().unwrap(),
            "/dashboard/login",
            "Route {} should redirect to /dashboard/login",
            route
        );
    }
}

// ── Test: JSON endpoints return 401 without auth ──

#[tokio::test]
async fn test_json_endpoints_return_401_without_auth() {
    let state = test_state();

    let json_routes = vec![
        "/dashboard/state",
        "/dashboard/feed/json",
        "/dashboard/stats/json",
    ];

    for route in json_routes {
        let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(route)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "JSON route {} should return 401 without auth",
            route
        );
    }
}

// ── Test: Dashboard state endpoint returns correct counts ──

#[tokio::test]
async fn test_dashboard_state_counts_reflect_all_loops() {
    let state = test_state();

    // Create loops in various states
    let implementing = make_loop("alice", LoopState::Implementing);
    let reviewing = make_loop("alice", LoopState::Reviewing);
    let converged = make_loop("bob", LoopState::Converged);
    let failed = make_loop("bob", LoopState::Failed);
    state.store.create_loop(&implementing).await.unwrap();
    state.store.create_loop(&reviewing).await.unwrap();
    state.store.create_loop(&converged).await.unwrap();
    state.store.create_loop(&failed).await.unwrap();

    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dashboard/state?team=true&state_filter=all")
                .header("authorization", format!("Bearer {}", API_KEY))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
    let data: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // Counts should reflect ALL loops
    assert_eq!(data["counts"]["active"], 2, "2 active loops (implementing + reviewing)");
    assert_eq!(data["counts"]["converged"], 1, "1 converged loop");
    assert_eq!(data["counts"]["failed"], 1, "1 failed loop");
    assert_eq!(data["loops"].as_array().unwrap().len(), 4, "all 4 loops in response");
}

// ─��� Test: Feed page with terminal loops ──

#[tokio::test]
async fn test_feed_page_shows_terminal_loops() {
    let state = test_state();
    let mut converged = make_loop("alice", LoopState::Converged);
    converged.spec_pr_url = Some("https://github.com/test/repo/pull/99".to_string());
    let active = make_loop("bob", LoopState::Implementing);
    state.store.create_loop(&converged).await.unwrap();
    state.store.create_loop(&active).await.unwrap();

    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dashboard/feed")
                .header("cookie", format!("nautiloop_api_key={}", API_KEY))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 262144).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    // Feed should show the converged loop but not the active one
    assert!(html.contains("CONVERGED"));
    assert!(html.contains("alice"));
}

// ── Test: Spec history page ──

#[tokio::test]
async fn test_spec_history_page() {
    let state = test_state();
    let converged = make_loop("alice", LoopState::Converged);
    let active = make_loop("bob", LoopState::Implementing);
    state.store.create_loop(&converged).await.unwrap();
    state.store.create_loop(&active).await.unwrap();

    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dashboard/specs/specs/dashboard-test.md")
                .header("cookie", format!("nautiloop_api_key={}", API_KEY))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 262144).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("dashboard-test.md"), "Should show spec path");
    assert!(html.contains("2 runs"), "Should show both runs");
}

// ── Test: Stats page ──

#[tokio::test]
async fn test_stats_page_renders() {
    let state = test_state();
    let record = make_loop("alice", LoopState::Converged);
    state.store.create_loop(&record).await.unwrap();

    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dashboard/stats?window=7d")
                .header("cookie", format!("nautiloop_api_key={}", API_KEY))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 262144).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Stats"));
    assert!(html.contains("alice"));
}

// ── Test: Full session continuity (login → grid → detail → action) ──

#[tokio::test]
async fn test_session_continuity_login_through_action() {
    let state = test_state();
    let mut record = make_loop("alice", LoopState::AwaitingApproval);
    record.state = LoopState::AwaitingApproval;
    let loop_id = record.id;
    state.store.create_loop(&record).await.unwrap();
    let r1 = make_round(loop_id, 1, "implement");
    state.store.create_round(&r1).await.unwrap();

    // Step 1: GET /dashboard/login to get CSRF token
    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dashboard/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let login_page_cookies = extract_cookies(&resp);
    let csrf_token = login_page_cookies
        .iter()
        .find(|c| c.starts_with("nautiloop_csrf="))
        .expect("CSRF cookie on login page")
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("nautiloop_csrf=")
        .unwrap()
        .to_string();

    // Step 2: POST /dashboard/login with valid key — get auth cookies
    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state.clone());
    let body = format!(
        "engineer_name=alice&api_key={}&csrf_token={}",
        API_KEY, csrf_token
    );
    let resp = app
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
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let auth_cookies = extract_cookies(&resp);
    let cookie_header = cookies_to_header(&auth_cookies);

    // Step 3: GET /dashboard — card grid using session cookies
    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dashboard?state_filter=all&team=true")
                .header("accept", "text/html")
                .header("cookie", &cookie_header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), 262144).await.unwrap();
    let html = String::from_utf8(body_bytes.to_vec()).unwrap();
    assert!(html.contains("AWAITING_APPROVAL"), "Grid should show loop state");
    // Extract fresh CSRF token from grid page cookies for action step
    // The auth middleware sets a fresh CSRF on each authed request.

    // Step 4: GET /dashboard/loops/:id — detail page using same session cookies
    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri(&format!("/dashboard/loops/{}", loop_id))
                .header("accept", "text/html")
                .header("cookie", &cookie_header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), 262144).await.unwrap();
    let html = String::from_utf8(body_bytes.to_vec()).unwrap();
    assert!(html.contains("AWAITING_APPROVAL"), "Detail should show loop state");
    assert!(html.contains("Approve"), "Detail should show approve button");
    assert!(html.contains("implement"), "Detail should show round stage");

    // Step 5: POST /dashboard/api/approve/:id — action using same session cookies
    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/dashboard/api/approve/{}", loop_id))
                .header("cookie", &cookie_header)
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Step 6: Verify side effect persisted
    let updated = state.store.get_loop(loop_id).await.unwrap().unwrap();
    assert!(
        updated.approve_requested,
        "approve_requested should be set after full session flow"
    );

    // Step 7: Verify the JSON state endpoint also works with same session cookies
    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dashboard/state?team=true&state_filter=all")
                .header("cookie", &cookie_header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
    let data: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(data["counts"]["active"], 1);
}

// ── Test: Static assets are public (no auth needed) ──

#[tokio::test]
async fn test_static_assets_public() {
    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(test_state());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dashboard/static/dashboard.css")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("content-type").unwrap().to_str().unwrap().contains("text/css"));

    let app = build_dashboard_router_with_key(Some(API_KEY.to_string())).with_state(test_state());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dashboard/static/dashboard.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("content-type").unwrap().to_str().unwrap().contains("javascript"));
}
