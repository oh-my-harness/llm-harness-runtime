use crate::event_ser::project_harness_state;
use crate::routes::control::resolve_agent;
use crate::state::InspectorState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;

/// `GET /api/agents` — list registered agents and the default key.
pub async fn list_agents(State(state): State<InspectorState>) -> Json<serde_json::Value> {
    let mut agents = state.registry.keys();
    if let Some(shared) = &state.shared {
        for a in shared.virtual_agents() {
            if !agents.contains(&a) {
                agents.push(a);
            }
        }
    }
    Json(serde_json::json!({
        "default": state.registry.default_key(),
        "agents": agents,
    }))
}

pub async fn get_config(
    State(state): State<InspectorState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let harness = resolve_agent(&state, &query)?;
    let harness_state = harness.state();
    let usage = harness.usage();
    let session_id = match harness.session_metadata().await {
        Ok(m) => Some(m.id),
        Err(e) => {
            tracing::warn!("inspector: session_metadata failed: {e}");
            None
        }
    };
    let projected = project_harness_state(&harness_state);
    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "model": projected.model,
        "thinking_level": projected.thinking_level,
        "temperature": projected.temperature,
        "tools": projected.tools,
        "active_tools": projected.active_tools,
        "final_answer_mode": projected.final_answer_mode,
        "system_prompt": projected.system_prompt,
        "phase": projected.phase,
        "error_message": projected.error_message,
        "budget": usage,
    })))
}

pub async fn get_session(
    State(state): State<InspectorState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let harness = resolve_agent(&state, &query)?;
    let mut errors: Vec<String> = Vec::new();

    let metadata = match harness.session_metadata().await {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::warn!("inspector: session_metadata failed: {e}");
            errors.push(format!("session_metadata: {e}"));
            None
        }
    };
    let branches = match harness.list_branches().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("inspector: list_branches failed: {e}");
            errors.push(format!("list_branches: {e}"));
            Vec::new()
        }
    };
    let active_path = match harness.read_active_path().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("inspector: read_active_path failed: {e}");
            errors.push(format!("read_active_path: {e}"));
            Vec::new()
        }
    };
    // BranchInfo has no Serialize — manually project (all fields are serializable types)
    let branches_json: Vec<serde_json::Value> = branches
        .iter()
        .map(|b| {
            serde_json::json!({
                "leaf_id": b.leaf_id,
                "label": b.label,
                "message_count": b.message_count,
                "last_activity": b.last_activity,
                "summary": b.summary,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({
        "metadata": metadata,
        "branches": branches_json,
        "active_path": active_path,
        "_errors": errors,
    })))
}
