use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use uuid::Uuid;

use super::AppState;
use crate::error::NemoError;
use crate::state::LoopFlag;
use crate::types::api::{
    ApproveResponse, CancelResponse, InspectResponse, LogsQuery, LoopSummary, ResumeResponse,
    RoundSummary, StartRequest, StartResponse, StatusQuery, StatusResponse,
};
use crate::types::{generate_branch_name, LoopKind, LoopRecord, LoopState};

/// POST /start - Submit a spec for processing.
pub async fn start(
    State(state): State<AppState>,
    Json(req): Json<StartRequest>,
) -> Result<impl IntoResponse, NemoError> {
    // If ship_mode, check that ship is allowed in config
    if req.ship_mode && !state.config.ship.allowed {
        return Err(NemoError::ShipNotEnabled);
    }

    // Validate spec exists in repo
    if !state.git.spec_exists(&req.spec_path).await? {
        return Err(NemoError::SpecNotFound {
            path: req.spec_path,
        });
    }

    // Read spec content for branch name hash
    let spec_content = state.git.read_file(&req.spec_path, "HEAD").await?;
    let branch = generate_branch_name(&req.engineer, &req.spec_path, &spec_content);

    // Check for active loop on this branch
    if state.store.has_active_loop_for_branch(&branch).await? {
        return Err(NemoError::ActiveLoopConflict {
            branch: branch.clone(),
        });
    }

    let loop_id = Uuid::new_v4();
    let now = chrono::Utc::now();
    let spec_content_hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(spec_content.as_bytes());
        hex::encode(hasher.finalize())[..8].to_string()
    };

    let kind = if req.harden || req.harden_only {
        LoopKind::Harden
    } else {
        LoopKind::Implement
    };

    // Determine max rounds from config
    let max_rounds = if req.harden || req.harden_only {
        state.config.limits.max_rounds_harden as i32
    } else {
        state.config.limits.max_rounds_implement as i32
    };

    // If ship_mode with require_harden and no --harden, auto-add harden (FR-20)
    let effective_harden = req.harden || (req.ship_mode && state.config.ship.require_harden);

    // ship --harden implies auto_approve (FR-20: zero human gates)
    let effective_auto_approve = req.auto_approve || req.ship_mode;

    let record = LoopRecord {
        id: loop_id,
        engineer: req.engineer.clone(),
        spec_path: req.spec_path.clone(),
        spec_content_hash,
        branch: branch.clone(),
        kind,
        state: LoopState::Pending,
        sub_state: None,
        round: 0,
        max_rounds,
        harden: effective_harden,
        harden_only: req.harden_only,
        auto_approve: effective_auto_approve,
        ship_mode: req.ship_mode,
        cancel_requested: false,
        approve_requested: false,
        resume_requested: false,
        paused_from_state: None,
        reauth_from_state: None,
        failure_reason: None,
        current_sha: None,
        session_id: None,
        active_job_name: None,
        retry_count: 0,
        model_implementor: req.model_overrides.as_ref().and_then(|m| m.implementor.clone()),
        model_reviewer: req.model_overrides.as_ref().and_then(|m| m.reviewer.clone()),
        merge_sha: None,
        merged_at: None,
        hardened_spec_path: None,
        spec_pr_url: None,
        created_at: now,
        updated_at: now,
    };

    state.store.create_loop(&record).await?;

    tracing::info!(
        loop_id = %loop_id,
        engineer = %req.engineer,
        spec_path = %req.spec_path,
        branch = %branch,
        ship_mode = req.ship_mode,
        "Started new loop"
    );

    let response = StartResponse {
        loop_id,
        branch,
        state: LoopState::Pending,
        merge_sha: None,
        merged_at: None,
        hardened_spec_path: None,
        spec_pr_url: None,
    };

    Ok((StatusCode::CREATED, Json(response)))
}

/// GET /status - Show running loops.
pub async fn status(
    State(state): State<AppState>,
    Query(query): Query<StatusQuery>,
) -> Result<Json<StatusResponse>, NemoError> {
    let loops = state
        .store
        .get_loops_for_engineer(query.engineer.as_deref(), query.team.unwrap_or(false))
        .await?;

    let summaries = loops
        .into_iter()
        .map(|l| LoopSummary {
            loop_id: l.id,
            engineer: l.engineer,
            spec_path: l.spec_path,
            branch: l.branch,
            state: l.state,
            sub_state: l.sub_state,
            round: l.round,
            created_at: l.created_at,
            updated_at: l.updated_at,
        })
        .collect();

    Ok(Json(StatusResponse { loops: summaries }))
}

/// GET /logs/:id - Stream logs via SSE.
pub async fn logs(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<LogsQuery>,
) -> Result<impl IntoResponse, NemoError> {
    // Verify loop exists
    let record = state
        .store
        .get_loop(id)
        .await?
        .ok_or(NemoError::LoopNotFound { id })?;

    // For terminal loops, return all historical logs
    if record.state.is_terminal() {
        let logs = state
            .store
            .get_logs(id, query.round, query.stage.as_deref())
            .await?;

        let events: Vec<crate::types::api::LogEventResponse> = logs
            .into_iter()
            .map(|l| crate::types::api::LogEventResponse {
                timestamp: l.timestamp,
                stage: l.stage,
                round: l.round,
                line: l.line,
            })
            .collect();

        return Ok(Json(events).into_response());
    }

    // For active loops, use SSE streaming
    Ok(
        super::sse::stream_logs(state.store, id, query.round, query.stage)
            .await
            .into_response(),
    )
}

/// DELETE /cancel/:id - Cancel a running loop.
pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<CancelResponse>, NemoError> {
    let record = state
        .store
        .get_loop(id)
        .await?
        .ok_or(NemoError::LoopNotFound { id })?;

    if record.state.is_terminal() {
        return Err(NemoError::InvalidStateTransition {
            action: "cancel".to_string(),
            state: record.state.to_string(),
            expected: "non-terminal state".to_string(),
        });
    }

    // Set cancel flag; the loop engine handles the actual cancellation
    state
        .store
        .set_loop_flag(id, LoopFlag::Cancel, true)
        .await?;

    Ok(Json(CancelResponse {
        loop_id: id,
        state: LoopState::Cancelled,
        reason: "Cancelled by user".to_string(),
    }))
}

/// POST /approve/:id - Approve a loop in AWAITING_APPROVAL.
pub async fn approve(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<ApproveResponse>, NemoError> {
    let record = state
        .store
        .get_loop(id)
        .await?
        .ok_or(NemoError::LoopNotFound { id })?;

    if record.state != LoopState::AwaitingApproval {
        return Err(NemoError::InvalidStateTransition {
            action: "approve".to_string(),
            state: record.state.to_string(),
            expected: "AWAITING_APPROVAL".to_string(),
        });
    }

    state
        .store
        .set_loop_flag(id, LoopFlag::Approve, true)
        .await?;

    Ok(Json(ApproveResponse {
        loop_id: id,
        state: LoopState::AwaitingApproval,
        approve_requested: true,
    }))
}

/// POST /resume/:id - Resume a PAUSED or AWAITING_REAUTH loop.
pub async fn resume(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<ResumeResponse>, NemoError> {
    let record = state
        .store
        .get_loop(id)
        .await?
        .ok_or(NemoError::LoopNotFound { id })?;

    if record.state != LoopState::Paused && record.state != LoopState::AwaitingReauth {
        return Err(NemoError::InvalidStateTransition {
            action: "resume".to_string(),
            state: record.state.to_string(),
            expected: "PAUSED or AWAITING_REAUTH".to_string(),
        });
    }

    state
        .store
        .set_loop_flag(id, LoopFlag::Resume, true)
        .await?;

    Ok(Json(ResumeResponse {
        loop_id: id,
        state: record.state,
        resume_requested: true,
    }))
}

/// GET /inspect/:user/:branch - View detailed loop state.
pub async fn inspect(
    State(state): State<AppState>,
    Path((user, branch_name)): Path<(String, String)>,
) -> Result<Json<InspectResponse>, NemoError> {
    let branch = format!("agent/{user}/{branch_name}");

    let record = state
        .store
        .get_loop_by_branch(&branch)
        .await?
        .ok_or(NemoError::LoopNotFound { id: Uuid::nil() })?;

    let rounds = state.store.get_rounds(record.id).await?;

    // Group rounds by round number
    let mut round_summaries: std::collections::BTreeMap<i32, RoundSummary> =
        std::collections::BTreeMap::new();

    for r in &rounds {
        let summary = round_summaries
            .entry(r.round)
            .or_insert_with(|| RoundSummary {
                round: r.round,
                implement: None,
                test: None,
                review: None,
                audit: None,
                revise: None,
            });

        match r.stage.as_str() {
            "implement" => summary.implement = r.output.clone(),
            "test" => summary.test = r.output.clone(),
            "review" => summary.review = r.output.clone(),
            "audit" => summary.audit = r.output.clone(),
            "revise" => summary.revise = r.output.clone(),
            _ => {}
        }
    }

    Ok(Json(InspectResponse {
        loop_id: record.id,
        engineer: record.engineer,
        branch: record.branch,
        state: record.state,
        rounds: round_summaries.into_values().collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{build_router, AppState};
    use crate::config::NemoConfig;
    use crate::git::mock::MockGitOperations;
    use crate::state::memory::MemoryStateStore;
    use crate::state::StateStore;
    use axum::body::Body;
    use axum::http::{self, Request};
    use axum::response::Response;
    use axum::Router;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn test_app() -> (Router, Arc<MemoryStateStore>, Arc<MockGitOperations>) {
        let store = Arc::new(MemoryStateStore::new());
        let git = Arc::new(MockGitOperations::new());
        let mut config = NemoConfig::default();
        config.ship.allowed = true;
        let state = AppState {
            store: store.clone(),
            git: git.clone(),
            config: Arc::new(config),
        };
        let router = crate::api::build_router_no_auth(state);
        (router, store, git)
    }

    async fn send_request(app: Router, request: Request<Body>) -> Response<Body> {
        app.oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn test_start_success() {
        let (app, _store, git) = test_app();

        git.add_file("specs/test.md", "# Test Spec\n").await;

        let body = serde_json::json!({
            "spec_path": "specs/test.md",
            "engineer": "alice",
            "auto_approve": true
        });

        let response = send_request(
            app,
            Request::builder()
                .method(http::Method::POST)
                .uri("/start")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::CREATED);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let resp: StartResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(resp.state, LoopState::Pending);
        assert!(resp.branch.starts_with("agent/alice/test-"));
    }

    #[tokio::test]
    async fn test_start_spec_not_found() {
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": "specs/nonexistent.md",
            "engineer": "alice"
        });

        let response = send_request(
            app,
            Request::builder()
                .method(http::Method::POST)
                .uri("/start")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_start_duplicate_branch_conflict() {
        let (app, store, git) = test_app();

        git.add_file("specs/test.md", "# Test Spec\n").await;

        let spec_content = "# Test Spec\n";
        let branch = generate_branch_name("alice", "specs/test.md", spec_content);
        let existing = LoopRecord {
            id: Uuid::new_v4(),
            engineer: "alice".to_string(),
            spec_path: "specs/test.md".to_string(),
            spec_content_hash: "abc12345".to_string(),
            branch: branch.clone(),
            kind: LoopKind::Implement,
            state: LoopState::Implementing,
            sub_state: None,
            round: 1,
            max_rounds: 15,
            harden: false,
            harden_only: false,
            auto_approve: true,
            ship_mode: false,
            cancel_requested: false,
            approve_requested: false,
            resume_requested: false,
            paused_from_state: None,
            reauth_from_state: None,
            failure_reason: None,
            current_sha: None,
            session_id: None,
            active_job_name: None,
            retry_count: 0,
            model_implementor: None,
            model_reviewer: None,
            merge_sha: None,
            merged_at: None,
            hardened_spec_path: None,
            spec_pr_url: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        store.create_loop(&existing).await.unwrap();

        let body = serde_json::json!({
            "spec_path": "specs/test.md",
            "engineer": "alice"
        });

        let response = send_request(
            app,
            Request::builder()
                .method(http::Method::POST)
                .uri("/start")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn test_approve_wrong_state() {
        let (app, store, _git) = test_app();

        let record = LoopRecord {
            id: Uuid::new_v4(),
            engineer: "alice".to_string(),
            spec_path: "specs/test.md".to_string(),
            spec_content_hash: "abc12345".to_string(),
            branch: "agent/alice/test-abc12345".to_string(),
            kind: LoopKind::Implement,
            state: LoopState::Implementing,
            sub_state: None,
            round: 1,
            max_rounds: 15,
            harden: false,
            harden_only: false,
            auto_approve: true,
            ship_mode: false,
            cancel_requested: false,
            approve_requested: false,
            resume_requested: false,
            paused_from_state: None,
            reauth_from_state: None,
            failure_reason: None,
            current_sha: None,
            session_id: None,
            active_job_name: None,
            retry_count: 0,
            model_implementor: None,
            model_reviewer: None,
            merge_sha: None,
            merged_at: None,
            hardened_spec_path: None,
            spec_pr_url: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        store.create_loop(&record).await.unwrap();

        let response = send_request(
            app,
            Request::builder()
                .method(http::Method::POST)
                .uri(&format!("/approve/{}", record.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn test_status_returns_loops() {
        let (app, store, _git) = test_app();

        let record = LoopRecord {
            id: Uuid::new_v4(),
            engineer: "alice".to_string(),
            spec_path: "specs/test.md".to_string(),
            spec_content_hash: "abc12345".to_string(),
            branch: "agent/alice/test-abc12345".to_string(),
            kind: LoopKind::Implement,
            state: LoopState::Implementing,
            sub_state: None,
            round: 1,
            max_rounds: 15,
            harden: false,
            harden_only: false,
            auto_approve: true,
            ship_mode: false,
            cancel_requested: false,
            approve_requested: false,
            resume_requested: false,
            paused_from_state: None,
            reauth_from_state: None,
            failure_reason: None,
            current_sha: None,
            session_id: None,
            active_job_name: None,
            retry_count: 0,
            model_implementor: None,
            model_reviewer: None,
            merge_sha: None,
            merged_at: None,
            hardened_spec_path: None,
            spec_pr_url: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        store.create_loop(&record).await.unwrap();

        let response = send_request(
            app,
            Request::builder()
                .method(http::Method::GET)
                .uri("/status?engineer=alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let resp: StatusResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(resp.loops.len(), 1);
        assert_eq!(resp.loops[0].engineer, "alice");
    }

    #[tokio::test]
    async fn test_ship_not_enabled() {
        let store = Arc::new(MemoryStateStore::new());
        let git = Arc::new(MockGitOperations::new());
        let config = NemoConfig::default(); // ship.allowed = false by default
        let state = AppState {
            store: store.clone(),
            git: git.clone(),
            config: Arc::new(config),
        };
        let app = crate::api::build_router_no_auth(state);

        git.add_file("specs/test.md", "# Test\n").await;

        let body = serde_json::json!({
            "spec_path": "specs/test.md",
            "engineer": "alice",
            "ship_mode": true
        });

        let response = send_request(
            app,
            Request::builder()
                .method(http::Method::POST)
                .uri("/start")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
