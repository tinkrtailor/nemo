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
    /// Optional per-loop override for the per-stage K8s Job
    /// `activeDeadlineSeconds` budget. CLI `--stage-timeout=<secs>`.
    /// Applies uniformly to every stage (audit/revise/implement/test/review).
    /// Floored to 300s server-side to avoid nonsense values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_timeout_secs: Option<u32>,
    /// Optional per-stage overrides read from the repo-level `nemo.toml`
    /// `[timeouts]` block. Any stage left `None` falls through to the
    /// uniform `stage_timeout_secs` (if set) or the cluster default.
    /// Per-stage wins over uniform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<StageTimeouts>,
    /// Optional `[cache.env]` overrides from the repo-level `nemo.toml`.
    /// Shape: `{"BUN_INSTALL_CACHE_DIR": "/cache/bun", ...}`. Merged
    /// with the cluster-default cache env when the driver builds each
    /// stage Job; per-loop keys win on collisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_env: Option<std::collections::HashMap<String, String>>,
}

/// Per-stage `activeDeadlineSeconds` overrides mirroring the
/// `[timeouts]` block in `nemo.toml`. All fields optional so operators
/// can pin just the stage(s) they care about; the rest fall through.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StageTimeouts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implement_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revise_secs: Option<u32>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec_pr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_from_state: Option<LoopState>,
    pub kind: String,
    pub max_rounds: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_implementor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_reviewer: Option<String>,
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

/// POST /resume/:id request body. Optional — an empty body is valid
/// and preserves prior behaviour. Present mostly so operators can raise
/// the per-stage timeout before resuming a loop that hit
/// `StageDeadlineExceeded`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResumeRequest {
    /// Optional per-loop override for per-stage Job `activeDeadlineSeconds`.
    /// Applies from the next redispatch onward. Floored to 300s server-side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_timeout_secs: Option<u32>,
    /// Optional per-stage overrides (mirrors `[timeouts]` in nemo.toml).
    /// Each field overrides the corresponding stage's deadline for
    /// future redispatches. Fields left `None` preserve whatever was
    /// already pinned on the loop row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<StageTimeouts>,
    /// Optional `[cache.env]` override replacement. When present,
    /// replaces the loop's existing cache_env_overrides wholesale
    /// (not merged). Pass an empty object to clear. Absent field
    /// leaves the stored overrides unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_env: Option<std::collections::HashMap<String, String>>,
}

/// POST /resume/:id response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeResponse {
    pub loop_id: Uuid,
    pub state: LoopState,
    pub resume_requested: bool,
    /// Echo of the per-loop stage timeout after any update made by the
    /// resume call. Useful so the CLI can confirm the override took.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage_timeout_secs: Option<u32>,
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
    /// Judge decisions made during this loop (FR-6c).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub judge_decisions: Vec<JudgeDecisionSummary>,
}

/// Summary of a judge decision for the inspect response (FR-6c).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeDecisionSummary {
    pub round: i32,
    pub phase: String,
    pub trigger: String,
    pub decision: String,
    pub confidence: Option<f32>,
    pub reasoning: Option<String>,
    pub hint: Option<String>,
    pub duration_ms: i32,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implement_duration_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_duration_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_duration_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_duration_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revise_duration_secs: Option<i64>,
}

/// GET /diff/:loop_id response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffResponse {
    pub diff: String,
    pub truncated: bool,
}

/// GET /diff/:loop_id query parameters.
#[derive(Debug, Clone, Deserialize)]
pub struct DiffQuery {
    pub round: Option<i32>,
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

/// GET /cache response body (FR-6b).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheResponse {
    /// Whether cache is disabled.
    pub disabled: bool,
    /// Resolved cache env vars (from config, or sccache defaults if absent).
    pub env: std::collections::HashMap<String, String>,
    /// PVC name (e.g. "nautiloop-cache").
    pub volume_name: String,
    /// PVC provisioned capacity in GiB, read from PVC status.capacity.
    /// None if kube client is unavailable or PVC not found.
    pub volume_capacity_gi: Option<u64>,
    /// Disk usage summary per subdirectory, if available.
    /// None when no running pod is available for inspection.
    pub disk_usage: Option<CacheDiskUsage>,
}

/// Disk usage information from a running agent pod.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheDiskUsage {
    /// Per-subdirectory sizes (e.g. "/cache/sccache" -> "1.8G").
    pub subdirectories: std::collections::HashMap<String, String>,
    /// Total cache directory size (e.g. "2.1G").
    pub total: String,
}

/// Generic error response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}
