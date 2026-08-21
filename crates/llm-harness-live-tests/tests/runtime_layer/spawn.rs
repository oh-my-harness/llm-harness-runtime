//! Tests: spawn infrastructure — MessageBus, HarnessSubAgentSpawner,
//! SpawnPlugin, and the spawn_agent / await_subagent_reply /
//! query_subagent tools.
//!
//! Spec §7: spawn_async completes; main→sub message; sub→main message;
//! abort cascade. These tests exercise the wiring and tool invocation paths
//! against a live LLM.

use std::sync::Arc;

use llm_harness_agent::Plugin;
use llm_harness_live_tests::skip_if_none;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, MULTI_TURN, assert_tool_was_called, make_harness_with, with_timeout,
};
use llm_harness_loop::convert::DefaultConvertToLlm;
use llm_harness_sandbox::OsEnvFactory;
use llm_harness_subagents::delivery::SubAgentMessageConverter;
use llm_harness_subagents::message_bus::{MAIN_AGENT_ID, MessageBus};
use llm_harness_subagents::plugin::SpawnPlugin;
use llm_harness_subagents::spawner::{HarnessSubAgentSpawner, JsonlSessionFactory};
use llm_harness_subagents::tools::{AwaitSubagentReplyTool, QuerySubagentTool, SpawnAgentTool};

// ── NoopPlugin for sub-agents (no recursive spawning) ──────────────────────

struct NoopPlugin;

impl Plugin for NoopPlugin {
    fn name(&self) -> &str {
        "noop"
    }
}

/// Build a fully wired harness with spawn infrastructure.
///
/// Returns `(harness, session_tmpdir, _sandbox_tmpdir)`. Both temp dirs must
/// be held alive for the duration of harness usage.
async fn make_spawn_harness(
    entry: &llm_harness_live_tests::LiveProviderEntry,
) -> (
    Arc<llm_harness_agent::AgentHarness>,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    // 1. Message bus — shared between main agent and sub-agents.
    let bus = MessageBus::new();

    // 2. Session dir for sub-agent JSONL sessions.
    let session_tmp = tempfile::tempdir().unwrap();

    // 3. Spawner — creates a fresh AgentHarness per sub-agent.
    let spawner = HarnessSubAgentSpawner::new(
        entry.model.clone(),
        entry.client.clone(),
        session_tmp.path().to_path_buf(),
        bus.clone(),
        |_cwd, _bus, _agent_id| Box::new(NoopPlugin),
    )
    .env_factory(Arc::new(OsEnvFactory))
    .session_factory(Arc::new(JsonlSessionFactory));

    let spawner = Arc::new(spawner);

    // 4. SpawnPlugin — drains sub-agent completion events and injects
    //    them as follow_up messages into the main agent's conversation.
    let spawn_plugin = SpawnPlugin::new(bus.clone());

    // 5. Build the main agent harness with spawn tools + plugin + converter.
    let spawner_clone = spawner.clone();
    let bus_clone = bus.clone();
    let (harness, sandbox_tmp) = make_harness_with(entry, |b| {
        b.tool(Arc::new(SpawnAgentTool::new(spawner_clone.clone())))
            .tool(Arc::new(AwaitSubagentReplyTool::new(
                bus_clone.clone(),
                MAIN_AGENT_ID,
            )))
            .tool(Arc::new(QuerySubagentTool::new(bus_clone.clone())))
            .install(spawn_plugin.as_ref())
            .convert_to_llm(Some(Arc::new(
                DefaultConvertToLlm::new()
                    .with_custom_converter(Arc::new(SubAgentMessageConverter)),
            )))
            .system_prompt(Some(
                "You are a helpful assistant that can dispatch sub-agents for sub-tasks. \
                 Use the spawn_agent tool to delegate work, await_subagent_reply to wait \
                 for results, and query_subagent to check status."
                    .to_string(),
            ))
    })
    .await;

    let harness = Arc::new(harness);

    // 6. Wire harness weak ref into plugin (after build, since it needs &harness).
    spawn_plugin.set_harness_weak(Arc::downgrade(&harness));

    (harness, session_tmp, sandbox_tmp)
}

// ── Tests ────────────────────────────────────────────────────────────────

/// spawn_async completes: main agent uses spawn_agent tool to dispatch a
/// sub-agent. The full wiring (MessageBus, spawner, SpawnPlugin)
/// must be in place for the spawn to succeed.
#[tokio::test]
async fn spawn_async_completes() {
    let provider = LiveProvider::from_env();
    skip_if_none!(provider.openai);
    let entry = provider.openai.as_ref().unwrap();

    let (harness, _session_tmp, _sandbox_tmp) = make_spawn_harness(entry).await;

    let cap = EventCapture::subscribe(&harness);
    let prompt = "Use the spawn_agent tool to spawn a sub-agent with the prompt \
                  'Say hello in one word.'";

    with_timeout(MULTI_TURN, harness.prompt(prompt))
        .await
        .expect("prompt should succeed");
    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "spawn_agent");
}

/// await_subagent_reply delivers: main agent spawns a sub-agent and then
/// blocks waiting for its reply via await_subagent_reply.
#[tokio::test]
async fn await_subagent_reply_delivers() {
    let provider = LiveProvider::from_env();
    skip_if_none!(provider.openai);
    let entry = provider.openai.as_ref().unwrap();

    let (harness, _session_tmp, _sandbox_tmp) = make_spawn_harness(entry).await;

    let cap = EventCapture::subscribe(&harness);
    let prompt = "Spawn a sub-agent with prompt 'What is 2+2? Answer with just the number.' \
                  Then use await_subagent_reply to get the response.";

    with_timeout(MULTI_TURN, harness.prompt(prompt))
        .await
        .expect("prompt should succeed");
    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "spawn_agent");
    assert_tool_was_called(&events, "await_subagent_reply");
}

/// query_subagent returns status: main agent spawns a sub-agent and queries
/// its status via query_subagent.
#[tokio::test]
async fn query_subagent_returns_status() {
    let provider = LiveProvider::from_env();
    skip_if_none!(provider.openai);
    let entry = provider.openai.as_ref().unwrap();

    let (harness, _session_tmp, _sandbox_tmp) = make_spawn_harness(entry).await;

    let cap = EventCapture::subscribe(&harness);
    let prompt = "Spawn a sub-agent with prompt 'Count from 1 to 5.' \
                  Then use query_subagent to check its status.";

    with_timeout(MULTI_TURN, harness.prompt(prompt))
        .await
        .expect("prompt should succeed");
    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "spawn_agent");
    assert_tool_was_called(&events, "query_subagent");
}
