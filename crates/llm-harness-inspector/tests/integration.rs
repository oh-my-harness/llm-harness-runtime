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

fn make_state(harness: Arc<AgentHarness>) -> llm_harness_inspector::state::InspectorState {
    let registry = llm_harness_inspector::state::HarnessRegistry::new("default");
    registry.register("default", harness);
    llm_harness_inspector::state::InspectorState {
        registry,
        token: llm_harness_inspector::state::AuthToken::new("test-token".into()),
        shared: None,
    }
}

#[tokio::test]
async fn config_endpoint_returns_model() {
    let harness = make_test_harness().await;
    let state = make_state(harness);
    let json = llm_harness_inspector::routes::config::get_config(
        axum::extract::State(state),
        axum::extract::Query(Default::default()),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(json["model"], "test-model");
    assert_eq!(json["phase"], "Idle");
}

#[tokio::test]
async fn session_endpoint_returns_metadata() {
    let harness = make_test_harness().await;
    let state = make_state(harness);
    let json = llm_harness_inspector::routes::config::get_session(
        axum::extract::State(state),
        axum::extract::Query(Default::default()),
    )
    .await
    .unwrap()
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

#[test]
fn auth_token_matches_correct() {
    let token = llm_harness_inspector::state::AuthToken::new("secret123".into());
    assert!(token.matches("secret123"));
    assert!(!token.matches("wrong"));
    assert!(!token.matches(""));
}

#[test]
fn auth_token_generate_is_unique() {
    let t1 = llm_harness_inspector::state::AuthToken::generate();
    let t2 = llm_harness_inspector::state::AuthToken::generate();
    assert!(!t1.as_str().is_empty());
    assert_ne!(t1.as_str(), t2.as_str());
}

// ── Multi-agent registry tests ───────────────────────────────────────────────

#[tokio::test]
async fn agents_endpoint_lists_registered_agents() {
    let a = make_test_harness().await;
    let b = make_test_harness().await;
    let registry = llm_harness_inspector::state::HarnessRegistry::new("a");
    registry.register("a", a);
    registry.register("b", b);
    let state = llm_harness_inspector::state::InspectorState {
        registry,
        token: llm_harness_inspector::state::AuthToken::new("t".into()),
        shared: None,
    };
    let json = llm_harness_inspector::routes::config::list_agents(axum::extract::State(state))
        .await
        .0;
    assert_eq!(json["default"], "a");
    let agents = json["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 2);
    assert_eq!(agents[0], "a");
    assert_eq!(agents[1], "b");
}

#[tokio::test]
async fn usage_endpoint_resolves_agent_query() {
    let a = make_test_harness().await;
    let b = make_test_harness().await;
    let registry = llm_harness_inspector::state::HarnessRegistry::new("a");
    registry.register("a", a);
    registry.register("b", b);
    let state = llm_harness_inspector::state::InspectorState {
        registry,
        token: llm_harness_inspector::state::AuthToken::new("t".into()),
        shared: None,
    };
    // Default (no ?agent=) resolves agent "a".
    let json = llm_harness_inspector::routes::control::get_usage(
        axum::extract::State(state.clone()),
        axum::extract::Query(Default::default()),
    )
    .await
    .unwrap()
    .0;
    assert!(json.is_object());
    // Explicit ?agent=b resolves agent "b" (200 path exercises resolution).
    let mut q = std::collections::HashMap::new();
    q.insert("agent".to_string(), "b".to_string());
    let json = llm_harness_inspector::routes::control::get_usage(
        axum::extract::State(state.clone()),
        axum::extract::Query(q),
    )
    .await
    .unwrap()
    .0;
    assert!(json.is_object());
    // Unknown agent → 404.
    let mut q = std::collections::HashMap::new();
    q.insert("agent".to_string(), "nope".to_string());
    let (status, _) = llm_harness_inspector::routes::control::get_usage(
        axum::extract::State(state),
        axum::extract::Query(q),
    )
    .await
    .unwrap_err();
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

// ── Mount-level tests: extra_routes / extra_static ───────────────────────────

#[tokio::test]
async fn extra_routes_are_served_over_http() {
    let harness = make_test_harness().await;
    let extra =
        axum::Router::new().route("/api/team/ping", axum::routing::get(|| async { "pong" }));
    let inspector = llm_harness_inspector::Inspector::builder(harness)
        .extra_routes(extra)
        .port(0)
        .mount()
        .await
        .unwrap();
    let addr = inspector.bound_addr().unwrap();
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/api/team/ping"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "pong");
    inspector.shutdown().await;
}

#[tokio::test]
async fn extra_static_serves_files() {
    let dir = std::env::temp_dir().join(format!(
        "insp-static-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hello.txt"), "static-ok").unwrap();

    let harness = make_test_harness().await;
    let inspector = llm_harness_inspector::Inspector::builder(harness)
        .extra_static("app", dir)
        .port(0)
        .mount()
        .await
        .unwrap();
    let addr = inspector.bound_addr().unwrap();
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/app/hello.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "static-ok");
    inspector.shutdown().await;
}

#[tokio::test]
async fn required_auth_protects_api_but_not_static_assets() {
    let harness = make_test_harness().await;
    let inspector = llm_harness_inspector::Inspector::builder(harness)
        .require_auth()
        .port(0)
        .mount()
        .await
        .unwrap();
    let addr = inspector.bound_addr().unwrap();
    let token = inspector.auth_token().expect("auth token").to_owned();
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    let resp = client
        .get(format!("{base}/api/agents"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    let resp = client
        .get(format!("{base}/api/agents"))
        .bearer_auth("not-the-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    let resp = client
        .get(format!("{base}/api/agents"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let resp = client
        .get(format!("{base}/api/agents?token={token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let resp = client
        .get(format!("{base}/api/agents"))
        .header("origin", "http://127.0.0.1.evil")
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);

    inspector.shutdown().await;
}

#[tokio::test]
async fn auth_token_is_available_without_logging() {
    let harness = make_test_harness().await;
    let inspector = llm_harness_inspector::Inspector::builder(harness)
        .port(0)
        .mount()
        .await
        .unwrap();
    let token = inspector.auth_token().expect("event auth token");
    assert!(!token.is_empty());
    inspector.shutdown().await;
}
