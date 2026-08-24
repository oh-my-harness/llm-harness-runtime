//! 11 — Spawn Sub-Agent: main agent dispatches sub-agents and collects results.
//!
//! Demonstrates:
//! - Wiring MessageBus, HarnessSubAgentSpawner, SpawnPlugin
//! - SpawnAgentTool: main agent spawns a sub-agent (async, fire-and-forget)
//! - AwaitSubagentReplyTool: main agent blocks waiting for sub-agent reply
//! - QuerySubagentTool: main agent queries sub-agent status
//! - Sub-agent results delivered back via event_sink + SpawnPlugin hooks
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 11_spawn_subagent

use std::sync::Arc;

use llm_harness_agent::Plugin;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, MULTI_TURN, make_harness_with, with_timeout,
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
    let (harness, _inspector, sandbox_tmp) = make_harness_with(entry, |b| {
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

    // 6. Wire harness weak ref into plugin (after build, since it needs &harness).
    spawn_plugin.set_harness_weak(Arc::downgrade(&harness));

    (harness, session_tmp, sandbox_tmp)
}

#[tokio::main]
async fn main() {
    println!("=== 11: Spawn Sub-Agent ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .expect("set OPENAI_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    let (harness, _session_tmp, _sandbox_tmp) = make_spawn_harness(entry).await;

    // ── Part 1: Basic spawn — main agent spawns a sub-agent ───────────────
    println!("--- Part 1: Basic spawn ---\n");

    let cap1 = EventCapture::subscribe(&harness);
    let prompt1 = "Use the spawn_agent tool to spawn a sub-agent with the prompt \
                   'What is 2+2? Answer briefly.' Then wait for the result using \
                   await_subagent_reply.";
    println!("Prompt: \"{}\"\n", prompt1);

    with_timeout(MULTI_TURN, harness.prompt(prompt1))
        .await
        .expect("prompt failed");
    cap1.settle().await;

    let tools1 = cap1.tool_calls();
    println!("Tools called: {:?}", tools1);
    println!(
        "spawn_agent was called: {}",
        tools1.iter().any(|t| t == "spawn_agent")
    );

    let text1 = cap1.texts();
    println!("\nResponse:\n{}\n", text1);

    // ── Part 2: Await sub-agent reply ─────────────────────────────────────
    println!("--- Part 2: Await sub-agent reply ---\n");

    let cap2 = EventCapture::subscribe(&harness);
    let prompt2 = "Spawn a sub-agent with the prompt 'Say hello in one word.' \
                   Then use await_subagent_reply to get the response. \
                   Report what the sub-agent said.";
    println!("Prompt: \"{}\"\n", prompt2);

    with_timeout(MULTI_TURN, harness.prompt(prompt2))
        .await
        .expect("prompt failed");
    cap2.settle().await;

    let tools2 = cap2.tool_calls();
    println!("Tools called: {:?}", tools2);
    println!(
        "await_subagent_reply was called: {}",
        tools2.iter().any(|t| t == "await_subagent_reply")
    );

    let text2 = cap2.texts();
    println!("\nResponse:\n{}\n", text2);

    // ── Part 3: Query sub-agent status ────────────────────────────────────
    println!("--- Part 3: Query sub-agent status ---\n");

    let cap3 = EventCapture::subscribe(&harness);
    let prompt3 = "Spawn a sub-agent with the prompt 'Count from 1 to 5.' \
                   Then use query_subagent to check its status. \
                   Report the status you see.";
    println!("Prompt: \"{}\"\n", prompt3);

    with_timeout(MULTI_TURN, harness.prompt(prompt3))
        .await
        .expect("prompt failed");
    cap3.settle().await;

    let tools3 = cap3.tool_calls();
    println!("Tools called: {:?}", tools3);
    println!(
        "query_subagent was called: {}",
        tools3.iter().any(|t| t == "query_subagent")
    );

    let text3 = cap3.texts();
    println!("\nResponse:\n{}\n", text3);

    // ── Summary ───────────────────────────────────────────────────────────
    println!("--- Summary ---");
    println!(
        "spawn_agent tool exercised: {}",
        tools1.iter().any(|t| t == "spawn_agent")
            || tools2.iter().any(|t| t == "spawn_agent")
            || tools3.iter().any(|t| t == "spawn_agent")
    );
    println!(
        "await_subagent_reply tool exercised: {}",
        tools2.iter().any(|t| t == "await_subagent_reply")
    );
    println!(
        "query_subagent tool exercised: {}",
        tools3.iter().any(|t| t == "query_subagent")
    );
    println!("\nAll spawn infrastructure wired and exercised ✅");
}
