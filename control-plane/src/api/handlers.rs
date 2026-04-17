use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use uuid::Uuid;

use super::AppState;
use crate::error::NautiloopError;
use crate::state::LoopFlag;
use crate::types::api::{
    ApproveResponse, CancelResponse, CredentialRequest, InspectResponse, LogsQuery, LoopSummary,
    ResumeResponse, RoundSummary, StartRequest, StartResponse, StatusQuery, StatusResponse,
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
    Json(req): Json<StartRequest>,
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
    let spec_from_local: bool;
    let spec_content = if let Some(ref content) = req.spec_content {
        // FR-3a: spec_content must be valid UTF-8 (guaranteed by JSON deserialization).
        // FR-3b: enforce 1 MB size limit.
        if content.len() > 1_048_576 {
            return Err(NautiloopError::SpecTooLarge {
                size: content.len(),
            });
        }
        spec_from_local = true;
        content.clone()
    } else {
        // FR-2b: legacy path — read from default branch.
        spec_from_local = false;
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
    if spec_from_local {
        match state
            .git
            .write_file(&branch, &req.spec_path, &spec_content)
            .await
        {
            Ok(()) => {
                // Update branch_sha to the post-write tip so current_sha reflects the spec commit.
                if let Ok(Some(new_sha)) = state.git.get_branch_sha(&branch).await {
                    branch_sha = new_sha;
                }
            }
            Err(e) => {
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
    state.store.set_current_sha(loop_id, &branch_sha).await?;

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
            engineer: loop_record.engineer,
            spec_path: loop_record.spec_path,
            branch: loop_record.branch,
            state: loop_record.state,
            sub_state: loop_record.sub_state,
            round: loop_record.round,
            current_stage,
            active_job_name,
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
        let loops = store.get_loops_for_engineer(Some("alice"), false, true).await.unwrap();
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].spec_path, "specs/local-only.md");
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
}
