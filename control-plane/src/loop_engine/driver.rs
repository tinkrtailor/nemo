use std::sync::Arc;
use uuid::Uuid;

use crate::config::NemoConfig;
use crate::error::{NemoError, Result};
use crate::git::GitOperations;
use crate::k8s::job_builder;
use crate::k8s::{JobDispatcher, JobStatus};
use crate::state::StateStore;
use crate::types::verdict::{
    AuditVerdict, FeedbackFile, FeedbackSource, ReviewVerdict, TestOutput,
};
use crate::types::{
    LoopContext, LoopKind, LoopRecord, LoopState, RoundRecord, StageConfig, SubState,
};

/// The convergent loop driver. Processes one tick per loop, advancing its state machine.
pub struct ConvergentLoopDriver {
    store: Arc<dyn StateStore>,
    dispatcher: Arc<dyn JobDispatcher>,
    git: Arc<dyn GitOperations>,
    config: NemoConfig,
}

impl ConvergentLoopDriver {
    pub fn new(
        store: Arc<dyn StateStore>,
        dispatcher: Arc<dyn JobDispatcher>,
        git: Arc<dyn GitOperations>,
        config: NemoConfig,
    ) -> Self {
        Self {
            store,
            dispatcher,
            git,
            config,
        }
    }

    /// Run one tick of the loop state machine for the given loop.
    /// All state writes happen within this function.
    /// Returns the new state after the tick.
    pub async fn tick(&self, loop_id: Uuid) -> Result<LoopState> {
        let record = self
            .store
            .get_loop(loop_id)
            .await?
            .ok_or(NemoError::LoopNotFound { id: loop_id })?;

        // Terminal states: clear stale flags and return (never transition out)
        if record.state.is_terminal() {
            if record.cancel_requested {
                let _ = self.store.set_loop_flag(record.id, crate::state::LoopFlag::Cancel, false).await;
            }
            return Ok(record.state);
        }

        // Check for cancel request (highest priority for non-terminal states)
        if record.cancel_requested {
            return self.handle_cancel(&record).await;
        }

        match record.state {
            LoopState::Pending => self.handle_pending(&record).await,
            LoopState::Hardening => self.handle_active_stage(&record).await,
            LoopState::AwaitingApproval => self.handle_awaiting_approval(&record).await,
            LoopState::Implementing => self.handle_active_stage(&record).await,
            LoopState::Testing => self.handle_active_stage(&record).await,
            LoopState::Reviewing => self.handle_active_stage(&record).await,
            LoopState::Paused => self.handle_paused(&record).await,
            LoopState::AwaitingReauth => self.handle_awaiting_reauth(&record).await,
            // Terminal states handled above; this arm is unreachable but required for exhaustiveness
            _ => Ok(record.state),
        }
    }

    /// Handle PENDING state: determine first stage and dispatch.
    async fn handle_pending(&self, record: &LoopRecord) -> Result<LoopState> {
        let mut updated = record.clone();

        // Fetch latest from remote per FR-8
        self.git.fetch().await?;

        if record.harden {
            // Start hardening loop
            updated.state = LoopState::Hardening;
            updated.sub_state = Some(SubState::Dispatched);
            updated.round = 1;
            updated.kind = LoopKind::Harden;

            let stage_config = self.audit_stage_config();
            let ctx = self.build_context(&updated).await?;
            let job = job_builder::build_job(
                &ctx,
                &stage_config,
                &self.config.cluster.jobs_namespace,
                &self.config.cluster.agent_image,
                &self.config.cluster.bare_repo_pvc,
            );
            self.persist_then_dispatch(&mut updated, "audit", &job).await?;

            tracing::info!(loop_id = %record.id, "Transitioned PENDING -> HARDENING/DISPATCHED");
            Ok(LoopState::Hardening)
        } else if !record.auto_approve {
            // Go to awaiting approval before implementing
            updated.state = LoopState::AwaitingApproval;
            updated.sub_state = None;
            self.store.update_loop(&updated).await?;

            tracing::info!(loop_id = %record.id, "Transitioned PENDING -> AWAITING_APPROVAL");
            Ok(LoopState::AwaitingApproval)
        } else {
            // Auto-approve: go directly to implementing
            self.start_implementing(&updated).await
        }
    }

    /// Handle an active stage (HARDENING, IMPLEMENTING, TESTING, REVIEWING).
    /// Checks job status and advances the state machine.
    async fn handle_active_stage(&self, record: &LoopRecord) -> Result<LoopState> {
        let sub_state = record.sub_state.unwrap_or(SubState::Dispatched);

        match sub_state {
            SubState::Dispatched | SubState::Running => {
                // Check job status
                let job_name = record.active_job_name.as_deref().unwrap_or("");
                if job_name.is_empty() {
                    // No active job but in dispatched/running state: re-dispatch
                    return self.redispatch_current_stage(record).await;
                }

                let status = self
                    .dispatcher
                    .get_job_status(job_name, &self.config.cluster.jobs_namespace)
                    .await?;

                match status {
                    JobStatus::Pending => {
                        // Still pending, no action
                        Ok(record.state)
                    }
                    JobStatus::Running => {
                        // Update sub-state to RUNNING if not already
                        if sub_state != SubState::Running {
                            let mut updated = record.clone();
                            updated.sub_state = Some(SubState::Running);
                            self.store.update_loop(&updated).await?;
                        }

                        // Divergence detection: if branch SHA changed unexpectedly, pause
                        if let Some(ref expected_sha) = record.current_sha
                            && self.git.has_diverged(&record.branch, expected_sha).await?
                        {
                            tracing::warn!(
                                loop_id = %record.id,
                                branch = %record.branch,
                                "Branch diverged while job running, pausing loop"
                            );
                            let mut paused = record.clone();
                            paused.state = LoopState::Paused;
                            paused.sub_state = None;
                            paused.paused_from_state = Some(record.state);
                            self.store.update_loop(&paused).await?;
                            return Ok(LoopState::Paused);
                        }

                        Ok(record.state)
                    }
                    JobStatus::Succeeded => {
                        // Job completed: parse output and evaluate
                        self.handle_job_completed(record).await
                    }
                    JobStatus::Failed { reason } => {
                        // Job failed: check retry logic
                        self.handle_job_failed(record, &reason).await
                    }
                    JobStatus::AuthExpired { reason } => {
                        // Auth expired (exit code 42): go directly to AWAITING_REAUTH
                        self.handle_auth_expired(record, &reason).await
                    }
                    JobStatus::NotFound => {
                        // Job disappeared: treat as failure
                        self.handle_job_failed(record, "Job not found (deleted externally)")
                            .await
                    }
                }
            }
            SubState::Completed => {
                // Should have been transitioned already; re-evaluate
                self.handle_job_completed(record).await
            }
        }
    }

    /// Handle a successfully completed job: ingest output, then evaluate and advance.
    async fn handle_job_completed(&self, record: &LoopRecord) -> Result<LoopState> {
        let mut updated = record.clone();
        updated.sub_state = Some(SubState::Completed);

        // Ingest job output: read verdict from git, update round record, set current_sha
        self.ingest_job_output(&mut updated).await?;

        match record.state {
            LoopState::Hardening => self.evaluate_harden_stage(&mut updated).await,
            LoopState::Implementing => self.advance_to_testing(&mut updated).await,
            LoopState::Testing => self.evaluate_test_stage(&mut updated).await,
            LoopState::Reviewing => self.evaluate_review_stage(&mut updated).await,
            _ => {
                // Should not happen
                Ok(record.state)
            }
        }
    }

    /// Ingest output from a completed job: read verdict from git, update round record, set current_sha.
    async fn ingest_job_output(&self, record: &mut LoopRecord) -> Result<()> {
        // Get the branch tip SHA and set current_sha
        if let Some(sha) = self.git.get_branch_sha(&record.branch).await? {
            record.current_sha = Some(sha);
        }

        // Determine verdict file path based on current stage
        let verdict_path = self.verdict_path_for_stage(record).await;

        // Read verdict JSON from git
        let git_ref = record
            .current_sha
            .as_deref()
            .unwrap_or(&record.branch);
        let verdict_json = match self.git.read_file(&verdict_path, git_ref).await {
            Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(
                        loop_id = %record.id,
                        path = verdict_path,
                        error = %e,
                        "Failed to parse verdict JSON"
                    );
                    None
                }
            },
            Err(_) => {
                tracing::debug!(
                    loop_id = %record.id,
                    path = verdict_path,
                    "No verdict file found"
                );
                None
            }
        };

        // Update the round record with output + completion time
        let rounds = self.store.get_rounds(record.id).await?;
        if let Some(round) = rounds
            .iter()
            .rfind(|r| r.round == record.round && r.completed_at.is_none())
        {
            let mut updated_round = round.clone();
            updated_round.output = verdict_json;
            updated_round.completed_at = Some(chrono::Utc::now());
            if let Some(started) = round.started_at {
                let duration = chrono::Utc::now() - started;
                updated_round.duration_secs = Some(duration.num_seconds());
            }
            self.store.update_round(&updated_round).await?;
        }

        Ok(())
    }

    /// Determine the verdict file path based on current stage and sub-stage.
    async fn verdict_path_for_stage(&self, record: &LoopRecord) -> String {
        match record.state {
            LoopState::Implementing => ".agent/implement-output.json".to_string(),
            LoopState::Testing => ".agent/test-output.json".to_string(),
            LoopState::Reviewing => ".agent/review-verdict.json".to_string(),
            LoopState::Hardening => {
                // Determine if this was an audit or revise job
                if let Ok(rounds) = self.store.get_rounds(record.id).await {
                    let last = rounds.iter().rfind(|r| r.round == record.round);
                    match last.map(|r| r.stage.as_str()) {
                        Some("revise") => return ".agent/revise-output.json".to_string(),
                        _ => return ".agent/audit-verdict.json".to_string(),
                    }
                }
                ".agent/audit-verdict.json".to_string()
            }
            _ => ".agent/verdict.json".to_string(),
        }
    }

    /// Evaluate harden stage output (audit or revise).
    async fn evaluate_harden_stage(&self, record: &mut LoopRecord) -> Result<LoopState> {
        // For the harden loop, we alternate between audit and revise.
        // After audit: if clean, converge or move to approval. If not clean, revise.
        // After revise: re-audit.
        //
        // We determine which sub-stage just completed by checking the round record.
        let rounds = self.store.get_rounds(record.id).await?;
        let last_round = rounds
            .iter()
            .rfind(|r| r.round == record.round);

        let stage_name = last_round.map(|r| r.stage.as_str()).unwrap_or("audit");

        match stage_name {
            "audit" => {
                // Parse audit verdict from the round output
                let verdict: Option<AuditVerdict> = last_round
                    .and_then(|r| r.output.as_ref())
                    .and_then(|v| serde_json::from_value(v.clone()).ok());

                match verdict {
                    Some(v) if v.clean => {
                        // Audit passed
                        if record.harden_only {
                            // Clean up .agent/ artifacts before PR creation
                            if let Err(e) = self.git.remove_path(&record.branch, ".agent").await {
                    tracing::warn!(loop_id = %record.id, error = %e, "Failed to clean up .agent/ artifacts, proceeding with PR");
                }

                            // Harden only: create spec PR, merge it, terminal HARDENED (FR-23)
                            let pr_title = format!(
                                "chore(spec): harden {} for {}",
                                record.spec_path, record.engineer,
                            );
                            let pr_body = format!(
                                "Spec hardening completed in {} round(s).\n\nSpec: {}\nBranch: {}",
                                record.round, record.spec_path, record.branch,
                            );
                            let pr_url = self
                                .git
                                .create_pr(&record.branch, &pr_title, &pr_body)
                                .await?;
                            record.spec_pr_url = Some(pr_url);

                            if self.config.harden.auto_merge_spec_pr {
                                let merge_sha = self
                                    .git
                                    .merge_pr(
                                        &record.branch,
                                        &self.config.harden.merge_strategy,
                                    )
                                    .await?;
                                record.merge_sha = Some(merge_sha);
                                record.merged_at = Some(chrono::Utc::now());

                                record.state = LoopState::Hardened;
                                record.sub_state = None;
                                record.active_job_name = None;
                                record.hardened_spec_path =
                                    Some(record.spec_path.clone());
                                self.store.update_loop(record).await?;
                                tracing::info!(loop_id = %record.id, "Harden loop HARDENED (spec PR merged)");
                                Ok(LoopState::Hardened)
                            } else {
                                // PR created but not auto-merged: still HARDENED
                                // (hardening converged, PR is the deliverable)
                                record.state = LoopState::Hardened;
                                record.sub_state = None;
                                record.active_job_name = None;
                                record.hardened_spec_path =
                                    Some(record.spec_path.clone());
                                self.store.update_loop(record).await?;
                                tracing::info!(loop_id = %record.id, "Harden loop HARDENED (spec PR created, human merge required)");
                                Ok(LoopState::Hardened)
                            }
                        } else if record.auto_approve {
                            // Auto-approve: go to implementing
                            self.start_implementing(record).await
                        } else {
                            // Need approval
                            record.state = LoopState::AwaitingApproval;
                            record.sub_state = None;
                            record.active_job_name = None;
                            self.store.update_loop(record).await?;
                            tracing::info!(loop_id = %record.id, "Harden passed -> AWAITING_APPROVAL");
                            Ok(LoopState::AwaitingApproval)
                        }
                    }
                    Some(_v) => {
                        // Audit found issues: dispatch revise
                        self.dispatch_revise(record).await
                    }
                    None => {
                        // Verdict parse failure: retry per FR-9
                        self.handle_verdict_parse_failure(record).await
                    }
                }
            }
            "revise" => {
                // Parse revise output to detect spec path changes
                let revise_output: Option<crate::types::verdict::ReviseOutput> = last_round
                    .and_then(|r| r.output.as_ref())
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                if let Some(ref output) = revise_output
                    && output.updated_spec_path != record.spec_path
                {
                    tracing::info!(
                        loop_id = %record.id,
                        old = %record.spec_path,
                        new = %output.updated_spec_path,
                        "Spec path updated by revise stage"
                    );
                    record.spec_path = output.updated_spec_path.clone();
                }

                // After revise: check max rounds, then re-audit
                if record.round >= record.max_rounds {
                    record.state = LoopState::Failed;
                    record.sub_state = None;
                    record.failure_reason =
                        Some(format!("Max harden rounds ({}) exceeded", record.max_rounds));
                    record.active_job_name = None;
                    self.store.update_loop(record).await?;
                    return Ok(LoopState::Failed);
                }

                record.round += 1;
                self.dispatch_audit(record).await
            }
            _ => Ok(record.state),
        }
    }

    /// Advance from IMPLEMENTING to TESTING.
    async fn advance_to_testing(&self, record: &mut LoopRecord) -> Result<LoopState> {
        record.state = LoopState::Testing;
        record.sub_state = Some(SubState::Dispatched);
        record.retry_count = 0; // Reset per-stage retry budget

        let stage_config = self.test_stage_config();
        let ctx = self.build_context(record).await?;
        let job = job_builder::build_job(
            &ctx,
            &stage_config,
            &self.config.cluster.jobs_namespace,
            &self.config.cluster.agent_image,
            &self.config.cluster.bare_repo_pvc,
        );
        self.persist_then_dispatch(record, "test", &job).await?;

        tracing::info!(loop_id = %record.id, round = record.round, "IMPLEMENTING -> TESTING/DISPATCHED");
        Ok(LoopState::Testing)
    }

    /// Evaluate test stage output.
    async fn evaluate_test_stage(&self, record: &mut LoopRecord) -> Result<LoopState> {
        let rounds = self.store.get_rounds(record.id).await?;
        let test_round = rounds
            .iter()
            .rfind(|r| r.round == record.round && r.stage == "test");

        let output: Option<TestOutput> = test_round
            .and_then(|r| r.output.as_ref())
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        match output {
            Some(test_output) if test_output.passed => {
                // Tests passed: advance to review
                self.dispatch_review(record).await
            }
            Some(test_output) => {
                // Tests failed: feed back to implement (no review dispatched per spec)
                if record.round >= record.max_rounds {
                    record.state = LoopState::Failed;
                    record.sub_state = None;
                    record.failure_reason = Some(format!(
                        "Max implement rounds ({}) exceeded",
                        record.max_rounds
                    ));
                    record.active_job_name = None;
                    self.store.update_loop(record).await?;
                    return Ok(LoopState::Failed);
                }

                // Create feedback file for next round
                let feedback = FeedbackFile {
                    round: record.round as u32,
                    source: FeedbackSource::Test,
                    issues: None,
                    failures: Some(test_output.failures),
                };

                record.round += 1;
                let feedback_path = format!(
                    ".agent/test-feedback-round-{}.json",
                    record.round - 1
                );
                self.dispatch_implement_with_feedback(record, &feedback, &feedback_path)
                    .await
            }
            None => {
                // No output: treat as failure, retry
                self.handle_job_failed(record, "Test stage produced no output")
                    .await
            }
        }
    }

    /// Evaluate review stage output.
    async fn evaluate_review_stage(&self, record: &mut LoopRecord) -> Result<LoopState> {
        let rounds = self.store.get_rounds(record.id).await?;
        let review_round = rounds
            .iter()
            .rfind(|r| r.round == record.round && r.stage == "review");

        let verdict: Option<ReviewVerdict> = review_round
            .and_then(|r| r.output.as_ref())
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        match verdict {
            Some(v) if v.clean => {
                // Create PR if not already created (idempotent across ticks)
                if record.spec_pr_url.is_none() {
                    if let Err(e) = self.git.remove_path(&record.branch, ".agent").await {
                        tracing::warn!(loop_id = %record.id, error = %e, "Failed to clean up .agent/ artifacts, proceeding with PR");
                    }

                    let pr_title = format!(
                        "feat(agent): {} for {}",
                        record.spec_path,
                        record.engineer,
                    );
                    let pr_body = format!(
                        "Automated convergence loop completed in {} round(s).\n\nSpec: {}\nBranch: {}",
                        record.round, record.spec_path, record.branch,
                    );
                    let pr_url = self
                        .git
                        .create_pr(&record.branch, &pr_title, &pr_body)
                        .await?;
                    record.spec_pr_url = Some(pr_url);
                    // Persist PR URL so next tick knows PR was already created
                    self.store.update_loop(record).await?;
                }

                if record.ship_mode {
                    let threshold = self.config.ship.max_rounds_for_auto_merge as i32;
                    if record.round <= threshold {
                        // Non-blocking CI check: one check per tick, return if pending
                        if self.config.ship.require_passing_ci {
                            match self.git.ci_status(&record.branch).await {
                                Ok(Some(true)) => {
                                    // CI passed, proceed to merge
                                }
                                Ok(Some(false)) => {
                                    // CI definitively failed
                                    record.state = LoopState::Converged;
                                    record.sub_state = None;
                                    record.active_job_name = None;
                                    record.failure_reason = Some(
                                        "CI checks failed. PR created but not merged.".to_string(),
                                    );
                                    self.store.update_loop(record).await?;
                                    tracing::warn!(
                                        loop_id = %record.id,
                                        "Ship mode: CI failed, converging without merge"
                                    );
                                    return Ok(LoopState::Converged);
                                }
                                Ok(None) => {
                                    // CI still pending: return current state, check again next tick
                                    tracing::debug!(
                                        loop_id = %record.id,
                                        "Ship mode: CI pending, will check on next tick"
                                    );
                                    return Ok(record.state);
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        loop_id = %record.id,
                                        error = %e,
                                        "CI check error, will retry next tick"
                                    );
                                    return Ok(record.state);
                                }
                            }
                        }

                        // Within threshold + CI passed: merge the PR -> SHIPPED
                        let merge_sha = self
                            .git
                            .merge_pr(&record.branch, &self.config.ship.merge_strategy)
                            .await?;

                        record.state = LoopState::Shipped;
                        record.sub_state = None;
                        record.active_job_name = None;
                        record.merge_sha = Some(merge_sha.clone());
                        record.merged_at = Some(chrono::Utc::now());
                        self.store.update_loop(record).await?;

                        // Log merge event (NFR-8)
                        let merge_event = crate::types::MergeEvent {
                            id: Uuid::new_v4(),
                            loop_id: record.id,
                            merge_sha,
                            merge_strategy: self.config.ship.merge_strategy.clone(),
                            ci_status: "passed".to_string(),
                            created_at: chrono::Utc::now(),
                        };
                        let _ = self.store.create_merge_event(&merge_event).await;

                        tracing::info!(
                            loop_id = %record.id,
                            round = record.round,
                            "Loop SHIPPED (auto-merge, within threshold)"
                        );
                        Ok(LoopState::Shipped)
                    } else {
                        // Above threshold: converge but don't auto-merge (PR already created)
                        record.state = LoopState::Converged;
                        record.sub_state = None;
                        record.active_job_name = None;
                        record.failure_reason = Some(format!(
                            "Converged in {} rounds (above auto-merge threshold of {}). PR created for human review.",
                            record.round, threshold
                        ));
                        self.store.update_loop(record).await?;
                        tracing::info!(
                            loop_id = %record.id,
                            round = record.round,
                            threshold,
                            "Loop CONVERGED (above ship threshold, PR created for review)"
                        );
                        Ok(LoopState::Converged)
                    }
                } else {
                    // No ship mode: standard CONVERGED (PR already created for review)
                    record.state = LoopState::Converged;
                    record.sub_state = None;
                    record.active_job_name = None;
                    self.store.update_loop(record).await?;
                    tracing::info!(loop_id = %record.id, round = record.round, "Loop CONVERGED");
                    Ok(LoopState::Converged)
                }
            }
            Some(v) => {
                // Review found issues: feed back to implement
                if record.round >= record.max_rounds {
                    record.state = LoopState::Failed;
                    record.sub_state = None;
                    record.failure_reason = Some(format!(
                        "Max implement rounds ({}) exceeded",
                        record.max_rounds
                    ));
                    record.active_job_name = None;
                    self.store.update_loop(record).await?;
                    return Ok(LoopState::Failed);
                }

                let feedback = FeedbackFile {
                    round: record.round as u32,
                    source: FeedbackSource::Review,
                    issues: Some(v.issues),
                    failures: None,
                };

                record.round += 1;
                let feedback_path = format!(
                    ".agent/review-feedback-round-{}.json",
                    record.round - 1
                );
                self.dispatch_implement_with_feedback(record, &feedback, &feedback_path)
                    .await
            }
            None => {
                // Verdict parse failure: retry per FR-9
                self.handle_verdict_parse_failure(record).await
            }
        }
    }

    /// Handle AWAITING_APPROVAL: check for approve flag.
    async fn handle_awaiting_approval(&self, record: &LoopRecord) -> Result<LoopState> {
        if record.approve_requested {
            // Perform transition first; only clear flag on success
            let result = self.start_implementing(record).await?;
            self.store
                .set_loop_flag(record.id, crate::state::LoopFlag::Approve, false)
                .await?;
            Ok(result)
        } else {
            // Still waiting
            Ok(LoopState::AwaitingApproval)
        }
    }

    /// Handle PAUSED: check for resume flag.
    async fn handle_paused(&self, record: &LoopRecord) -> Result<LoopState> {
        if record.resume_requested {
            // Resume to the stage we paused from; clear flag only on success
            if let Some(paused_from) = record.paused_from_state {
                let mut updated = record.clone();
                updated.state = paused_from;
                updated.paused_from_state = None;
                updated.retry_count += 1; // Bump to generate unique job name
                // Refresh current_sha to current branch tip so divergence check
                // doesn't immediately re-pause after resume
                if let Ok(Some(sha)) = self.git.get_branch_sha(&record.branch).await {
                    updated.current_sha = Some(sha);
                }
                let result = self.redispatch_current_stage(&updated).await?;
                self.store
                    .set_loop_flag(record.id, crate::state::LoopFlag::Resume, false)
                    .await?;
                Ok(result)
            } else {
                // No paused_from_state: shouldn't happen, re-evaluate
                Ok(LoopState::Paused)
            }
        } else {
            Ok(LoopState::Paused)
        }
    }

    /// Handle AWAITING_REAUTH: check for resume flag (after creds re-pushed).
    async fn handle_awaiting_reauth(&self, record: &LoopRecord) -> Result<LoopState> {
        if record.resume_requested {
            // Perform transition first; clear flag only on success
            if let Some(reauth_from) = record.reauth_from_state {
                let mut updated = record.clone();
                updated.state = reauth_from;
                updated.reauth_from_state = None;
                updated.retry_count += 1; // Bump to generate unique job name
                let result = self.redispatch_current_stage(&updated).await?;
                self.store
                    .set_loop_flag(record.id, crate::state::LoopFlag::Resume, false)
                    .await?;
                Ok(result)
            } else {
                Ok(LoopState::AwaitingReauth)
            }
        } else {
            Ok(LoopState::AwaitingReauth)
        }
    }

    /// Handle cancel request: kill job and transition to CANCELLED.
    async fn handle_cancel(&self, record: &LoopRecord) -> Result<LoopState> {
        // Delete active job if any
        if let Some(ref job_name) = record.active_job_name {
            let _ = self
                .dispatcher
                .delete_job(job_name, &self.config.cluster.jobs_namespace)
                .await;
        }

        // Perform transition first, then clear flag
        let mut updated = record.clone();
        updated.state = LoopState::Cancelled;
        updated.sub_state = None;
        updated.failure_reason = Some("Cancelled by user".to_string());
        updated.active_job_name = None;
        self.store.update_loop(&updated).await?;

        self.store
            .set_loop_flag(record.id, crate::state::LoopFlag::Cancel, false)
            .await?;

        tracing::info!(loop_id = %record.id, "Loop CANCELLED by user");
        Ok(LoopState::Cancelled)
    }

    /// Handle auth expiry (exit code 42 detected by K8s pod inspection).
    async fn handle_auth_expired(&self, record: &LoopRecord, reason: &str) -> Result<LoopState> {
        let mut updated = record.clone();

        if let Some(ref job_name) = record.active_job_name {
            let _ = self
                .dispatcher
                .delete_job(job_name, &self.config.cluster.jobs_namespace)
                .await;
        }

        updated.state = LoopState::AwaitingReauth;
        updated.sub_state = None;
        updated.reauth_from_state = Some(record.state);
        updated.active_job_name = None;
        self.store.update_loop(&updated).await?;
        tracing::warn!(
            loop_id = %record.id,
            reason = reason,
            "Auth expired (exit code 42), transitioning to AWAITING_REAUTH"
        );
        Ok(LoopState::AwaitingReauth)
    }

    /// Handle a failed job: detect auth errors, retry, or fail the loop.
    async fn handle_job_failed(&self, record: &LoopRecord, reason: &str) -> Result<LoopState> {
        let mut updated = record.clone();

        // Detect credential expiry (FR-10): transition to AWAITING_REAUTH
        if is_auth_error(reason) && record.state.is_active_stage() {
            // Delete the failed Job so redispatch on resume doesn't hit AlreadyExists
            if let Some(ref job_name) = record.active_job_name {
                let _ = self
                    .dispatcher
                    .delete_job(job_name, &self.config.cluster.jobs_namespace)
                    .await;
            }

            updated.state = LoopState::AwaitingReauth;
            updated.sub_state = None;
            updated.reauth_from_state = Some(record.state);
            updated.active_job_name = None;
            self.store.update_loop(&updated).await?;
            tracing::warn!(
                loop_id = %record.id,
                reason = reason,
                "Credentials expired, transitioning to AWAITING_REAUTH"
            );
            return Ok(LoopState::AwaitingReauth);
        }

        if updated.retry_count < self.max_retries_for_stage(record.state) as i32 {
            // Retry
            updated.retry_count += 1;
            tracing::warn!(
                loop_id = %record.id,
                retry = updated.retry_count,
                reason = reason,
                "Job failed, retrying"
            );
            self.redispatch_current_stage(&updated).await
        } else {
            // Exhausted retries: fail the loop
            updated.state = LoopState::Failed;
            updated.sub_state = None;
            updated.failure_reason = Some(format!(
                "{reason} (after {} retries)",
                updated.retry_count
            ));
            updated.active_job_name = None;
            self.store.update_loop(&updated).await?;

            tracing::error!(loop_id = %record.id, reason = reason, "Loop FAILED after retries exhausted");
            Ok(LoopState::Failed)
        }
    }

    /// Handle malformed verdict JSON: retry per FR-9.
    async fn handle_verdict_parse_failure(&self, record: &mut LoopRecord) -> Result<LoopState> {
        if record.retry_count < 2 {
            record.retry_count += 1;
            tracing::warn!(
                loop_id = %record.id,
                retry = record.retry_count,
                "Malformed verdict, retrying"
            );
            self.redispatch_current_stage(record).await
        } else {
            record.state = LoopState::Failed;
            record.sub_state = None;
            record.failure_reason = Some(format!(
                "Malformed verdict after {} retries",
                record.retry_count
            ));
            record.active_job_name = None;
            self.store.update_loop(record).await?;

            Ok(LoopState::Failed)
        }
    }

    /// Start the implement phase.
    async fn start_implementing(&self, record: &LoopRecord) -> Result<LoopState> {
        let mut updated = record.clone();
        updated.state = LoopState::Implementing;
        updated.sub_state = Some(SubState::Dispatched);
        updated.kind = LoopKind::Implement;
        if updated.round == 0 {
            updated.round = 1;
        }
        updated.retry_count = 0;

        let stage_config = self.implement_stage_config(record);
        let ctx = self.build_context(&updated).await?;
        let job = job_builder::build_job(
            &ctx,
            &stage_config,
            &self.config.cluster.jobs_namespace,
            &self.config.cluster.agent_image,
            &self.config.cluster.bare_repo_pvc,
        );
        self.persist_then_dispatch(&mut updated, "implement", &job).await?;

        tracing::info!(loop_id = %record.id, round = updated.round, "Started IMPLEMENTING/DISPATCHED");
        Ok(LoopState::Implementing)
    }

    /// Dispatch an audit job (harden loop).
    async fn dispatch_audit(&self, record: &mut LoopRecord) -> Result<LoopState> {
        record.state = LoopState::Hardening;
        record.sub_state = Some(SubState::Dispatched);
        record.retry_count = 0;

        let stage_config = self.audit_stage_config();
        let ctx = self.build_context(record).await?;
        let job = job_builder::build_job(
            &ctx,
            &stage_config,
            &self.config.cluster.jobs_namespace,
            &self.config.cluster.agent_image,
            &self.config.cluster.bare_repo_pvc,
        );
        self.persist_then_dispatch(record, "audit", &job).await?;

        Ok(LoopState::Hardening)
    }

    /// Dispatch a revise job (harden loop).
    async fn dispatch_revise(&self, record: &mut LoopRecord) -> Result<LoopState> {
        record.sub_state = Some(SubState::Dispatched);
        record.retry_count = 0;

        let stage_config = self.revise_stage_config(record);
        let ctx = self.build_context(record).await?;
        let job = job_builder::build_job(
            &ctx,
            &stage_config,
            &self.config.cluster.jobs_namespace,
            &self.config.cluster.agent_image,
            &self.config.cluster.bare_repo_pvc,
        );
        self.persist_then_dispatch(record, "revise", &job).await?;

        Ok(LoopState::Hardening)
    }

    /// Dispatch a review job.
    async fn dispatch_review(&self, record: &mut LoopRecord) -> Result<LoopState> {
        record.state = LoopState::Reviewing;
        record.sub_state = Some(SubState::Dispatched);
        record.retry_count = 0;

        let stage_config = self.review_stage_config(record);
        let ctx = self.build_context(record).await?;
        let job = job_builder::build_job(
            &ctx,
            &stage_config,
            &self.config.cluster.jobs_namespace,
            &self.config.cluster.agent_image,
            &self.config.cluster.bare_repo_pvc,
        );
        self.persist_then_dispatch(record, "review", &job).await?;

        tracing::info!(loop_id = %record.id, round = record.round, "TESTING -> REVIEWING/DISPATCHED");
        Ok(LoopState::Reviewing)
    }

    /// Dispatch implement with feedback from previous round.
    /// Writes the feedback JSON to the worktree before dispatching.
    async fn dispatch_implement_with_feedback(
        &self,
        record: &mut LoopRecord,
        feedback: &FeedbackFile,
        feedback_path: &str,
    ) -> Result<LoopState> {
        // Write feedback file to the branch worktree so the agent can read it
        let feedback_json = serde_json::to_string_pretty(feedback)
            .map_err(|e| crate::error::NemoError::Internal(format!("Failed to serialize feedback: {e}")))?;
        self.git
            .write_file(&record.branch, feedback_path, &feedback_json)
            .await?;

        // Refresh current_sha after the commit so divergence detection doesn't false-pause
        if let Some(new_sha) = self.git.get_branch_sha(&record.branch).await? {
            record.current_sha = Some(new_sha);
        }

        record.state = LoopState::Implementing;
        record.sub_state = Some(SubState::Dispatched);
        record.retry_count = 0;

        let stage_config = self.implement_stage_config(record);
        let mut ctx = self.build_context(record).await?;
        ctx.feedback_path = Some(feedback_path.to_string());

        let job = job_builder::build_job(
            &ctx,
            &stage_config,
            &self.config.cluster.jobs_namespace,
            &self.config.cluster.agent_image,
            &self.config.cluster.bare_repo_pvc,
        );
        self.persist_then_dispatch(record, "implement", &job).await?;

        tracing::info!(
            loop_id = %record.id,
            round = record.round,
            feedback = feedback_path,
            "Re-dispatching IMPLEMENTING with feedback"
        );
        Ok(LoopState::Implementing)
    }

    /// Re-dispatch the current stage (after retry or resume).
    /// Deletes the old K8s Job first to avoid AlreadyExists on deterministic names.
    async fn redispatch_current_stage(&self, record: &LoopRecord) -> Result<LoopState> {
        // Clean up the old job before creating a new one with the same name
        if let Some(ref old_job) = record.active_job_name {
            let _ = self
                .dispatcher
                .delete_job(old_job, &self.config.cluster.jobs_namespace)
                .await;
        }

        let mut updated = record.clone();
        updated.sub_state = Some(SubState::Dispatched);

        let stage_config = match record.state {
            LoopState::Hardening => {
                // Determine which harden sub-stage to redispatch by checking the latest round
                let rounds = self.store.get_rounds(record.id).await?;
                let last_stage = rounds
                    .iter()
                    .rfind(|r| r.round == record.round)
                    .map(|r| r.stage.as_str());
                match last_stage {
                    Some("revise") => self.revise_stage_config(record),
                    _ => self.audit_stage_config(),
                }
            }
            LoopState::Implementing => self.implement_stage_config(record),
            LoopState::Testing => self.test_stage_config(),
            LoopState::Reviewing => self.review_stage_config(record),
            _ => return Ok(record.state),
        };

        let mut ctx = self.build_context(&updated).await?;

        // Restore feedback_path for implementing redispatch (N30):
        // look at the prior round's stage to determine review vs test feedback
        if record.state == LoopState::Implementing && record.round > 1 {
            let rounds = self.store.get_rounds(record.id).await?;
            // Find the last round before current that produced feedback
            let prior_round = record.round - 1;
            let prior_stage = rounds
                .iter()
                .rfind(|r| r.round == prior_round)
                .map(|r| r.stage.as_str());
            ctx.feedback_path = Some(match prior_stage {
                Some("test") => format!(".agent/test-feedback-round-{prior_round}.json"),
                _ => format!(".agent/review-feedback-round-{prior_round}.json"),
            });
        }

        let job = job_builder::build_job(
            &ctx,
            &stage_config,
            &self.config.cluster.jobs_namespace,
            &self.config.cluster.agent_image,
            &self.config.cluster.bare_repo_pvc,
        );

        // Persist state FIRST, then create K8s Job
        let job_name = job
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        updated.active_job_name = Some(job_name);
        self.store.update_loop(&updated).await?;

        if let Err(e) = self.dispatcher.create_job(&job).await {
            updated.active_job_name = None;
            let _ = self.store.update_loop(&updated).await;
            return Err(e);
        }

        Ok(record.state)
    }

    // --- Stage config helpers ---

    fn audit_stage_config(&self) -> StageConfig {
        StageConfig {
            name: "audit".to_string(),
            model: Some(self.config.models.reviewer.clone()),
            prompt_template: Some(".nemo/prompts/audit.md".to_string()),
            timeout: self.config.timeouts.audit_duration(),
            max_retries: 2,
        }
    }

    fn revise_stage_config(&self, record: &LoopRecord) -> StageConfig {
        StageConfig {
            name: "revise".to_string(),
            model: Some(
                record
                    .model_implementor
                    .clone()
                    .unwrap_or_else(|| self.config.models.implementor.clone()),
            ),
            prompt_template: Some(".nemo/prompts/revise.md".to_string()),
            timeout: self.config.timeouts.revise_duration(),
            max_retries: 2,
        }
    }

    fn implement_stage_config(&self, record: &LoopRecord) -> StageConfig {
        StageConfig {
            name: "implement".to_string(),
            model: Some(
                record
                    .model_implementor
                    .clone()
                    .unwrap_or_else(|| self.config.models.implementor.clone()),
            ),
            prompt_template: Some(".nemo/prompts/implement.md".to_string()),
            timeout: self.config.timeouts.implement_duration(),
            max_retries: 2,
        }
    }

    fn test_stage_config(&self) -> StageConfig {
        StageConfig {
            name: "test".to_string(),
            model: None,
            prompt_template: None,
            timeout: self.config.timeouts.test_duration(),
            max_retries: 2,
        }
    }

    fn review_stage_config(&self, record: &LoopRecord) -> StageConfig {
        StageConfig {
            name: "review".to_string(),
            model: Some(
                record
                    .model_reviewer
                    .clone()
                    .unwrap_or_else(|| self.config.models.reviewer.clone()),
            ),
            prompt_template: Some(".nemo/prompts/review.md".to_string()),
            timeout: self.config.timeouts.review_duration(),
            max_retries: 2,
        }
    }

    fn max_retries_for_stage(&self, _state: LoopState) -> u32 {
        2 // All stages default to 2 retries
    }

    /// Build context with credentials loaded from the store.
    async fn build_context(&self, record: &LoopRecord) -> Result<LoopContext> {
        // feedback_path is set explicitly by dispatch_implement_with_feedback;
        // for redispatch/resume, it's restored by redispatch_current_stage.
        let feedback_path = None;

        // Load engineer credentials for injection into job pods
        let credentials = self
            .store
            .get_credentials(&record.engineer)
            .await?
            .into_iter()
            .filter(|c| c.valid)
            .map(|c| (c.provider, c.credential_ref))
            .collect();

        Ok(LoopContext {
            loop_id: record.id,
            engineer: record.engineer.clone(),
            spec_path: record.spec_path.clone(),
            branch: record.branch.clone(),
            current_sha: record.current_sha.clone().unwrap_or_default(),
            round: record.round as u32,
            max_rounds: record.max_rounds as u32,
            retry_count: record.retry_count as u32,
            session_id: record.session_id.clone(),
            feedback_path,
            credentials,
        })
    }

    /// Persist state FIRST, then create K8s Job. If K8s creation fails,
    /// clear active_job_name so the loop can retry on next tick.
    /// This prevents orphan jobs from DB write failures after job creation.
    async fn persist_then_dispatch(
        &self,
        record: &mut LoopRecord,
        stage: &str,
        job: &k8s_openapi::api::batch::v1::Job,
    ) -> Result<String> {
        let job_name = job
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| "unknown".to_string());

        // Persist state to DB first
        record.active_job_name = Some(job_name.clone());
        self.create_round_record(record, stage, &job_name).await?;
        self.store.update_loop(record).await?;

        // Now create the K8s Job
        match self.dispatcher.create_job(job).await {
            Ok(name) => Ok(name),
            Err(e) => {
                // K8s creation failed: clear job name so next tick can retry
                record.active_job_name = None;
                let _ = self.store.update_loop(record).await;
                Err(e)
            }
        }
    }

    async fn create_round_record(
        &self,
        record: &LoopRecord,
        stage: &str,
        job_name: &str,
    ) -> Result<()> {
        let round_record = RoundRecord {
            id: Uuid::new_v4(),
            loop_id: record.id,
            round: record.round,
            stage: stage.to_string(),
            input: None,
            output: None,
            started_at: Some(chrono::Utc::now()),
            completed_at: None,
            duration_secs: None,
            job_name: Some(job_name.to_string()),
        };
        self.store.create_round(&round_record).await
    }
}

/// Detect if a job failure reason indicates expired credentials.
/// Agents use exit code 42 or specific error messages when auth fails.
fn is_auth_error(reason: &str) -> bool {
    let reason_lower = reason.to_lowercase();
    // Convention: exit code 42 = auth expired
    if reason_lower.contains("exit code 42") || reason_lower.contains("exitcode: 42") {
        return true;
    }
    reason_lower.contains("auth")
        || reason_lower.contains("credential")
        || reason_lower.contains("unauthorized")
        || reason_lower.contains("token expired")
        || reason_lower.contains("api key")
        || reason_lower.contains("401")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::mock::MockGitOperations;
    use crate::k8s::mock::MockJobDispatcher;
    use crate::state::memory::MemoryStateStore;

    fn make_driver(
        store: Arc<MemoryStateStore>,
        dispatcher: Arc<MockJobDispatcher>,
    ) -> ConvergentLoopDriver {
        let git = Arc::new(MockGitOperations::new());
        ConvergentLoopDriver::new(store, dispatcher, git, NemoConfig::default())
    }

    fn make_pending_loop(auto_approve: bool) -> LoopRecord {
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
            auto_approve,
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
        }
    }

    #[tokio::test]
    async fn test_pending_auto_approve_transitions_to_implementing() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());

        let record = make_pending_loop(true);
        store.create_loop(&record).await.unwrap();

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Implementing);

        let updated = store.get_loop(record.id).await.unwrap().unwrap();
        assert_eq!(updated.state, LoopState::Implementing);
        assert_eq!(updated.sub_state, Some(SubState::Dispatched));
        assert_eq!(updated.round, 1);
        assert!(updated.active_job_name.is_some());
    }

    #[tokio::test]
    async fn test_pending_no_auto_approve_transitions_to_awaiting_approval() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());

        let record = make_pending_loop(false);
        store.create_loop(&record).await.unwrap();

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::AwaitingApproval);
    }

    #[tokio::test]
    async fn test_awaiting_approval_approve_transitions_to_implementing() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());

        let mut record = make_pending_loop(false);
        record.state = LoopState::AwaitingApproval;
        record.approve_requested = true;
        store.create_loop(&record).await.unwrap();

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Implementing);
    }

    #[tokio::test]
    async fn test_cancel_from_any_state() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());

        let mut record = make_pending_loop(true);
        record.state = LoopState::Implementing;
        record.sub_state = Some(SubState::Running);
        record.cancel_requested = true;
        record.active_job_name = Some("nemo-test-job".to_string());
        store.create_loop(&record).await.unwrap();

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Cancelled);

        let updated = store.get_loop(record.id).await.unwrap().unwrap();
        assert_eq!(updated.state, LoopState::Cancelled);
        assert_eq!(
            updated.failure_reason,
            Some("Cancelled by user".to_string())
        );
    }

    #[tokio::test]
    async fn test_pending_harden_transitions_to_hardening() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());

        let mut record = make_pending_loop(false);
        record.harden = true;
        store.create_loop(&record).await.unwrap();

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Hardening);

        let updated = store.get_loop(record.id).await.unwrap().unwrap();
        assert_eq!(updated.sub_state, Some(SubState::Dispatched));
        assert_eq!(updated.round, 1);
    }

    #[tokio::test]
    async fn test_implementing_job_running_updates_substate() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());

        let mut record = make_pending_loop(true);
        record.state = LoopState::Implementing;
        record.sub_state = Some(SubState::Dispatched);
        record.round = 1;
        record.active_job_name = Some("test-job".to_string());
        store.create_loop(&record).await.unwrap();

        // Set job to running
        dispatcher
            .set_job_status("test-job", JobStatus::Running)
            .await;
        // Create the job in dispatcher first
        let job = k8s_openapi::api::batch::v1::Job {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some("test-job".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        dispatcher.create_job(&job).await.unwrap();
        dispatcher
            .set_job_status("test-job", JobStatus::Running)
            .await;

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Implementing);

        let updated = store.get_loop(record.id).await.unwrap().unwrap();
        assert_eq!(updated.sub_state, Some(SubState::Running));
    }

    #[tokio::test]
    async fn test_terminal_state_noop() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());

        let mut record = make_pending_loop(true);
        record.state = LoopState::Converged;
        store.create_loop(&record).await.unwrap();

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Converged);
    }

    #[tokio::test]
    async fn test_paused_resume_redispatches() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());

        let mut record = make_pending_loop(true);
        record.state = LoopState::Paused;
        record.paused_from_state = Some(LoopState::Implementing);
        record.resume_requested = true;
        record.round = 2;
        store.create_loop(&record).await.unwrap();

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Implementing);
    }

    #[tokio::test]
    async fn test_job_failed_retries() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());

        let mut record = make_pending_loop(true);
        record.state = LoopState::Implementing;
        record.sub_state = Some(SubState::Dispatched);
        record.round = 1;
        record.retry_count = 0;
        record.active_job_name = Some("test-job".to_string());
        store.create_loop(&record).await.unwrap();

        // Create job in dispatcher and set to failed
        let job = k8s_openapi::api::batch::v1::Job {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some("test-job".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        dispatcher.create_job(&job).await.unwrap();
        dispatcher
            .set_job_status(
                "test-job",
                JobStatus::Failed {
                    reason: "OOM".to_string(),
                },
            )
            .await;

        // First failure: should retry
        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Implementing);

        let updated = store.get_loop(record.id).await.unwrap().unwrap();
        assert_eq!(updated.retry_count, 1);
    }

    #[tokio::test]
    async fn test_job_failed_exhausts_retries() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());

        let mut record = make_pending_loop(true);
        record.state = LoopState::Implementing;
        record.sub_state = Some(SubState::Dispatched);
        record.round = 1;
        record.retry_count = 2; // Already exhausted
        record.active_job_name = Some("test-job".to_string());
        store.create_loop(&record).await.unwrap();

        let job = k8s_openapi::api::batch::v1::Job {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some("test-job".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        dispatcher.create_job(&job).await.unwrap();
        dispatcher
            .set_job_status(
                "test-job",
                JobStatus::Failed {
                    reason: "OOM".to_string(),
                },
            )
            .await;

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Failed);

        let updated = store.get_loop(record.id).await.unwrap().unwrap();
        assert!(updated.failure_reason.unwrap().contains("OOM"));
    }

    #[tokio::test]
    async fn test_auth_error_transitions_to_awaiting_reauth() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());

        let mut record = make_pending_loop(true);
        record.state = LoopState::Implementing;
        record.sub_state = Some(SubState::Dispatched);
        record.round = 1;
        record.retry_count = 0;
        record.active_job_name = Some("test-job".to_string());
        store.create_loop(&record).await.unwrap();

        let job = k8s_openapi::api::batch::v1::Job {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some("test-job".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        dispatcher.create_job(&job).await.unwrap();
        dispatcher
            .set_job_status(
                "test-job",
                JobStatus::Failed {
                    reason: "unauthorized: token expired".to_string(),
                },
            )
            .await;

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::AwaitingReauth);

        let updated = store.get_loop(record.id).await.unwrap().unwrap();
        assert_eq!(updated.state, LoopState::AwaitingReauth);
        assert_eq!(updated.reauth_from_state, Some(LoopState::Implementing));
    }

    #[test]
    fn test_is_auth_error_detection() {
        assert!(is_auth_error("unauthorized: token expired"));
        assert!(is_auth_error("Authentication failed: 401"));
        assert!(is_auth_error("API key invalid"));
        assert!(is_auth_error("credential refresh failed"));
        assert!(!is_auth_error("OOMKilled"));
        assert!(!is_auth_error("timeout exceeded"));
    }

    #[tokio::test]
    async fn test_harden_only_converges_to_hardened() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());

        // Create a loop in HARDENING state with harden_only=true,
        // simulating audit just completed with clean verdict
        let mut record = make_pending_loop(false);
        record.harden = true;
        record.harden_only = true;
        record.state = LoopState::Hardening;
        record.sub_state = Some(SubState::Completed);
        record.round = 1;
        record.active_job_name = Some("audit-job".to_string());
        store.create_loop(&record).await.unwrap();

        // Create a round record with clean audit verdict
        let round_record = RoundRecord {
            id: Uuid::new_v4(),
            loop_id: record.id,
            round: 1,
            stage: "audit".to_string(),
            input: None,
            output: Some(serde_json::json!({
                "clean": true,
                "confidence": 0.95,
                "issues": [],
                "summary": "All good.",
                "token_usage": { "input": 1000, "output": 200 }
            })),
            started_at: Some(chrono::Utc::now()),
            completed_at: Some(chrono::Utc::now()),
            duration_secs: Some(10),
            job_name: Some("audit-job".to_string()),
        };
        store.create_round(&round_record).await.unwrap();

        // Set job to succeeded
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

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Hardened);

        let updated = store.get_loop(record.id).await.unwrap().unwrap();
        assert_eq!(updated.state, LoopState::Hardened);
        assert!(updated.state.is_terminal());
    }

    #[tokio::test]
    async fn test_ship_mode_within_threshold_transitions_to_shipped() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());

        // Create a loop in REVIEWING state with ship_mode=true, round=2
        let mut record = make_pending_loop(true);
        record.ship_mode = true;
        record.state = LoopState::Reviewing;
        record.sub_state = Some(SubState::Completed);
        record.round = 2;
        record.active_job_name = Some("review-job".to_string());
        store.create_loop(&record).await.unwrap();

        // Create round record with clean review verdict
        let round_record = RoundRecord {
            id: Uuid::new_v4(),
            loop_id: record.id,
            round: 2,
            stage: "review".to_string(),
            input: None,
            output: Some(serde_json::json!({
                "clean": true,
                "confidence": 0.95,
                "issues": [],
                "summary": "Clean review.",
                "token_usage": { "input": 5000, "output": 500 }
            })),
            started_at: Some(chrono::Utc::now()),
            completed_at: Some(chrono::Utc::now()),
            duration_secs: Some(60),
            job_name: Some("review-job".to_string()),
        };
        store.create_round(&round_record).await.unwrap();

        // Set job to succeeded
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

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Shipped);

        let updated = store.get_loop(record.id).await.unwrap().unwrap();
        assert_eq!(updated.state, LoopState::Shipped);
        assert!(updated.state.is_terminal());
    }

    #[tokio::test]
    async fn test_ship_mode_above_threshold_converges_not_shipped() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());

        // Create a loop in REVIEWING state with ship_mode=true, round=10 (above default threshold of 5)
        let mut record = make_pending_loop(true);
        record.ship_mode = true;
        record.state = LoopState::Reviewing;
        record.sub_state = Some(SubState::Completed);
        record.round = 10;
        record.active_job_name = Some("review-job".to_string());
        store.create_loop(&record).await.unwrap();

        // Create round record with clean review verdict
        let round_record = RoundRecord {
            id: Uuid::new_v4(),
            loop_id: record.id,
            round: 10,
            stage: "review".to_string(),
            input: None,
            output: Some(serde_json::json!({
                "clean": true,
                "confidence": 0.9,
                "issues": [],
                "summary": "Clean after many rounds.",
                "token_usage": { "input": 5000, "output": 500 }
            })),
            started_at: Some(chrono::Utc::now()),
            completed_at: Some(chrono::Utc::now()),
            duration_secs: Some(60),
            job_name: Some("review-job".to_string()),
        };
        store.create_round(&round_record).await.unwrap();

        // Set job to succeeded
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

        let new_state = driver.tick(record.id).await.unwrap();
        // Should be CONVERGED, not SHIPPED (above threshold)
        assert_eq!(new_state, LoopState::Converged);

        let updated = store.get_loop(record.id).await.unwrap().unwrap();
        assert_eq!(updated.state, LoopState::Converged);
        assert!(updated.failure_reason.unwrap().contains("above auto-merge threshold"));
    }

    #[tokio::test]
    async fn test_output_ingestion_on_job_completion() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let git = Arc::new(MockGitOperations::new());
        let driver = ConvergentLoopDriver::new(
            store.clone(),
            dispatcher.clone(),
            git.clone(),
            NemoConfig::default(),
        );

        // Set up branch SHA and verdict file in git
        git.set_branch_sha("agent/alice/test-abc12345", "aabbccdd11223344")
            .await;
        git.add_file(
            ".agent/review-verdict.json",
            r#"{"clean": true, "confidence": 0.95, "issues": [], "summary": "LGTM", "token_usage": {"input": 1000, "output": 200}}"#,
        )
        .await;

        // Create a loop in REVIEWING/DISPATCHED state
        let mut record = make_pending_loop(true);
        record.state = LoopState::Reviewing;
        record.sub_state = Some(SubState::Dispatched);
        record.round = 1;
        record.active_job_name = Some("review-job".to_string());
        store.create_loop(&record).await.unwrap();

        // Create the round record (output is None initially)
        let round_id = Uuid::new_v4();
        let round_record = RoundRecord {
            id: round_id,
            loop_id: record.id,
            round: 1,
            stage: "review".to_string(),
            input: None,
            output: None,
            started_at: Some(chrono::Utc::now() - chrono::Duration::seconds(30)),
            completed_at: None,
            duration_secs: None,
            job_name: Some("review-job".to_string()),
        };
        store.create_round(&round_record).await.unwrap();

        // Set job to succeeded
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

        // Tick: should ingest output, then evaluate -> CONVERGED
        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Converged);

        // Verify round record was updated with output
        let rounds = store.get_rounds(record.id).await.unwrap();
        let updated_round = rounds.iter().find(|r| r.id == round_id).unwrap();
        assert!(updated_round.output.is_some(), "Round output should be populated after ingestion");
        assert!(updated_round.completed_at.is_some(), "completed_at should be set");
        assert!(updated_round.duration_secs.is_some(), "duration_secs should be set");

        // Verify current_sha was set
        let updated_loop = store.get_loop(record.id).await.unwrap().unwrap();
        assert_eq!(
            updated_loop.current_sha,
            Some("aabbccdd11223344".to_string()),
            "current_sha should be populated from branch tip"
        );
    }

    #[tokio::test]
    async fn test_divergence_detection_pauses_loop() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let git = Arc::new(MockGitOperations::new());
        let driver = ConvergentLoopDriver::new(
            store.clone(),
            dispatcher.clone(),
            git.clone(),
            NemoConfig::default(),
        );

        // Set branch to a different SHA than expected
        git.set_branch_sha("agent/alice/test-abc12345", "diverged_sha")
            .await;

        let mut record = make_pending_loop(true);
        record.state = LoopState::Implementing;
        record.sub_state = Some(SubState::Dispatched);
        record.round = 1;
        record.current_sha = Some("original_sha".to_string());
        record.active_job_name = Some("impl-job".to_string());
        store.create_loop(&record).await.unwrap();

        // Create job in Running state
        let job = k8s_openapi::api::batch::v1::Job {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some("impl-job".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        dispatcher.create_job(&job).await.unwrap();
        dispatcher
            .set_job_status("impl-job", JobStatus::Running)
            .await;

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Paused);

        let updated = store.get_loop(record.id).await.unwrap().unwrap();
        assert_eq!(updated.state, LoopState::Paused);
        assert_eq!(updated.paused_from_state, Some(LoopState::Implementing));
    }

    #[tokio::test]
    async fn test_terminal_states_hardened_shipped_are_noop() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());

        let mut record = make_pending_loop(true);
        record.state = LoopState::Hardened;
        store.create_loop(&record).await.unwrap();
        let state = driver.tick(record.id).await.unwrap();
        assert_eq!(state, LoopState::Hardened);

        let mut record2 = make_pending_loop(true);
        record2.state = LoopState::Shipped;
        store.create_loop(&record2).await.unwrap();
        let state2 = driver.tick(record2.id).await.unwrap();
        assert_eq!(state2, LoopState::Shipped);
    }
}
