pub mod auth;
pub mod handlers;
pub mod render;

use axum::Router;
use axum::middleware;
use axum::routing::{delete, get, post};

use super::AppState;

/// Build the dashboard router. All routes are under `/dashboard/*`.
/// Auth middleware redirects unauthenticated requests to `/dashboard/login`.
///
/// Accepts the API key to inject into the auth middleware via Extension.
/// When `api_key` is `None`, the middleware falls back to the `NAUTILOOP_API_KEY` env var.
pub fn build_dashboard_router_with_key(api_key: Option<String>) -> Router<AppState> {
    // Routes that need auth
    let mut authed = Router::new()
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
        // Dashboard-namespaced action proxies (cookie-authed, FR-4b)
        .route("/dashboard/api/approve/{id}", post(handlers::proxy_approve))
        .route("/dashboard/api/cancel/{id}", delete(handlers::proxy_cancel))
        .route("/dashboard/api/resume/{id}", post(handlers::proxy_resume))
        .route("/dashboard/api/extend/{id}", post(handlers::proxy_extend))
        .route(
            "/dashboard/api/pod-introspect/{id}",
            get(handlers::proxy_pod_introspect),
        )
        .layer(middleware::from_fn(auth::dashboard_auth_middleware));

    if let Some(key) = api_key {
        authed = authed.layer(axum::Extension(auth::DashboardApiKey(key)));
    }

    // Public routes (no auth required)
    let public = Router::new()
        .route(
            "/dashboard/login",
            get(handlers::login_page).post(handlers::login_submit),
        )
        .route("/dashboard/static/dashboard.css", get(handlers::static_css))
        .route("/dashboard/static/dashboard.js", get(handlers::static_js));

    Router::new().merge(authed).merge(public)
}

/// Build the dashboard router (legacy convenience, reads key from env).
pub fn build_dashboard_router() -> Router<AppState> {
    let api_key = std::env::var("NAUTILOOP_API_KEY").ok();
    build_dashboard_router_with_key(api_key)
}
