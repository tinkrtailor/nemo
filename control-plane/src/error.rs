use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Top-level error type for the Nemo control plane.
#[derive(Debug, thiserror::Error)]
pub enum NemoError {
    #[error("Spec not found: {path}")]
    SpecNotFound { path: String },

    #[error("Active loop exists for branch {branch}")]
    ActiveLoopConflict { branch: String },

    #[error("Loop not found: {id}")]
    LoopNotFound { id: uuid::Uuid },

    #[error("Cannot {action}: loop is in {state}, not {expected}")]
    InvalidStateTransition {
        action: String,
        state: String,
        expected: String,
    },

    #[error("Authentication failed")]
    AuthenticationFailed,

    #[error("Unknown engineer. Run `nemo auth` first")]
    UnknownEngineer,

    #[error("Database unavailable: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Kubernetes API error: {0}")]
    Kube(#[from] kube::Error),

    #[error("Cluster unavailable")]
    ClusterUnavailable,

    #[error("Malformed verdict after {retries} retries")]
    MalformedVerdict { retries: u32 },

    #[error("Max rounds exceeded for loop {loop_id}")]
    MaxRoundsExceeded { loop_id: uuid::Uuid },

    #[error("Git operation failed: {0}")]
    Git(String),

    #[error("nemo ship is not enabled for this repo. Set [ship] allowed = true in nemo.toml")]
    ShipNotEnabled,

    #[error("Config error: {0}")]
    Config(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl NemoError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::SpecNotFound { .. } | Self::LoopNotFound { .. } => StatusCode::NOT_FOUND,
            Self::ActiveLoopConflict { .. } | Self::InvalidStateTransition { .. } => {
                StatusCode::CONFLICT
            }
            Self::AuthenticationFailed | Self::UnknownEngineer => StatusCode::UNAUTHORIZED,
            Self::ShipNotEnabled => StatusCode::BAD_REQUEST,
            Self::Database(_) | Self::ClusterUnavailable | Self::Kube(_) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for NemoError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = serde_json::json!({
            "error": self.to_string(),
        });
        (status, axum::Json(body)).into_response()
    }
}

pub type Result<T> = std::result::Result<T, NemoError>;
