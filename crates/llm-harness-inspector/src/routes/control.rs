//! Control routes: inject, abort, prompt, usage, skills — per agent.
//!
//! Every handler accepts an optional `?agent=<key>` query parameter that
//! selects the target harness from the registry. Missing key = the default
//! agent, so existing single-agent callers are unaffected.

use crate::state::InspectorState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use std::sync::Arc;

type Resolved = Result<Arc<llm_harness_agent::AgentHarness>, (StatusCode, Json<serde_json::Value>)>;

/// Resolve the target harness from the `?agent=` query (None = default).
pub(crate) fn resolve_agent(
    state: &InspectorState,
    query: &std::collections::HashMap<String, String>,
) -> Resolved {
    state
        .resolve(query.get("agent").map(String::as_str))
        .map_err(|e| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "ok": false, "error": e })),
            )
        })
}

// ── POST /api/inject ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct InjectBody {
    pub text: String,
}

/// Inject a message into the target agent via `harness.steer()`.
///
/// The message is enqueued for the current turn if one is in progress,
/// or for the next `prompt()` call if the harness is idle.
pub async fn inject(
    State(state): State<InspectorState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<InjectBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let harness = resolve_agent(&state, &query)?;
    harness.steer(&body.text);
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── POST /api/abort ──────────────────────────────────────────────────────────

/// Abort the target agent's current turn. Non-blocking; the agent loop will
/// stop after the current tool/LLM call completes its cancellation path.
pub async fn abort(
    State(state): State<InspectorState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let harness = resolve_agent(&state, &query)?;
    harness.abort();
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── POST /api/prompt ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PromptBody {
    pub text: String,
}

pub async fn prompt(
    State(state): State<InspectorState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<PromptBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    let harness = resolve_agent(&state, &query)?;

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

/// Return the current token/cost usage snapshot for the target agent.
pub async fn get_usage(
    State(state): State<InspectorState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let harness = resolve_agent(&state, &query)?;
    let usage = harness.usage();
    Ok(Json(
        serde_json::to_value(&usage).unwrap_or(serde_json::json!({})),
    ))
}

// ── GET /api/agent/skills ────────────────────────────────────────────────────

/// Return the list of registered skills for the target agent.
pub async fn get_skills(
    State(state): State<InspectorState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let harness = resolve_agent(&state, &query)?;
    let skills = harness.skills();
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
    Ok(Json(serde_json::json!({ "skills": skills_json })))
}
