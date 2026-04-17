//! Integration tests for the orchestrator judge wired into the full driver tick.
//!
//! NFR-5: Tests that construct a ConvergentLoopDriver with a mock judge,
//! execute ticks through both review and harden paths with each JudgeDecision
//! variant, and assert resulting LoopState and judge_decisions records.

use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use nautiloop_control_plane::config::{NautiloopConfig, OrchestratorConfig};
use nautiloop_control_plane::error::Result;
use nautiloop_control_plane::git::mock::MockGitOperations;
use nautiloop_control_plane::k8s::mock::MockJobDispatcher;
use nautiloop_control_plane::k8s::{JobDispatcher, JobStatus};
use nautiloop_control_plane::loop_engine::judge::{JudgeModelClient, OrchestratorJudge};
use nautiloop_control_plane::loop_engine::ConvergentLoopDriver;
use nautiloop_control_plane::state::memory::MemoryStateStore;
use nautiloop_control_plane::state::StateStore;
use nautiloop_control_plane::types::{
    JudgeDecisionRecord, LoopKind, LoopRecord, LoopState, RoundRecord, SubState,
};

// ---------------------------------------------------------------------------
// Mock model client helpers
// ---------------------------------------------------------------------------

struct MockJudgeClient {
    response: String,
}

#[async_trait::async_trait]
impl JudgeModelClient for MockJudgeClient {
    async fn call(&self, _model: &str, _system: &str, _user: &str) -> Result<String> {
        Ok(self.response.clone())
    }
}

// ---------------------------------------------------------------------------
// Shared test helpers
// ---------------------------------------------------------------------------

fn make_config() -> NautiloopConfig {
    NautiloopConfig {
        orchestrator: OrchestratorConfig {
            judge_enabled: true,
            judge_min_round: 3,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn make_loop_record() -> LoopRecord {
    LoopRecord {
        id: Uuid::new_v4(),
        engineer: "alice".to_string(),
        spec_path: "specs/test.md".to_string(),
        spec_content_hash: "abc12345".to_string(),
        branch: "agent/alice/test-abc12345".to_string(),
        kind: LoopKind::Implement,
        state: LoopState::Pending,
        sub_state: None,
        round: 0,
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
        failed_from_state: None,
        failure_reason: None,
        current_sha: Some("0000000000000000000000000000000000000000".to_string()),
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
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

/// Install fresh Claude credentials so dispatch doesn't block on stale creds.
async fn install_fresh_creds(dispatcher: &MockJobDispatcher) {
    let future_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 60 * 60 * 1000;
    let bundle = format!(r#"{{"claudeAiOauth":{{"expiresAt":{future_ms}}}}}"#).into_bytes();
    dispatcher
        .set_secret_key("nautiloop-creds-alice", "claude", &bundle)
        .await;
}

/// Set up a reviewing loop with a completed review job and mock verdict logs.
async fn setup_reviewing_loop(
    store: &MemoryStateStore,
    dispatcher: &MockJobDispatcher,
    git: &MockGitOperations,
    round: i32,
    verdict_json: serde_json::Value,
) -> LoopRecord {
    let mut record = make_loop_record();
    record.state = LoopState::Reviewing;
    record.sub_state = Some(SubState::Dispatched);
    record.round = round;
    record.active_job_name = Some("review-job".to_string());
    store.create_loop(&record).await.unwrap();

    // Create the review round record (open, not yet completed)
    let round_record = RoundRecord {
        id: Uuid::new_v4(),
        loop_id: record.id,
        round,
        stage: "review".to_string(),
        input: None,
        output: None,
        started_at: Some(Utc::now()),
        completed_at: None,
        duration_secs: None,
        job_name: Some("review-job".to_string()),
    };
    store.create_round(&round_record).await.unwrap();

    // Set job to Succeeded
    let job = k8s_openapi::api::batch::v1::Job {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some("review-job".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    dispatcher.create_job(&job).await.unwrap();
    dispatcher
        .set_job_status("review-job", JobStatus::Succeeded)
        .await;

    // Set logs with NAUTILOOP_RESULT
    let envelope = serde_json::json!({
        "stage": "review",
        "data": verdict_json,
    });
    let logs = format!("NAUTILOOP_RESULT:{}", serde_json::to_string(&envelope).unwrap());
    dispatcher.set_job_logs("review-job", &logs).await;

    // Set spec file in git
    git.add_file("specs/test.md", "# Test spec\nSome requirements")
        .await;

    record
}

/// Set up a hardening loop with a completed audit job and mock verdict logs.
async fn setup_hardening_loop(
    store: &MemoryStateStore,
    dispatcher: &MockJobDispatcher,
    git: &MockGitOperations,
    round: i32,
    verdict_json: serde_json::Value,
) -> LoopRecord {
    let mut record = make_loop_record();
    record.state = LoopState::Hardening;
    record.sub_state = Some(SubState::Dispatched);
    record.round = round;
    record.harden = true;
    record.harden_only = true;
    record.kind = LoopKind::Harden;
    record.active_job_name = Some("audit-job".to_string());
    store.create_loop(&record).await.unwrap();

    // Create the audit round record
    let round_record = RoundRecord {
        id: Uuid::new_v4(),
        loop_id: record.id,
        round,
        stage: "audit".to_string(),
        input: None,
        output: None,
        started_at: Some(Utc::now()),
        completed_at: None,
        duration_secs: None,
        job_name: Some("audit-job".to_string()),
    };
    store.create_round(&round_record).await.unwrap();

    // Set job to Succeeded
    let job = k8s_openapi::api::batch::v1::Job {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some("audit-job".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    dispatcher.create_job(&job).await.unwrap();
    dispatcher
        .set_job_status("audit-job", JobStatus::Succeeded)
        .await;

    // Set logs with NAUTILOOP_RESULT
    let envelope = serde_json::json!({
        "stage": "audit",
        "data": verdict_json,
    });
    let logs = format!("NAUTILOOP_RESULT:{}", serde_json::to_string(&envelope).unwrap());
    dispatcher.set_job_logs("audit-job", &logs).await;

    // Set spec file in git
    git.add_file("specs/test.md", "# Test spec\nSome requirements")
        .await;

    record
}

fn build_driver(
    store: Arc<MemoryStateStore>,
    dispatcher: Arc<MockJobDispatcher>,
    git: Arc<MockGitOperations>,
    judge_response: &str,
) -> ConvergentLoopDriver {
    let config = make_config();
    let model_client: Arc<dyn JudgeModelClient> = Arc::new(MockJudgeClient {
        response: judge_response.to_string(),
    });
    let judge = Arc::new(OrchestratorJudge::new(
        config.orchestrator.clone(),
        store.clone(),
        model_client,
        "test prompt".to_string(),
    ));
    ConvergentLoopDriver::new(store, dispatcher, git, config).with_judge(judge)
}

// ---------------------------------------------------------------------------
// Review path tests
// ---------------------------------------------------------------------------

/// Judge returns Continue on review with issues -> dispatches next implement round.
#[tokio::test]
async fn test_review_judge_continue_dispatches_implement() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "high",
            "category": "correctness",
            "file": "src/main.rs",
            "line": 42,
            "description": "Bug found",
            "suggestion": "Fix it"
        }],
        "summary": "Issues found",
        "token_usage": {"input": 100, "output": 50}
    });

    let record = setup_reviewing_loop(&store, &dispatcher, &git, 4, verdict).await;

    let judge_response = r#"{"decision": "continue", "confidence": 0.8, "reasoning": "Issues are substantive, keep iterating", "hint": "Focus on the correctness bug"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Implementing);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    assert_eq!(updated.state, LoopState::Implementing);
    assert_eq!(updated.round, 5); // incremented

    // Verify judge decision was persisted
    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision, "continue");
    assert_eq!(decisions[0].phase, "review");
    assert_eq!(decisions[0].round, 4);
}

/// Judge returns ExitClean on review -> creates PR and converges.
#[tokio::test]
async fn test_review_judge_exit_clean_converges() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "low",
            "description": "Cosmetic nit",
            "suggestion": "Optional cleanup"
        }],
        "summary": "Minor nits",
        "token_usage": {"input": 100, "output": 50}
    });

    let record = setup_reviewing_loop(&store, &dispatcher, &git, 3, verdict).await;

    let judge_response = r#"{"decision": "exit_clean", "confidence": 0.95, "reasoning": "Only cosmetic nits remain, spec requirements are met"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Converged);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    assert_eq!(updated.state, LoopState::Converged);
    assert!(updated.spec_pr_url.is_some());

    // Verify judge decision persisted
    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision, "exit_clean");

    // FR-5b: terminal state should backfill
    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert!(decisions[0].loop_final_state.is_some());
    assert_eq!(
        decisions[0].loop_final_state.as_deref(),
        Some("CONVERGED")
    );
}

/// Judge returns ExitEscalate on review -> AWAITING_APPROVAL.
#[tokio::test]
async fn test_review_judge_exit_escalate() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "high",
            "category": "correctness",
            "file": "src/main.rs",
            "line": 42,
            "description": "Same bug as before",
            "suggestion": "Fix it"
        }],
        "summary": "Recurring issue",
        "token_usage": {"input": 100, "output": 50}
    });

    let record = setup_reviewing_loop(&store, &dispatcher, &git, 5, verdict).await;

    let judge_response = r#"{"decision": "exit_escalate", "confidence": 0.9, "reasoning": "Churn detected, same finding recurring across rounds"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::AwaitingApproval);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    assert!(updated.failure_reason.as_ref().unwrap().contains("Judge escalated"));

    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision, "exit_escalate");
}

/// Judge returns ExitFail on review -> FAILED.
#[tokio::test]
async fn test_review_judge_exit_fail() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "critical",
            "description": "Fundamental design flaw",
            "suggestion": "Needs rethink"
        }],
        "summary": "Cannot satisfy spec",
        "token_usage": {"input": 100, "output": 50}
    });

    let record = setup_reviewing_loop(&store, &dispatcher, &git, 5, verdict).await;

    let judge_response = r#"{"decision": "exit_fail", "confidence": 0.95, "reasoning": "Spec cannot be satisfied with current approach"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Failed);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    assert!(updated.failure_reason.as_ref().unwrap().contains("Judge failed"));

    // FR-5b: backfill on terminal state
    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].loop_final_state.as_deref(), Some("FAILED"));
}

// ---------------------------------------------------------------------------
// Harden path tests
// ---------------------------------------------------------------------------

/// Judge returns Continue on harden audit -> dispatches revise.
#[tokio::test]
async fn test_harden_judge_continue_dispatches_revise() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "high",
            "category": "completeness",
            "description": "Missing edge case",
            "suggestion": "Add coverage"
        }],
        "summary": "Spec incomplete",
        "token_usage": {"input": 100, "output": 50}
    });

    let record = setup_hardening_loop(&store, &dispatcher, &git, 3, verdict).await;

    let judge_response = r#"{"decision": "continue", "confidence": 0.75, "reasoning": "Issues are actionable, keep iterating", "hint": "Address the edge case"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Hardening);

    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision, "continue");
    assert_eq!(decisions[0].phase, "harden");
}

/// Judge returns ExitClean on harden audit -> HARDENED (harden_only mode).
#[tokio::test]
async fn test_harden_judge_exit_clean_hardens() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "low",
            "description": "Minor wording",
            "suggestion": "Consider rephrasing"
        }],
        "summary": "Minor style",
        "token_usage": {"input": 100, "output": 50}
    });

    let record = setup_hardening_loop(&store, &dispatcher, &git, 3, verdict).await;

    let judge_response = r#"{"decision": "exit_clean", "confidence": 0.92, "reasoning": "Only minor wording issues, spec is functionally complete"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Hardened);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    assert_eq!(updated.state, LoopState::Hardened);

    // FR-5b: backfill on terminal
    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision, "exit_clean");
    assert_eq!(
        decisions[0].loop_final_state.as_deref(),
        Some("HARDENED")
    );
}

/// Judge returns ExitEscalate on harden -> AWAITING_APPROVAL.
#[tokio::test]
async fn test_harden_judge_exit_escalate() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "high",
            "description": "Same issue recurring",
            "suggestion": "Needs human review"
        }],
        "summary": "Stuck",
        "token_usage": {"input": 100, "output": 50}
    });

    let record = setup_hardening_loop(&store, &dispatcher, &git, 5, verdict).await;

    let judge_response = r#"{"decision": "exit_escalate", "confidence": 0.88, "reasoning": "Churn detected in harden loop"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::AwaitingApproval);
}

/// Judge returns ExitFail on harden -> FAILED.
#[tokio::test]
async fn test_harden_judge_exit_fail() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "critical",
            "description": "Contradictory requirements",
            "suggestion": "Cannot resolve"
        }],
        "summary": "Impossible",
        "token_usage": {"input": 100, "output": 50}
    });

    let record = setup_hardening_loop(&store, &dispatcher, &git, 5, verdict).await;

    let judge_response = r#"{"decision": "exit_fail", "confidence": 0.95, "reasoning": "Spec has contradictory requirements"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Failed);
}

// ---------------------------------------------------------------------------
// Fallback tests
// ---------------------------------------------------------------------------

/// Judge disabled -> fallback to heuristic (review path, not at max rounds).
#[tokio::test]
async fn test_review_judge_disabled_uses_heuristic() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "high",
            "description": "Bug",
            "suggestion": "Fix"
        }],
        "summary": "Issues",
        "token_usage": {"input": 100, "output": 50}
    });

    let record = setup_reviewing_loop(&store, &dispatcher, &git, 3, verdict).await;

    // Build driver WITHOUT judge (judge disabled)
    let mut config = make_config();
    config.orchestrator.judge_enabled = false;
    let driver = ConvergentLoopDriver::new(
        store.clone(),
        dispatcher,
        git,
        config,
    );

    let new_state = driver.tick(record.id).await.unwrap();
    // Heuristic: clean=false, round < max_rounds -> dispatch implement
    assert_eq!(new_state, LoopState::Implementing);

    // No judge decisions should be recorded
    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert!(decisions.is_empty());
}

/// Judge disabled at max_rounds -> heuristic FAILED.
#[tokio::test]
async fn test_review_judge_disabled_max_rounds_fails() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "high",
            "description": "Bug",
            "suggestion": "Fix"
        }],
        "summary": "Issues",
        "token_usage": {"input": 100, "output": 50}
    });

    let record = setup_reviewing_loop(&store, &dispatcher, &git, 15, verdict).await;

    let mut config = make_config();
    config.orchestrator.judge_enabled = false;
    let driver = ConvergentLoopDriver::new(store.clone(), dispatcher, git, config);

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Failed);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    assert!(updated.failure_reason.as_ref().unwrap().contains("Max implement rounds"));
    assert!(updated.failure_reason.as_ref().unwrap().contains("judge unavailable"));
}

// ---------------------------------------------------------------------------
// FR-5b: Outcome backfill
// ---------------------------------------------------------------------------

/// Verify that judge decisions get loop_final_state backfilled when loop terminates.
#[tokio::test]
async fn test_judge_decisions_backfilled_on_terminal() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "high",
            "description": "Bug",
            "suggestion": "Fix"
        }],
        "summary": "Issues",
        "token_usage": {"input": 100, "output": 50}
    });

    let record = setup_reviewing_loop(&store, &dispatcher, &git, 5, verdict).await;

    let judge_response = r#"{"decision": "exit_fail", "confidence": 0.95, "reasoning": "Cannot fix"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    // Pre-populate an earlier judge decision (simulating prior round)
    let prior_decision = JudgeDecisionRecord {
        id: Uuid::new_v4(),
        loop_id: record.id,
        round: 3,
        phase: "review".to_string(),
        trigger: "not_clean".to_string(),
        input_json: serde_json::json!({}),
        decision: "continue".to_string(),
        confidence: Some(0.8),
        reasoning: Some("Keep going".to_string()),
        hint: None,
        duration_ms: 200,
        created_at: Utc::now(),
        loop_final_state: None,
        loop_terminated_at: None,
    };
    store.create_judge_decision(&prior_decision).await.unwrap();

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Failed);

    // Both decisions should be backfilled
    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 2);
    for d in &decisions {
        assert_eq!(d.loop_final_state.as_deref(), Some("FAILED"));
        assert!(d.loop_terminated_at.is_some());
    }
}

// ---------------------------------------------------------------------------
// FR-7a: One-shot exit_clean guard
// ---------------------------------------------------------------------------

/// Second exit_clean is downgraded to continue by the judge module.
#[tokio::test]
async fn test_review_second_exit_clean_downgraded() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "low",
            "description": "Nit",
            "suggestion": "Optional"
        }],
        "summary": "Minor",
        "token_usage": {"input": 100, "output": 50}
    });

    let record = setup_reviewing_loop(&store, &dispatcher, &git, 5, verdict).await;

    // Pre-populate an exit_clean decision from a prior round
    let prior = JudgeDecisionRecord {
        id: Uuid::new_v4(),
        loop_id: record.id,
        round: 3,
        phase: "review".to_string(),
        trigger: "not_clean".to_string(),
        input_json: serde_json::json!({}),
        decision: "exit_clean".to_string(),
        confidence: Some(0.9),
        reasoning: Some("Looks good".to_string()),
        hint: None,
        duration_ms: 150,
        created_at: Utc::now(),
        loop_final_state: None,
        loop_terminated_at: None,
    };
    store.create_judge_decision(&prior).await.unwrap();

    // Judge would return exit_clean again, but it should be downgraded
    let judge_response = r#"{"decision": "exit_clean", "confidence": 0.92, "reasoning": "Still looks good"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    let new_state = driver.tick(record.id).await.unwrap();
    // Should be implementing (continue after downgrade), not converged
    assert_eq!(new_state, LoopState::Implementing);

    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    // The new decision should be "continue" (downgraded)
    let latest = decisions.iter().find(|d| d.round == 5).unwrap();
    assert_eq!(latest.decision, "continue");
}

// ---------------------------------------------------------------------------
// NFR-1: Cost ceiling
// ---------------------------------------------------------------------------

/// After max judge calls, judge falls back to heuristic.
#[tokio::test]
async fn test_cost_ceiling_falls_back_to_heuristic() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "high",
            "description": "Bug",
            "suggestion": "Fix"
        }],
        "summary": "Issues",
        "token_usage": {"input": 100, "output": 50}
    });

    let record = setup_reviewing_loop(&store, &dispatcher, &git, 5, verdict).await;

    // Pre-populate max_judge_calls_per_loop decisions (default 10)
    for i in 0..10 {
        let d = JudgeDecisionRecord {
            id: Uuid::new_v4(),
            loop_id: record.id,
            round: i + 1,
            phase: "review".to_string(),
            trigger: "not_clean".to_string(),
            input_json: serde_json::json!({}),
            decision: "continue".to_string(),
            confidence: Some(0.8),
            reasoning: Some("test".to_string()),
            hint: None,
            duration_ms: 100,
            created_at: Utc::now(),
            loop_final_state: None,
            loop_terminated_at: None,
        };
        store.create_judge_decision(&d).await.unwrap();
    }

    // Judge returns exit_clean, but should not be invoked due to ceiling
    let judge_response = r#"{"decision": "exit_clean", "confidence": 0.95, "reasoning": "test"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    let new_state = driver.tick(record.id).await.unwrap();
    // Heuristic: clean=false, round < max_rounds -> dispatch implement
    assert_eq!(new_state, LoopState::Implementing);

    // Should have 10 prior decisions + 0 new (ceiling prevented the call)
    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 10);
}

// ---------------------------------------------------------------------------
// Review Continue at max_rounds includes reasoning
// ---------------------------------------------------------------------------

/// When judge returns Continue at max_rounds, failure message includes judge reasoning.
#[tokio::test]
async fn test_review_continue_at_max_rounds_includes_reasoning() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "high",
            "description": "Critical bug",
            "suggestion": "Fix"
        }],
        "summary": "Issues",
        "token_usage": {"input": 100, "output": 50}
    });

    let record = setup_reviewing_loop(&store, &dispatcher, &git, 15, verdict).await;

    let judge_response = r#"{"decision": "continue", "confidence": 0.7, "reasoning": "Issues still need work but we are out of rounds"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Failed);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    let reason = updated.failure_reason.as_ref().unwrap();
    assert!(reason.contains("Max implement rounds"));
    assert!(reason.contains("Judge wanted to continue"));
    assert!(reason.contains("Issues still need work"));
}

// ---------------------------------------------------------------------------
// Harden revise-evaluation path tests (NFR-5 gap)
// ---------------------------------------------------------------------------

/// Set up a hardening loop where the revise stage has completed at max_rounds.
/// This models the path in driver.rs where a revise job finishes and the driver
/// must decide final disposition via the judge (lines 754-842).
///
/// The setup includes a prior audit round (so extract_issues_from_output can find it)
/// and the current revise round as the active completed job.
async fn setup_harden_revise_at_max_rounds(
    store: &MemoryStateStore,
    dispatcher: &MockJobDispatcher,
    git: &MockGitOperations,
    audit_verdict: serde_json::Value,
) -> LoopRecord {
    let max_rounds = 5;
    let mut record = make_loop_record();
    record.state = LoopState::Hardening;
    record.sub_state = Some(SubState::Dispatched);
    record.round = max_rounds;
    record.max_rounds = max_rounds;
    record.harden = true;
    record.harden_only = true;
    record.kind = LoopKind::Harden;
    record.active_job_name = Some("revise-job".to_string());
    store.create_loop(&record).await.unwrap();

    // Create a prior completed audit round (the driver extracts issues from the last audit round)
    let audit_round = RoundRecord {
        id: Uuid::new_v4(),
        loop_id: record.id,
        round: max_rounds - 1,
        stage: "audit".to_string(),
        input: None,
        output: Some(audit_verdict.clone()),
        started_at: Some(Utc::now()),
        completed_at: Some(Utc::now()),
        duration_secs: Some(10),
        job_name: Some("audit-job-prev".to_string()),
    };
    store.create_round(&audit_round).await.unwrap();

    // Create the current revise round (open, not yet completed - driver will complete it)
    let revise_output = serde_json::json!({
        "revised_spec_path": "specs/test.md",
        "summary": "Revised spec"
    });
    let revise_envelope = serde_json::json!({
        "stage": "revise",
        "data": revise_output,
    });
    let revise_round = RoundRecord {
        id: Uuid::new_v4(),
        loop_id: record.id,
        round: max_rounds,
        stage: "revise".to_string(),
        input: None,
        output: None,
        started_at: Some(Utc::now()),
        completed_at: None,
        duration_secs: None,
        job_name: Some("revise-job".to_string()),
    };
    store.create_round(&revise_round).await.unwrap();

    // Set revise job to Succeeded
    let job = k8s_openapi::api::batch::v1::Job {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some("revise-job".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    dispatcher.create_job(&job).await.unwrap();
    dispatcher
        .set_job_status("revise-job", JobStatus::Succeeded)
        .await;

    // Set revise job logs with NAUTILOOP_RESULT
    let logs = format!(
        "NAUTILOOP_RESULT:{}",
        serde_json::to_string(&revise_envelope).unwrap()
    );
    dispatcher.set_job_logs("revise-job", &logs).await;

    // Set spec file in git
    git.add_file("specs/test.md", "# Test spec\nSome requirements")
        .await;

    record
}

/// Judge returns ExitClean after revise at max_rounds -> HARDENED (harden_only).
#[tokio::test]
async fn test_harden_revise_judge_exit_clean_hardens() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let audit_verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "low",
            "category": "style",
            "description": "Minor wording issue",
            "suggestion": "Rephrase"
        }],
        "summary": "Minor style issues",
        "token_usage": {"input": 100, "output": 50}
    });

    let record =
        setup_harden_revise_at_max_rounds(&store, &dispatcher, &git, audit_verdict).await;

    let judge_response = r#"{"decision": "exit_clean", "confidence": 0.90, "reasoning": "Spec is functionally complete despite minor wording"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Hardened);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    assert_eq!(updated.state, LoopState::Hardened);

    // FR-5b: backfill on terminal
    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision, "exit_clean");
    assert_eq!(decisions[0].phase, "harden");
    assert_eq!(decisions[0].trigger, "max_rounds");
    assert_eq!(
        decisions[0].loop_final_state.as_deref(),
        Some("HARDENED")
    );
}

/// Judge returns ExitEscalate after revise at max_rounds -> AWAITING_APPROVAL.
#[tokio::test]
async fn test_harden_revise_judge_exit_escalate() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let audit_verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "high",
            "category": "completeness",
            "description": "Missing edge case coverage",
            "suggestion": "Needs human review"
        }],
        "summary": "Incomplete",
        "token_usage": {"input": 100, "output": 50}
    });

    let record =
        setup_harden_revise_at_max_rounds(&store, &dispatcher, &git, audit_verdict).await;

    let judge_response = r#"{"decision": "exit_escalate", "confidence": 0.85, "reasoning": "Revise could not resolve completeness issues, needs human input"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::AwaitingApproval);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    let reason = updated.failure_reason.as_ref().unwrap();
    assert!(reason.contains("Judge escalated at max rounds"));
    assert!(reason.contains("needs human input"));

    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision, "exit_escalate");
    assert_eq!(decisions[0].phase, "harden");
}

/// Judge returns ExitFail after revise at max_rounds -> FAILED with judge reasoning.
#[tokio::test]
async fn test_harden_revise_judge_exit_fail() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let audit_verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "critical",
            "category": "correctness",
            "description": "Contradictory requirements",
            "suggestion": "Cannot be resolved"
        }],
        "summary": "Impossible spec",
        "token_usage": {"input": 100, "output": 50}
    });

    let record =
        setup_harden_revise_at_max_rounds(&store, &dispatcher, &git, audit_verdict).await;

    let judge_response = r#"{"decision": "exit_fail", "confidence": 0.95, "reasoning": "Spec has irreconcilable contradictions"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Failed);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    let reason = updated.failure_reason.as_ref().unwrap();
    assert!(reason.contains("Judge failed at max rounds"));
    assert!(reason.contains("irreconcilable contradictions"));

    // FR-5b: backfill on terminal
    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision, "exit_fail");
    assert_eq!(
        decisions[0].loop_final_state.as_deref(),
        Some("FAILED")
    );
}

/// Judge returns Continue after revise at max_rounds -> FAILED (cannot dispatch more rounds).
#[tokio::test]
async fn test_harden_revise_judge_continue_at_max_rounds_fails() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let audit_verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "high",
            "category": "correctness",
            "description": "Logic gap in spec",
            "suggestion": "Revise section 3"
        }],
        "summary": "Issues remain",
        "token_usage": {"input": 100, "output": 50}
    });

    let record =
        setup_harden_revise_at_max_rounds(&store, &dispatcher, &git, audit_verdict).await;

    let judge_response = r#"{"decision": "continue", "confidence": 0.6, "reasoning": "Issues are fixable but we are out of rounds"}"#;
    let driver = build_driver(store.clone(), dispatcher, git, judge_response);

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Failed);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    let reason = updated.failure_reason.as_ref().unwrap();
    assert!(reason.contains("Max harden rounds"));
    assert!(reason.contains("Judge wanted to continue"));
    assert!(reason.contains("out of rounds"));
}

/// No judge available after revise at max_rounds -> FAILED with generic message.
#[tokio::test]
async fn test_harden_revise_no_judge_at_max_rounds_fails() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let audit_verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "high",
            "description": "Bug",
            "suggestion": "Fix"
        }],
        "summary": "Issues",
        "token_usage": {"input": 100, "output": 50}
    });

    let record =
        setup_harden_revise_at_max_rounds(&store, &dispatcher, &git, audit_verdict).await;

    // Build driver WITHOUT judge (disabled)
    let mut config = make_config();
    config.orchestrator.judge_enabled = false;
    let driver = ConvergentLoopDriver::new(
        store.clone(),
        dispatcher,
        git,
        config,
    );

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Failed);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    let reason = updated.failure_reason.as_ref().unwrap();
    assert!(reason.contains("Max harden rounds"));

    // No judge decisions should exist
    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 0);
}

/// FR-5b: Cancelling a loop with prior judge decisions correctly backfills
/// loop_final_state='CANCELLED' and loop_terminated_at on all judge_decisions rows.
#[tokio::test]
async fn test_cancel_backfills_judge_decisions() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_creds(&dispatcher).await;

    let review_verdict = serde_json::json!({
        "clean": false,
        "issues": [{
            "severity": "medium",
            "category": "style",
            "file": "src/lib.rs",
            "line": 10,
            "description": "Nit",
            "suggestion": "Fix"
        }],
        "summary": "Minor issues",
        "token_usage": {"input": 100, "output": 50}
    });

    // Set up a reviewing loop at round 3 so the judge is triggered
    let record =
        setup_reviewing_loop(&store, &dispatcher, &git, 3, review_verdict).await;

    // Have the judge return "continue"
    let judge_response = r#"{"decision": "continue", "confidence": 0.7, "reasoning": "Keep iterating"}"#;
    let driver = build_driver(store.clone(), dispatcher.clone(), git.clone(), judge_response);

    // Tick to trigger the judge and process the review
    let new_state = driver.tick(record.id).await.unwrap();
    // Should dispatch next implement round (Implementing state)
    assert_eq!(new_state, LoopState::Implementing);

    // Verify judge decision was created
    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert!(decisions[0].loop_final_state.is_none());
    assert!(decisions[0].loop_terminated_at.is_none());

    // Now cancel the loop
    store
        .set_loop_flag(record.id, nautiloop_control_plane::state::LoopFlag::Cancel, true)
        .await
        .unwrap();

    let cancel_state = driver.tick(record.id).await.unwrap();
    assert_eq!(cancel_state, LoopState::Cancelled);

    // Verify backfill: all judge_decisions rows should have CANCELLED state
    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(
        decisions[0].loop_final_state.as_deref(),
        Some("CANCELLED"),
        "Judge decision should be backfilled with CANCELLED state"
    );
    assert!(
        decisions[0].loop_terminated_at.is_some(),
        "Judge decision should have loop_terminated_at set"
    );
}
