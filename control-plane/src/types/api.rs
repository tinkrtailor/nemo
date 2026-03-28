use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{LoopState, SubState};

/// POST /start request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartRequest {
    pub spec_path: String,
    pub engineer: String,
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
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// GET /status query parameters.
#[derive(Debug, Clone, Deserialize)]
pub struct StatusQuery {
    pub engineer: Option<String>,
    #[serde(default)]
    pub team: Option<bool>,
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
    pub reason: String,
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

/// GET /inspect/:user/:branch response body.
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

/// Generic error response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}
