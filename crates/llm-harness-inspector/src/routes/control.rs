//! Control routes: inject, abort, prompt, usage, skills.
//!
//! These endpoints let the Inspector Web UI interact with the running agent —
//! inject messages, abort the current turn, start a new task, and inspect
//! usage/skills.

use crate::state::InspectorState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use serde::Deserialize;

// ── POST /api/inject ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct InjectBody {
    pub text: String,
}

/// Inject a message into the running agent via `harness.steer()`.
///
/// The message is enqueued for the current turn if one is in progress,
/// or for the next `prompt()` call if the harness is idle.
pub async fn inject(
    State(state): State<InspectorState>,
    Json(body): Json<InjectBody>,
) -> impl IntoResponse {
    state.harness.steer(&body.text);
    Json(serde_json::json!({ "ok": true }))
}

// ── POST /api/abort ──────────────────────────────────────────────────────────

/// Abort the current turn. Non-blocking; the agent loop will stop after
/// the current tool/LLM call completes its cancellation path.
pub async fn abort(State(state): State<InspectorState>) -> impl IntoResponse {
    state.harness.abort();
    Json(serde_json::json!({ "ok": true }))
}

// ── POST /api/prompt ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PromptBody {
    pub text: String,
}

pub async fn prompt(
    State(state): State<InspectorState>,
    Json(body): Json<PromptBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), Json<serde_json::Value>> {
    let harness = state.harness.clone();

    // Check phase — only allow prompting when idle.
    let current_state = harness.state();
    if current_state.phase != llm_harness_types::HarnessPhase::Idle {
        return Ok((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "ok": false,
                "error": "harness is not idle",
                "phase": format!("{:?}", current_state.phase),
            })),
        ));
    }

    // Spawn the prompt in the background so the HTTP call returns immediately.
    tokio::spawn(async move {
        if let Err(e) = harness.prompt(&body.text).await {
            tracing::error!("inspector: prompt failed: {e}");
        }
    });

    Ok((StatusCode::OK, Json(serde_json::json!({ "ok": true }))))
}
/// Return the current token/cost usage snapshot.
pub async fn get_usage(State(state): State<InspectorState>) -> impl IntoResponse {
    let usage = state.harness.usage();
    Json(serde_json::to_value(&usage).unwrap_or(serde_json::json!({})))
}

// ── GET /api/agent/skills ────────────────────────────────────────────────────

/// Return the list of registered skills.
pub async fn get_skills(State(state): State<InspectorState>) -> impl IntoResponse {
    let skills = state.harness.skills();
    let skills_json: Vec<serde_json::Value> = skills
        .iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "label": s.label,
                "description": s.description,
                "source": s.source.to_string_lossy(),
                "disable_model_invocation": s.disable_model_invocation,
            })
        })
        .collect();
    Json(serde_json::json!({ "skills": skills_json }))
}
