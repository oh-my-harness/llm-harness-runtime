pub mod config;
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
        .route("/api/events", axum::routing::get(events::ws_handler))
        .with_state(state)
}
