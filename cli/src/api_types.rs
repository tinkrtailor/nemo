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
    /// Judge decision for this round, if the judge was invoked (FR-6c).
    #[serde(default)]
    pub judge_decision: Option<JudgeDecisionSummary>,
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
