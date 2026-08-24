#![cfg(feature = "inspect")]

use llm_harness_agent::{AgentHarness, AgentHarnessOptions};
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse, NoOpEnv};
use std::sync::Arc;

async fn make_test_harness() -> Arc<AgentHarness> {
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
    let env: Arc<dyn llm_harness_types::ExecutionEnv> = Arc::new(NoOpEnv);
    let opts = AgentHarnessOptions::new("test-model");
    Arc::new(AgentHarness::new_in_memory(client, env, opts).await)
}

#[tokio::test]
async fn config_endpoint_returns_model() {
    let harness = make_test_harness().await;
    let state = llm_harness_inspector::state::InspectorState { harness };
    let json = llm_harness_inspector::routes::config::get_config(axum::extract::State(state))
        .await
        .0;
    assert_eq!(json["model"], "test-model");
    assert_eq!(json["phase"], "Idle");
}

#[tokio::test]
async fn session_endpoint_returns_metadata() {
    let harness = make_test_harness().await;
    let state = llm_harness_inspector::state::InspectorState { harness };
    let json = llm_harness_inspector::routes::config::get_session(axum::extract::State(state))
        .await
        .0;
    assert!(json["metadata"]["id"].as_str().is_some());
    assert!(json["active_path"].is_array());
}

// ── WebSocket message tests ──────────────────────────────────────────────────

use chrono::Utc;
use llm_harness_agent::AgentHarnessEvent;
use llm_harness_inspector::event_ser::{SerializableAgentHarnessEvent, WsMessage};
use llm_harness_types::{AgentEvent, HarnessPhase};

#[test]
fn ws_message_from_text_delta_has_inner_type() {
    let event = AgentHarnessEvent::Agent(AgentEvent::TextDelta {
        message_id: "msg1".to_string(),
        text: "hi".to_string(),
    });
    let projected = SerializableAgentHarnessEvent::from(&event);
    let msg = WsMessage::from_event(&projected, Utc::now());
    assert_eq!(msg.r#type, "Agent");
    assert_eq!(msg.inner_type.as_deref(), Some("TextDelta"));
}

#[test]
fn ws_message_from_phase_change_has_null_inner_type() {
    let event = AgentHarnessEvent::PhaseChange {
        from: HarnessPhase::Idle,
        to: HarnessPhase::Turning,
    };
    let projected = SerializableAgentHarnessEvent::from(&event);
    let msg = WsMessage::from_event(&projected, Utc::now());
    assert_eq!(msg.r#type, "PhaseChange");
    assert!(msg.inner_type.is_none());
}

#[test]
fn ws_message_snapshot_has_correct_type() {
    let state = llm_harness_inspector::event_ser::SerializableHarnessState {
        phase: "Idle".to_string(),
        model: "test-model".to_string(),
        thinking_level: "Off".to_string(),
        temperature: None,
        tools: vec![],
        active_tools: None,
        final_answer_mode: "Auto".to_string(),
        system_prompt: None,
        error_message: None,
    };
    let usage = llm_harness_types::CostAggregate::default();
    let msg = WsMessage::snapshot(&state, &usage, Utc::now());
    assert_eq!(msg.r#type, "Snapshot");
    assert!(msg.inner_type.is_none());
    let json = serde_json::to_value(&msg).unwrap();
    assert!(json["data"]["state"].is_object());
    assert!(json["data"]["usage"].is_object());
}
