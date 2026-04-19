pub mod aggregate;
pub mod auth;
pub mod handlers;
pub mod templates;

use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::{get, post};

use super::AppState;
use handlers::DashboardState;

/// Build the dashboard sub-router.
/// Public (unauthenticated) routes: /login, /static/*
/// All other /dashboard/* routes require cookie or Bearer auth.
pub fn build_dashboard_router(app_state: AppState) -> Router<AppState> {
    let dash_state = DashboardState {
        app: app_state,
        fleet_cache: Arc::new(aggregate::FleetSummaryCache::new()),
        stats_cache: Arc::new(aggregate::StatsCache::new()),
    };

    // Public routes (no auth)
    let public: Router<AppState> = Router::new()
        .route("/dashboard/login", get(handlers::login_page).post(handlers::login_submit))
        .route("/dashboard/logout", post(handlers::logout))
        .route("/dashboard/static/dashboard.css", get(handlers::static_css))
        .route("/dashboard/static/dashboard.js", get(handlers::static_js));

    // Authed routes — HTML pages and JSON endpoints.
    //
    // Type conversion chain:
    //   Router<DashboardState>  (handlers extract State<DashboardState>)
    //     → .with_state(dash_state) → Router<()>
    //     → merged into public Router<AppState> which becomes Router<()> when
    //       the parent build_router() calls .with_state(app_state).
    //
    // Note: public routes above are Router<AppState> but their handlers don't
    // extract State, so they work after the parent .with_state() call. If a
    // future public handler needs AppState, it must be given its own
    // .with_state(app_state.clone()) to avoid confusing type errors.
    let authed = Router::new()
        .route("/dashboard", get(handlers::dashboard_page))
        .route("/dashboard/state", get(handlers::dashboard_state))
        .route("/dashboard/loops/{id}", get(handlers::loop_detail_page))
        .route("/dashboard/stream/{id}", get(handlers::dashboard_stream))
        .route("/dashboard/feed", get(handlers::feed_page))
        .route("/dashboard/specs", get(handlers::specs_page))
        .route("/dashboard/stats", get(handlers::stats_page))
        .layer(middleware::from_fn(auth::dashboard_auth_middleware))
        .with_state(dash_state);

    public.merge(authed)
}
