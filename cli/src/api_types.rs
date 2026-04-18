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
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RoundSummary {
    pub round: i32,
    pub implement: Option<serde_json::Value>,
    pub test: Option<serde_json::Value>,
    pub review: Option<serde_json::Value>,
    pub audit: Option<serde_json::Value>,
    pub revise: Option<serde_json::Value>,
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
    pub created_at: String,
    pub updated_at: String,
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
