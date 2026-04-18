pub mod auth;
pub mod cache;
pub mod handlers;
pub mod introspect;
pub mod sse;

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};

use crate::config::NautiloopConfig;
use crate::git::GitOperations;
use crate::state::StateStore;

/// Shared application state for all API handlers.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn StateStore>,
    pub git: Arc<dyn GitOperations>,
    pub config: Arc<NautiloopConfig>,
    /// Optional kube client for creating K8s Secrets during credential registration.
    /// None in test environments.
    pub kube_client: Option<kube::Client>,
    /// Optional Postgres pool for snapshot recording (FR-6a).
    /// Separate from the trait-based StateStore so we can write directly
    /// without adding snapshot methods to the test-focused trait.
    pub pool: Option<sqlx::PgPool>,
}

/// Build the axum router with all endpoints and auth middleware.
/// The /health endpoint is outside the auth layer so K8s probes work without an API key.
pub fn build_router(state: AppState) -> Router {
    let authed = build_routes(state.clone()).layer(middleware::from_fn(auth::auth_middleware));

    Router::new()
        .route("/health", get(health))
        .merge(authed)
        .with_state(state)
}

/// Health check that verifies Postgres connectivity.
/// Returns 200 with `{"status":"ok"}` if the store is reachable,
/// 503 with `{"status":"degraded"}` otherwise.
/// K8s liveness/readiness probes use this to detect a dead control plane.
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let version = env!("CARGO_PKG_VERSION");
    match state.store.health_check().await {
        Ok(()) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({"status": "ok", "version": version})),
        ),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"status": "degraded", "version": version})),
        ),
    }
}

/// Build the axum router without auth middleware (for testing).
#[cfg(test)]
pub fn build_router_no_auth(state: AppState) -> Router {
    build_routes(state.clone()).with_state(state)
}

fn build_routes(_state: AppState) -> Router<AppState> {
    Router::new()
        .route("/start", post(handlers::start))
        .route("/status", get(handlers::status))
        .route("/logs/{id}", get(handlers::logs))
        .route("/pod-logs/{id}", get(handlers::pod_logs))
        .route("/pod-introspect/{id}", get(introspect::pod_introspect))
        .route("/cancel/{id}", delete(handlers::cancel))
        .route("/approve/{id}", post(handlers::approve))
        .route("/resume/{id}", post(handlers::resume))
        .route("/extend/{id}", post(handlers::extend))
        .route("/inspect", get(handlers::inspect))
        .route("/diff/{id}", get(handlers::diff))
        .route("/credentials", get(handlers::list_credentials))
        .route("/credentials", post(handlers::upsert_credentials))
        .route("/cache", get(cache::cache_show))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NautiloopConfig;
    use crate::git::mock::MockGitOperations;
    use crate::state::memory::MemoryStateStore;
    use crate::state::{LoopFlag, StateStore};
    use crate::types::{
        EngineerCredential, LogEvent, LoopRecord, LoopState, MergeEvent, RoundRecord, SubState,
    };
    use crate::error::NautiloopError;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    use uuid::Uuid;

    fn test_app() -> Router {
        let store = Arc::new(MemoryStateStore::new());
        let git = Arc::new(MockGitOperations::new());
        let state = AppState {
            store: store.clone(),
            git: git.clone(),
            config: Arc::new(NautiloopConfig::default()),
            kube_client: None,
            pool: None,
        };
        build_router(state)
    }

    /// A StateStore that always fails health_check, for testing the degraded path.
    #[derive(Debug)]
    struct FailingHealthStore;

    #[async_trait::async_trait]
    impl StateStore for FailingHealthStore {
        async fn health_check(&self) -> crate::error::Result<()> {
            Err(NautiloopError::Internal("database unavailable".into()))
        }
        async fn create_loop(&self, _: &LoopRecord) -> crate::error::Result<LoopRecord> {
            unimplemented!()
        }
        async fn get_loop(&self, _: Uuid) -> crate::error::Result<Option<LoopRecord>> {
            unimplemented!()
        }
        async fn get_loop_by_branch(&self, _: &str) -> crate::error::Result<Option<LoopRecord>> {
            unimplemented!()
        }
        async fn get_loop_by_branch_any(
            &self,
            _: &str,
        ) -> crate::error::Result<Option<LoopRecord>> {
            unimplemented!()
        }
        async fn get_active_loops(&self) -> crate::error::Result<Vec<LoopRecord>> {
            unimplemented!()
        }
        async fn get_loops_for_engineer(
            &self,
            _: Option<&str>,
            _: bool,
            _: bool,
        ) -> crate::error::Result<Vec<LoopRecord>> {
            unimplemented!()
        }
        async fn update_loop_state(
            &self,
            _: Uuid,
            _: LoopState,
            _: Option<SubState>,
        ) -> crate::error::Result<()> {
            unimplemented!()
        }
        async fn update_loop(&self, _: &LoopRecord) -> crate::error::Result<()> {
            unimplemented!()
        }
        async fn set_loop_flag(
            &self,
            _: Uuid,
            _: LoopFlag,
            _: bool,
        ) -> crate::error::Result<()> {
            unimplemented!()
        }
        async fn set_current_sha(&self, _: Uuid, _: &str) -> crate::error::Result<()> {
            unimplemented!()
        }
        async fn has_active_loop_for_branch(&self, _: &str) -> crate::error::Result<bool> {
            unimplemented!()
        }
        async fn create_round(&self, _: &RoundRecord) -> crate::error::Result<()> {
            unimplemented!()
        }
        async fn update_round(&self, _: &RoundRecord) -> crate::error::Result<()> {
            unimplemented!()
        }
        async fn get_rounds(&self, _: Uuid) -> crate::error::Result<Vec<RoundRecord>> {
            unimplemented!()
        }
        async fn append_log(&self, _: &LogEvent) -> crate::error::Result<()> {
            unimplemented!()
        }
        async fn get_logs(
            &self,
            _: Uuid,
            _: Option<i32>,
            _: Option<&str>,
        ) -> crate::error::Result<Vec<LogEvent>> {
            unimplemented!()
        }
        async fn get_logs_after(
            &self,
            _: Uuid,
            _: chrono::DateTime<chrono::Utc>,
        ) -> crate::error::Result<Vec<LogEvent>> {
            unimplemented!()
        }
        async fn get_credentials(
            &self,
            _: &str,
        ) -> crate::error::Result<Vec<EngineerCredential>> {
            unimplemented!()
        }
        async fn upsert_credential(&self, _: &EngineerCredential) -> crate::error::Result<()> {
            unimplemented!()
        }
        async fn are_credentials_valid(&self, _: &str, _: &str) -> crate::error::Result<bool> {
            unimplemented!()
        }
        async fn create_merge_event(&self, _: &MergeEvent) -> crate::error::Result<()> {
            unimplemented!()
        }
        async fn try_advisory_lock(&self, _: Uuid) -> crate::error::Result<bool> {
            unimplemented!()
        }
        async fn advisory_unlock(&self, _: Uuid) -> crate::error::Result<()> {
            unimplemented!()
        }
        async fn cleanup_pod_snapshots(&self, _: u32) -> crate::error::Result<u64> {
            unimplemented!()
        }
        async fn create_judge_decision(
            &self,
            _: &crate::types::JudgeDecisionRecord,
        ) -> crate::error::Result<()> {
            unimplemented!()
        }
        async fn get_judge_decisions(
            &self,
            _: Uuid,
        ) -> crate::error::Result<Vec<crate::types::JudgeDecisionRecord>> {
            unimplemented!()
        }
        async fn count_judge_decisions(&self, _: Uuid) -> crate::error::Result<u32> {
            unimplemented!()
        }
        async fn backfill_judge_decisions(
            &self,
            _: Uuid,
            _: &str,
            _: chrono::DateTime<chrono::Utc>,
        ) -> crate::error::Result<()> {
            unimplemented!()
        }
        async fn count_exit_clean_decisions(&self, _: Uuid) -> crate::error::Result<u32> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn test_health_returns_json_ok() {
        let app = test_app();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            content_type.starts_with("application/json"),
            "expected application/json, got {content_type}"
        );

        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn test_health_returns_degraded_on_db_failure() {
        let store = Arc::new(FailingHealthStore);
        let git = Arc::new(MockGitOperations::new());
        let state = AppState {
            store,
            git: git.clone(),
            config: Arc::new(NautiloopConfig::default()),
            kube_client: None,
            pool: None,
        };
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let content_type = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            content_type.starts_with("application/json"),
            "expected application/json, got {content_type}"
        );

        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "degraded");
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    }
}
