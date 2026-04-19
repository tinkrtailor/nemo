pub mod auth;
pub mod handlers;
pub mod render;

use axum::Router;
use axum::middleware;
use axum::routing::{get, post};

use super::AppState;

/// Build the dashboard router. All routes are under `/dashboard/*`.
/// Auth middleware redirects unauthenticated requests to `/dashboard/login`.
pub fn build_dashboard_router() -> Router<AppState> {
    // Routes that need auth
    let authed = Router::new()
        .route("/dashboard", get(handlers::grid_page))
        .route("/dashboard/loops/{id}", get(handlers::detail_page))
        .route("/dashboard/stream/{id}", get(handlers::stream_logs))
        .route("/dashboard/state", get(handlers::dashboard_state))
        .route("/dashboard/feed", get(handlers::feed_page))
        .route("/dashboard/feed/json", get(handlers::feed_json))
        .route("/dashboard/specs/{*path}", get(handlers::specs_page))
        .route("/dashboard/stats", get(handlers::stats_page))
        .route("/dashboard/stats/json", get(handlers::stats_json))
        .route("/dashboard/logout", post(handlers::logout))
        .layer(middleware::from_fn(auth::dashboard_auth_middleware));

    // Public routes (no auth required)
    let public = Router::new()
        .route("/dashboard/login", get(handlers::login_page).post(handlers::login_submit))
        .route("/dashboard/static/dashboard.css", get(handlers::static_css))
        .route("/dashboard/static/dashboard.js", get(handlers::static_js));

    Router::new().merge(authed).merge(public)
}
