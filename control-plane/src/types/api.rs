use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{LoopState, SubState};

/// POST /start request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartRequest {
    pub spec_path: String,
    pub engineer: String,
    /// Optional spec file content sent by the CLI (FR-1b).
    /// When present, the server uses this instead of reading from the default branch.
    #[serde(default)]
    pub spec_content: Option<String>,
    #[serde(default)]
    pub harden: bool,
    #[serde(default)]
    pub harden_only: bool,
    #[serde(default)]
    pub auto_approve: bool,
    #[serde(default)]
    pub ship_mode: bool,
    #[serde(default)]
    pub model_overrides: Option<ModelOverrides>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOverrides {
    pub implementor: Option<String>,
    pub reviewer: Option<String>,
}

/// POST /start response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartResponse {
    pub loop_id: Uuid,
    pub branch: String,
    pub state: LoopState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hardened_spec_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec_pr_url: Option<String>,
}

/// GET /status response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub loops: Vec<LoopSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopSummary {
    pub loop_id: Uuid,
    pub engineer: String,
    pub spec_path: String,
    pub branch: String,
    pub state: LoopState,
    pub sub_state: Option<SubState>,
    pub round: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_stage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_job_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// GET /status query parameters.
#[derive(Debug, Clone, Deserialize)]
pub struct StatusQuery {
    pub engineer: Option<String>,
    #[serde(default)]
    pub team: Option<bool>,
    /// Include terminal (completed/failed/shipped) loops. Default: false (active only).
    #[serde(default)]
    pub all: Option<bool>,
}

/// GET /logs/:id query parameters.
#[derive(Debug, Clone, Deserialize)]
pub struct LogsQuery {
    pub round: Option<i32>,
    pub stage: Option<String>,
}

/// SSE log event sent to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEventResponse {
    pub timestamp: DateTime<Utc>,
    pub stage: String,
    pub round: i32,
    pub line: String,
}

/// DELETE /cancel/:id response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelResponse {
    pub loop_id: Uuid,
    pub state: LoopState,
    pub cancel_requested: bool,
}

/// POST /approve/:id response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApproveResponse {
    pub loop_id: Uuid,
    pub state: LoopState,
    pub approve_requested: bool,
}

/// POST /resume/:id response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeResponse {
    pub loop_id: Uuid,
    pub state: LoopState,
    pub resume_requested: bool,
}

/// POST /extend/:id request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtendRequest {
    /// Number of rounds to add to the loop's current max_rounds.
    pub add_rounds: u32,
}

/// POST /extend/:id response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtendResponse {
    pub loop_id: Uuid,
    pub prior_max_rounds: u32,
    pub new_max_rounds: u32,
    pub resumed_to_state: LoopState,
}

/// GET /inspect?branch=... response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectResponse {
    pub loop_id: Uuid,
    pub engineer: String,
    pub branch: String,
    pub state: LoopState,
    pub rounds: Vec<RoundSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundSummary {
    pub round: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implement: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revise: Option<serde_json::Value>,
}

/// POST /credentials request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRequest {
    pub engineer: String,
    pub provider: String,
    pub credential_ref: String,
    #[serde(default = "default_valid")]
    pub valid: bool,
    /// Engineer display name for git commit attribution. Optional.
    #[serde(default)]
    pub name: Option<String>,
    /// Engineer email for git commit attribution. Optional.
    #[serde(default)]
    pub email: Option<String>,
}

fn default_valid() -> bool {
    true
}

/// GET /credentials response - List registered credential providers for an engineer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialsResponse {
    pub engineer: String,
    pub providers: Vec<ProviderInfo>,
}

/// Information about a registered credential provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub provider: String,
    pub valid: bool,
    pub updated_at: String,
}

/// Query parameters for GET /credentials
#[derive(Debug, serde::Deserialize)]
pub struct CredentialsQuery {
    pub engineer: String,
}

/// GET /pod-introspect/:loop_id response body (FR-1b).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodIntrospectResponse {
    pub loop_id: Uuid,
    pub pod_name: String,
    pub pod_phase: String,
    pub collected_at: DateTime<Utc>,
    pub container_stats: Option<ContainerStats>,
    pub processes: Vec<ProcessInfo>,
    pub worktree: WorktreeInfo,
    /// Non-empty when the snapshot is partial/degraded (e.g. exec timed out).
    /// Callers use this to distinguish "genuinely idle" from "data unavailable".
    /// Always serialized (no skip_serializing_if) so consumers without
    /// #[serde(default)] don't break on absent field.
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerStats {
    pub cpu_millicores: u64,
    pub memory_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub user: String,
    pub cpu_percent: f64,
    pub cmd: String,
    pub age_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    pub target_dir_artifacts: Option<u64>,
    pub target_dir_bytes: Option<u64>,
    pub uncommitted_files: Option<u64>,
    pub head_sha: Option<String>,
}

/// Generic error response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}
