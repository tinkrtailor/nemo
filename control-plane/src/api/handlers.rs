use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use uuid::Uuid;

use super::AppState;
use crate::error::NautiloopError;
use crate::state::LoopFlag;
use crate::types::api::{
    ApproveResponse, CancelResponse, CredentialRequest, DiffQuery, DiffResponse, ExtendRequest,
    ExtendResponse, InspectResponse, LogsQuery, LoopSummary, ResumeResponse, RoundSummary,
    StartRequest, StartResponse, StatusQuery, StatusResponse,
};
use crate::types::{LoopKind, LoopRecord, LoopState, generate_branch_name};

/// Query parameters for GET /inspect
#[derive(Debug, serde::Deserialize)]
pub struct InspectQuery {
    pub branch: String,
}

/// POST /start - Submit a spec for processing.
pub async fn start(
    State(state): State<AppState>,
    Json(mut req): Json<StartRequest>,
) -> Result<impl IntoResponse, NautiloopError> {
    // Validate engineer name: must be non-empty, lowercase alphanumeric + hyphens.
    // Lowercase enforced to prevent normalization collisions in K8s Secret names.
    if req.engineer.is_empty()
        || req.engineer.len() > 63
        || !req
            .engineer
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || !req
            .engineer
            .starts_with(|c: char| c.is_ascii_alphanumeric())
        || !req.engineer.ends_with(|c: char| c.is_ascii_alphanumeric())
    {
        return Err(NautiloopError::BadRequest(
            "engineer must be 1-63 chars, lowercase alphanumeric with hyphens, \
             starting and ending with alphanumeric"
                .to_string(),
        ));
    }

    // If ship_mode, check that ship is allowed in config
    if req.ship_mode && !state.config.ship.allowed {
        return Err(NautiloopError::ShipNotEnabled);
    }

    // Fetch latest from remote so spec validation and branch creation use current state
    state.git.fetch().await?;

    // Determine spec source: local upload (FR-1b) or default branch (FR-2b).
    let default_ref = state.config.default_remote_ref();
    let spec_from_local = req.spec_content.is_some();

    // Defense-in-depth: apply traversal and absolute-path checks to ALL requests,
    // not just local uploads. While git.read_file limits blast radius to the repo,
    // rejecting malicious paths early is safer. The .md extension and stem checks
    // remain gated behind spec_from_local for NFR-1 backward compat.
    if req.spec_path.trim().is_empty() {
        return Err(NautiloopError::BadRequest(
            "spec_path must not be empty or whitespace-only".to_string(),
        ));
    }
    if req.spec_path.contains('\\') {
        return Err(NautiloopError::BadRequest(
            "spec_path must not contain backslashes".to_string(),
        ));
    }
    if req.spec_path.starts_with('/') {
        return Err(NautiloopError::BadRequest(
            "spec_path must not start with '/'".to_string(),
        ));
    }
    // Check for ".." as a path component (traversal), not as a substring.
    // This allows legitimate filenames containing ".." (e.g. "v2..final.md").
    if req
        .spec_path
        .split('/')
        .any(|seg| seg == ".." || seg == "." || seg.is_empty())
    {
        return Err(NautiloopError::BadRequest(
            "spec_path must not contain '..', '.', or empty path segments".to_string(),
        ));
    }
    if req.spec_path.bytes().any(|b| b < 0x20) {
        return Err(NautiloopError::BadRequest(
            "spec_path must not contain control characters".to_string(),
        ));
    }

    // Additional checks for local uploads only (NFR-1 backward compat for legacy callers).
    if spec_from_local {
        if !req.spec_path.ends_with(".md") {
            return Err(NautiloopError::BadRequest(
                "spec_path must end with '.md'".to_string(),
            ));
        }
        // Reject paths with empty stem (e.g. ".md", "  .md") or control characters.
        let stem = req
            .spec_path
            .rsplit('/')
            .next()
            .unwrap_or(&req.spec_path)
            .strip_suffix(".md")
            .unwrap_or("");
        if stem.is_empty() || stem.trim().is_empty() {
            return Err(NautiloopError::BadRequest(
                "spec_path filename must have a non-empty name before '.md'".to_string(),
            ));
        }
        if stem.starts_with('.') {
            return Err(NautiloopError::BadRequest(
                "spec_path filename must not be a hidden file (no leading '.' before '.md')"
                    .to_string(),
            ));
        }
        if stem.ends_with(".md") {
            return Err(NautiloopError::BadRequest(
                "spec_path filename must not have a double '.md' extension".to_string(),
            ));
        }
    }

    let spec_content = if let Some(content) = req.spec_content.take() {
        // Reject empty spec content — an empty spec is clearly invalid input.
        if content.is_empty() {
            return Err(NautiloopError::BadRequest(
                "spec_content must not be empty".to_string(),
            ));
        }
        // FR-3a: spec_content must be valid UTF-8 (guaranteed by JSON deserialization).
        // FR-3b: enforce 1 MB size limit (byte count, not char count).
        if content.len() > 1_048_576 {
            return Err(NautiloopError::SpecTooLarge {
                size: content.len(),
            });
        }
        content
    } else {
        // FR-2b: legacy path — read from default branch.
        state
            .git
            .read_file(&req.spec_path, &default_ref)
            .await
            .map_err(|_| NautiloopError::SpecNotFound {
                path: req.spec_path.clone(),
            })?
    };
    let branch = generate_branch_name(&req.engineer, &req.spec_path, &spec_content);

    let loop_id = Uuid::new_v4();
    let now = chrono::Utc::now();
    let spec_content_hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(spec_content.as_bytes());
        hex::encode(hasher.finalize())[..8].to_string()
    };

    // Compute effective flags first so max_rounds uses the right values
    // harden_only implies harden; ship_mode with require_harden also implies harden (FR-20)
    let effective_harden =
        req.harden || req.harden_only || (req.ship_mode && state.config.ship.require_harden);

    // ship --harden implies auto_approve (FR-20: zero human gates)
    let effective_auto_approve = req.auto_approve || req.ship_mode;

    let kind = if effective_harden || req.harden_only {
        LoopKind::Harden
    } else {
        LoopKind::Implement
    };

    // Determine max rounds from config (uses effective_harden, not raw req.harden)
    let max_rounds = if effective_harden || req.harden_only {
        state.config.limits.max_rounds_harden as i32
    } else {
        state.config.limits.max_rounds_implement as i32
    };

    // DB insert FIRST — DB is the source of truth; git follows.
    // The partial unique index on (branch, active state) prevents duplicates
    // atomically, so concurrent /start requests are serialized here.
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
        failed_from_state: None,
        failure_reason: None,
        current_sha: None, // Set after git branch creation below
        opencode_session_id: None,
        claude_session_id: None,
        active_job_name: None,
        retry_count: 0,
        model_implementor: req
            .model_overrides
            .as_ref()
            .and_then(|m| m.implementor.clone()),
        model_reviewer: req
            .model_overrides
            .as_ref()
            .and_then(|m| m.reviewer.clone()),
        merge_sha: None,
        merged_at: None,
        hardened_spec_path: None,
        spec_pr_url: None,
        resolved_default_branch: Some(
            default_ref
                .strip_prefix("origin/")
                .unwrap_or(&default_ref)
                .to_string(),
        ),
        created_at: now,
        updated_at: now,
    };

    match state.store.create_loop(&record).await {
        Ok(_) => {}
        Err(NautiloopError::Database(ref e)) if is_unique_violation(e) => {
            return Err(NautiloopError::ActiveLoopConflict {
                branch: branch.clone(),
            });
        }
        Err(e) => return Err(e),
    }

    // DB insert succeeded — we own this branch name. Now create the git branch.
    // If git fails, mark the loop as FAILED via narrow state update (no full record overwrite).
    let mut branch_sha = match state.git.create_branch(&branch, &default_ref).await {
        Ok(sha) => sha,
        Err(e) => {
            let _ = state
                .store
                .update_loop_state(loop_id, LoopState::Failed, None)
                .await;
            return Err(e);
        }
    };

    // FR-2a: If spec was uploaded locally, commit it onto the agent branch as the first commit.
    // FR-3d: Use the engineer's identity for the spec commit, not the control-plane default.
    // Track whether push succeeded so cleanup can delete the remote branch if a later step fails.
    let mut pushed_to_remote = false;
    if spec_from_local {
        match commit_spec_to_branch(
            &state,
            &req.engineer,
            &req.spec_path,
            &spec_content,
            &branch,
            loop_id,
        )
        .await
        {
            Ok(new_sha) => {
                branch_sha = new_sha;
                pushed_to_remote = true;
            }
            Err(e) => {
                let _ = state.git.delete_branch(&branch).await;
                // Best-effort cleanup: the push inside commit_spec_to_branch may have
                // partially succeeded (e.g. network timeout after remote accepted),
                // leaving an orphan remote branch. Try to delete it.
                let _ = state.git.delete_remote_branch(&branch).await;
                let _ = state
                    .store
                    .update_loop_state(loop_id, LoopState::Failed, None)
                    .await;
                return Err(e);
            }
        }
    }

    // Persist the SHA via a narrow SQL update — never use update_loop() from /start
    // because the reconciler may have already advanced the record.
    if let Err(e) = state.store.set_current_sha(loop_id, &branch_sha).await {
        // Clean up: delete local branch, and remote branch if it was already pushed.
        let _ = state.git.delete_branch(&branch).await;
        if pushed_to_remote {
            let _ = state.git.delete_remote_branch(&branch).await;
        }
        let _ = state
            .store
            .update_loop_state(loop_id, LoopState::Failed, None)
            .await;
        return Err(e);
    }

    let spec_source = if spec_from_local {
        "local"
    } else {
        "default_branch"
    };
    tracing::info!(
        loop_id = %loop_id,
        engineer = %req.engineer,
        spec_path = %req.spec_path,
        branch = %branch,
        ship_mode = req.ship_mode,
        spec_source = spec_source,
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
) -> Result<Json<StatusResponse>, NautiloopError> {
    let show_all = query.all.unwrap_or(false);
    let loops = state
        .store
        .get_loops_for_engineer(
            query.engineer.as_deref(),
            query.team.unwrap_or(false),
            show_all,
        )
        .await?;

    let mut summaries = Vec::with_capacity(loops.len());
    for loop_record in loops {
        let current_stage = current_stage_for_loop(&state, &loop_record).await?;
        let active_job_name = loop_record.active_job_name.clone();
        summaries.push(LoopSummary {
            loop_id: loop_record.id,
            engineer: loop_record.engineer.clone(),
            spec_path: loop_record.spec_path,
            branch: loop_record.branch,
            state: loop_record.state,
            sub_state: loop_record.sub_state,
            round: loop_record.round,
            current_stage,
            active_job_name,
            spec_pr_url: loop_record.spec_pr_url,
            failed_from_state: loop_record.failed_from_state,
            kind: match loop_record.kind {
                LoopKind::Harden => "harden".to_string(),
                LoopKind::Implement => "implement".to_string(),
            },
            max_rounds: loop_record.max_rounds,
            model_implementor: loop_record.model_implementor.clone(),
            model_reviewer: loop_record.model_reviewer.clone(),
            created_at: loop_record.created_at,
            updated_at: loop_record.updated_at,
        });
    }

    Ok(Json(StatusResponse { loops: summaries }))
}

fn current_stage_source_state(record: &LoopRecord) -> Option<LoopState> {
    if record.state.is_active_stage() {
        return Some(record.state);
    }

    match record.state {
        LoopState::Paused => record.paused_from_state,
        LoopState::AwaitingReauth => record.reauth_from_state,
        LoopState::Failed => record.failed_from_state,
        _ => None,
    }
}

async fn current_stage_for_loop(
    state: &AppState,
    record: &LoopRecord,
) -> Result<Option<String>, NautiloopError> {
    let Some(source_state) = current_stage_source_state(record) else {
        return Ok(None);
    };

    let direct_stage = match source_state {
        LoopState::Implementing => Some("implement"),
        LoopState::Testing => Some("test"),
        LoopState::Reviewing => Some("review"),
        _ => None,
    };
    if let Some(stage) = direct_stage {
        return Ok(Some(stage.to_string()));
    }

    if source_state != LoopState::Hardening {
        return Ok(None);
    }

    let rounds = state.store.get_rounds(record.id).await?;
    Ok(Some(
        rounds
            .iter()
            .rfind(|round| round.round == record.round)
            .map(|round| round.stage.clone())
            .unwrap_or_else(|| "audit".to_string()),
    ))
}

/// GET /logs/:id - Stream logs via SSE.
pub async fn logs(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<LogsQuery>,
) -> Result<impl IntoResponse, NautiloopError> {
    // Verify loop exists
    let record = state
        .store
        .get_loop(id)
        .await?
        .ok_or(NautiloopError::LoopNotFound { id })?;

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

/// GET /pod-logs/:id?tail=N&container=agent - Live pod logs for a running loop (#99).
///
/// Unlike `/logs/{id}`, this bypasses the Postgres logs table and reads the
/// active pod's container logs directly from the kubernetes API. It's the
/// fastest path to "what is the agent actually printing right now" without
/// requiring kubectl access. Returns 200 with a plain-text body or 404 if
/// the loop has no active pod (terminated, cancelled, between-stage).
///
/// Query params:
/// - `tail`: max lines to return (default 500, max 10000)
/// - `container`: container name to read from (default "agent"; "auth-sidecar"
///   is the other interesting one for egress debugging)
pub async fn pod_logs(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Result<axum::response::Response, NautiloopError> {
    use axum::http::header;
    use axum::response::IntoResponse;

    let record = state
        .store
        .get_loop(id)
        .await?
        .ok_or(NautiloopError::LoopNotFound { id })?;

    // Helper: build a 200 OK informational text body so the CLI
    // doesn't treat the benign "pod not ready yet" race condition
    // like an error. The control plane persists active_job_name
    // before the pod exists, so nemo logs --tail can land here
    // right after a dispatch and succeed a few seconds later. A
    // 4xx here would trigger unnecessary alerting.
    fn info_response(msg: String) -> axum::response::Response {
        use axum::http::header;
        use axum::response::IntoResponse;
        // Custom header so the CLI can detect "info, no real logs"
        // without guessing from body content (codex round 6 on #99).
        (
            axum::http::StatusCode::NO_CONTENT,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            msg,
        )
            .into_response()
    }

    // Terminal loops with no active pod: there's nothing to tail.
    // Return an appropriate error so the CLI can fall back to the
    // historical /logs/{id} endpoint. Only non-terminal loops with
    // no active_job_name (between-stage gap, just-dispatched) get
    // the benign 200 info_response.
    let Some(job_name) = record.active_job_name.clone() else {
        if record.state.is_terminal() {
            return Err(NautiloopError::BadRequest(format!(
                "loop {id} is {}: use `nemo logs {}` (without --tail) for historical logs",
                record.state, id,
            )));
        }
        return Ok(info_response(format!(
            "# loop {id} is between stages (state={}), no active pod right now\n",
            record.state,
        )));
    };

    let tail_lines: i64 = query
        .get("tail")
        .and_then(|s| s.parse().ok())
        .map(|n: i64| n.clamp(1, 10_000))
        .unwrap_or(500);
    let container = query
        .get("container")
        .cloned()
        .unwrap_or_else(|| "agent".to_string());
    if container != "agent" && container != "auth-sidecar" {
        return Err(NautiloopError::BadRequest(format!(
            "container must be 'agent' or 'auth-sidecar', got {container}"
        )));
    }

    let kube_client = state.kube_client.as_ref().ok_or_else(|| {
        NautiloopError::Internal("K8s client not available — pod logs disabled".to_string())
    })?;
    let namespace = &state.config.cluster.jobs_namespace;
    let pods_api: kube::Api<k8s_openapi::api::core::v1::Pod> =
        kube::Api::namespaced(kube_client.clone(), namespace);
    let lp = kube::api::ListParams::default().labels(&format!("job-name={job_name}"));
    let pod_list = pods_api.list(&lp).await.map_err(|e| {
        NautiloopError::Internal(format!("Failed to list pods for {job_name}: {e}"))
    })?;

    if pod_list.items.is_empty() {
        return Ok(info_response(format!(
            "# no pod yet for job {job_name} (pre-creation race or TTL cleanup)\n"
        )));
    }

    // Sort matching pods: Running > Pending > rest, newest first within
    // each phase. After eviction/node loss a Job can have multiple pods
    // and list order is not guaranteed, so without this sort we'd risk
    // returning stale output from an older terminated pod. The sort key
    // is (phase_rank, negated creation timestamp) so `first()` after
    // sort is the best candidate.
    let mut sorted_pods = pod_list.items.clone();
    sorted_pods.sort_by(|a, b| {
        let phase_rank = |p: &k8s_openapi::api::core::v1::Pod| -> u8 {
            match p.status.as_ref().and_then(|s| s.phase.as_deref()) {
                Some("Running") => 0,
                Some("Pending") => 1,
                _ => 2,
            }
        };
        let ts = |p: &k8s_openapi::api::core::v1::Pod| -> i64 {
            p.metadata
                .creation_timestamp
                .as_ref()
                .map(|t| t.0.timestamp())
                .unwrap_or(0)
        };
        phase_rank(a)
            .cmp(&phase_rank(b))
            .then_with(|| ts(b).cmp(&ts(a)))
    });

    let log_params = kube::api::LogParams {
        container: Some(container),
        tail_lines: Some(tail_lines),
        ..Default::default()
    };
    let mut logs: Option<String> = None;
    for pod in &sorted_pods {
        if let Some(pod_name) = &pod.metadata.name {
            match pods_api.logs(pod_name, &log_params).await {
                Ok(l) => {
                    logs = Some(l);
                    break;
                }
                Err(e) => {
                    tracing::debug!(
                        pod = %pod_name,
                        error = %e,
                        "Failed to read logs from pod (trying next)"
                    );
                }
            }
        }
    }
    let logs = match logs {
        Some(l) => l,
        None => {
            // All pods' logs() calls failed. This is normal right
            // after dispatch when the container is still creating.
            // Check if any pod is Pending — if so, return the
            // benign 200 hint instead of a 500.
            let any_pending = pod_list.items.iter().any(|p| {
                p.status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    .is_none_or(|ph| ph == "Pending")
            });
            if any_pending {
                return Ok(info_response(format!(
                    "# pod for job {job_name} is still initializing (container creating)\n"
                )));
            }
            return Err(NautiloopError::Internal(format!(
                "All {} matching pods failed to return logs for job {job_name}",
                pod_list.items.len()
            )));
        }
    };

    Ok((
        axum::http::StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        logs,
    )
        .into_response())
}

/// DELETE /cancel/:id - Cancel a running loop.
pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<CancelResponse>, NautiloopError> {
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

    // Set cancel flag; the loop engine handles the actual cancellation
    state
        .store
        .set_loop_flag(id, LoopFlag::Cancel, true)
        .await?;

    // Return current state + cancel_requested flag (not CANCELLED, which hasn't happened yet)
    Ok(Json(CancelResponse {
        loop_id: id,
        state: record.state,
        cancel_requested: true,
    }))
}

/// POST /approve/:id - Approve a loop in AWAITING_APPROVAL.
pub async fn approve(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<ApproveResponse>, NautiloopError> {
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

    Ok(Json(ApproveResponse {
        loop_id: id,
        state: LoopState::AwaitingApproval,
        approve_requested: true,
    }))
}

/// POST /resume/:id - Resume a PAUSED, AWAITING_REAUTH, or FAILED loop (#96).
///
/// FAILED loops can only be resumed if `failed_from_state` is set, which
/// happens for transient failures (job retry exhaustion, malformed verdict).
/// Max-rounds-exhausted and other logical failures leave `failed_from_state`
/// NULL and are rejected — resuming them would just rerun the same condition.
pub async fn resume(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<ResumeResponse>, NautiloopError> {
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

    // #96: For FAILED loops, verify no replacement loop has taken
    // over the same deterministic branch. Checking any loop (not just
    // active) with a newer updated_at: a replacement that has since
    // converged, shipped, or itself failed still mutated the worktree
    // after this loop's failure, so resuming the older row would run
    // against a worktree the older loop no longer understands. The
    // operator has to abandon this row and cut a fresh loop instead.
    if record.state == LoopState::Failed
        && let Some(other) = state.store.get_loop_by_branch_any(&record.branch).await?
        && other.id != record.id
        && other.updated_at > record.updated_at
    {
        return Err(NautiloopError::InvalidStateTransition {
            action: "resume".to_string(),
            state: record.state.to_string(),
            expected: format!(
                "branch {} was taken over by a newer loop {} (state {}) — the worktree no longer matches this row; start a fresh loop instead",
                record.branch, other.id, other.state
            ),
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

/// POST /extend/:id - Extend a FAILED loop's max_rounds and resume it.
///
/// Only permitted on loops in FAILED state with a `failed_from_state` set
/// (transient or max-rounds failures — logical failures with no prior stage
/// are rejected the same way resume rejects them).
///
/// Bumps `max_rounds` by `add_rounds`, clears `failure_reason`, and flags
/// the resume so the reconciler picks up where the loop left off.
pub async fn extend(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<ExtendRequest>,
) -> Result<Json<ExtendResponse>, NautiloopError> {
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
                "FAILED loop with a preserved failed_from_state (older failures without this metadata are not extendable — start a fresh loop)"
                    .to_string(),
        });
    };

    // Same safety check as resume: if another loop has taken over this branch,
    // refuse. The worktree contents no longer correspond to this row.
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

    Ok(Json(ExtendResponse {
        loop_id: id,
        prior_max_rounds: prior_max,
        new_max_rounds: new_max,
        resumed_to_state: resume_state,
    }))
}

/// GET /inspect?branch=agent/alice/slug-hash - View detailed loop state.
/// Branch passed as query param because branch names contain slashes.
pub async fn inspect(
    State(state): State<AppState>,
    Query(params): Query<InspectQuery>,
) -> Result<Json<InspectResponse>, NautiloopError> {
    let branch = &params.branch;

    // Use get_loop_by_branch_any to include terminal loops (N5)
    let record = state
        .store
        .get_loop_by_branch_any(branch)
        .await?
        .ok_or_else(|| NautiloopError::BadRequest(format!("No loop found for branch: {branch}")))?;

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
                implement_duration_secs: None,
                test_duration_secs: None,
                review_duration_secs: None,
                audit_duration_secs: None,
                revise_duration_secs: None,
            });

        match r.stage.as_str() {
            "implement" => {
                summary.implement = r.output.clone();
                summary.implement_duration_secs = r.duration_secs;
            }
            "test" => {
                summary.test = r.output.clone();
                summary.test_duration_secs = r.duration_secs;
            }
            "review" => {
                summary.review = r.output.clone();
                summary.review_duration_secs = r.duration_secs;
            }
            "audit" => {
                summary.audit = r.output.clone();
                summary.audit_duration_secs = r.duration_secs;
            }
            "revise" => {
                summary.revise = r.output.clone();
                summary.revise_duration_secs = r.duration_secs;
            }
            _ => {}
        }
    }

    // Load judge decisions for this loop (FR-6c)
    let judge_decisions = state
        .store
        .get_judge_decisions(record.id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|d| crate::types::api::JudgeDecisionSummary {
            round: d.round,
            phase: d.phase,
            trigger: d.trigger,
            decision: d.decision,
            confidence: d.confidence,
            reasoning: d.reasoning,
            hint: d.hint,
            duration_ms: d.duration_ms,
        })
        .collect();

    Ok(Json(InspectResponse {
        loop_id: record.id,
        engineer: record.engineer,
        branch: record.branch,
        state: record.state,
        rounds: round_summaries.into_values().collect(),
        judge_decisions,
    }))
}

/// GET /diff/:id - Get unified diff for a loop's branch vs origin/main.
///
/// Returns the diff text and a truncation flag. Diffs > 100KB are truncated
/// to avoid pulling large diffs into the terminal (FR-5d).
pub async fn diff(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<DiffQuery>,
) -> Result<Json<DiffResponse>, NautiloopError> {
    let record = state
        .store
        .get_loop(id)
        .await?
        .ok_or(NautiloopError::LoopNotFound { id })?;

    let default_branch = record.resolved_default_branch.as_deref().unwrap_or("main");
    let base_ref = format!("origin/{default_branch}");

    // FR-5d: truncate at 100KB
    let max_bytes: usize = 100 * 1024;

    // Per-round scoping: if round is specified, diff only that round's commits
    let (diff_branch, diff_base) = if let Some(round_num) = query.round {
        // Look up round records to find commit SHAs
        let rounds = state.store.get_rounds(id).await?;

        // Find the latest SHA produced in the requested round (from implement or revise output).
        // The `new_sha` field is set by ImplResultData and ReviseResultData in types/verdict.rs.
        let round_sha = rounds
            .iter()
            .filter(|r| r.round == round_num)
            .filter_map(|r| {
                r.output.as_ref().and_then(|o| {
                    o.get("new_sha")
                        .and_then(|v| v.as_str().map(|s| s.to_string()))
                })
            })
            .next_back();

        let round_sha = round_sha.ok_or_else(|| {
            NautiloopError::BadRequest(format!(
                "No commit SHA found for round {round_num}. The round may not have completed an implement or revise stage."
            ))
        })?;

        // Find the previous round's last SHA as the base
        let prev_sha = if round_num > 1 {
            rounds
                .iter()
                .filter(|r| r.round == round_num - 1)
                .filter_map(|r| {
                    r.output.as_ref().and_then(|o| {
                        o.get("new_sha")
                            .and_then(|v| v.as_str().map(|s| s.to_string()))
                    })
                })
                .next_back()
                .unwrap_or_else(|| base_ref.clone())
        } else {
            base_ref.clone()
        };

        (round_sha, prev_sha)
    } else {
        (record.branch.clone(), base_ref)
    };

    let diff_text = state
        .git
        .diff(&diff_branch, &diff_base, Some(max_bytes))
        .await?;

    let truncated = diff_text.contains("[truncated");

    Ok(Json(DiffResponse {
        diff: diff_text,
        truncated,
    }))
}

/// POST /credentials - Register or update engineer credentials.
///
/// Stores credential metadata in Postgres and creates/updates a K8s Secret
/// `nautiloop-creds-{engineer}` in the jobs namespace so job pods can mount it.
pub async fn upsert_credentials(
    State(state): State<AppState>,
    Json(req): Json<CredentialRequest>,
) -> Result<impl IntoResponse, NautiloopError> {
    if req.engineer.is_empty()
        || req.engineer.len() > 63
        || !req
            .engineer
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || !req
            .engineer
            .starts_with(|c: char| c.is_ascii_alphanumeric())
        || !req.engineer.ends_with(|c: char| c.is_ascii_alphanumeric())
    {
        return Err(NautiloopError::BadRequest(
            "engineer must be 1-63 chars, lowercase alphanumeric with hyphens, \
             starting and ending with alphanumeric"
                .to_string(),
        ));
    }

    // K8s Secret key = provider name (claude, anthropic, openai, ssh).
    // "claude" = session dir for implement/revise agent mount.
    // "anthropic" = API key for sidecar proxy.
    let secret_key = req.provider.as_str();

    // Process credential content based on provider type.
    // "claude" = session directory content, stored as-is (not an API key).
    // "anthropic"/"openai" = API keys, extracted from JSON if needed.
    // "ssh" = PEM key, stored as-is.
    let raw_content = req.credential_ref.trim().to_string();
    let api_key = if secret_key == "claude" || secret_key == "ssh" {
        // Store verbatim — not an API key
        raw_content
    } else if raw_content.starts_with('{') {
        // Try to extract api_key / key / apiKey from JSON
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw_content) {
            parsed
                .get("api_key")
                .or_else(|| parsed.get("key"))
                .or_else(|| parsed.get("apiKey"))
                .or_else(|| parsed.get("ANTHROPIC_API_KEY"))
                .or_else(|| parsed.get("OPENAI_API_KEY"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or(raw_content)
        } else {
            raw_content
        }
    } else {
        raw_content
    };

    // Write K8s Secret FIRST, then Postgres metadata.
    // This ensures jobs never mount stale secrets when Postgres says creds are valid.
    let kube_client = state.kube_client.as_ref().ok_or_else(|| {
        NautiloopError::Internal("K8s client not available — cannot store credentials".to_string())
    })?;
    {
        // Normalize engineer name for K8s: lowercase, replace _ with -
        let safe_engineer: String = req.engineer.to_lowercase().replace('_', "-");
        let secret_name = format!("nautiloop-creds-{safe_engineer}");
        let namespace = &state.config.cluster.jobs_namespace;
        let secrets_api: kube::Api<k8s_openapi::api::core::v1::Secret> =
            kube::Api::namespaced(kube_client.clone(), namespace);

        // Get existing secret to merge keys and preserve resourceVersion
        let (mut data, resource_version) = match secrets_api.get(&secret_name).await {
            Ok(existing) => {
                let rv = existing.metadata.resource_version.clone();
                let mut d = std::collections::BTreeMap::new();
                if let Some(existing_data) = existing.data {
                    d = existing_data;
                }
                (d, rv)
            }
            Err(_) => (std::collections::BTreeMap::new(), None),
        };
        if req.valid {
            data.insert(
                secret_key.to_string(),
                k8s_openapi::ByteString(api_key.into_bytes()),
            );
        } else {
            // Invalidated credentials: remove the key so pods can't use stale secrets
            data.remove(secret_key);
        }

        let secret = k8s_openapi::api::core::v1::Secret {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some(secret_name.clone()),
                namespace: Some(namespace.clone()),
                resource_version: resource_version.clone(),
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        };

        if resource_version.is_some() {
            // Existing secret — replace with correct resourceVersion
            secrets_api
                .replace(&secret_name, &kube::api::PostParams::default(), &secret)
                .await
                .map_err(|e| {
                    NautiloopError::Internal(format!(
                        "Failed to update K8s Secret {secret_name}: {e}"
                    ))
                })?;
        } else {
            // New secret — create
            secrets_api
                .create(&kube::api::PostParams::default(), &secret)
                .await
                .map_err(|e| {
                    NautiloopError::Internal(format!(
                        "Failed to create K8s Secret {secret_name}: {e}"
                    ))
                })?;
        }
    }

    // Postgres metadata written AFTER K8s Secret succeeds
    let cred = crate::types::EngineerCredential {
        id: Uuid::new_v4(),
        engineer: req.engineer.clone(),
        provider: req.provider.clone(),
        credential_ref: "k8s-secret".to_string(),
        valid: req.valid,
        updated_at: chrono::Utc::now(),
    };
    state.store.upsert_credential(&cred).await?;

    // Persist engineer identity fields (used for git commit attribution)
    for (provider, value) in [("_name", &req.name), ("_email", &req.email)] {
        if let Some(v) = value
            && !v.is_empty()
        {
            let identity_cred = crate::types::EngineerCredential {
                id: Uuid::new_v4(),
                engineer: req.engineer.clone(),
                provider: provider.to_string(),
                credential_ref: v.clone(),
                valid: true,
                updated_at: chrono::Utc::now(),
            };
            state.store.upsert_credential(&identity_cred).await?;
        }
    }

    Ok((StatusCode::OK, Json(serde_json::json!({"status": "ok"}))))
}

/// GET /credentials - List registered credential providers for an engineer.
pub async fn list_credentials(
    State(state): State<AppState>,
    Query(query): Query<crate::types::api::CredentialsQuery>,
) -> Result<Json<crate::types::api::CredentialsResponse>, NautiloopError> {
    if query.engineer.is_empty() {
        return Err(NautiloopError::BadRequest(
            "engineer query parameter is required".to_string(),
        ));
    }

    let creds = state.store.get_credentials(&query.engineer).await?;

    let providers = creds
        .into_iter()
        .filter(|c| !c.provider.starts_with('_')) // Skip internal _name, _email
        .map(|c| crate::types::api::ProviderInfo {
            provider: c.provider,
            valid: c.valid,
            updated_at: c.updated_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(crate::types::api::CredentialsResponse {
        engineer: query.engineer,
        providers,
    }))
}

/// Commit the local spec content onto the agent branch and push it to the remote.
///
/// Returns the new branch SHA on success. On failure, the caller is responsible for
/// cleaning up the local branch and marking the loop as failed.
async fn commit_spec_to_branch(
    state: &AppState,
    engineer: &str,
    spec_path: &str,
    spec_content: &str,
    branch: &str,
    loop_id: Uuid,
) -> Result<String, NautiloopError> {
    // Look up engineer identity from stored credentials (same as loop engine driver).
    let all_creds = state.store.get_credentials(engineer).await?;

    // Extract engineer name and email in a single pass over credentials,
    // preferring the most recently updated valid entry for each provider.
    let (mut engineer_name, mut engineer_email): (Option<String>, Option<String>) = (None, None);
    let mut best_name_ts = None;
    let mut best_email_ts = None;
    for c in &all_creds {
        if !c.valid {
            continue;
        }
        if c.provider == "_name" && best_name_ts.is_none_or(|ts| c.updated_at > ts) {
            engineer_name = Some(c.credential_ref.clone());
            best_name_ts = Some(c.updated_at);
        }
        if c.provider == "_email" && best_email_ts.is_none_or(|ts| c.updated_at > ts) {
            engineer_email = Some(c.credential_ref.clone());
            best_email_ts = Some(c.updated_at);
        }
    }
    if engineer_name.is_none() || engineer_email.is_none() {
        tracing::warn!(
            loop_id = %loop_id,
            engineer = %engineer,
            has_name = engineer_name.is_some(),
            has_email = engineer_email.is_some(),
            "Engineer credentials incomplete; falling back to synthetic identity for spec commit. \
             Run `nemo auth` to set name/email."
        );
    }
    let engineer_name = engineer_name.unwrap_or_else(|| engineer.to_string());
    let engineer_email = engineer_email.unwrap_or_else(|| format!("{engineer}@nautiloop.dev"));

    let commit_message = format!("chore(spec): add {spec_path}");

    state
        .git
        .write_file_as(
            branch,
            spec_path,
            spec_content,
            &engineer_name,
            &engineer_email,
            &commit_message,
        )
        .await?;

    // FR-2a: current_sha must reflect the spec commit, not the default-branch tip.
    let new_sha = state
        .git
        .get_branch_sha(branch)
        .await?
        .ok_or_else(|| NautiloopError::Git("Branch not found after spec commit".to_string()))?;

    // Push the branch to the remote so the spec content is durable
    // before returning 201 to the client.
    state.git.push_branch(branch).await.map_err(|e| {
        tracing::error!(
            loop_id = %loop_id,
            branch = %branch,
            error = %e,
            "Failed to push agent branch after spec commit"
        );
        e
    })?;

    Ok(new_sha)
}

/// Check if a sqlx error is a unique constraint violation.
fn is_unique_violation(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Database(db_err) => db_err.kind() == sqlx::error::ErrorKind::UniqueViolation,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AppState;
    use crate::config::NautiloopConfig;
    use crate::git::mock::MockGitOperations;
    use crate::state::StateStore;
    use crate::state::memory::MemoryStateStore;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{self, Request};
    use axum::response::Response;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn test_app() -> (Router, Arc<MemoryStateStore>, Arc<MockGitOperations>) {
        let store = Arc::new(MemoryStateStore::new());
        let git = Arc::new(MockGitOperations::new());
        let mut config = NautiloopConfig::default();
        config.ship.allowed = true;
        let state = AppState {
            store: store.clone(),
            git: git.clone(),
            config: Arc::new(config),
            kube_client: None,
            pool: None,
            stats_cache: Arc::new(tokio::sync::RwLock::new(None)),
            fleet_cache: Arc::new(tokio::sync::RwLock::new(None)),
            api_key: None,
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
            failed_from_state: None,
            failure_reason: None,
            current_sha: None,
            opencode_session_id: None,
            claude_session_id: None,
            active_job_name: Some("implement-job".to_string()),
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
            failed_from_state: None,
            failure_reason: None,
            current_sha: None,
            opencode_session_id: None,
            claude_session_id: None,
            active_job_name: Some("implement-job".to_string()),
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
        };
        store.create_loop(&record).await.unwrap();

        let response = send_request(
            app,
            Request::builder()
                .method(http::Method::POST)
                .uri(format!("/approve/{}", record.id))
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
            failed_from_state: None,
            failure_reason: None,
            current_sha: None,
            opencode_session_id: None,
            claude_session_id: None,
            active_job_name: Some("implement-job".to_string()),
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
        assert_eq!(resp.loops[0].current_stage.as_deref(), Some("implement"));
        assert_eq!(
            resp.loops[0].active_job_name.as_deref(),
            Some("implement-job")
        );
    }

    #[tokio::test]
    async fn test_ship_not_enabled() {
        let store = Arc::new(MemoryStateStore::new());
        let git = Arc::new(MockGitOperations::new());
        let config = NautiloopConfig::default(); // ship.allowed = false by default
        let state = AppState {
            store: store.clone(),
            git: git.clone(),
            config: Arc::new(config),
            kube_client: None,
            pool: None,
            stats_cache: Arc::new(tokio::sync::RwLock::new(None)),
            fleet_cache: Arc::new(tokio::sync::RwLock::new(None)),
            api_key: None,
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

    #[tokio::test]
    async fn test_start_with_local_spec_content() {
        let (app, store, _git) = test_app();
        // Do NOT add the file to mock git — it only exists locally.

        // The mock's create_branch always returns this SHA as the initial branch tip.
        // We verify that after write_file_as, the SHA changes (FR-2a).
        let default_sha = "0000000000000000000000000000000000000000";

        let body = serde_json::json!({
            "spec_path": "specs/local-only.md",
            "engineer": "alice",
            "spec_content": "# Local Spec\nDraft content here.",
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
        assert!(resp.branch.starts_with("agent/alice/local-only-"));

        // Verify the loop was stored
        let loops = store
            .get_loops_for_engineer(Some("alice"), false, true)
            .await
            .unwrap();
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].spec_path, "specs/local-only.md");

        // FR-2a: current_sha must be the SHA of the spec commit, not the default-branch tip.
        // The mock write_file_as generates a distinct SHA, so this must differ from the
        // original default branch SHA.
        let stored_sha = loops[0].current_sha.as_deref().unwrap_or("");
        assert!(
            !stored_sha.is_empty(),
            "current_sha must be set after spec commit"
        );
        assert_ne!(
            stored_sha, default_sha,
            "current_sha must differ from the default branch SHA after spec commit"
        );
    }

    #[tokio::test]
    async fn test_start_with_local_spec_no_git_read() {
        // When spec_content is provided, git.read_file should NOT be called.
        // If it were called, it would fail because the file doesn't exist in mock git.
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": "specs/doesnt-exist-on-main.md",
            "engineer": "alice",
            "spec_content": "# Draft\n"
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

        // Should succeed because spec_content bypasses the git read
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_start_spec_content_too_large() {
        let (app, _store, _git) = test_app();

        // Create content > 1 MB
        let large_content = "x".repeat(1_048_577);
        let body = serde_json::json!({
            "spec_path": "specs/huge.md",
            "engineer": "alice",
            "spec_content": large_content
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

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn test_start_without_spec_content_falls_back_to_git() {
        let (app, _store, _git) = test_app();
        // No spec_content and no file in git → should get 404

        let body = serde_json::json!({
            "spec_path": "specs/missing.md",
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
    async fn test_start_local_spec_overrides_git_content() {
        let (app, _store, git) = test_app();
        git.add_file("specs/test.md", "# Old Content\n").await;

        let body = serde_json::json!({
            "spec_path": "specs/test.md",
            "engineer": "alice",
            "spec_content": "# New Local Content\n",
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

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let resp: StartResponse = serde_json::from_slice(&body_bytes).unwrap();

        // Branch should be based on local content hash, not git content
        let expected_branch =
            generate_branch_name("alice", "specs/test.md", "# New Local Content\n");
        assert_eq!(resp.branch, expected_branch);
    }

    #[tokio::test]
    async fn test_start_empty_spec_content_rejected() {
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": "specs/empty.md",
            "engineer": "alice",
            "spec_content": ""
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
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(err["error"].as_str().unwrap().contains("must not be empty"));
    }

    #[tokio::test]
    async fn test_start_path_traversal_rejected() {
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": "../../etc/passwd.md",
            "engineer": "alice",
            "spec_content": "# evil"
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
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(err["error"].as_str().unwrap().contains(".."));
    }

    #[tokio::test]
    async fn test_start_absolute_path_rejected() {
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": "/etc/cron.d/pwn.md",
            "engineer": "alice",
            "spec_content": "# evil"
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

    #[tokio::test]
    async fn test_start_non_md_path_rejected() {
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": "specs/test.txt",
            "engineer": "alice",
            "spec_content": "# not markdown extension"
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
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(err["error"].as_str().unwrap().contains(".md"));
    }

    #[tokio::test]
    async fn test_start_double_dots_in_filename_allowed() {
        // "v2..final.md" contains ".." but not as a path component — should be allowed.
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": "specs/v2..final.md",
            "engineer": "alice",
            "spec_content": "# Legit spec with dots in name"
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
    }

    #[tokio::test]
    async fn test_start_bare_md_extension_rejected() {
        // ".md" with no stem is not a meaningful spec path.
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": ".md",
            "engineer": "alice",
            "spec_content": "# No stem"
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
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(err["error"].as_str().unwrap().contains("non-empty"));
    }

    #[tokio::test]
    async fn test_start_backslash_path_rejected() {
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": "specs\\..\\..\\evil.md",
            "engineer": "alice",
            "spec_content": "# evil"
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
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(err["error"].as_str().unwrap().contains("backslash"));
    }

    #[tokio::test]
    async fn test_start_dot_path_segment_rejected() {
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": "specs/./test.md",
            "engineer": "alice",
            "spec_content": "# dot segment"
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
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            err["error"]
                .as_str()
                .unwrap()
                .contains("empty path segments")
        );
    }

    #[tokio::test]
    async fn test_start_empty_path_segment_rejected() {
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": "specs//test.md",
            "engineer": "alice",
            "spec_content": "# empty segment"
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
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            err["error"]
                .as_str()
                .unwrap()
                .contains("empty path segments")
        );
    }

    #[tokio::test]
    async fn test_start_whitespace_only_spec_path_rejected() {
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": "   ",
            "engineer": "alice",
            "spec_content": "# Some spec"
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
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            err["error"]
                .as_str()
                .unwrap()
                .contains("empty or whitespace")
        );
    }

    #[tokio::test]
    async fn test_start_whitespace_only_spec_path_rejected_legacy() {
        // Legacy callers (no spec_content) should also be rejected for whitespace-only paths.
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": "   ",
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

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            err["error"]
                .as_str()
                .unwrap()
                .contains("empty or whitespace")
        );
    }

    #[tokio::test]
    async fn test_start_hidden_file_stem_rejected() {
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": "specs/.hidden.md",
            "engineer": "alice",
            "spec_content": "# hidden file"
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
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(err["error"].as_str().unwrap().contains("hidden file"));
    }

    #[tokio::test]
    async fn test_start_double_md_extension_rejected() {
        // "test.md.md" should be rejected — the stem ends in ".md".
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": "specs/test.md.md",
            "engineer": "alice",
            "spec_content": "# double extension"
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
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(err["error"].as_str().unwrap().contains("double"));
    }

    #[tokio::test]
    async fn test_start_local_spec_records_engineer_identity() {
        // FR-3d: The spec commit must use the engineer's identity.
        let (app, store, git) = test_app();

        // Register engineer identity credentials
        let name_cred = crate::types::EngineerCredential {
            id: Uuid::new_v4(),
            engineer: "alice".to_string(),
            provider: "_name".to_string(),
            credential_ref: "Alice Smith".to_string(),
            valid: true,
            updated_at: chrono::Utc::now(),
        };
        let email_cred = crate::types::EngineerCredential {
            id: Uuid::new_v4(),
            engineer: "alice".to_string(),
            provider: "_email".to_string(),
            credential_ref: "alice@example.com".to_string(),
            valid: true,
            updated_at: chrono::Utc::now(),
        };
        store.upsert_credential(&name_cred).await.unwrap();
        store.upsert_credential(&email_cred).await.unwrap();

        let body = serde_json::json!({
            "spec_path": "specs/identity-test.md",
            "engineer": "alice",
            "spec_content": "# Identity Test Spec",
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

        // Verify the mock recorded the correct author identity
        let calls = git.get_write_file_as_calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].path, "specs/identity-test.md");
        assert_eq!(calls[0].content, "# Identity Test Spec");
        assert_eq!(calls[0].author_name, "Alice Smith");
        assert_eq!(calls[0].author_email, "alice@example.com");
        assert_eq!(
            calls[0].commit_message,
            "chore(spec): add specs/identity-test.md"
        );
    }

    #[tokio::test]
    async fn test_start_dir_bare_md_extension_rejected() {
        // "specs/.md" — stem is empty after the last slash.
        let (app, _store, _git) = test_app();

        let body = serde_json::json!({
            "spec_path": "specs/.md",
            "engineer": "alice",
            "spec_content": "# No stem"
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

    #[tokio::test]
    async fn test_start_legacy_non_md_path_not_rejected() {
        // NFR-1: Legacy callers (no spec_content) should not be subject to new path
        // validation. A non-.md path that exists on the default branch should still work.
        let (app, _store, git) = test_app();
        git.add_file("specs/README.txt", "# Readme\n").await;

        let body = serde_json::json!({
            "spec_path": "specs/README.txt",
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

        // Legacy path: no spec_content → validation skipped → succeeds if file exists in git
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_start_legacy_path_traversal_rejected() {
        // Path traversal checks now apply to ALL requests, not just local uploads.
        let (app, _store, git) = test_app();
        git.add_file("../../etc/passwd.md", "# evil\n").await;

        let body = serde_json::json!({
            "spec_path": "../../etc/passwd.md",
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

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_start_legacy_absolute_path_rejected() {
        // Absolute path checks now apply to ALL requests, not just local uploads.
        let (app, _store, git) = test_app();
        git.add_file("/etc/passwd.md", "# evil\n").await;

        let body = serde_json::json!({
            "spec_path": "/etc/passwd.md",
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

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_start_local_spec_fallback_identity() {
        // When no _name/_email credentials exist, the handler should still succeed
        // using synthetic fallback identity (engineer name + @nautiloop.dev email).
        let (app, _store, git) = test_app();

        let body = serde_json::json!({
            "spec_path": "specs/fallback-id.md",
            "engineer": "bob",
            "spec_content": "# Fallback identity test"
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

        // Verify fallback identity was used
        let calls = git.get_write_file_as_calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].author_name, "bob");
        assert_eq!(calls[0].author_email, "bob@nautiloop.dev");
    }

    // --- FR-4b: inspect endpoint includes judge_decisions ---

    #[tokio::test]
    async fn test_inspect_includes_judge_decisions() {
        use crate::types::JudgeDecisionRecord;

        let (app, store, _git) = test_app();

        let record = LoopRecord {
            id: Uuid::new_v4(),
            engineer: "alice".to_string(),
            spec_path: "specs/test.md".to_string(),
            spec_content_hash: "abc12345".to_string(),
            branch: "agent/alice/test-abc12345".to_string(),
            kind: LoopKind::Implement,
            state: LoopState::Converged,
            sub_state: None,
            round: 3,
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
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let loop_id = record.id;
        store.create_loop(&record).await.unwrap();

        // Add a judge decision
        let decision = JudgeDecisionRecord {
            id: Uuid::new_v4(),
            loop_id,
            round: 2,
            phase: "review".to_string(),
            trigger: "not_clean".to_string(),
            input_json: serde_json::json!({}),
            decision: "continue".to_string(),
            confidence: Some(0.85),
            reasoning: Some("Issues being fixed".to_string()),
            hint: Some("Focus on the null check".to_string()),
            duration_ms: 1200,
            created_at: chrono::Utc::now(),
            loop_final_state: None,
            loop_terminated_at: None,
        };
        store.create_judge_decision(&decision).await.unwrap();

        let response = send_request(
            app,
            Request::builder()
                .method(http::Method::GET)
                .uri("/inspect?branch=agent/alice/test-abc12345")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let resp: InspectResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(resp.loop_id, loop_id);
        assert_eq!(resp.judge_decisions.len(), 1);
        assert_eq!(resp.judge_decisions[0].decision, "continue");
        assert_eq!(resp.judge_decisions[0].phase, "review");
        assert_eq!(resp.judge_decisions[0].trigger, "not_clean");
        assert_eq!(resp.judge_decisions[0].confidence, Some(0.85));
        assert_eq!(
            resp.judge_decisions[0].reasoning,
            Some("Issues being fixed".to_string())
        );
        assert_eq!(
            resp.judge_decisions[0].hint,
            Some("Focus on the null check".to_string())
        );
    }
}
