pub mod auth;
pub mod handlers;
pub mod sse;

use std::sync::Arc;

use axum::middleware;
use axum::routing::{delete, get, post};
use axum::Router;

use crate::config::NemoConfig;
use crate::git::GitOperations;
use crate::state::StateStore;

/// Shared application state for all API handlers.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn StateStore>,
    pub git: Arc<dyn GitOperations>,
    pub config: Arc<NemoConfig>,
}

/// Build the axum router with all endpoints and auth middleware.
pub fn build_router(state: AppState) -> Router {
    build_routes(state.clone())
        .layer(middleware::from_fn(auth::auth_middleware))
        .with_state(state)
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
        .route("/cancel/{id}", delete(handlers::cancel))
        .route("/approve/{id}", post(handlers::approve))
        .route("/resume/{id}", post(handlers::resume))
        .route("/inspect/{user}/{branch}", get(handlers::inspect))
}
