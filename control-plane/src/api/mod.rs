pub mod auth;
pub mod handlers;
pub mod sse;

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::middleware;
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
/// Returns 200 if the store is reachable, 503 otherwise.
/// K8s liveness/readiness probes use this to detect a dead control plane.
async fn health(State(state): State<AppState>) -> axum::http::StatusCode {
    match state.store.health_check().await {
        Ok(()) => axum::http::StatusCode::OK,
        Err(_) => axum::http::StatusCode::SERVICE_UNAVAILABLE,
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
        .route("/cancel/{id}", delete(handlers::cancel))
        .route("/approve/{id}", post(handlers::approve))
        .route("/resume/{id}", post(handlers::resume))
        .route("/extend/{id}", post(handlers::extend))
        .route("/inspect", get(handlers::inspect))
        .route("/credentials", get(handlers::list_credentials))
        .route("/credentials", post(handlers::upsert_credentials))
}
