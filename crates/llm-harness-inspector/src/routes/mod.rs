pub mod config;
pub mod control;
pub mod events;

use crate::state::InspectorState;
use axum::Router;

pub fn build_router(state: InspectorState) -> Router {
    Router::new()
        // Headless: no bundled UI. Root redirects to the app layer's panel
        // if one is mounted (extra_static), else 404s.
        .route(
            "/",
            axum::routing::get(async || axum::response::Redirect::temporary("/app/team.html")),
        )
        .route("/api/agents", axum::routing::get(config::list_agents))
        .route("/api/agent/config", axum::routing::get(config::get_config))
        .route(
            "/api/agent/session",
            axum::routing::get(config::get_session),
        )
        .route("/api/agent/usage", axum::routing::get(control::get_usage))
        .route("/api/agent/skills", axum::routing::get(control::get_skills))
        .route("/api/events", axum::routing::get(events::ws_handler))
        .route("/api/inject", axum::routing::post(control::inject))
        .route("/api/abort", axum::routing::post(control::abort))
        .route("/api/prompt", axum::routing::post(control::prompt))
        .with_state(state)
}
