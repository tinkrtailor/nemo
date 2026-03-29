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

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl NemoError {
    /// Whether this error is fatal (non-retryable) and should transition the loop to FAILED.
    /// Transient errors (DB timeout, K8s API blip) are retryable.
    pub fn is_fatal(&self) -> bool {
        match self {
            // Git errors from missing binaries, corrupt repos, or invalid state
            Self::Git(msg) => {
                msg.contains("not found")
                    || msg.contains("not a git repository")
                    || msg.contains("corrupt")
                    || msg.contains("No such file or directory")
                    || msg.contains("has an open PR")
            }
            // Config/serialization errors won't self-heal
            Self::Config(_) | Self::Serialization(_) => true,
            // Internal errors may be transient (e.g., pod log retrieval hiccup)
            Self::Internal(msg) => {
                !msg.contains("Failed to retrieve logs")
                    && !msg.contains("pod")
                    && !msg.contains("log")
            }
            // Spec not found, ship not enabled — logic errors, won't change on retry
            Self::SpecNotFound { .. } | Self::ShipNotEnabled => true,
            // Loop not found — data integrity issue
            Self::LoopNotFound { .. } => true,
            // DB and K8s errors are typically transient
            Self::Database(_) | Self::Kube(_) | Self::ClusterUnavailable => false,
            // Everything else: assume retryable
            _ => false,
        }
    }

    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::SpecNotFound { .. } | Self::LoopNotFound { .. } => StatusCode::NOT_FOUND,
            Self::ActiveLoopConflict { .. } | Self::InvalidStateTransition { .. } => {
                StatusCode::CONFLICT
            }
            Self::AuthenticationFailed | Self::UnknownEngineer => StatusCode::UNAUTHORIZED,
            Self::ShipNotEnabled | Self::BadRequest(_) => StatusCode::BAD_REQUEST,
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
