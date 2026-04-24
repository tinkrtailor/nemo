#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct StatusResponse {
    pub loops: Vec<LoopSummary>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct InspectResponse {
    pub loop_id: uuid::Uuid,
    pub engineer: String,
    pub branch: String,
    pub state: String,
    pub rounds: Vec<RoundSummary>,
    #[serde(default)]
    pub judge_decisions: Vec<JudgeDecisionSummary>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
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

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RoundSummary {
    pub round: i32,
    pub implement: Option<serde_json::Value>,
    pub test: Option<serde_json::Value>,
    pub review: Option<serde_json::Value>,
    pub audit: Option<serde_json::Value>,
    pub revise: Option<serde_json::Value>,
    #[serde(default)]
    pub implement_duration_secs: Option<i64>,
    #[serde(default)]
    pub test_duration_secs: Option<i64>,
    #[serde(default)]
    pub review_duration_secs: Option<i64>,
    #[serde(default)]
    pub audit_duration_secs: Option<i64>,
    #[serde(default)]
    pub revise_duration_secs: Option<i64>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DiffResponse {
    pub diff: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LoopSummary {
    pub loop_id: uuid::Uuid,
    pub engineer: String,
    pub spec_path: String,
    pub branch: String,
    pub state: String,
    pub sub_state: Option<String>,
    pub round: i32,
    pub current_stage: Option<String>,
    pub active_job_name: Option<String>,
    pub spec_pr_url: Option<String>,
    pub failed_from_state: Option<String>,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub max_rounds: i32,
    #[serde(default)]
    pub model_implementor: Option<String>,
    #[serde(default)]
    pub model_reviewer: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Heartbeat from the reconciler (ISO-8601 string). `None` for
    /// loops that have never had a pod. Surfaced in `nemo status`
    /// as a relative "Xm ago" so operators can spot wedged loops
    /// without exec'ing into the cluster.
    #[serde(default)]
    pub last_activity_at: Option<String>,
}

/// GET /pod-introspect/:loop_id response.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PodIntrospectResponse {
    pub loop_id: uuid::Uuid,
    pub pod_name: String,
    pub pod_phase: String,
    pub collected_at: String,
    pub container_stats: Option<ContainerStats>,
    pub processes: Vec<ProcessInfo>,
    pub worktree: WorktreeInfo,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ContainerStats {
    pub cpu_millicores: u64,
    pub memory_bytes: u64,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub user: String,
    pub cpu_percent: f64,
    pub cmd: String,
    pub age_seconds: u64,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct WorktreeInfo {
    pub path: String,
    pub target_dir_artifacts: Option<u64>,
    pub target_dir_bytes: Option<u64>,
    pub uncommitted_files: Option<u64>,
    pub head_sha: Option<String>,
}

/// GET /cache response.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct CacheResponse {
    pub disabled: bool,
    pub env: std::collections::HashMap<String, String>,
    pub volume_name: String,
    #[serde(default)]
    pub volume_capacity_gi: Option<u64>,
    pub disk_usage: Option<CacheDiskUsage>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct CacheDiskUsage {
    pub subdirectories: std::collections::HashMap<String, String>,
    pub total: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct CredentialsResponse {
    pub engineer: String,
    pub providers: Vec<ProviderInfo>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ProviderInfo {
    pub provider: String,
    pub valid: bool,
    pub updated_at: String,
}
