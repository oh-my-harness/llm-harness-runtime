pub mod config;
pub mod control;
pub mod events;
pub mod ui;

use crate::state::InspectorState;
use axum::Router;

pub fn build_router(state: InspectorState) -> Router {
    Router::new()
        .route("/", axum::routing::get(ui::serve_ui))
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
