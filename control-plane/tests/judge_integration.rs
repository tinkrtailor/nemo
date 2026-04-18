//! Integration tests for the orchestrator judge wired into the convergent loop driver.
//!
//! These tests exercise the full `tick()` path with a mock judge model client,
//! verifying correct state transitions and `judge_decisions` table writes for
//! each decision variant (continue, exit_clean, exit_escalate, exit_fail) in
//! both review and harden phases, plus the fallback test (NFR-5).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use uuid::Uuid;

use nautiloop_control_plane::config::NautiloopConfig;
use nautiloop_control_plane::error::Result;
use nautiloop_control_plane::git::mock::MockGitOperations;
use nautiloop_control_plane::k8s::mock::MockJobDispatcher;
use nautiloop_control_plane::k8s::{JobDispatcher, JobStatus};
use nautiloop_control_plane::loop_engine::judge::JudgeModelClient;
use nautiloop_control_plane::loop_engine::ConvergentLoopDriver;
use nautiloop_control_plane::state::memory::MemoryStateStore;
use nautiloop_control_plane::state::StateStore;
use nautiloop_control_plane::types::{LoopKind, LoopRecord, LoopState, RoundRecord, SubState};

// ---------------------------------------------------------------------------
// Mock judge client that returns a configurable response
// ---------------------------------------------------------------------------

struct MockJudgeClient {
    response: tokio::sync::Mutex<String>,
}

impl MockJudgeClient {
    fn new(response: &str) -> Self {
        Self {
            response: tokio::sync::Mutex::new(response.to_string()),
        }
    }
}

#[async_trait]
impl JudgeModelClient for MockJudgeClient {
    async fn invoke(&self, _model: &str, _prompt: &str) -> Result<String> {
        Ok(self.response.lock().await.clone())
    }
}

/// Mock client that always errors (for fallback test).
struct FailingJudgeClient;

#[async_trait]
impl JudgeModelClient for FailingJudgeClient {
    async fn invoke(&self, _model: &str, _prompt: &str) -> Result<String> {
        Err(nautiloop_control_plane::error::NautiloopError::Internal(
            "mock model failure".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_config() -> NautiloopConfig {
    NautiloopConfig::default()
}

fn make_driver_with_judge(
    store: Arc<MemoryStateStore>,
    dispatcher: Arc<MockJobDispatcher>,
    git: Arc<MockGitOperations>,
    model_client: Arc<dyn JudgeModelClient>,
) -> ConvergentLoopDriver {
    ConvergentLoopDriver::with_judge(store, dispatcher, git, make_config(), model_client)
}

fn make_driver_no_judge(
    store: Arc<MemoryStateStore>,
    dispatcher: Arc<MockJobDispatcher>,
    git: Arc<MockGitOperations>,
) -> ConvergentLoopDriver {
    ConvergentLoopDriver::new(store, dispatcher, git, make_config())
}

async fn install_fresh_claude_creds(dispatcher: &MockJobDispatcher) {
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

fn make_reviewing_loop(round: i32) -> LoopRecord {
    LoopRecord {
        id: Uuid::new_v4(),
        engineer: "alice".to_string(),
        spec_path: "specs/test.md".to_string(),
        spec_content_hash: "abc12345".to_string(),
        branch: "agent/alice/test-abc12345".to_string(),
        kind: LoopKind::Implement,
        state: LoopState::Reviewing,
        sub_state: Some(SubState::Completed),
        round,
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
        current_sha: Some("aaa".to_string()),
        opencode_session_id: None,
        claude_session_id: None,
        active_job_name: Some("review-job".to_string()),
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

fn make_hardening_loop(round: i32) -> LoopRecord {
    LoopRecord {
        id: Uuid::new_v4(),
        engineer: "alice".to_string(),
        spec_path: "specs/test.md".to_string(),
        spec_content_hash: "abc12345".to_string(),
        branch: "agent/alice/harden-abc12345".to_string(),
        kind: LoopKind::Harden,
        state: LoopState::Hardening,
        sub_state: Some(SubState::Completed),
        round,
        max_rounds: 10,
        harden: true,
        harden_only: true,
        auto_approve: true,
        ship_mode: false,
        cancel_requested: false,
        approve_requested: false,
        resume_requested: false,
        paused_from_state: None,
        reauth_from_state: None,
        failed_from_state: None,
        failure_reason: None,
        current_sha: Some("bbb".to_string()),
        opencode_session_id: None,
        claude_session_id: None,
        active_job_name: Some("audit-job".to_string()),
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

/// Create a review round with a non-clean verdict (issues present).
fn make_review_round_not_clean(loop_id: Uuid, round: i32) -> RoundRecord {
    RoundRecord {
        id: Uuid::new_v4(),
        loop_id,
        round,
        stage: "review".to_string(),
        input: None,
        output: Some(serde_json::json!({
            "clean": false,
            "confidence": 0.7,
            "issues": [{
                "severity": "low",
                "category": "style",
                "file": "src/lib.rs",
                "line": 10,
                "description": "minor cosmetic nit",
                "suggestion": "rename variable"
            }],
            "summary": "Minor nits found.",
            "token_usage": { "input": 5000, "output": 500 }
        })),
        started_at: Some(Utc::now()),
        completed_at: Some(Utc::now()),
        duration_secs: Some(30),
        job_name: Some("review-job".to_string()),
    }
}

/// Create an audit round with a non-clean verdict.
fn make_audit_round_not_clean(loop_id: Uuid, round: i32) -> RoundRecord {
    RoundRecord {
        id: Uuid::new_v4(),
        loop_id,
        round,
        stage: "audit".to_string(),
        input: None,
        output: Some(serde_json::json!({
            "clean": false,
            "confidence": 0.7,
            "issues": [{
                "severity": "low",
                "category": "style",
                "file": "specs/test.md",
                "line": 5,
                "description": "minor style issue",
                "suggestion": "rephrase"
            }],
            "summary": "Minor issues.",
            "token_usage": { "input": 3000, "output": 300 }
        })),
        started_at: Some(Utc::now()),
        completed_at: Some(Utc::now()),
        duration_secs: Some(20),
        job_name: Some("audit-job".to_string()),
    }
}

/// Set up a succeeded review job in the mock dispatcher.
async fn setup_succeeded_job(dispatcher: &MockJobDispatcher, job_name: &str) {
    let job = k8s_openapi::api::batch::v1::Job {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(job_name.to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    dispatcher.create_job(&job).await.unwrap();
    dispatcher
        .set_job_status(job_name, JobStatus::Succeeded)
        .await;
}

/// Set up git mocks so converge_review_clean succeeds (branch differs from main).
/// The branch SHA must match the record's `current_sha` to pass the divergence check.
async fn setup_git_for_review(git: &MockGitOperations, branch: &str, current_sha: &str) {
    git.set_branch_sha(branch, current_sha).await;
    git.set_branch_sha("origin/main", "origin-main-sha").await;
    git.add_file("specs/test.md", "# Test Spec\nSome content").await;
}

/// Convenience wrapper: set up git mocks with the default `current_sha` ("aaa")
/// matching `make_reviewing_loop`.
async fn setup_git_for_convergence(git: &MockGitOperations, branch: &str) {
    setup_git_for_review(git, branch, "aaa").await;
}

// ---------------------------------------------------------------------------
// Review phase tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_review_exit_clean_converges() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_claude_creds(&dispatcher).await;
    setup_git_for_convergence(&git, "agent/alice/test-abc12345").await;

    let judge_response = r#"{"decision": "exit_clean", "confidence": 0.9, "reasoning": "only cosmetic nits remain", "hint": null}"#;
    let driver = make_driver_with_judge(
        store.clone(),
        dispatcher.clone(),
        git.clone(),
        Arc::new(MockJudgeClient::new(judge_response)),
    );

    let record = make_reviewing_loop(2);
    store.create_loop(&record).await.unwrap();
    store
        .create_round(&make_review_round_not_clean(record.id, 2))
        .await
        .unwrap();
    setup_succeeded_job(&dispatcher, "review-job").await;

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Converged);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    assert_eq!(updated.state, LoopState::Converged);
    assert!(updated.spec_pr_url.is_some());

    // Verify judge_decisions row written and backfilled
    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision, "exit_clean");
    assert_eq!(
        decisions[0].loop_final_state.as_deref(),
        Some("CONVERGED")
    );
    assert!(decisions[0].loop_terminated_at.is_some());
}

#[tokio::test]
async fn test_review_exit_escalate_transitions_to_awaiting_approval() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_claude_creds(&dispatcher).await;
    setup_git_for_convergence(&git, "agent/alice/test-abc12345").await;

    let judge_response = r#"{"decision": "exit_escalate", "confidence": 0.7, "reasoning": "recurring churn detected", "hint": null}"#;
    let driver = make_driver_with_judge(
        store.clone(),
        dispatcher.clone(),
        git.clone(),
        Arc::new(MockJudgeClient::new(judge_response)),
    );

    let record = make_reviewing_loop(3);
    store.create_loop(&record).await.unwrap();
    store
        .create_round(&make_review_round_not_clean(record.id, 3))
        .await
        .unwrap();
    setup_succeeded_job(&dispatcher, "review-job").await;

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::AwaitingApproval);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    assert_eq!(updated.state, LoopState::AwaitingApproval);
    assert!(updated
        .failure_reason
        .as_ref()
        .unwrap()
        .contains("escalated"));

    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision, "exit_escalate");
}

#[tokio::test]
async fn test_review_exit_fail_transitions_to_failed() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_claude_creds(&dispatcher).await;

    let judge_response = r#"{"decision": "exit_fail", "confidence": 0.95, "reasoning": "fundamental spec contradiction", "hint": null}"#;
    let driver = make_driver_with_judge(
        store.clone(),
        dispatcher.clone(),
        git.clone(),
        Arc::new(MockJudgeClient::new(judge_response)),
    );

    let record = make_reviewing_loop(4);
    store.create_loop(&record).await.unwrap();
    store
        .create_round(&make_review_round_not_clean(record.id, 4))
        .await
        .unwrap();
    setup_succeeded_job(&dispatcher, "review-job").await;

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Failed);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    assert_eq!(updated.state, LoopState::Failed);
    assert!(updated
        .failure_reason
        .as_ref()
        .unwrap()
        .contains("fundamental spec contradiction"));

    // Backfill should have fired
    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision, "exit_fail");
    assert_eq!(decisions[0].loop_final_state.as_deref(), Some("FAILED"));
}

#[tokio::test]
async fn test_review_continue_dispatches_next_round() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_claude_creds(&dispatcher).await;
    setup_git_for_convergence(&git, "agent/alice/test-abc12345").await;

    let judge_response = r#"{"decision": "continue", "confidence": 0.8, "reasoning": "issues being resolved", "hint": "focus on the null check"}"#;
    let driver = make_driver_with_judge(
        store.clone(),
        dispatcher.clone(),
        git.clone(),
        Arc::new(MockJudgeClient::new(judge_response)),
    );

    let record = make_reviewing_loop(2);
    store.create_loop(&record).await.unwrap();
    store
        .create_round(&make_review_round_not_clean(record.id, 2))
        .await
        .unwrap();
    setup_succeeded_job(&dispatcher, "review-job").await;

    let new_state = driver.tick(record.id).await.unwrap();
    // continue → dispatches implement-with-feedback → Implementing
    assert_eq!(new_state, LoopState::Implementing);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    assert_eq!(updated.state, LoopState::Implementing);
    assert_eq!(updated.round, 3); // round incremented

    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision, "continue");
}

// ---------------------------------------------------------------------------
// Harden phase tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_harden_exit_clean_converges_to_hardened() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_claude_creds(&dispatcher).await;
    // Harden needs branch to differ from main for PR creation.
    // SHA must match `current_sha` in make_hardening_loop ("bbb").
    git.set_branch_sha("agent/alice/harden-abc12345", "bbb")
        .await;
    git.set_branch_sha("origin/main", "main-sha").await;
    git.add_file("specs/test.md", "# Test Spec").await;

    let judge_response = r#"{"decision": "exit_clean", "confidence": 0.85, "reasoning": "remaining issues are trivial", "hint": null}"#;
    let driver = make_driver_with_judge(
        store.clone(),
        dispatcher.clone(),
        git.clone(),
        Arc::new(MockJudgeClient::new(judge_response)),
    );

    let record = make_hardening_loop(2);
    store.create_loop(&record).await.unwrap();
    store
        .create_round(&make_audit_round_not_clean(record.id, 2))
        .await
        .unwrap();
    setup_succeeded_job(&dispatcher, "audit-job").await;

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Hardened);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    assert_eq!(updated.state, LoopState::Hardened);

    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision, "exit_clean");
    assert_eq!(
        decisions[0].loop_final_state.as_deref(),
        Some("HARDENED")
    );
}

#[tokio::test]
async fn test_harden_exit_escalate() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_claude_creds(&dispatcher).await;
    git.add_file("specs/test.md", "# Test Spec").await;

    let judge_response = r#"{"decision": "exit_escalate", "confidence": 0.6, "reasoning": "ambiguous findings", "hint": null}"#;
    let driver = make_driver_with_judge(
        store.clone(),
        dispatcher.clone(),
        git.clone(),
        Arc::new(MockJudgeClient::new(judge_response)),
    );

    let record = make_hardening_loop(3);
    store.create_loop(&record).await.unwrap();
    store
        .create_round(&make_audit_round_not_clean(record.id, 3))
        .await
        .unwrap();
    setup_succeeded_job(&dispatcher, "audit-job").await;

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::AwaitingApproval);

    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision, "exit_escalate");
    assert_eq!(decisions[0].phase, "harden");
}

#[tokio::test]
async fn test_harden_exit_fail() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_claude_creds(&dispatcher).await;
    git.add_file("specs/test.md", "# Test Spec").await;

    let judge_response = r#"{"decision": "exit_fail", "confidence": 0.9, "reasoning": "cannot converge", "hint": null}"#;
    let driver = make_driver_with_judge(
        store.clone(),
        dispatcher.clone(),
        git.clone(),
        Arc::new(MockJudgeClient::new(judge_response)),
    );

    let record = make_hardening_loop(4);
    store.create_loop(&record).await.unwrap();
    store
        .create_round(&make_audit_round_not_clean(record.id, 4))
        .await
        .unwrap();
    setup_succeeded_job(&dispatcher, "audit-job").await;

    let new_state = driver.tick(record.id).await.unwrap();
    assert_eq!(new_state, LoopState::Failed);

    let updated = store.get_loop(record.id).await.unwrap().unwrap();
    assert!(updated
        .failure_reason
        .as_ref()
        .unwrap()
        .contains("cannot converge"));

    let decisions = store.get_judge_decisions(record.id).await.unwrap();
    assert_eq!(decisions[0].loop_final_state.as_deref(), Some("FAILED"));
}

// ---------------------------------------------------------------------------
// Fallback test: judge error → heuristic behavior
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_fallback_on_judge_error_matches_no_judge_behavior() {
    // With a failing judge, review with non-clean verdict at round < max_rounds
    // should dispatch implement-with-feedback, identical to no-judge behavior.

    let store_with = Arc::new(MemoryStateStore::new());
    let dispatcher_with = Arc::new(MockJobDispatcher::new());
    let git_with = Arc::new(MockGitOperations::new());
    install_fresh_claude_creds(&dispatcher_with).await;
    setup_git_for_convergence(&git_with, "agent/alice/test-abc12345").await;

    let driver_with = make_driver_with_judge(
        store_with.clone(),
        dispatcher_with.clone(),
        git_with.clone(),
        Arc::new(FailingJudgeClient),
    );

    let record_with = make_reviewing_loop(2);
    store_with.create_loop(&record_with).await.unwrap();
    store_with
        .create_round(&make_review_round_not_clean(record_with.id, 2))
        .await
        .unwrap();
    setup_succeeded_job(&dispatcher_with, "review-job").await;

    let state_with = driver_with.tick(record_with.id).await.unwrap();

    // Without judge
    let store_without = Arc::new(MemoryStateStore::new());
    let dispatcher_without = Arc::new(MockJobDispatcher::new());
    let git_without = Arc::new(MockGitOperations::new());
    install_fresh_claude_creds(&dispatcher_without).await;
    setup_git_for_convergence(&git_without, "agent/alice/test-abc12345").await;

    let driver_without = make_driver_no_judge(
        store_without.clone(),
        dispatcher_without.clone(),
        git_without.clone(),
    );

    let mut record_without = make_reviewing_loop(2);
    record_without.id = Uuid::new_v4(); // different ID so no conflict
    store_without.create_loop(&record_without).await.unwrap();
    store_without
        .create_round(&make_review_round_not_clean(record_without.id, 2))
        .await
        .unwrap();
    setup_succeeded_job(&dispatcher_without, "review-job").await;

    let state_without = driver_without.tick(record_without.id).await.unwrap();

    // Both should produce the same state transition
    assert_eq!(state_with, state_without);
    assert_eq!(state_with, LoopState::Implementing);

    // Failing judge should NOT have written any decisions
    let decisions = store_with
        .get_judge_decisions(record_with.id)
        .await
        .unwrap();
    assert!(decisions.is_empty());
}

// ---------------------------------------------------------------------------
// FR-7a: exit_clean one-shot guard (DB-persisted)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_exit_clean_one_shot_guard_second_attempt_treated_as_continue() {
    let store = Arc::new(MemoryStateStore::new());
    let dispatcher = Arc::new(MockJobDispatcher::new());
    let git = Arc::new(MockGitOperations::new());
    install_fresh_claude_creds(&dispatcher).await;
    setup_git_for_convergence(&git, "agent/alice/test-abc12345").await;

    let judge_response = r#"{"decision": "exit_clean", "confidence": 0.9, "reasoning": "trivial nits", "hint": null}"#;
    let driver = make_driver_with_judge(
        store.clone(),
        dispatcher.clone(),
        git.clone(),
        Arc::new(MockJudgeClient::new(judge_response)),
    );

    // Pre-seed a prior exit_clean decision in the DB for this loop
    let loop_record = make_reviewing_loop(3);
    let loop_id = loop_record.id;
    store.create_loop(&loop_record).await.unwrap();
    store
        .create_round(&make_review_round_not_clean(loop_id, 3))
        .await
        .unwrap();
    setup_succeeded_job(&dispatcher, "review-job").await;

    // Pre-seed a prior exit_clean decision
    let prior_decision = nautiloop_control_plane::types::JudgeDecisionRecord {
        id: Uuid::new_v4(),
        loop_id,
        round: 2,
        phase: "review".to_string(),
        trigger: "not_clean".to_string(),
        input_json: serde_json::json!({}),
        decision: "exit_clean".to_string(),
        confidence: Some(0.9),
        reasoning: Some("prior exit_clean".to_string()),
        hint: None,
        duration_ms: 100,
        created_at: Utc::now(),
        loop_final_state: None,
        loop_terminated_at: None,
    };
    store.create_judge_decision(&prior_decision).await.unwrap();

    // Now tick — the judge returns exit_clean again, but the guard should block it
    let new_state = driver.tick(loop_id).await.unwrap();

    // Should fall through to continue/heuristic behavior (Implementing)
    assert_eq!(new_state, LoopState::Implementing);

    // Two exit_clean decisions should exist (prior + new from this tick)
    let decisions = store.get_judge_decisions(loop_id).await.unwrap();
    let exit_clean_count = decisions
        .iter()
        .filter(|d| d.decision == "exit_clean")
        .count();
    assert_eq!(exit_clean_count, 2);
}
