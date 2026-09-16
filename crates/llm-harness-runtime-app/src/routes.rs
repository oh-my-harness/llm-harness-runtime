use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use llm_harness_agent_team_studio::diagnostics::read_lifecycle_events;
use llm_harness_agent_team_studio::runtime::StartupRecovery;
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;

const APP_NAME: &str = "llm-harness-runtime-app";
const DIAGNOSTICS_LIMIT: usize = 20;

#[derive(Clone)]
pub struct AppState {
    pub data_root: PathBuf,
    pub startup_recovery: Arc<RwLock<Option<StartupRecovery>>>,
}

pub fn app_router(state: AppState) -> Router {
    Router::new()
        .route("/api/app/health", get(health))
        .route("/api/app/diagnostics", get(diagnostics))
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Response {
    let startup_recovery = state.startup_recovery.read().clone();
    match startup_recovery {
        Some(recovery) => Json(serde_json::json!({
            "status": if recovery.status == llm_harness_agent_team_studio::runtime::StartupStatus::Healthy { "ok" } else { "degraded" },
            "app": APP_NAME,
            "version": env!("CARGO_PKG_VERSION"),
            "features": ["agent-team"],
            "startup_recovery": recovery,
        }))
        .into_response(),
        None => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "starting",
                "app": APP_NAME,
                "version": env!("CARGO_PKG_VERSION"),
            })),
        )
            .into_response(),
    }
}

async fn diagnostics(State(state): State<AppState>) -> Response {
    match read_lifecycle_events(&state.data_root, DIAGNOSTICS_LIMIT) {
        Ok(events) => Json(serde_json::json!({
            "app": APP_NAME,
            "version": env!("CARGO_PKG_VERSION"),
            "lifecycle_events": events,
        }))
        .into_response(),
        Err(error) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "ok": false,
                "error": error.to_string(),
            })),
        )
            .into_response(),
    }
}
