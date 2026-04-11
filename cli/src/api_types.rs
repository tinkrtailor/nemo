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
