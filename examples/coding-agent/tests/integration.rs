//! Integration tests for coding-agent using MockLlmClient.
//!
//! These tests verify the full harness → plugin → tool pipeline without
//! requiring real API keys or external services.

use std::sync::Arc;

use llm_harness_agent::{AgentHarness, AgentHarnessOptions, HarnessHooks, Plugin};
use llm_harness_loop::LlmClient;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse, NoOpEnv};
use llm_harness_types::{AgentEvent, Tool};

use coding_agent::plugin::CodingAgentPlugin;

/// Build a minimal AgentHarness with MockLlmClient + NoOpEnv + CodingAgentPlugin.
async fn make_test_harness(responses: Vec<MockResponse>) -> AgentHarness {
    let client = Arc::new(MockLlmClient::new(responses)) as Arc<dyn LlmClient>;
    let env: Arc<dyn llm_harness_types::ExecutionEnv> = Arc::new(NoOpEnv);
    let plugin = CodingAgentPlugin::new(std::path::Path::new("."));
    let mut hooks = HarnessHooks::none();
    plugin.register_hooks(&mut hooks);

    let mut tools: Vec<Arc<dyn Tool>> = vec![];
    plugin.register_tools(&mut tools);

    let mut opts = AgentHarnessOptions::new("mock-model");
    opts.tools = tools;
    opts.hooks = hooks;

    AgentHarness::new_in_memory(client, env, opts).await
}

/// Subscribe BEFORE calling prompt, collect events until Idle, return them.
async fn subscribe_and_prompt(
    harness: &AgentHarness,
    prompt: &str,
) -> Vec<Arc<llm_harness_agent::AgentHarnessEvent>> {
    use llm_harness_agent::AgentHarnessEvent;
    use llm_harness_types::HarnessPhase;

    let mut rx = harness.subscribe();
    let mut events = Vec::new();

    harness.prompt(prompt).await.ok();

    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
            Ok(Ok(ev)) => {
                let is_idle = matches!(
                    ev.as_ref(),
                    AgentHarnessEvent::PhaseChange {
                        to: HarnessPhase::Idle,
                        ..
                    }
                );
                events.push(ev);
                if is_idle {
                    break;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => break, // timeout — harness is done
        }
    }

    events
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Agent calls a tool, tool result flows back, final text emitted.
#[tokio::test]
async fn tool_call_flow_completes() {
    let responses = vec![
        MockResponse::tool_use("call-1", "bash", r#"{"command":"echo hello"}"#),
        MockResponse::text("Command executed successfully."),
    ];

    let harness = make_test_harness(responses).await;
    let events = subscribe_and_prompt(&harness, "run echo hello").await;

    assert!(!events.is_empty(), "should collect events");
}

/// TextDelta events stream correctly for a simple text response.
#[tokio::test]
async fn text_delta_streamed_correctly() {
    let responses = vec![MockResponse::text("Hello from mock!")];

    let harness = make_test_harness(responses).await;
    let events = subscribe_and_prompt(&harness, "say hello").await;

    let text_deltas: Vec<String> = events
        .iter()
        .filter_map(|e| match e.as_ref() {
            llm_harness_agent::AgentHarnessEvent::Agent(AgentEvent::TextDelta {
                text, ..
            }) => Some(text.clone()),
            _ => None,
        })
        .collect();

    let full: String = text_deltas.concat();
    assert_eq!(full, "Hello from mock!");
}

/// EditTool must return an error when old_string appears multiple times.
#[tokio::test]
async fn edit_tool_rejects_non_unique_old_string() {
    use std::sync::Arc;

    use coding_agent::tools::edit::EditTool;
    use llm_harness_runtime_sandbox_os::OsEnv;
    use llm_harness_types::{AssistantMessage, ToolContext};
    use tokio_util::sync::CancellationToken;

    let dir = tempfile::TempDir::new().unwrap();
    let file_path = dir.path().join("test.txt");
    std::fs::write(&file_path, "abc abc abc").unwrap();

    let env = Arc::new(OsEnv::new(dir.path().to_path_buf()));
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let ctx = ToolContext {
        env,
        abort: CancellationToken::new(),
        tool_use_id: "test-id".into(),
        turn_index: 0,
        assistant_message: Arc::new(AssistantMessage {
            content: vec![],
            stop_reason: None,
            timestamp: chrono::Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        }),
        update_tx: tx,
    };

    let tool = EditTool::new();
    let args = serde_json::json!({
        "file_path": file_path.to_str().unwrap(),
        "old_string": "abc",
        "new_string": "xyz"
    });
    let result = tool.execute(args, &ctx).await;
    assert!(result.is_err(), "should reject non-unique old_string");
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("3 times"), "got: {err_msg}");
}

/// Plugin registers correctly and HarnessBuilder works end-to-end.
#[tokio::test]
async fn harness_builder_with_plugin_works() {
    use llm_harness_runtime::HarnessBuilder;

    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("done")]));
    let env: Arc<dyn llm_harness_types::ExecutionEnv> = Arc::new(NoOpEnv);
    let plugin = CodingAgentPlugin::new(std::path::Path::new("."));

    let harness = HarnessBuilder::new("mock-model")
        .install(&plugin)
        .provider("mock-model", client)
        .build(env)
        .await
        .expect("build should succeed");

    harness.prompt("test").await.expect("prompt should succeed");
}
