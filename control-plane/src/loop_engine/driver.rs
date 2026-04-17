use std::sync::Arc;
use uuid::Uuid;

use crate::config::NautiloopConfig;
use crate::error::{NautiloopError, Result};
use crate::git::GitOperations;
use crate::k8s::job_builder;
use crate::k8s::{JobDispatcher, JobStatus};
use crate::loop_engine::judge::OrchestratorJudge;
use crate::state::StateStore;
use crate::types::verdict::{
    AuditVerdict, FeedbackFile, FeedbackSource, JudgeDecision, ReviewResultData, ReviewVerdict,
    TestOutput, TestResultData,
};
use crate::types::{
    LogEvent, LoopContext, LoopKind, LoopRecord, LoopState, RoundRecord, StageConfig, SubState,
};

/// The convergent loop driver. Processes one tick per loop, advancing its state machine.
pub struct ConvergentLoopDriver {
    store: Arc<dyn StateStore>,
    dispatcher: Arc<dyn JobDispatcher>,
    git: Arc<dyn GitOperations>,
    config: NautiloopConfig,
    judge: Option<Arc<OrchestratorJudge>>,
}

impl ConvergentLoopDriver {
    pub fn new(
        store: Arc<dyn StateStore>,
        dispatcher: Arc<dyn JobDispatcher>,
        git: Arc<dyn GitOperations>,
        config: NautiloopConfig,
    ) -> Self {
        Self {
            store,
            dispatcher,
            git,
            config,
            judge: None,
        }
    }

    /// Set the orchestrator judge for LLM-driven transition decisions.
    pub fn with_judge(mut self, judge: Arc<OrchestratorJudge>) -> Self {
        self.judge = Some(judge);
        self
    }

    /// Build the K8s job configuration from cluster config.
    fn job_build_config(&self) -> job_builder::JobBuildConfig {
        job_builder::JobBuildConfig {
            namespace: self.config.cluster.jobs_namespace.clone(),
            agent_image: self.config.cluster.agent_image.clone(),
            sidecar_image: self.config.cluster.sidecar_image.clone(),
            bare_repo_pvc: self.config.cluster.bare_repo_pvc.clone(),
            sessions_pvc: self.config.cluster.sessions_pvc.clone(),
            image_pull_secret: self.config.cluster.image_pull_secret.clone(),
            git_repo_url: self.config.cluster.git_repo_url.clone(),
            ssh_known_hosts_configmap: self.config.cluster.ssh_known_hosts_configmap.clone(),
            skip_iptables: self.config.cluster.skip_iptables,
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
            .ok_or(NautiloopError::LoopNotFound { id: loop_id })?;

        let was_terminal = record.state.is_terminal();

        // Terminal states: clear stale flags and return, EXCEPT for
        // FAILED with a pending resume_requested flag — issue #96 lets
        // `nemo resume` bring a transient-failed loop back into the loop.
        if record.state.is_terminal() {
            if record.cancel_requested {
                let _ = self
                    .store
                    .set_loop_flag(record.id, crate::state::LoopFlag::Cancel, false)
                    .await;
            }
            if record.state == LoopState::Failed && record.resume_requested {
                return self.handle_failed(&record).await;
            }
            return Ok(record.state);
        }

        // Check for cancel request (highest priority for non-terminal states)
        if record.cancel_requested {
            return self.handle_cancel(&record).await;
        }

        let new_state = match record.state {
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
        }?;

        // FR-5b: When a loop reaches a terminal state, back-fill loop_final_state
        // on all judge_decisions rows for this loop.
        // Invariant: this runs in the same tick as the terminal transition.
        // The `!was_terminal` guard ensures we backfill exactly once — on the tick
        // that transitions to terminal. If the loop is already terminal on entry
        // (line 80-91), we skip the match block entirely, so backfill already ran
        // on the prior tick that caused the transition.
        if !was_terminal && new_state.is_terminal() {
            let now = chrono::Utc::now();
            if let Err(e) = self
                .store
                .backfill_judge_outcomes(loop_id, &new_state.to_string(), now)
                .await
            {
                tracing::warn!(
                    loop_id = %loop_id,
                    error = %e,
                    "Failed to backfill judge decision outcomes (non-fatal)"
                );
            }
        }

        Ok(new_state)
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

            let stage_config = self.audit_stage_config(record);
            let mut ctx = self.build_context(&updated).await?;
            ctx.session_id = Self::session_id_for_stage(record, "audit");
            let job = job_builder::build_job(&ctx, &stage_config, &self.job_build_config());
            self.persist_then_dispatch(&mut updated, "audit", &job)
                .await?;

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

                        self.sync_current_stage_logs(record).await;

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
                        // Job disappeared — could be TTL cleanup after completion.
                        // Try to ingest output first; if pod logs are available, the
                        // job succeeded and was cleaned up. Only fail if no output.
                        tracing::warn!(
                            loop_id = %record.id,
                            job_name = record.active_job_name.as_deref().unwrap_or("?"),
                            "Job not found (TTL cleanup or external deletion), attempting output recovery"
                        );
                        match self.handle_job_completed(record).await {
                            Ok(state) => Ok(state),
                            Err(_) => {
                                self.handle_job_failed(
                                    record,
                                    "Job not found and output unrecoverable (TTL cleanup after >5m delay?)",
                                )
                                .await
                            }
                        }
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

        self.sync_current_stage_logs(record).await;

        // Ingest job output: read verdict from git, update round record, set current_sha
        self.ingest_job_output(&mut updated).await?;

        match record.state {
            LoopState::Hardening => self.evaluate_harden_stage(&mut updated).await,
            LoopState::Implementing => {
                // Validate implement output exists before advancing to test.
                // A job that exits 0 but omits NAUTILOOP_RESULT should not advance.
                let rounds = self.store.get_rounds(record.id).await?;
                let impl_round = rounds
                    .iter()
                    .rfind(|r| r.round == updated.round && r.stage == "implement");
                if impl_round.is_none() || impl_round.is_some_and(|r| r.output.is_none()) {
                    tracing::warn!(
                        loop_id = %record.id,
                        "Implement stage completed without result output, treating as failure"
                    );
                    return self
                        .handle_job_failed_non_resumable(
                            &updated,
                            "Implement stage exited successfully but produced no NAUTILOOP_RESULT",
                        )
                        .await;
                }
                self.advance_to_testing(&mut updated).await
            }
            LoopState::Testing => self.evaluate_test_stage(&mut updated).await,
            LoopState::Reviewing => self.evaluate_review_stage(&mut updated).await,
            _ => {
                // Should not happen
                Ok(record.state)
            }
        }
    }

    /// Ingest output from a completed job: read NAUTILOOP_RESULT from pod logs, update round record, set current_sha.
    /// Returns Err with a Paused transition if branch has diverged since dispatch.
    async fn ingest_job_output(&self, record: &mut LoopRecord) -> Result<()> {
        // Get the branch tip SHA
        let tip_sha = self.git.get_branch_sha(&record.branch).await?;

        // Divergence check: if expected SHA is NOT an ancestor of the branch tip,
        // someone force-pushed or rebased between job exit and this tick.
        // Normal fast-forwards (agent commits) are fine — the expected SHA will
        // be an ancestor of the new tip. We accept those and advance current_sha.
        if let (Some(expected), Some(tip)) = (&record.current_sha, &tip_sha)
            && self.git.has_diverged(&record.branch, expected).await?
        {
            tracing::warn!(
                loop_id = %record.id,
                expected_sha = %expected,
                tip_sha = %tip,
                "Branch diverged after job completed, pausing to avoid ingesting wrong output"
            );
            let from_state = record.state;
            record.state = LoopState::Paused;
            record.sub_state = None;
            record.paused_from_state = Some(from_state);
            self.store.update_loop(record).await?;
            return Err(crate::error::NautiloopError::Git(
                "Branch diverged after job completed".to_string(),
            ));
        }

        // Safe to ingest: update current_sha to branch tip
        if let Some(sha) = tip_sha {
            record.current_sha = Some(sha);
        }

        // Read NAUTILOOP_RESULT from pod logs instead of git verdict files.
        // The entrypoint wraps all stage output with NAUTILOOP_RESULT: prefix.
        let job_name = record.active_job_name.as_deref().unwrap_or("unknown");
        let namespace = &self.config.cluster.jobs_namespace;
        let logs = match self.dispatcher.get_job_logs(job_name, namespace).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(
                    loop_id = %record.id,
                    job_name = job_name,
                    error = %e,
                    "Failed to retrieve pod logs — cannot determine job output"
                );
                return Err(e);
            }
        };

        let verdict_json = Self::extract_nautiloop_result(&logs);
        if verdict_json.is_none() {
            tracing::warn!(
                loop_id = %record.id,
                job_name = job_name,
                "No NAUTILOOP_RESULT line found in pod logs"
            );
        }

        let rounds = self.store.get_rounds(record.id).await?;
        let active_round = rounds
            .iter()
            .rfind(|round| round.round == record.round && round.completed_at.is_none())
            .cloned();
        let stage_name = active_round
            .as_ref()
            .map(|round| round.stage.as_str())
            .or_else(|| {
                rounds
                    .iter()
                    .rfind(|round| round.round == record.round)
                    .map(|round| round.stage.as_str())
            });

        if let Some(ref data) = verdict_json
            && let Some(sid) = data.get("session_id").and_then(|v| v.as_str())
        {
            if let Some(stage) = stage_name {
                Self::persist_session_id_for_stage(record, stage, sid);
            } else {
                tracing::warn!(
                    loop_id = %record.id,
                    session_id = sid,
                    "Could not determine stage for session ID persistence"
                );
            }
        }

        // Update the round record with output + completion time
        if let Some(mut updated_round) = active_round {
            updated_round.output = verdict_json;
            updated_round.completed_at = Some(chrono::Utc::now());
            if let Some(started) = updated_round.started_at {
                let duration = chrono::Utc::now() - started;
                updated_round.duration_secs = Some(duration.num_seconds());
            }
            self.store.update_round(&updated_round).await?;
        }

        Ok(())
    }

    /// Extract the NAUTILOOP_RESULT data from pod log output.
    /// Scans for the last line starting with "NAUTILOOP_RESULT:" and returns the `data` field
    /// from the envelope `{"stage":"...", "data": {...}}`.
    fn extract_nautiloop_result(logs: &str) -> Option<serde_json::Value> {
        logs.lines().rev().find_map(|line| {
            let trimmed = line.trim();
            if let Some(json_str) = trimmed.strip_prefix("NAUTILOOP_RESULT:") {
                let envelope: serde_json::Value = serde_json::from_str(json_str).ok()?;
                // Return the data field from the envelope, falling back to the whole thing
                envelope.get("data").cloned().or(Some(envelope))
            } else {
                None
            }
        })
    }

    fn persist_session_id_for_stage(record: &mut LoopRecord, stage: &str, session_id: &str) {
        match stage {
            "audit" | "review" => {
                if session_id.starts_with("ses_") {
                    record.opencode_session_id = Some(session_id.to_string());
                } else {
                    tracing::warn!(
                        loop_id = %record.id,
                        stage,
                        session_id,
                        "Stage emitted non-opencode session ID; not persisting"
                    );
                }
            }
            "implement" | "revise" => {
                if uuid::Uuid::try_parse(session_id).is_ok() {
                    record.claude_session_id = Some(session_id.to_string());
                } else {
                    tracing::warn!(
                        loop_id = %record.id,
                        stage,
                        session_id,
                        "Stage emitted non-claude session ID; not persisting"
                    );
                }
            }
            _ => {
                tracing::warn!(
                    loop_id = %record.id,
                    stage,
                    session_id,
                    "Non-resumable stage emitted a session ID; ignoring"
                );
            }
        }
    }

    async fn sync_current_stage_logs(&self, record: &LoopRecord) {
        let Some(job_name) = record.active_job_name.as_deref() else {
            return;
        };

        let Some((round, stage)) = self.current_log_context(record).await else {
            return;
        };

        let logs = match self
            .dispatcher
            .get_job_logs(job_name, &self.config.cluster.jobs_namespace)
            .await
        {
            Ok(logs) => logs,
            Err(error) => {
                tracing::warn!(
                    loop_id = %record.id,
                    job_name,
                    error = %error,
                    "Failed to sync live stage logs"
                );
                return;
            }
        };

        if let Err(error) = self
            .append_new_log_lines(record.id, round, &stage, &logs)
            .await
        {
            tracing::warn!(
                loop_id = %record.id,
                round,
                stage,
                error = %error,
                "Failed to persist live stage logs"
            );
        }
    }

    async fn current_log_context(&self, record: &LoopRecord) -> Option<(i32, String)> {
        if record.round <= 0 {
            return None;
        }

        let rounds = match self.store.get_rounds(record.id).await {
            Ok(rounds) => rounds,
            Err(error) => {
                tracing::warn!(
                    loop_id = %record.id,
                    error = %error,
                    "Failed to load rounds for log sync"
                );
                return None;
            }
        };

        rounds
            .iter()
            .rfind(|round| round.round == record.round && round.completed_at.is_none())
            .or_else(|| rounds.iter().rfind(|round| round.round == record.round))
            .map(|round| (round.round, round.stage.clone()))
    }

    async fn append_new_log_lines(
        &self,
        loop_id: Uuid,
        round: i32,
        stage: &str,
        logs: &str,
    ) -> Result<()> {
        let existing = self
            .store
            .get_logs(loop_id, Some(round), Some(stage))
            .await?;
        let existing_lines: Vec<String> = existing.into_iter().map(|event| event.line).collect();
        let new_lines: Vec<String> = logs
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty() && !line.starts_with("NAUTILOOP_RESULT:"))
            .map(ToOwned::to_owned)
            .collect();

        if new_lines.is_empty() {
            return Ok(());
        }

        let max_overlap = existing_lines.len().min(new_lines.len());
        let overlap = (0..=max_overlap)
            .rev()
            .find(|count| {
                existing_lines[existing_lines.len().saturating_sub(*count)..] == new_lines[..*count]
            })
            .unwrap_or(0);

        let base_timestamp = chrono::Utc::now();
        for (offset, line) in new_lines.into_iter().skip(overlap).enumerate() {
            self.store
                .append_log(&LogEvent {
                    id: Uuid::new_v4(),
                    loop_id,
                    round,
                    stage: stage.to_string(),
                    timestamp: base_timestamp + chrono::Duration::milliseconds(offset as i64),
                    line,
                })
                .await?;
        }

        Ok(())
    }

    /// Evaluate harden stage output (audit or revise).
    async fn evaluate_harden_stage(&self, record: &mut LoopRecord) -> Result<LoopState> {
        // For the harden loop, we alternate between audit and revise.
        // After audit: if clean, converge or move to approval. If not clean, revise.
        // After revise: re-audit.
        //
        // We determine which sub-stage just completed by checking the round record.
        let rounds = self.store.get_rounds(record.id).await?;
        let last_round = rounds.iter().rfind(|r| r.round == record.round);

        let stage_name = last_round.map(|r| r.stage.as_str()).unwrap_or("audit");

        match stage_name {
            "audit" => {
                // Parse audit verdict from the round output.
                // Try ReviewResultData envelope first (has .verdict field),
                // then fall back to direct AuditVerdict for backward compat.
                let verdict: Option<AuditVerdict> =
                    last_round.and_then(|r| r.output.as_ref()).and_then(|v| {
                        // New shape: { verdict: {...}, token_usage: {...}, ... }
                        if let Ok(rd) = serde_json::from_value::<ReviewResultData>(v.clone()) {
                            serde_json::from_value(rd.verdict).ok()
                        } else {
                            // Legacy: direct AuditVerdict at top level
                            serde_json::from_value(v.clone()).ok()
                        }
                    });

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
                                .create_pr(
                                    &record.branch,
                                    &pr_title,
                                    &pr_body,
                                    &self.default_branch_for(record),
                                )
                                .await?;
                            record.spec_pr_url = Some(pr_url);

                            if self.config.harden.auto_merge_spec_pr {
                                let merge_sha = self
                                    .git
                                    .merge_pr(
                                        &record.branch,
                                        &self.config.harden.merge_strategy,
                                        &self.default_branch_for(record),
                                    )
                                    .await?;
                                record.merge_sha = Some(merge_sha);
                                record.merged_at = Some(chrono::Utc::now());

                                record.state = LoopState::Hardened;
                                record.sub_state = None;
                                record.active_job_name = None;
                                record.hardened_spec_path = Some(record.spec_path.clone());
                                self.store.update_loop(record).await?;
                                tracing::info!(loop_id = %record.id, "Harden loop HARDENED (spec PR merged)");
                                Ok(LoopState::Hardened)
                            } else {
                                // PR created but not auto-merged: still HARDENED
                                // (hardening converged, PR is the deliverable)
                                record.state = LoopState::Hardened;
                                record.sub_state = None;
                                record.active_job_name = None;
                                record.hardened_spec_path = Some(record.spec_path.clone());
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
                    Some(v) => {
                        // Audit found issues: invoke judge before deciding
                        let verdict_json = last_round
                            .and_then(|r| r.output.clone())
                            .unwrap_or(serde_json::json!({}));

                        if let Some(judge_output) = self
                            .invoke_judge_for_stage(
                                record, "harden", &verdict_json, &v.issues, &rounds,
                            )
                            .await
                        {
                            match judge_output.decision {
                                JudgeDecision::Continue => {
                                    // Guard: if at max_rounds, fail immediately instead
                                    // of dispatching a doomed revise job
                                    if record.round >= record.max_rounds {
                                        record.state = LoopState::Failed;
                                        record.sub_state = None;
                                        record.failure_reason = Some(format!(
                                            "Max harden rounds ({}) exceeded (judge chose continue at limit)",
                                            record.max_rounds
                                        ));
                                        record.active_job_name = None;
                                        self.store.update_loop(record).await?;
                                        return Ok(LoopState::Failed);
                                    }
                                    // Continue with revise, injecting hint if present
                                    self.dispatch_revise(record, judge_output.hint).await
                                }
                                JudgeDecision::ExitClean => {
                                    // Override: treat as clean, same as v.clean == true path
                                    if record.harden_only {
                                        self.harden_converge_clean(record).await
                                    } else if record.auto_approve {
                                        self.start_implementing(record).await
                                    } else {
                                        record.state = LoopState::AwaitingApproval;
                                        record.sub_state = None;
                                        record.active_job_name = None;
                                        self.store.update_loop(record).await?;
                                        tracing::info!(loop_id = %record.id, "Judge exit_clean -> AWAITING_APPROVAL");
                                        Ok(LoopState::AwaitingApproval)
                                    }
                                }
                                JudgeDecision::ExitEscalate => {
                                    record.state = LoopState::AwaitingApproval;
                                    record.sub_state = None;
                                    record.active_job_name = None;
                                    record.failure_reason = Some(format!(
                                        "Judge escalated: {}",
                                        judge_output.reasoning
                                    ));
                                    self.store.update_loop(record).await?;
                                    tracing::info!(loop_id = %record.id, "Judge exit_escalate -> AWAITING_APPROVAL");
                                    Ok(LoopState::AwaitingApproval)
                                }
                                JudgeDecision::ExitFail => {
                                    record.state = LoopState::Failed;
                                    record.sub_state = None;
                                    record.active_job_name = None;
                                    record.failure_reason = Some(format!(
                                        "Judge failed: {}",
                                        judge_output.reasoning
                                    ));
                                    self.store.update_loop(record).await?;
                                    tracing::info!(loop_id = %record.id, "Judge exit_fail -> FAILED");
                                    Ok(LoopState::Failed)
                                }
                            }
                        } else {
                            // No judge (disabled/error/timeout): use heuristic
                            if record.round >= record.max_rounds {
                                record.state = LoopState::Failed;
                                record.sub_state = None;
                                record.failure_reason = Some(format!(
                                    "Max harden rounds ({}) exceeded",
                                    record.max_rounds
                                ));
                                record.active_job_name = None;
                                self.store.update_loop(record).await?;
                                Ok(LoopState::Failed)
                            } else {
                                self.dispatch_revise(record, None).await
                            }
                        }
                    }
                    None => {
                        // Verdict parse failure: retry per FR-9
                        self.handle_verdict_parse_failure(record).await
                    }
                }
            }
            "revise" => {
                // Parse revise output to detect spec path changes.
                // Try new ReviseResultData first, fall back to legacy ReviseOutput.
                let updated_spec_path: Option<String> =
                    last_round.and_then(|r| r.output.as_ref()).and_then(|v| {
                        if let Ok(rd) = serde_json::from_value::<
                            crate::types::verdict::ReviseResultData,
                        >(v.clone())
                        {
                            Some(rd.revised_spec_path)
                        } else if let Ok(legacy) =
                            serde_json::from_value::<crate::types::verdict::ReviseOutput>(v.clone())
                        {
                            Some(legacy.updated_spec_path)
                        } else {
                            None
                        }
                    });
                if let Some(ref new_path) = updated_spec_path
                    && *new_path != record.spec_path
                {
                    tracing::info!(
                        loop_id = %record.id,
                        old = %record.spec_path,
                        new = %new_path,
                        "Spec path updated by revise stage"
                    );
                    record.spec_path = new_path.clone();
                }

                // After revise: invoke judge if near max rounds or recurring findings
                if record.round >= record.max_rounds {
                    // At max rounds: invoke judge for final disposition.
                    // Extract issues from the last AUDIT round (not revise), since revise
                    // output contains the revised spec, not audit findings.
                    let (verdict_json, last_round_issues) = rounds
                        .iter()
                        .rev()
                        .find(|r| r.stage == "audit")
                        .and_then(|r| r.output.as_ref())
                        .map(|output| {
                            let issues =
                                crate::loop_engine::judge::extract_issues_from_output(output);
                            (output.clone(), issues)
                        })
                        .unwrap_or_else(|| (serde_json::json!({}), vec![]));

                    if let Some(judge_output) = self
                        .invoke_judge_for_stage(
                            record,
                            "harden",
                            &verdict_json,
                            &last_round_issues,
                            &rounds,
                        )
                        .await
                    {
                        match judge_output.decision {
                            JudgeDecision::ExitClean => {
                                if record.harden_only {
                                    return self.harden_converge_clean(record).await;
                                } else if record.auto_approve {
                                    return self.start_implementing(record).await;
                                } else {
                                    record.state = LoopState::AwaitingApproval;
                                    record.sub_state = None;
                                    record.active_job_name = None;
                                    self.store.update_loop(record).await?;
                                    return Ok(LoopState::AwaitingApproval);
                                }
                            }
                            JudgeDecision::ExitEscalate => {
                                record.state = LoopState::AwaitingApproval;
                                record.sub_state = None;
                                record.active_job_name = None;
                                record.failure_reason = Some(format!(
                                    "Judge escalated at max rounds: {}",
                                    judge_output.reasoning
                                ));
                                self.store.update_loop(record).await?;
                                return Ok(LoopState::AwaitingApproval);
                            }
                            JudgeDecision::ExitFail => {
                                // Use judge reasoning instead of generic message
                                record.state = LoopState::Failed;
                                record.sub_state = None;
                                record.failure_reason = Some(format!(
                                    "Judge failed at max rounds: {}",
                                    judge_output.reasoning
                                ));
                                record.active_job_name = None;
                                self.store.update_loop(record).await?;
                                return Ok(LoopState::Failed);
                            }
                            JudgeDecision::Continue => {
                                // At max_rounds, continue is not possible; fall through
                                // to generic max-rounds failure below
                            }
                        }
                    }

                    record.state = LoopState::Failed;
                    record.sub_state = None;
                    record.failure_reason = Some(format!(
                        "Max harden rounds ({}) exceeded",
                        record.max_rounds
                    ));
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
        let mut ctx = self.build_context(record).await?;

        // Inject affected_services for the TEST stage (FR-42a).
        // Compute from git diff: only services whose path prefix matches changed files.
        let diff_files = self
            .git
            .changed_files(&record.branch, &self.default_branch_for(record))
            .await
            .unwrap_or_default();

        let affected: Vec<String> = if diff_files.is_empty() {
            // Can't determine diff — test all services
            self.config.services.keys().cloned().collect()
        } else {
            self.config
                .services
                .iter()
                .filter(|(_, svc)| {
                    // Use path + "/" for prefix matching to prevent false positives
                    // (e.g., "cli" matching "client", "api" matching "api-gateway").
                    // Root service (".") matches everything.
                    let prefix = if svc.path == "." {
                        String::new()
                    } else if svc.path.ends_with('/') {
                        svc.path.clone()
                    } else {
                        format!("{}/", svc.path)
                    };
                    diff_files
                        .iter()
                        .any(|f| prefix.is_empty() || f.starts_with(&prefix) || f == &svc.path)
                })
                .map(|(name, _)| name.clone())
                .collect()
        };

        // If no services matched, still test all (safety net)
        let service_names = if affected.is_empty() {
            self.config.services.keys().cloned().collect()
        } else {
            affected
        };

        let services_json =
            serde_json::to_string(&service_names).unwrap_or_else(|_| "[]".to_string());
        ctx.credentials
            .push(("affected_services".to_string(), services_json));

        // Inject service_tags for JVM resource escalation (FR-28) — only from affected services
        let all_tags: Vec<String> = self
            .config
            .services
            .iter()
            .filter(|(name, _)| service_names.contains(name))
            .flat_map(|(_, s)| s.tags.iter().cloned())
            .collect();
        if !all_tags.is_empty() {
            let tags_json = serde_json::to_string(&all_tags).unwrap_or_else(|_| "[]".to_string());
            ctx.credentials
                .push(("service_tags".to_string(), tags_json));
        }

        let job = job_builder::build_job(&ctx, &stage_config, &self.job_build_config());
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

        let raw = test_round.and_then(|r| r.output.as_ref());

        // Try new TestResultData shape first (from entrypoint NAUTILOOP_RESULT),
        // fall back to legacy TestOutput for backward compatibility
        let passed = raw.and_then(|v| {
            if let Ok(td) = serde_json::from_value::<TestResultData>(v.clone()) {
                Some(td.all_passed)
            } else if let Ok(legacy) = serde_json::from_value::<TestOutput>(v.clone()) {
                Some(legacy.passed)
            } else {
                None
            }
        });

        match passed {
            Some(true) => {
                // Tests passed: advance to review
                self.dispatch_review(record).await
            }
            Some(false) => {
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

                // Extract test failures from either format for feedback
                let failures = raw.and_then(|v| {
                    if let Ok(td) = serde_json::from_value::<TestResultData>(v.clone()) {
                        let fails: Vec<_> = td
                            .services
                            .into_iter()
                            .filter(|s| s.exit_code != 0)
                            .map(|s| crate::types::verdict::TestFailure {
                                service: s.name,
                                test_command: s.test_command,
                                test_name: None,
                                exit_code: s.exit_code,
                                stdout: s.stdout,
                                stderr: s.stderr,
                            })
                            .collect();
                        Some(fails)
                    } else if let Ok(legacy) = serde_json::from_value::<TestOutput>(v.clone()) {
                        Some(legacy.failures)
                    } else {
                        None
                    }
                });

                // Create feedback file for next round
                let feedback = FeedbackFile {
                    round: record.round as u32,
                    source: FeedbackSource::Test,
                    issues: None,
                    failures,
                    orchestrator_hint: None,
                };

                record.round += 1;
                let feedback_path = format!(".agent/test-feedback-round-{}.json", record.round - 1);
                self.dispatch_implement_with_feedback(record, &feedback, &feedback_path)
                    .await
            }
            None => {
                // No output: treat as failure, retry.
                // Non-resumable — ingest_job_output already stamped
                // completed_at on this round, so a resumed run's output
                // would be ignored and the evaluator would re-read the
                // same empty output. See #96 round-4 codex review.
                self.handle_job_failed_non_resumable(record, "Test stage produced no output")
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

        // Try ReviewResultData envelope first (has .verdict field),
        // then fall back to direct ReviewVerdict for backward compat.
        let verdict: Option<ReviewVerdict> =
            review_round.and_then(|r| r.output.as_ref()).and_then(|v| {
                if let Ok(rd) = serde_json::from_value::<ReviewResultData>(v.clone()) {
                    serde_json::from_value(rd.verdict).ok()
                } else {
                    serde_json::from_value(v.clone()).ok()
                }
            });

        match verdict {
            Some(v) if v.clean => {
                // Create PR if not already created (idempotent across ticks)
                if record.spec_pr_url.is_none() {
                    if let Err(e) = self.git.remove_path(&record.branch, ".agent").await {
                        tracing::warn!(loop_id = %record.id, error = %e, "Failed to clean up .agent/ artifacts, proceeding with PR");
                    }

                    let pr_title =
                        format!("feat(agent): {} for {}", record.spec_path, record.engineer,);
                    let pr_body = format!(
                        "Automated convergence loop completed in {} round(s).\n\nSpec: {}\nBranch: {}",
                        record.round, record.spec_path, record.branch,
                    );
                    let pr_url = self
                        .git
                        .create_pr(
                            &record.branch,
                            &pr_title,
                            &pr_body,
                            &self.default_branch_for(record),
                        )
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
                            .merge_pr(
                                &record.branch,
                                &self.config.ship.merge_strategy,
                                &self.default_branch_for(record),
                            )
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
                // Review found issues: invoke judge before deciding
                let verdict_json = review_round
                    .and_then(|r| r.output.clone())
                    .unwrap_or(serde_json::json!({}));

                if let Some(judge_output) = self
                    .invoke_judge_for_stage(
                        record, "review", &verdict_json, &v.issues, &rounds,
                    )
                    .await
                {
                    match judge_output.decision {
                        JudgeDecision::ExitClean => {
                            // Override: treat as clean, create PR and converge
                            return self.review_converge_clean(record).await;
                        }
                        JudgeDecision::ExitEscalate => {
                            record.state = LoopState::AwaitingApproval;
                            record.sub_state = None;
                            record.active_job_name = None;
                            record.failure_reason = Some(format!(
                                "Judge escalated: {}",
                                judge_output.reasoning
                            ));
                            self.store.update_loop(record).await?;
                            tracing::info!(loop_id = %record.id, "Judge exit_escalate -> AWAITING_APPROVAL");
                            return Ok(LoopState::AwaitingApproval);
                        }
                        JudgeDecision::ExitFail => {
                            record.state = LoopState::Failed;
                            record.sub_state = None;
                            record.active_job_name = None;
                            record.failure_reason = Some(format!(
                                "Judge failed: {}",
                                judge_output.reasoning
                            ));
                            self.store.update_loop(record).await?;
                            tracing::info!(loop_id = %record.id, "Judge exit_fail -> FAILED");
                            return Ok(LoopState::Failed);
                        }
                        JudgeDecision::Continue => {
                            // Fall through to normal feedback dispatch,
                            // but inject the hint if present.
                            // Note: if round >= max_rounds, the judge was already
                            // invoked with the MaxRounds trigger (determine_trigger
                            // prioritizes it) and chose Continue — meaning it wants
                            // to continue but can't. We fail here rather than making
                            // a redundant second judge call (unlike the harden path
                            // which has a separate max_rounds code path with its own
                            // judge invocation for the revise evaluation).
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
                                orchestrator_hint: judge_output.hint,
                            };

                            record.round += 1;
                            let feedback_path = format!(
                                ".agent/review-feedback-round-{}.json",
                                record.round - 1
                            );
                            return self
                                .dispatch_implement_with_feedback(
                                    record,
                                    &feedback,
                                    &feedback_path,
                                )
                                .await;
                        }
                    }
                }

                // No judge (disabled/error/timeout): use heuristic
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
                    orchestrator_hint: None,
                };

                record.round += 1;
                let feedback_path =
                    format!(".agent/review-feedback-round-{}.json", record.round - 1);
                self.dispatch_implement_with_feedback(record, &feedback, &feedback_path)
                    .await
            }
            None => {
                // Verdict parse failure: retry per FR-9
                self.handle_verdict_parse_failure(record).await
            }
        }
    }

    /// Invoke the orchestrator judge for a stage transition.
    /// Returns None if the judge is disabled, not configured, or fails.
    async fn invoke_judge_for_stage(
        &self,
        record: &LoopRecord,
        phase: &str,
        current_verdict: &serde_json::Value,
        current_issues: &[crate::types::verdict::Issue],
        rounds: &[RoundRecord],
    ) -> Option<crate::types::verdict::JudgeOutput> {
        let judge = self.judge.as_ref()?;

        // FR-1c: Skip judge on clean verdicts at round 1 (handled by caller).
        // Read spec content for the judge context.
        let spec_content = self
            .git
            .read_file(&record.spec_path, &record.branch)
            .await
            .unwrap_or_else(|_| String::new());

        judge
            .evaluate(
                record.id,
                &record.spec_path,
                &spec_content,
                phase,
                record.round,
                record.max_rounds,
                current_verdict,
                current_issues,
                rounds,
            )
            .await
    }

    /// Converge the harden loop as clean (shared by audit clean path and judge exit_clean).
    async fn harden_converge_clean(&self, record: &mut LoopRecord) -> Result<LoopState> {
        if let Err(e) = self.git.remove_path(&record.branch, ".agent").await {
            tracing::warn!(loop_id = %record.id, error = %e, "Failed to clean up .agent/ artifacts, proceeding with PR");
        }

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
            .create_pr(
                &record.branch,
                &pr_title,
                &pr_body,
                &self.default_branch_for(record),
            )
            .await?;
        record.spec_pr_url = Some(pr_url);

        if self.config.harden.auto_merge_spec_pr {
            let merge_sha = self
                .git
                .merge_pr(
                    &record.branch,
                    &self.config.harden.merge_strategy,
                    &self.default_branch_for(record),
                )
                .await?;
            record.merge_sha = Some(merge_sha);
            record.merged_at = Some(chrono::Utc::now());
        }

        record.state = LoopState::Hardened;
        record.sub_state = None;
        record.active_job_name = None;
        record.hardened_spec_path = Some(record.spec_path.clone());
        self.store.update_loop(record).await?;
        tracing::info!(loop_id = %record.id, "Harden loop HARDENED");
        Ok(LoopState::Hardened)
    }

    /// Converge the review stage as clean: create PR and transition to Converged/Shipped.
    /// Shared by the normal clean path and judge exit_clean.
    async fn review_converge_clean(&self, record: &mut LoopRecord) -> Result<LoopState> {
        // Create PR if not already created (idempotent across ticks)
        if record.spec_pr_url.is_none() {
            if let Err(e) = self.git.remove_path(&record.branch, ".agent").await {
                tracing::warn!(loop_id = %record.id, error = %e, "Failed to clean up .agent/ artifacts, proceeding with PR");
            }

            let pr_title =
                format!("feat(agent): {} for {}", record.spec_path, record.engineer,);
            let pr_body = format!(
                "Automated convergence loop completed in {} round(s).\n\nSpec: {}\nBranch: {}",
                record.round, record.spec_path, record.branch,
            );
            let pr_url = self
                .git
                .create_pr(
                    &record.branch,
                    &pr_title,
                    &pr_body,
                    &self.default_branch_for(record),
                )
                .await?;
            record.spec_pr_url = Some(pr_url);
            self.store.update_loop(record).await?;
        }

        // Support ship mode for judge exit_clean, same as reviewer clean path
        if record.ship_mode {
            let threshold = self.config.ship.max_rounds_for_auto_merge as i32;
            if record.round <= threshold {
                if self.config.ship.require_passing_ci {
                    match self.git.ci_status(&record.branch).await {
                        Ok(Some(true)) => {
                            // CI passed, proceed to merge
                        }
                        Ok(Some(false)) => {
                            record.state = LoopState::Converged;
                            record.sub_state = None;
                            record.active_job_name = None;
                            record.failure_reason = Some(
                                "CI checks failed. PR created but not merged.".to_string(),
                            );
                            self.store.update_loop(record).await?;
                            tracing::warn!(
                                loop_id = %record.id,
                                "Ship mode (judge exit_clean): CI failed, converging without merge"
                            );
                            return Ok(LoopState::Converged);
                        }
                        Ok(None) => {
                            // CI still pending: return current state, check again next tick
                            tracing::debug!(
                                loop_id = %record.id,
                                "Ship mode (judge exit_clean): CI pending, will check on next tick"
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

                let merge_sha = self
                    .git
                    .merge_pr(
                        &record.branch,
                        &self.config.ship.merge_strategy,
                        &self.default_branch_for(record),
                    )
                    .await?;

                record.state = LoopState::Shipped;
                record.sub_state = None;
                record.active_job_name = None;
                record.merge_sha = Some(merge_sha.clone());
                record.merged_at = Some(chrono::Utc::now());
                self.store.update_loop(record).await?;

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
                    "Loop SHIPPED (judge exit_clean, auto-merge within threshold)"
                );
                return Ok(LoopState::Shipped);
            } else {
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
                    "Loop CONVERGED (judge exit_clean, above ship threshold)"
                );
                return Ok(LoopState::Converged);
            }
        }

        record.state = LoopState::Converged;
        record.sub_state = None;
        record.active_job_name = None;
        self.store.update_loop(record).await?;
        tracing::info!(loop_id = %record.id, round = record.round, "Loop CONVERGED (judge exit_clean)");
        Ok(LoopState::Converged)
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

    /// Delete stale k8s Job objects from a failed loop's previous
    /// exhausted retry attempts so the resumed dispatch (which resets
    /// retry_count back to 0) can reuse the lower `-t{N}` name slots
    /// without hitting AlreadyExists.
    ///
    /// This is NOT best-effort. The kube dispatcher treats 404 NotFound
    /// as Ok(()) internally (see k8s/client.rs), so any Err returned
    /// here is a real API/RBAC/network failure. If we swallowed it and
    /// let redispatch proceed, create_job would hit AlreadyExists on the
    /// still-present stale attempt, the loop would transition out of
    /// Failed with failed_from_state=None, and the resume would be
    /// silently wedged until manual cleanup. Propagate the error so
    /// handle_failed bails out cleanly with the loop still in Failed
    /// state and the operator can retry the resume after fixing the
    /// underlying k8s condition. See codex round-2 review of #96.
    async fn delete_stale_failed_attempts(
        &self,
        record: &LoopRecord,
        failed_from: LoopState,
    ) -> Result<()> {
        // Map the failed-from state to the stage name used in job names.
        // For Hardening we inspect the last round record to tell audit
        // apart from revise, same logic as redispatch_current_stage.
        let stage_name: Option<String> = match failed_from {
            LoopState::Hardening => {
                let rounds = self.store.get_rounds(record.id).await?;
                rounds
                    .iter()
                    .rfind(|r| r.round == record.round)
                    .map(|r| r.stage.clone())
                    .or_else(|| Some("audit".to_string()))
            }
            LoopState::Implementing => Some("implement".to_string()),
            LoopState::Testing => Some("test".to_string()),
            LoopState::Reviewing => Some("review".to_string()),
            _ => None,
        };

        let Some(stage) = stage_name else {
            return Ok(());
        };

        let short_id = &record.id.to_string()[..8];
        let namespace = &self.config.cluster.jobs_namespace;
        // Delete attempts 1..=retry_count+1 — the full range that the
        // failed loop had consumed before transitioning to Failed.
        let max_attempt = record.retry_count + 1;
        for attempt in 1..=max_attempt {
            let job_name = format!("nautiloop-{short_id}-{stage}-r{}-t{attempt}", record.round);
            self.dispatcher.delete_job(&job_name, namespace).await?;
        }
        Ok(())
    }

    /// Handle FAILED: check for resume flag (#96).
    ///
    /// When an engineer runs `nemo resume <loop-id>` on a FAILED loop, the
    /// API handler sets resume_requested=true. The next reconciler tick
    /// lands here, flips state back to failed_from_state, and calls
    /// redispatch_current_stage. The existing worktree is preserved
    /// because redispatch_current_stage does not touch the PVC layout —
    /// it only issues a fresh K8s Job against the same branch/sha pair.
    async fn handle_failed(&self, record: &LoopRecord) -> Result<LoopState> {
        if record.resume_requested {
            if let Some(failed_from) = record.failed_from_state {
                // Before resetting retry_count, delete the stale k8s Job
                // objects from the prior exhausted attempts. Their TTL
                // window can be up to an hour, so without this cleanup
                // the resumed dispatch hits AlreadyExists on names like
                // `...-r{round}-t1` and spins. A failure here aborts
                // the resume — see the long comment on the helper.
                //
                // On abort we clear resume_requested so the branch
                // ownership query (which treats FAILED+resume as
                // active) stops counting this row. The loop goes back
                // to plain FAILED: the operator can either re-run
                // `nemo resume` after fixing the k8s condition, or
                // abandon it with a fresh `nemo harden` on the same
                // spec (which is now unblocked from the branch). See
                // codex round-4 review of #96.
                if let Err(e) = self.delete_stale_failed_attempts(record, failed_from).await {
                    tracing::error!(
                        loop_id = %record.id,
                        error = %e,
                        "Failed to clean up stale k8s Jobs; releasing branch ownership so operator can retry or abandon"
                    );
                    let _ = self
                        .store
                        .set_loop_flag(record.id, crate::state::LoopFlag::Resume, false)
                        .await;
                    return Err(e);
                }

                let mut updated = record.clone();
                updated.state = failed_from;
                updated.failed_from_state = None;
                updated.retry_count = 0; // Fresh budget for the resumed stage
                updated.failure_reason = None;
                updated.active_job_name = None;
                // Refresh current_sha so the divergence check doesn't
                // immediately re-pause after resume (same reasoning as
                // handle_paused / handle_awaiting_reauth).
                if let Ok(Some(sha)) = self.git.get_branch_sha(&record.branch).await {
                    updated.current_sha = Some(sha);
                }
                // Redispatch can still fail after stale cleanup (e.g.
                // the worktree/branch can no longer be resolved, build_job
                // fails, k8s create rejects). Clear the resume flag on
                // error so this terminal row doesn't keep getting picked
                // up by the reconciler holding the branch hostage. The
                // loop stays Failed; operator can retry after fixing the
                // underlying cause.
                match self.redispatch_current_stage(&updated).await {
                    Ok(result) => {
                        self.store
                            .set_loop_flag(record.id, crate::state::LoopFlag::Resume, false)
                            .await?;
                        tracing::info!(
                            loop_id = %record.id,
                            round = updated.round,
                            target_state = ?failed_from,
                            "Resumed FAILED loop"
                        );
                        Ok(result)
                    }
                    Err(e) => {
                        // redispatch_current_stage persists the cloned
                        // record at the target active state BEFORE
                        // calling create_job, so a failure here leaves
                        // the row in e.g. Hardening with no job and no
                        // failure metadata. Roll it back to FAILED with
                        // the original failed_from_state restored so
                        // the operator sees the same row they had
                        // before the failed resume attempt, and the
                        // reconciler doesn't auto-redispatch.
                        tracing::error!(
                            loop_id = %record.id,
                            error = %e,
                            "Redispatch during resume failed; rolling row back to FAILED and releasing branch"
                        );
                        if let Ok(Some(mut current)) = self.store.get_loop(record.id).await {
                            current.state = LoopState::Failed;
                            current.sub_state = None;
                            current.failed_from_state = Some(failed_from);
                            current.failure_reason = Some(format!("Resume redispatch failed: {e}"));
                            current.active_job_name = None;
                            if let Err(update_err) = self.store.update_loop(&current).await {
                                tracing::error!(
                                    loop_id = %record.id,
                                    error = %update_err,
                                    "Failed to roll row back to FAILED after resume error"
                                );
                            }
                        }
                        let _ = self
                            .store
                            .set_loop_flag(record.id, crate::state::LoopFlag::Resume, false)
                            .await;
                        Err(e)
                    }
                }
            } else {
                // No failed_from_state: either the loop failed via a
                // non-transient path (max rounds, logical failure) or it
                // predates #96. Either way, there's nothing to resume to —
                // clear the flag and stay Failed so nemo resume doesn't
                // spin forever.
                self.store
                    .set_loop_flag(record.id, crate::state::LoopFlag::Resume, false)
                    .await?;
                tracing::warn!(
                    loop_id = %record.id,
                    "Resume requested on FAILED loop with no failed_from_state; ignoring"
                );
                Ok(LoopState::Failed)
            }
        } else {
            Ok(LoopState::Failed)
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
                // Refresh current_sha so divergence check doesn't false-pause
                if let Ok(Some(sha)) = self.git.get_branch_sha(&record.branch).await {
                    updated.current_sha = Some(sha);
                }
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
        self.sync_current_stage_logs(record).await;

        // Delete active job if any (log failure but proceed — orphan cleanup handles stragglers)
        if let Some(ref job_name) = record.active_job_name
            && let Err(e) = self
                .dispatcher
                .delete_job(job_name, &self.config.cluster.jobs_namespace)
                .await
        {
            tracing::warn!(loop_id = %record.id, job = job_name, error = %e, "Failed to delete job during cancel");
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

        self.sync_current_stage_logs(record).await;

        if let Some(ref job_name) = record.active_job_name
            && let Err(e) = self
                .dispatcher
                .delete_job(job_name, &self.config.cluster.jobs_namespace)
                .await
        {
            tracing::warn!(loop_id = %record.id, job = job_name, error = %e, "Failed to delete job during auth expiry");
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
        self.handle_job_failed_inner(record, reason, true).await
    }

    /// Like `handle_job_failed` but does NOT mark the exhausted Failed state
    /// as resumable via #96. Use this for failures where ingest_job_output
    /// has already stamped completed_at on the current round (e.g. a job
    /// that succeeded but produced no NAUTILOOP_RESULT line). Redispatching
    /// those would emit a new round output that ingest_job_output ignores
    /// because it only writes rows with completed_at IS NULL, so the
    /// evaluator would just re-read the stale empty output and fail again.
    async fn handle_job_failed_non_resumable(
        &self,
        record: &LoopRecord,
        reason: &str,
    ) -> Result<LoopState> {
        self.handle_job_failed_inner(record, reason, false).await
    }

    async fn handle_job_failed_inner(
        &self,
        record: &LoopRecord,
        reason: &str,
        resumable_on_exhaustion: bool,
    ) -> Result<LoopState> {
        let mut updated = record.clone();

        self.sync_current_stage_logs(record).await;

        // Detect credential expiry (FR-10): transition to AWAITING_REAUTH
        if is_auth_error(reason) && record.state.is_active_stage() {
            if let Some(ref job_name) = record.active_job_name
                && let Err(e) = self
                    .dispatcher
                    .delete_job(job_name, &self.config.cluster.jobs_namespace)
                    .await
            {
                tracing::warn!(loop_id = %record.id, job = job_name, error = %e, "Failed to delete job during reauth");
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
            // Exhausted retries: fail the loop.
            // Only mark the Failed state resumable (#96) when the
            // caller vouches that redispatch would actually produce
            // new round output. Logical failures (empty test output,
            // implement completed without result) leave failed_from_state
            // None so /resume rejects them cleanly.
            if resumable_on_exhaustion {
                updated.failed_from_state = Some(updated.state);
            }
            updated.state = LoopState::Failed;
            updated.sub_state = None;
            updated.failure_reason =
                Some(format!("{reason} (after {} retries)", updated.retry_count));
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
            // NOT resumable via #96: by the time we get here, the round
            // record has already been marked completed by ingest_job_output
            // with the malformed verdict. Redispatching would produce a
            // new run whose output gets dropped (ingest_job_output only
            // writes to rounds where completed_at IS NULL), so the
            // evaluator would just re-read the same malformed output and
            // fail again. Leave failed_from_state None so api::resume
            // rejects it with a clear message until we add per-resume
            // round-reset logic. See codex round-3 review.
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
        // Phase transition: clear sessions from the preceding harden
        // phase so the first implement stage doesn't --resume a revise
        // conversation. The implement phase builds its own sessions.
        updated.opencode_session_id = None;
        updated.claude_session_id = None;

        // #98: Claude credential preflight. See the comment on the
        // matching block in dispatch_revise for why we insert a
        // sentinel implement round only on the reauth path, not
        // the fresh-creds path — without a round record, a later
        // redispatch_current_stage creates a pod but ingest_job_output
        // has nowhere to attach the result and the resumed run is
        // dropped as "produced no NAUTILOOP_RESULT" (codex round 5).
        if let Some(reauth_state) = self
            .preflight_claude_creds(&updated, LoopState::Implementing)
            .await?
        {
            self.create_round_record(&updated, "implement", "preflight-pending")
                .await?;
            return Ok(reauth_state);
        }

        let stage_config = self.implement_stage_config(record);
        let mut ctx = self.build_context(&updated).await?;
        ctx.session_id = Self::session_id_for_stage(&updated, "implement");
        let job = job_builder::build_job(&ctx, &stage_config, &self.job_build_config());
        self.persist_then_dispatch(&mut updated, "implement", &job)
            .await?;

        tracing::info!(loop_id = %record.id, round = updated.round, "Started IMPLEMENTING/DISPATCHED");
        Ok(LoopState::Implementing)
    }

    /// Dispatch an audit job (harden loop).
    async fn dispatch_audit(&self, record: &mut LoopRecord) -> Result<LoopState> {
        record.state = LoopState::Hardening;
        record.sub_state = Some(SubState::Dispatched);
        record.retry_count = 0;

        let stage_config = self.audit_stage_config(record);
        let mut ctx = self.build_context(record).await?;
        ctx.session_id = Self::session_id_for_stage(record, "audit");
        let job = job_builder::build_job(&ctx, &stage_config, &self.job_build_config());
        self.persist_then_dispatch(record, "audit", &job).await?;

        Ok(LoopState::Hardening)
    }

    /// Dispatch a revise job (harden loop).
    /// If `hint` is provided, writes an audit feedback file with the orchestrator hint
    /// so the revise agent can read it (FR-3: hint injection for harden/Continue path).
    async fn dispatch_revise(
        &self,
        record: &mut LoopRecord,
        hint: Option<String>,
    ) -> Result<LoopState> {
        record.sub_state = Some(SubState::Dispatched);
        record.retry_count = 0;

        // Write audit feedback file with hint if present
        let feedback_path = if hint.is_some() {
            let path = format!(".agent/audit-feedback-round-{}.json", record.round);
            let feedback = FeedbackFile {
                round: record.round as u32,
                source: FeedbackSource::Audit,
                issues: None,
                failures: None,
                orchestrator_hint: hint,
            };
            let feedback_json = serde_json::to_string_pretty(&feedback).map_err(|e| {
                crate::error::NautiloopError::Internal(format!(
                    "Failed to serialize audit feedback: {e}"
                ))
            })?;
            self.git
                .write_file(&record.branch, &path, &feedback_json)
                .await?;
            // Refresh current_sha after commit so divergence detection doesn't false-pause
            if let Some(new_sha) = self.git.get_branch_sha(&record.branch).await? {
                record.current_sha = Some(new_sha);
            }
            Some(path)
        } else {
            None
        };

        // #98: Claude credential preflight. If it blocks, write a
        // sentinel `revise` round record ONLY in that case so that
        // a later `nemo resume` landing in redispatch_current_stage
        // picks `revise` (not `audit`) when it disambiguates the
        // Hardening sub-stage. The sentinel is NOT created on the
        // fresh-creds path — persist_then_dispatch writes the real
        // revise round there, and creating a second synthetic row
        // makes ingest_job_output / rfind-by-round ambiguous when
        // both rows land on the same Postgres timestamp (codex
        // round 4 on #98).
        if let Some(reauth_state) = self
            .preflight_claude_creds(record, LoopState::Hardening)
            .await?
        {
            self.create_round_record(record, "revise", "preflight-pending")
                .await?;
            return Ok(reauth_state);
        }

        let stage_config = self.revise_stage_config(record);
        let mut ctx = self.build_context(record).await?;
        ctx.session_id = Self::session_id_for_stage(record, "revise");
        ctx.feedback_path = feedback_path;
        let job = job_builder::build_job(&ctx, &stage_config, &self.job_build_config());
        self.persist_then_dispatch(record, "revise", &job).await?;

        Ok(LoopState::Hardening)
    }

    /// Dispatch a review job.
    async fn dispatch_review(&self, record: &mut LoopRecord) -> Result<LoopState> {
        record.state = LoopState::Reviewing;
        record.sub_state = Some(SubState::Dispatched);
        record.retry_count = 0;

        let stage_config = self.review_stage_config(record);
        let mut ctx = self.build_context(record).await?;
        ctx.session_id = Self::session_id_for_stage(record, "review");
        let job = job_builder::build_job(&ctx, &stage_config, &self.job_build_config());
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
        let feedback_json = serde_json::to_string_pretty(feedback).map_err(|e| {
            crate::error::NautiloopError::Internal(format!("Failed to serialize feedback: {e}"))
        })?;
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

        // #98: Claude credential preflight. See start_implementing
        // for the sentinel rationale — without a round record, a
        // resumed dispatch has nowhere to attach its output.
        if let Some(reauth_state) = self
            .preflight_claude_creds(record, LoopState::Implementing)
            .await?
        {
            self.create_round_record(record, "implement", "preflight-pending")
                .await?;
            return Ok(reauth_state);
        }

        let stage_config = self.implement_stage_config(record);
        let mut ctx = self.build_context(record).await?;
        ctx.session_id = Self::session_id_for_stage(record, "implement");
        ctx.feedback_path = Some(feedback_path.to_string());

        let job = job_builder::build_job(&ctx, &stage_config, &self.job_build_config());
        self.persist_then_dispatch(record, "implement", &job)
            .await?;

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
        // Clean up the old job before anything else — even if the
        // preflight (below) is about to send us to AWAITING_REAUTH,
        // we still need to delete the stale pod. Otherwise the
        // preflight clears active_job_name on the record, the delete
        // gets skipped, and the orphaned pod (e.g. a PAUSED job that
        // was still running) keeps owning the worktree until k8s TTL
        // cleanup. A later resume would then create a second job
        // against the same branch. See codex round 2 on #98.
        if let Some(ref old_job) = record.active_job_name {
            self.dispatcher
                .delete_job(old_job, &self.config.cluster.jobs_namespace)
                .await?;
        }

        // #98: Redispatch paths (paused resume, reauth resume, failed
        // resume, retry) also create pods and must not bypass the
        // Claude credential preflight. Run it here for
        // implement/revise redispatches; Hardening needs the rounds
        // table to tell audit (opencode, no Claude) from revise
        // (claude), so we inspect that first. If the preflight finds
        // stale creds it transitions the loop to AWAITING_REAUTH and
        // we short-circuit before touching k8s.
        let is_claude_redispatch = match record.state {
            LoopState::Implementing => true,
            LoopState::Hardening => {
                let rounds = self.store.get_rounds(record.id).await?;
                let last_stage = rounds
                    .iter()
                    .rfind(|r| r.round == record.round)
                    .map(|r| r.stage.as_str());
                matches!(last_stage, Some("revise"))
            }
            _ => false,
        };
        if is_claude_redispatch
            && let Some(reauth_state) = self.preflight_claude_creds(record, record.state).await?
        {
            return Ok(reauth_state);
        }

        let mut updated = record.clone();
        updated.sub_state = Some(SubState::Dispatched);

        let (stage_config, stage_name) = match record.state {
            LoopState::Hardening => {
                // Determine which harden sub-stage to redispatch by checking the latest round
                let rounds = self.store.get_rounds(record.id).await?;
                let last_stage = rounds
                    .iter()
                    .rfind(|r| r.round == record.round)
                    .map(|r| r.stage.as_str());
                match last_stage {
                    Some("revise") => (self.revise_stage_config(record), "revise"),
                    _ => (self.audit_stage_config(record), "audit"),
                }
            }
            LoopState::Implementing => (self.implement_stage_config(record), "implement"),
            LoopState::Testing => (self.test_stage_config(), "test"),
            LoopState::Reviewing => (self.review_stage_config(record), "review"),
            _ => return Ok(record.state),
        };

        let mut ctx = self.build_context(&updated).await?;
        ctx.session_id = Self::session_id_for_stage(record, stage_name);

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

        // Restore feedback_path for revise redispatch: check for audit hint file
        if record.state == LoopState::Hardening && stage_name == "revise" {
            let audit_feedback = format!(".agent/audit-feedback-round-{}.json", record.round);
            // Only set if the file was previously written (hint was present)
            if self
                .git
                .read_file(&audit_feedback, &record.branch)
                .await
                .is_ok()
            {
                ctx.feedback_path = Some(audit_feedback);
            }
        }

        let job = job_builder::build_job(&ctx, &stage_config, &self.job_build_config());

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

    fn audit_stage_config(&self, record: &LoopRecord) -> StageConfig {
        StageConfig {
            name: "audit".to_string(),
            model: Some(
                record
                    .model_reviewer
                    .clone()
                    .unwrap_or_else(|| self.config.models.reviewer.clone()),
            ),
            prompt_template: Some(".nautiloop/prompts/spec-audit.md".to_string()),
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
            prompt_template: Some(".nautiloop/prompts/spec-revise.md".to_string()),
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
            prompt_template: Some(".nautiloop/prompts/implement.md".to_string()),
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
            prompt_template: Some(".nautiloop/prompts/review.md".to_string()),
            timeout: self.config.timeouts.review_duration(),
            max_retries: 2,
        }
    }

    /// Resolve the default branch for a loop record (frozen per-loop, fallback to config).
    fn default_branch_for(&self, record: &LoopRecord) -> String {
        record
            .resolved_default_branch
            .clone()
            .unwrap_or_else(|| self.config.cluster.default_branch.clone())
    }

    fn max_retries_for_stage(&self, _state: LoopState) -> u32 {
        2 // All stages default to 2 retries
    }

    /// Build context with credentials loaded from the store.
    /// Resolve the session ID for a given stage's tool. opencode stages
    /// (audit, review) get the opencode session; claude stages (implement,
    /// revise) get the claude session. Callers set ctx.session_id after
    /// build_context using this helper.
    ///
    /// Session IDs are NOT forwarded across phase boundaries:
    /// - audit ↔ revise (same harden phase): shared opencode + claude sessions
    /// - audit → review, revise → implement: different phases, fresh sessions
    ///
    /// A `review` or `implement` stage at the START of its phase must NOT
    /// inherit a session from the harden phase that preceded it. The helper
    /// uses `record.state` to determine the current phase and only returns
    /// a session ID if the stage matches the phase.
    fn session_id_for_stage(record: &LoopRecord, stage: &str) -> Option<String> {
        let in_harden_phase = matches!(record.state, LoopState::Hardening);
        match stage {
            "audit" if in_harden_phase => record.opencode_session_id.clone(),
            "revise" if in_harden_phase => record.claude_session_id.clone(),
            "implement" if !in_harden_phase => record.claude_session_id.clone(),
            "review" if !in_harden_phase => record.opencode_session_id.clone(),
            // Cross-phase transitions (audit → implement, revise → review, etc.)
            // start fresh sessions. This matches the pre-#100 behavior where the
            // bash filter in agent-entry would drop the wrong-format ID.
            _ => None,
        }
    }

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

        // Engineer identity: look up name and email from stored credentials,
        // fall back to engineer slug / {engineer}@nautiloop.dev if not set.
        let all_creds = self.store.get_credentials(&record.engineer).await?;
        let engineer_name = all_creds
            .iter()
            .find(|c| c.provider == "_name" && c.valid)
            .map(|c| c.credential_ref.clone())
            .unwrap_or_else(|| record.engineer.clone());
        let engineer_email = all_creds
            .iter()
            .find(|c| c.provider == "_email" && c.valid)
            .map(|c| c.credential_ref.clone())
            .unwrap_or_else(|| format!("{}@nautiloop.dev", record.engineer));

        // Derive worktree sub-path from branch name.
        // Use "wt/" prefix (not "worktrees/") to avoid colliding with git's
        // internal worktree metadata directory in the bare repo.
        let worktree_dir = record.branch.replace('/', "-");
        let worktree_path = format!("wt/{worktree_dir}");

        // Ensure the worktree exists on disk before any job tries to mount it.
        self.git
            .ensure_worktree(&record.branch, &worktree_path)
            .await?;

        // Resolve current_sha. The happy path is that POST /start already
        // set it (create_branch -> set_current_sha before returning 201).
        // But the handler has an inherent race: the loop row is inserted
        // with current_sha = NULL *before* create_branch and set_current_sha
        // run, so a reconciler tick that fires in that 1-3s window picks up
        // the PENDING loop with no SHA and — via the old
        // `record.current_sha.clone().unwrap_or_default()` path — dispatches
        // a job with SHA="". The agent entrypoint correctly rejects that:
        //
        //     NAUTILOOP_ERROR: entrypoint: missing required environment
        //     variables: SHA
        //
        // Loop then fails as BackoffLimitExceeded after the K8s Job retries.
        //
        // Fall back to resolving the branch tip from the bare repo. By the
        // time we get here `ensure_worktree` has already succeeded for
        // `record.branch`, so the branch is guaranteed to exist and
        // `get_branch_sha` returns a real SHA.
        let current_sha = match record.current_sha.clone() {
            Some(sha) if !sha.is_empty() => sha,
            _ => self
                .git
                .get_branch_sha(&record.branch)
                .await?
                .ok_or_else(|| {
                    crate::error::NautiloopError::Internal(format!(
                        "Failed to resolve current_sha for branch {} — \
                         branch does not exist in bare repo",
                        record.branch
                    ))
                })?,
        };

        Ok(LoopContext {
            loop_id: record.id,
            engineer: record.engineer.clone(),
            engineer_name,
            engineer_email,
            spec_path: record.spec_path.clone(),
            branch: record.branch.clone(),
            current_sha,
            round: record.round as u32,
            max_rounds: record.max_rounds as u32,
            retry_count: record.retry_count as u32,
            // Stage-aware session ID resolution (#100): dispatch
            // function callers override this with the right per-tool
            // session ID after build_context returns. Default to None
            // because build_context doesn't know the stage.
            session_id: None,
            feedback_path,
            worktree_path,
            credentials,
            base_branch: self.default_branch_for(record),
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

    /// #98: Check whether the engineer's Claude credentials are
    /// fresh before building a pod that invokes the `claude` CLI.
    /// When stale, transition the loop to AWAITING_REAUTH in place
    /// and return `Ok(Some(AwaitingReauth))` so the caller can
    /// short-circuit its dispatch. When fresh (or no bundle is
    /// present), return `Ok(None)` and let dispatch proceed.
    ///
    /// `reauth_from` is the stage we should resume to once the user
    /// re-runs `nemo auth`. For implement/revise that's the stage
    /// itself; for Hardening-wrapped revise that's Hardening.
    async fn preflight_claude_creds(
        &self,
        record: &LoopRecord,
        reauth_from: LoopState,
    ) -> Result<Option<LoopState>> {
        let Some(reason) = self.claude_creds_stale_reason(&record.engineer).await else {
            return Ok(None);
        };
        tracing::warn!(
            loop_id = %record.id,
            reason = %reason,
            "Claude credentials failed pre-dispatch freshness check; transitioning to AWAITING_REAUTH"
        );
        let mut updated = record.clone();
        updated.state = LoopState::AwaitingReauth;
        updated.sub_state = None;
        updated.reauth_from_state = Some(reauth_from);
        updated.active_job_name = None;
        updated.failure_reason = Some(format!("Credential preflight: {reason}"));
        // Offset the retry counter by -1 to cancel the mandatory +1
        // bump that handle_awaiting_reauth applies on the next
        // resume. The preflight never created a pod, so no job-name
        // collision justifies burning a retry slot on it. Starting
        // at retry_count - 1 means:
        //   start_implementing, retry_count=0 → preflight → stored
        //     as -1 → resume bumps to 0 → first real dispatch is
        //     attempt 1 (matches normal behavior).
        //   mid-retry handle_job_failed bumped to 2 → preflight →
        //     stored as 1 → resume bumps to 2 → dispatch attempt 3
        //     (preserves the two prior real failures).
        // See codex rounds 3, 7, and 8 on #98 for the full
        // ping-pong that led here.
        updated.retry_count -= 1;
        self.store.update_loop(&updated).await?;
        Ok(Some(LoopState::AwaitingReauth))
    }

    /// Read the engineer's Claude credential bundle straight from the
    /// K8s API server (not from any cached or pod-mounted view) and
    /// return a human-readable reason if it's stale, or None if it's
    /// fresh. "Stale" includes three cases:
    ///
    /// - The `claude` key is missing. This is the only source of
    ///   claude credentials in the mounted pod (job_builder.rs:82-99
    ///   mounts the secret's `claude` key at ~/.claude/.credentials.json
    ///   for implement/revise stages), so missing means the pod
    ///   will 401 on its first claude call — fatal for the stages
    ///   that call this helper.
    /// - The bundle has an expiresAt within 5 minutes of now.
    /// - The bundle is unparseable.
    ///
    /// Bundles without an `expiresAt` field (legacy / Linux session
    /// files) pass through as fresh since we can't prove they're
    /// stale; the existing runtime 401 detection handles them.
    ///
    /// Returns None if the underlying secret GET itself fails
    /// (RBAC/network) so the preflight never hard-blocks dispatch
    /// on control-plane infrastructure flakes. See issue #98.
    async fn claude_creds_stale_reason(&self, engineer: &str) -> Option<String> {
        const BUFFER_MS: u64 = 5 * 60 * 1000;
        let safe_engineer: String = engineer.to_lowercase().replace('_', "-");
        let secret_name = format!("nautiloop-creds-{safe_engineer}");
        let namespace = &self.config.cluster.jobs_namespace;

        let bytes = match self
            .dispatcher
            .get_secret_key(&secret_name, namespace, "claude")
            .await
        {
            Ok(Some(b)) => b,
            Ok(None) => {
                // The preflight is only called from stages that
                // actually invoke `claude` (implement / revise /
                // redispatch-of-those). A missing key means the
                // pod will 401 on its first claude call — same
                // failure mode as an expired token, so treat it
                // the same way: signal stale and let AWAITING_REAUTH
                // drive the user to `nemo auth --claude`.
                return Some(
                    "Claude credentials not registered — run `nemo auth --claude`".to_string(),
                );
            }
            Err(e) => {
                tracing::warn!(
                    engineer = %engineer,
                    error = %e,
                    "Could not read Claude credentials secret; skipping preflight"
                );
                return None;
            }
        };
        let Ok(text) = String::from_utf8(bytes) else {
            return Some("credential bundle is not UTF-8".to_string());
        };
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Some("credential bundle is not JSON".to_string());
        };
        let expires_at = parsed
            .get("claudeAiOauth")
            .and_then(|o| o.get("expiresAt"))
            .and_then(|v| v.as_u64());
        let Some(expires_at) = expires_at else {
            // Legacy / Linux session bundles may omit expiresAt. We
            // can't prove they're stale, so don't block dispatch.
            // The worst case is a 401 from the agent itself, which
            // the existing is_auth_error detection handles via the
            // regular AWAITING_REAUTH path.
            tracing::debug!(
                engineer = %engineer,
                "Claude credential bundle has no expiresAt; trusting it and continuing"
            );
            return None;
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if expires_at.saturating_sub(BUFFER_MS) <= now_ms {
            Some(format!(
                "Claude token expired or within 5-minute buffer (expiresAt={expires_at}, now={now_ms})"
            ))
        } else {
            None
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
        ConvergentLoopDriver::new(store, dispatcher, git, NautiloopConfig::default())
    }

    /// #98 test helper: pre-populate fresh Claude credentials in the
    /// mock dispatcher so tests that dispatch implement/revise sail
    /// through the credential preflight. Tests that explicitly
    /// exercise stale creds override this with their own
    /// set_secret_key call after this.
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
            failed_from_state: None,
            failure_reason: None,
            // Matches the real /start handler, which always calls
            // set_current_sha after create_branch before returning 201.
            // The race-window case (current_sha = None reaching the
            // reconciler) is covered by its own dedicated test below:
            // test_build_context_falls_back_to_git_sha_when_record_missing_it
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
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_pending_auto_approve_transitions_to_implementing() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());
        install_fresh_claude_creds(&dispatcher).await;

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
        install_fresh_claude_creds(&dispatcher).await;

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
        install_fresh_claude_creds(&dispatcher).await;

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
        install_fresh_claude_creds(&dispatcher).await;

        let mut record = make_pending_loop(true);
        record.state = LoopState::Implementing;
        record.sub_state = Some(SubState::Running);
        record.cancel_requested = true;
        record.active_job_name = Some("nautiloop-test-job".to_string());
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
        install_fresh_claude_creds(&dispatcher).await;

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
        install_fresh_claude_creds(&dispatcher).await;

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

    /// Regression test for the POST /start race condition: if the reconciler
    /// wins against the handler's set_current_sha call, `build_context` must
    /// fall back to resolving the branch tip from git instead of shipping
    /// SHA="" to the agent. Without this fallback, the agent container
    /// exits with NAUTILOOP_ERROR: missing required environment variables:
    /// SHA and the loop FAILs as BackoffLimitExceeded.
    #[tokio::test]
    async fn test_build_context_falls_back_to_git_sha_when_record_missing_it() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let git = Arc::new(MockGitOperations::new());

        // Simulate the state the API handler produces before it has had
        // the chance to call set_current_sha: the branch exists in the
        // bare repo (create_branch ran) but the loop row still has
        // current_sha = None.
        git.set_branch_sha("agent/alice/test-abc12345", "deadbeefcafebabe")
            .await;

        let driver = ConvergentLoopDriver::new(store, dispatcher, git, NautiloopConfig::default());

        let mut record = make_pending_loop(true);
        record.current_sha = None;

        let ctx = driver.build_context(&record).await.unwrap();
        assert_eq!(
            ctx.current_sha, "deadbeefcafebabe",
            "build_context should fall back to the branch tip in git when \
             the record's current_sha is None (POST /start race window)"
        );
        assert!(
            !ctx.current_sha.is_empty(),
            "current_sha must NEVER reach the agent as an empty string"
        );
    }

    #[tokio::test]
    async fn test_terminal_state_noop() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());
        install_fresh_claude_creds(&dispatcher).await;

        let mut record = make_pending_loop(true);
        record.state = LoopState::Converged;
        store.create_loop(&record).await.unwrap();

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Converged);
    }

    /// Helper: build an expired Claude credential bundle for #98
    /// preflight tests. `expires_at_ms` is the absolute epoch-ms.
    fn make_claude_bundle(expires_at_ms: u64) -> Vec<u8> {
        format!(r#"{{"claudeAiOauth":{{"expiresAt":{expires_at_ms}}}}}"#).into_bytes()
    }

    #[tokio::test]
    async fn test_stale_claude_creds_block_dispatch() {
        // #98: A loop whose engineer's Claude token has expired
        // should transition to AWAITING_REAUTH at dispatch time
        // instead of spinning up a pod that will die on 401.
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());
        install_fresh_claude_creds(&dispatcher).await;

        let mut record = make_pending_loop(false); // implement loop
        record.state = LoopState::AwaitingApproval;
        record.approve_requested = true;
        store.create_loop(&record).await.unwrap();

        // Stash a bundle that's already expired in the mock k8s
        // secret store, matching the name scheme the driver uses.
        dispatcher
            .set_secret_key(
                "nautiloop-creds-alice",
                "claude",
                &make_claude_bundle(1_000), // epoch 1s — ancient
            )
            .await;

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::AwaitingReauth);

        let updated = store.get_loop(record.id).await.unwrap().unwrap();
        assert!(
            updated
                .failure_reason
                .as_ref()
                .unwrap()
                .contains("preflight"),
            "failure_reason should mention the preflight: got {:?}",
            updated.failure_reason
        );
        // No job should have been created.
        assert!(
            dispatcher.created_jobs().await.is_empty(),
            "preflight must short-circuit before any job is created"
        );
    }

    #[tokio::test]
    async fn test_fresh_claude_creds_pass_preflight() {
        // #98: With a fresh bundle, dispatch proceeds normally.
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());
        install_fresh_claude_creds(&dispatcher).await;

        let mut record = make_pending_loop(false);
        record.state = LoopState::AwaitingApproval;
        record.approve_requested = true;
        store.create_loop(&record).await.unwrap();

        // One hour in the future — comfortably outside the 5-minute buffer.
        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 60 * 60 * 1000;
        dispatcher
            .set_secret_key(
                "nautiloop-creds-alice",
                "claude",
                &make_claude_bundle(future_ms),
            )
            .await;

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Implementing);
        assert!(
            !dispatcher.created_jobs().await.is_empty(),
            "fresh creds should let dispatch create a job"
        );
    }

    #[tokio::test]
    async fn test_missing_claude_secret_blocks_dispatch() {
        // #98 codex round 1: a missing claude key at an
        // implement/revise dispatch means the pod would 401 on its
        // first claude call. Treat it the same as an expired token
        // so the engineer is pushed through nemo auth --claude
        // before wasting a dispatch.
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());
        // Deliberately NO install_fresh_claude_creds — this test
        // exercises the missing-secret path.

        let mut record = make_pending_loop(false);
        record.state = LoopState::AwaitingApproval;
        record.approve_requested = true;
        store.create_loop(&record).await.unwrap();
        // NO set_secret_key — secret is absent.

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::AwaitingReauth);
        assert!(dispatcher.created_jobs().await.is_empty());
    }

    #[tokio::test]
    async fn test_paused_resume_redispatches() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());
        install_fresh_claude_creds(&dispatcher).await;

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
        install_fresh_claude_creds(&dispatcher).await;

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
        install_fresh_claude_creds(&dispatcher).await;

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
        assert!(updated.failure_reason.as_ref().unwrap().contains("OOM"));
        // #96: failed_from_state is captured so `nemo resume` can
        // redispatch the same stage later without guessing.
        assert_eq!(updated.failed_from_state, Some(LoopState::Implementing));
    }

    #[tokio::test]
    async fn test_failed_resume_redispatches_same_stage() {
        // #96: A transient-FAILED loop with resume_requested=true
        // should flip back to failed_from_state and redispatch on tick.
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());
        install_fresh_claude_creds(&dispatcher).await;

        let mut record = make_pending_loop(true);
        record.state = LoopState::Failed;
        record.failed_from_state = Some(LoopState::Hardening);
        record.failure_reason = Some("insufficient_quota (after 2 retries)".to_string());
        record.active_job_name = Some("stale-job".to_string());
        record.retry_count = 2;
        record.round = 4;
        store.create_loop(&record).await.unwrap();
        store
            .set_loop_flag(record.id, crate::state::LoopFlag::Resume, true)
            .await
            .unwrap();

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Hardening);

        let updated = store.get_loop(record.id).await.unwrap().unwrap();
        assert_eq!(updated.state, LoopState::Hardening);
        assert_eq!(updated.failed_from_state, None);
        assert_eq!(updated.failure_reason, None);
        assert_eq!(updated.retry_count, 0);
        assert!(
            !updated.resume_requested,
            "resume flag should be cleared after successful redispatch"
        );
    }

    #[tokio::test]
    async fn test_failed_resume_without_failed_from_state_noops() {
        // #96: A FAILED loop with NO failed_from_state (e.g. max-rounds
        // exhaustion) should stay Failed and just clear the flag — no
        // infinite reconciler loop, no guessing a stage.
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());
        install_fresh_claude_creds(&dispatcher).await;

        let mut record = make_pending_loop(true);
        record.state = LoopState::Failed;
        record.failed_from_state = None;
        record.failure_reason = Some("Max harden rounds (10) exceeded".to_string());
        store.create_loop(&record).await.unwrap();
        store
            .set_loop_flag(record.id, crate::state::LoopFlag::Resume, true)
            .await
            .unwrap();

        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Failed);

        let updated = store.get_loop(record.id).await.unwrap().unwrap();
        assert!(
            !updated.resume_requested,
            "resume flag should be cleared even when no target stage exists"
        );
    }

    #[tokio::test]
    async fn test_auth_error_transitions_to_awaiting_reauth() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let driver = make_driver(store.clone(), dispatcher.clone());
        install_fresh_claude_creds(&dispatcher).await;

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
        install_fresh_claude_creds(&dispatcher).await;

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
        install_fresh_claude_creds(&dispatcher).await;

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
        install_fresh_claude_creds(&dispatcher).await;

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
        assert!(
            updated
                .failure_reason
                .unwrap()
                .contains("above auto-merge threshold")
        );
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
            NautiloopConfig::default(),
        );

        // Set up branch SHA in git and NAUTILOOP_RESULT in mock pod logs
        git.set_branch_sha("agent/alice/test-abc12345", "aabbccdd11223344")
            .await;
        dispatcher.set_job_logs(
            "review-job",
            "some other output\nNAUTILOOP_RESULT:{\"stage\":\"review\",\"data\":{\"clean\":true,\"confidence\":0.95,\"issues\":[],\"summary\":\"LGTM\",\"token_usage\":{\"input\":1000,\"output\":200}}}\n",
        ).await;

        // Create a loop in REVIEWING/DISPATCHED state
        let mut record = make_pending_loop(true);
        record.state = LoopState::Reviewing;
        record.sub_state = Some(SubState::Dispatched);
        record.round = 1;
        record.active_job_name = Some("review-job".to_string());
        // Match the SHA seeded in the mock above so the has_diverged check
        // during output ingestion doesn't false-positive. make_pending_loop
        // now sets a placeholder current_sha (to exercise the common
        // post-/start path), so ingestion tests that also seed a branch in
        // the mock need to pin the record to the same value.
        record.current_sha = Some("aabbccdd11223344".to_string());
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
        assert!(
            updated_round.output.is_some(),
            "Round output should be populated after ingestion"
        );
        assert!(
            updated_round.completed_at.is_some(),
            "completed_at should be set"
        );
        assert!(
            updated_round.duration_secs.is_some(),
            "duration_secs should be set"
        );

        // Verify current_sha was set
        let updated_loop = store.get_loop(record.id).await.unwrap().unwrap();
        assert_eq!(
            updated_loop.current_sha,
            Some("aabbccdd11223344".to_string()),
            "current_sha should be populated from branch tip"
        );
    }

    #[tokio::test]
    async fn test_running_job_syncs_live_logs_without_duplicates() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let git = Arc::new(MockGitOperations::new());
        let driver = ConvergentLoopDriver::new(
            store.clone(),
            dispatcher.clone(),
            git.clone(),
            NautiloopConfig::default(),
        );

        git.set_branch_sha("agent/alice/test-abc12345", "aabbccdd11223344")
            .await;

        let mut record = make_pending_loop(true);
        record.state = LoopState::Implementing;
        record.sub_state = Some(SubState::Dispatched);
        record.round = 1;
        record.active_job_name = Some("impl-job".to_string());
        record.current_sha = Some("aabbccdd11223344".to_string());
        store.create_loop(&record).await.unwrap();

        store
            .create_round(&RoundRecord {
                id: Uuid::new_v4(),
                loop_id: record.id,
                round: 1,
                stage: "implement".to_string(),
                input: None,
                output: None,
                started_at: Some(chrono::Utc::now() - chrono::Duration::seconds(30)),
                completed_at: None,
                duration_secs: None,
                job_name: Some("impl-job".to_string()),
            })
            .await
            .unwrap();

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

        dispatcher
            .set_job_logs("impl-job", "first line\nsecond line\n")
            .await;
        let new_state = driver.tick(record.id).await.unwrap();
        assert_eq!(new_state, LoopState::Implementing);

        let logs = store
            .get_logs(record.id, Some(1), Some("implement"))
            .await
            .unwrap();
        let lines: Vec<&str> = logs.iter().map(|event| event.line.as_str()).collect();
        assert_eq!(lines, vec!["first line", "second line"]);

        driver.tick(record.id).await.unwrap();
        let logs = store
            .get_logs(record.id, Some(1), Some("implement"))
            .await
            .unwrap();
        let lines: Vec<&str> = logs.iter().map(|event| event.line.as_str()).collect();
        assert_eq!(lines, vec!["first line", "second line"]);

        dispatcher
            .set_job_logs("impl-job", "first line\nsecond line\nthird line\n")
            .await;
        driver.tick(record.id).await.unwrap();

        let logs = store
            .get_logs(record.id, Some(1), Some("implement"))
            .await
            .unwrap();
        let lines: Vec<&str> = logs.iter().map(|event| event.line.as_str()).collect();
        assert_eq!(lines, vec!["first line", "second line", "third line"]);
    }

    #[tokio::test]
    async fn test_output_ingestion_rejects_wrong_tool_session_shape() {
        let store = Arc::new(MemoryStateStore::new());
        let dispatcher = Arc::new(MockJobDispatcher::new());
        let git = Arc::new(MockGitOperations::new());
        let driver = ConvergentLoopDriver::new(
            store.clone(),
            dispatcher.clone(),
            git.clone(),
            NautiloopConfig::default(),
        );

        git.set_branch_sha("agent/alice/test-abc12345", "aabbccdd11223344")
            .await;
        dispatcher
            .set_job_logs(
                "review-job",
                "NAUTILOOP_RESULT:{\"stage\":\"review\",\"data\":{\"clean\":true,\"confidence\":0.95,\"issues\":[],\"summary\":\"LGTM\",\"token_usage\":{\"input\":1000,\"output\":200},\"session_id\":\"550e8400-e29b-41d4-a716-446655440000\"}}\n",
            )
            .await;

        let mut record = make_pending_loop(true);
        record.state = LoopState::Reviewing;
        record.sub_state = Some(SubState::Dispatched);
        record.round = 1;
        record.active_job_name = Some("review-job".to_string());
        record.current_sha = Some("aabbccdd11223344".to_string());
        store.create_loop(&record).await.unwrap();

        store
            .create_round(&RoundRecord {
                id: Uuid::new_v4(),
                loop_id: record.id,
                round: 1,
                stage: "review".to_string(),
                input: None,
                output: None,
                started_at: Some(chrono::Utc::now() - chrono::Duration::seconds(30)),
                completed_at: None,
                duration_secs: None,
                job_name: Some("review-job".to_string()),
            })
            .await
            .unwrap();

        let mut updated = store.get_loop(record.id).await.unwrap().unwrap();
        driver.ingest_job_output(&mut updated).await.unwrap();

        assert_eq!(updated.opencode_session_id, None);
        assert_eq!(updated.claude_session_id, None);
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
            NautiloopConfig::default(),
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
        install_fresh_claude_creds(&dispatcher).await;

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
