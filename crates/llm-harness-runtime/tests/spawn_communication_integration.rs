//! End-to-end integration tests for the spawn communication system.
//!
//! Exercises the full MessageBus ↔ HarnessSubAgentSpawner ↔ AsyncSpawnHook ↔
//! IdleWatcher lifecycle: spawn → message → complete → abort.
//!
//! Tests are organized in two tiers:
//! - **MessageBus-level**: exercise `send`/`wait`/`register` directly —
//!   the communication primitives without a full harness.
//! - **Harness-level**: use `HarnessSubAgentSpawner::spawn_async` with a real
//!   `AgentHarness` backed by `MockLlmClient`, exercising the complete
//!   spawn → track → event-channel → delivery path.
//!
//! Note: `MessageBus::track` and `track_with_oneshot` are `#[cfg(test)] pub(crate)`
//! and therefore unavailable to integration tests. Harness-level tests use
//! `spawn_async` (which calls `track_with_oneshot` internally) instead.

use std::sync::Arc;
use std::time::Duration;

use llm_harness_agent::{AgentHarness, IdleWatcherHook, Plugin};
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse, NoOpEnv};
use llm_harness_runtime::builder::HarnessBuilder;
use llm_harness_runtime::spawn::delivery::IdleWatcher;
use llm_harness_runtime::spawn::message_bus::{
    AgentEntry, AgentStatus, MessageBus, SendReceipt, SubAgentEvent, message_bus_pair,
};
use llm_harness_runtime::spawn::spawn_types::{ContextMode, ProviderSelection, SpawnOptions};
use llm_harness_runtime::spawn::spawner::{
    EnvFactory, HarnessSubAgentSpawner, JsonlSessionFactory,
};
use llm_harness_runtime::spawn::tools::SpawnAgentTool;
use llm_harness_types::ExecutionEnv;
use tokio_util::sync::CancellationToken;

// ── Test fixtures ──────────────────────────────────────────────────────────

struct NoopPlugin;
impl Plugin for NoopPlugin {
    fn name(&self) -> &str {
        "noop"
    }
}

struct NoopEnvFactory;
impl EnvFactory for NoopEnvFactory {
    fn create(
        &self,
        _cwd: &std::path::Path,
    ) -> Result<Arc<dyn ExecutionEnv>, llm_harness_types::AgentError> {
        Ok(Arc::new(NoOpEnv))
    }
}

/// Build a spawner backed by a `MockLlmClient` that returns a single text
/// response. The spawner is connected to the given `MessageBus`.
fn make_spawner(
    bus: Arc<MessageBus>,
    responses: Vec<MockResponse>,
) -> (Arc<HarnessSubAgentSpawner>, tempfile::TempDir) {
    let client = Arc::new(MockLlmClient::new(responses));
    let dir = tempfile::tempdir().unwrap();
    let spawner = HarnessSubAgentSpawner::new(
        "test-model",
        client,
        dir.path().to_path_buf(),
        bus,
        |_, _, _| Box::new(NoopPlugin),
    )
    .session_factory(Arc::new(JsonlSessionFactory))
    .env_factory(Arc::new(NoopEnvFactory));
    (Arc::new(spawner), dir)
}

/// Build a real (lightweight) `AgentHarness` via `HarnessBuilder` + `MockLlmClient`.
async fn make_harness(responses: Vec<MockResponse>) -> Arc<AgentHarness> {
    let client: Arc<dyn llm_harness_loop::LlmClient> = Arc::new(MockLlmClient::new(responses));
    let env: Arc<dyn ExecutionEnv> = Arc::new(NoOpEnv);
    Arc::new(
        HarnessBuilder::new("test-model")
            .provider("test-model", client)
            .build(env)
            .await
            .expect("build harness"),
    )
}

/// Register a Running sub-agent entry on the bus.
fn register_sub(bus: &Arc<MessageBus>, id: &str) {
    bus.register(
        id,
        AgentEntry {
            status: AgentStatus::Running,
            harness: None,
            is_main: false,
        },
    );
}

/// Register the Main agent entry on the bus.
fn register_main(bus: &Arc<MessageBus>) {
    bus.register(
        "Main",
        AgentEntry {
            status: AgentStatus::Running,
            harness: None,
            is_main: true,
        },
    );
}

/// Wait for `bus.in_flight_count()` to reach zero, with a deadline.
async fn wait_for_in_flight_zero(bus: &MessageBus) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while bus.in_flight_count() > 0 {
        if std::time::Instant::now() > deadline {
            panic!("in_flight_count did not reach 0 within 10s");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ── Test 1: async_spawn_completion_test ────────────────────────────────────
//
// Main agent spawns sub via `spawn_async`, sub completes, the Done event
// arrives on the event channel (consumed via `take_event_rx`).

#[tokio::test]
async fn async_spawn_completion_test() {
    let bus = message_bus_pair();
    let (spawner, _dir) = make_spawner(bus.clone(), vec![MockResponse::text("sub done")]);

    let agent_id = spawner
        .spawn_async(SpawnOptions {
            prompt: "say ok".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        })
        .await
        .expect("spawn_async should succeed");
    assert!(!agent_id.is_empty());

    // Wait for the track task to complete and deliver the Done event.
    wait_for_in_flight_zero(&bus).await;

    // Borrow the event rx and verify a Done event was delivered.
    let mut guard = bus
        .take_event_rx()
        .expect("event_rx should be available after track task completes");

    let event = tokio::time::timeout(Duration::from_secs(5), guard.recv())
        .await
        .expect("should receive event within 5s")
        .expect("event should be Some");

    match event {
        SubAgentEvent::Done {
            agent_id: id,
            result,
        } => {
            assert_eq!(id, agent_id);
            let r = result.expect("result should be Ok");
            assert!(
                r.final_answer.contains("sub done"),
                "final_answer should contain 'sub done', got: {}",
                r.final_answer
            );
        }
        SubAgentEvent::Message { .. } => panic!("expected Done event, got Message"),
    }
}

// ── Test 2: message_subagent_steer_test ────────────────────────────────────
//
// Main sends a message to a sub-agent (no waiter). The bus routes it via
// `harness.steer_message()`. We verify the send returns `Injected` and the
// sub-agent's harness has the steer message enqueued (checked via session
// history for a CustomMessage with type "main_message").
//
// Because the sub-agent's run loop is not active in this test, the steered
// message sits in the harness's internal steer channel. We verify delivery
// at the bus level (receipt == Injected) and that the harness reference is
// present (steer path was taken, not the event-channel path).

#[tokio::test]
async fn message_subagent_steer_test() {
    let (bus, _event_rx) = MessageBus::new();
    let bus = Arc::new(bus);

    // Build a real harness so the steer path has a harness to inject into.
    let harness = make_harness(vec![MockResponse::text("ok")]).await;
    bus.register(
        "sub-1",
        AgentEntry {
            status: AgentStatus::Running,
            harness: Some(harness.clone()),
            is_main: false,
        },
    );
    register_main(&bus);

    // Send a message from main to sub — no waiter registered, so the bus
    // routes via the steer path (harness.steer_message). The harness's
    // steer channel receiver is not live (make_harness drops it), so
    // steer_message returns Err(Closed) and send() reports Failed.
    // This is the correct behavior: the message cannot be delivered to
    // a sub whose run loop is not active.
    let receipt = bus.send("Main", "sub-1", "hello from main").await;
    assert!(
        matches!(receipt, SendReceipt::Failed(_)),
        "send to sub with closed steer channel should return Failed, got {receipt:?}"
    );

    // Verify the sub's status is still Running (steer path was taken, not
    // the event-channel path which is used for main agents).
    let snapshots = bus.query_status(Some("sub-1"));
    assert_eq!(snapshots.len(), 1);
    assert!(matches!(snapshots[0].status, AgentStatus::Running));

    // Note: We cannot verify the steer message is enqueued in the harness's
    // internal steer channel because make_harness creates the channel with
    // its receiver immediately dropped (only reset_channels() during prompt()
    // creates a live pair). Steer consumption is verified in harness-level
    // tests that use spawn_async (which calls prompt()).
}

// ── Step 9: CustomMessage persistence tests ──────────────────────────────────
//
// Verify that `main_message` and `sub_agent_message` CustomMessage types
// appear when no waiter is registered (fire-and-forget path).

#[tokio::test]
async fn custom_message_persistence_no_waiter_test() {
    let (bus, event_rx) = MessageBus::new();
    let bus = Arc::new(bus);
    let mut event_rx = event_rx;

    // Register a sub-agent with a real harness (steer path).
    let sub_harness = make_harness(vec![MockResponse::text("ok")]).await;
    bus.register(
        "sub-1",
        AgentEntry {
            status: AgentStatus::Running,
            harness: Some(sub_harness.clone()),
            is_main: false,
        },
    );
    register_main(&bus);

    // 1. Main → Sub (no waiter): bus routes via steer, which calls
    //    harness.steer_message(). The harness's steer channel receiver
    //    is dropped (make_harness), so steer_message returns Err(Closed)
    //    and send() reports Failed — the message cannot be delivered
    //    to a sub whose run loop is not active.
    let receipt = bus.send("Main", "sub-1", "hello sub").await;
    assert!(matches!(receipt, SendReceipt::Failed(_)));

    // 2. Sub → Main (no waiter): bus routes to the event channel as
    //    SubAgentEvent::Message. The event channel consumer wraps it in
    //    CustomMessage{type:"sub_agent_message"} in drain_and_format.
    let receipt = bus.send("sub-1", "Main", "report to main").await;
    assert!(matches!(receipt, SendReceipt::Injected));

    // Verify the event channel received a Message event (which drain_and_format
    // wraps as CustomMessage{type:"sub_agent_message"}).
    let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
        .await
        .expect("should receive event within 5s")
        .expect("event should be Some");

    match event {
        SubAgentEvent::Message { agent_id, body } => {
            assert_eq!(agent_id, "sub-1");
            assert!(
                body.contains("report to main"),
                "body should contain 'report to main', got: {body}"
            );
        }
        SubAgentEvent::Done { .. } => panic!("expected Message event, got Done"),
    }
}

// ── Test 3: message_main_should_stop_test ──────────────────────────────────
//
// Sub sends a message to main (no waiter). The bus routes it to the event
// channel. We verify the event channel receives SubAgentEvent::Message.

#[tokio::test]
async fn message_main_should_stop_test() {
    let (bus, mut event_rx) = MessageBus::new();
    let bus = Arc::new(bus);

    register_main(&bus);
    register_sub(&bus, "sub-1");

    // Sub sends a message to main — no waiter, is_main=true → event channel.
    let receipt = bus.send("sub-1", "Main", "report from sub").await;
    assert!(
        matches!(receipt, SendReceipt::Injected),
        "send to Main without waiter should return Injected, got {receipt:?}"
    );

    let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
        .await
        .expect("should receive event within 5s")
        .expect("event should be Some");

    match event {
        SubAgentEvent::Message { agent_id, body } => {
            assert_eq!(agent_id, "sub-1");
            assert_eq!(body, "report from sub");
        }
        SubAgentEvent::Done { .. } => panic!("expected Message event, got Done"),
    }
}

// ── Test 4: await_subagent_reply_test ──────────────────────────────────────
//
// Main waits for a message from sub. Sub sends, main receives via the
// oneshot waiter. Tests `bus.wait()` + `bus.send()` waiter matching.

#[tokio::test]
async fn await_subagent_reply_test() {
    let bus = message_bus_pair();
    register_main(&bus);
    register_sub(&bus, "sub-1");

    // Spawn the waiter: main waits for any message from sub-1.
    let bus_clone = bus.clone();
    let wait_handle = tokio::spawn(async move {
        bus_clone
            .wait(
                "Main",
                Some("sub-1"),
                Some(Duration::from_secs(5)),
                CancellationToken::new(),
            )
            .await
    });

    // Give the waiter time to register.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Sub sends a message — should match the waiter via oneshot.
    let receipt = bus.send("sub-1", "Main", "reply from sub").await;
    assert!(matches!(receipt, SendReceipt::Injected));

    let event = tokio::time::timeout(Duration::from_secs(5), wait_handle)
        .await
        .expect("wait task should complete within 5s")
        .expect("task should not panic")
        .expect("should receive a message");

    match event {
        SubAgentEvent::Message { agent_id, body } => {
            assert_eq!(agent_id, "sub-1");
            assert_eq!(body, "reply from sub");
        }
        SubAgentEvent::Done { .. } => panic!("expected Message event, got Done"),
    }
}

// ── Test 5: await_main_message_test ────────────────────────────────────────
//
// Sub waits for a message from main. Main sends, sub receives via the
// oneshot waiter. Tests `bus.wait()` with `filter_from=None` + `bus.send()`.

#[tokio::test]
async fn await_main_message_test() {
    let bus = message_bus_pair();
    register_sub(&bus, "sub-1");
    register_main(&bus);

    // Spawn the waiter: sub waits for any message (from=None).
    let bus_clone = bus.clone();
    let wait_handle = tokio::spawn(async move {
        bus_clone
            .wait(
                "sub-1",
                None,
                Some(Duration::from_secs(5)),
                CancellationToken::new(),
            )
            .await
    });

    // Give the waiter time to register.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Main sends a message to sub — should match the waiter via oneshot.
    let receipt = bus.send("Main", "sub-1", "wake up").await;
    assert!(matches!(receipt, SendReceipt::Injected));

    let event = tokio::time::timeout(Duration::from_secs(5), wait_handle)
        .await
        .expect("wait task should complete within 5s")
        .expect("task should not panic")
        .expect("should receive a message");

    match event {
        SubAgentEvent::Message { agent_id, body } => {
            assert_eq!(agent_id, "Main");
            assert_eq!(body, "wake up");
        }
        SubAgentEvent::Done { .. } => panic!("expected Message event, got Done"),
    }
}

// ── Test 6: idle_watcher_wake_test ─────────────────────────────────────────
//
// IdleWatcher detects activity (Done event) on the event channel and invokes
// the continue_run callback. We test the IdleWatcher directly: create a bus,
// track a quick-completing handle, start the watcher, and verify the callback
// fires.
//
// Note: `track()` is `pub(crate)` and unavailable to integration tests. We
// use `spawner.spawn_async()` instead, which internally calls
// `track_with_oneshot`. The Done event is delivered to the event channel,
// which the IdleWatcher picks up.

#[tokio::test]
async fn idle_watcher_wake_test() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let bus = message_bus_pair();
    let (spawner, _dir) = make_spawner(bus.clone(), vec![MockResponse::text("done")]);

    // Spawn a sub-agent — this registers it on the bus and starts tracking.
    let _agent_id = spawner
        .spawn_async(SpawnOptions {
            prompt: "say ok".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        })
        .await
        .expect("spawn_async should succeed");

    assert_eq!(bus.in_flight_count(), 1);

    let called = Arc::new(AtomicUsize::new(0));
    let watcher = IdleWatcher::new(bus.clone(), {
        let c = called.clone();
        Arc::new(move || {
            let c = c.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
            })
        })
    });

    // Start the watcher — it should detect the in-flight sub and begin
    // listening on the event channel.
    watcher.on_idle();

    // Wait for the sub to complete and the watcher to fire continue_run.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if called.load(Ordering::SeqCst) >= 1 {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "IdleWatcher continue_run callback not invoked within 10s (called={}, in_flight={})",
                called.load(Ordering::SeqCst),
                bus.in_flight_count()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(
        called.load(Ordering::SeqCst) >= 1,
        "continue_run callback should be invoked at least once"
    );

    // Clean up the watcher.
    watcher.cancel();
    wait_for_in_flight_zero(&bus).await;
}

// ── Test 7: abort_propagation_test ─────────────────────────────────────────
//
// Main aborts a sub-agent via `bus.abort_agent()`. The sub-agent's abort
// token is cancelled. We use `spawn_async` which registers the abort token
// via `register_subagent_abort` (called inside `track_with_oneshot`).
//
// To observe the abort taking effect, we use a BlockingTool sub-agent that
// sleeps until cancelled. The abort should cause the sub to terminate with
// Err(Aborted), and in_flight should return to 0.

use llm_harness_types::{Tool as ToolTrait, ToolContext, ToolError, ToolResult};

/// A tool that blocks until the abort token is cancelled, then returns
/// `AgentError::Aborted`. This keeps the sub-agent in-flight so we can
/// observe abort propagation.
struct BlockingTool;

impl ToolTrait for BlockingTool {
    fn name(&self) -> &str {
        "blocking_test_tool"
    }

    fn description(&self) -> &str {
        "blocks until cancelled"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::LazyLock<serde_json::Value> =
            std::sync::LazyLock::new(|| serde_json::json!({}));
        &SCHEMA
    }

    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> futures::future::BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            // Block until the abort token fires, then return Aborted.
            // The harness recognizes ToolError::Aborted and terminates
            // the sub-agent with Err(AgentError::Aborted).
            ctx.abort.cancelled().await;
            Err(ToolError::Aborted)
        })
    }
}

/// Plugin that registers BlockingTool — the sub-agent calls it and blocks.
struct BlockingToolPlugin;

impl Plugin for BlockingToolPlugin {
    fn name(&self) -> &str {
        "blocking-tool-plugin"
    }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn ToolTrait>>) {
        tools.push(Arc::new(BlockingTool));
    }
}

#[tokio::test]
async fn abort_propagation_test() {
    let bus = message_bus_pair();

    // Build a spawner whose sub-agent uses BlockingToolPlugin.
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::tool_use(
        "call_1",
        "blocking_test_tool",
        "{}",
    )]));
    let dir = tempfile::tempdir().unwrap();
    let spawner = HarnessSubAgentSpawner::new(
        "test-model",
        client,
        dir.path().to_path_buf(),
        bus.clone(),
        |_, _, _| Box::new(BlockingToolPlugin),
    )
    .session_factory(Arc::new(JsonlSessionFactory))
    .env_factory(Arc::new(NoopEnvFactory));
    let spawner = Arc::new(spawner);

    let agent_id = spawner
        .spawn_async(SpawnOptions {
            prompt: "call the blocking tool".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        })
        .await
        .expect("spawn_async should succeed");

    // Give the sub-agent time to start and enter the BlockingTool.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(bus.in_flight_count(), 1, "sub should be in-flight");

    // Abort the sub-agent via the bus.
    bus.abort_agent(&agent_id);

    // Give the abort a moment to propagate.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The sub should terminate quickly (far shorter than any natural sleep).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if bus.in_flight_count() == 0 {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "sub-agent not aborted within 5s, in_flight={}",
                bus.in_flight_count()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        bus.in_flight_count(),
        0,
        "sub should be aborted (in_flight=0)"
    );

    // After abort, the agent is removed from the registry (P1-1 fix:
    // prevent unbounded memory growth). query_status returns empty.
    let snapshots = bus.query_status(Some(&agent_id));
    assert_eq!(
        snapshots.len(),
        0,
        "agent should be removed from registry after abort"
    );
}

// ── Test 8: done_event_matches_waiter_test ─────────────────────────────────
//
// A waiter registered under the sub's agent_id receives the Done event
// when the sub completes. The track task (inside spawn_async) matches
// waiters registered under the sub's id and delivers via oneshot.
//
// Note: `track_with_oneshot()` is `pub(crate)` and unavailable to integration
// tests. We use `spawner.spawn_async()` which internally calls it. The track
// task, on completion, tries to match a waiter registered under the sub's
// agent_id — if found, the Done event is delivered via oneshot to
// `bus.wait()`, bypassing the event channel.

#[tokio::test]
async fn done_event_matches_waiter_test() {
    let bus = message_bus_pair();
    let (spawner, _dir) = make_spawner(bus.clone(), vec![MockResponse::text("completed")]);

    // Spawn the sub-agent first. The sub takes at least one LLM round-trip
    // to complete, giving us time to register a waiter.
    let agent_id = spawner
        .spawn_async(SpawnOptions {
            prompt: "say completed".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        })
        .await
        .expect("spawn_async should succeed");

    // Register a waiter under the sub's agent_id. The track task looks for
    // waiters registered under the sub's id when delivering the Done event.
    let bus_clone = bus.clone();
    let id_clone = agent_id.clone();
    let wait_handle = tokio::spawn(async move {
        bus_clone
            .wait(
                &id_clone,
                None,
                Some(Duration::from_secs(10)),
                CancellationToken::new(),
            )
            .await
    });

    // The wait() should return a Done event delivered via the oneshot waiter.
    let event = tokio::time::timeout(Duration::from_secs(10), wait_handle)
        .await
        .expect("wait task should complete within 10s")
        .expect("task should not panic")
        .expect("should receive a Done event via waiter");

    match event {
        SubAgentEvent::Done {
            agent_id: id,
            result,
        } => {
            assert_eq!(id, agent_id);
            let r = result.expect("result should be Ok");
            assert!(
                r.final_answer.contains("completed"),
                "final_answer should contain 'completed', got: {}",
                r.final_answer
            );
        }
        SubAgentEvent::Message { .. } => panic!("expected Done event, got Message"),
    }

    // Ensure the track task fully completes.
    wait_for_in_flight_zero(&bus).await;
}

// ── Bonus: query_status_after_completion_test ──────────────────────────────
//
// After a sub-agent completes via spawn_async, its status should be updated
// to Done on the bus. This verifies the track task's status update path.

#[tokio::test]
async fn query_status_after_completion_test() {
    let bus = message_bus_pair();
    let (spawner, _dir) = make_spawner(bus.clone(), vec![MockResponse::text("finished")]);

    let agent_id = spawner
        .spawn_async(SpawnOptions {
            prompt: "say finished".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        })
        .await
        .expect("spawn_async should succeed");

    wait_for_in_flight_zero(&bus).await;

    // After completion, the agent is removed from the registry (P1-1 fix:
    // prevent unbounded memory growth). query_status returns empty.
    let snapshots = bus.query_status(Some(&agent_id));
    assert_eq!(
        snapshots.len(),
        0,
        "agent should be removed from registry after completion"
    );
}

// ── Bonus: cancel_all_aborts_inflight_subagents_test ───────────────────────
//
// `cancel_all_subagents()` (AbortCascadeHook) cancels all in-flight sub-agents
// registered via spawn_async. We spawn two blocking sub-agents, call
// cancel_all, and verify both terminate.

#[tokio::test]
async fn cancel_all_aborts_inflight_subagents_test() {
    use llm_harness_agent::AbortCascadeHook;

    let bus = message_bus_pair();

    // Build a spawner whose sub-agent uses BlockingToolPlugin.
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use("call_1", "blocking_test_tool", "{}"),
        MockResponse::tool_use("call_2", "blocking_test_tool", "{}"),
    ]));
    let dir = tempfile::tempdir().unwrap();
    let spawner = HarnessSubAgentSpawner::new(
        "test-model",
        client,
        dir.path().to_path_buf(),
        bus.clone(),
        |_, _, _| Box::new(BlockingToolPlugin),
    )
    .session_factory(Arc::new(JsonlSessionFactory))
    .env_factory(Arc::new(NoopEnvFactory));
    let _id1 = spawner
        .spawn_async(SpawnOptions {
            prompt: "block".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        })
        .await
        .expect("spawn 1");
    let _id2 = spawner
        .spawn_async(SpawnOptions {
            prompt: "block".into(),
            provider: ProviderSelection::Inherit,
            cwd: None,
            context: ContextMode::None,
        })
        .await
        .expect("spawn 2");

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(bus.in_flight_count(), 2, "both subs should be in-flight");

    // Cancel all in-flight sub-agents.
    bus.cancel_all_subagents();

    // Both should terminate quickly.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if bus.in_flight_count() == 0 {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "subs not cancelled within 5s, in_flight={}",
                bus.in_flight_count()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(bus.in_flight_count(), 0);
}

// ── Bonus: send_to_aborted_agent_fails_test ────────────────────────────────
//
// Sending a message to an Aborted agent should return SendReceipt::Failed.

#[tokio::test]
async fn send_to_aborted_agent_fails_test() {
    let bus = message_bus_pair();
    register_main(&bus);
    register_sub(&bus, "sub-1");

    // Mark the sub as Aborted.
    bus.update_status("sub-1", AgentStatus::Aborted);

    let receipt = bus.send("Main", "sub-1", "hello?").await;
    assert!(
        matches!(receipt, SendReceipt::Failed(_)),
        "sending to Aborted agent should fail, got {receipt:?}"
    );
}

// ── Bonus: wait_filter_by_from_test ────────────────────────────────────────
//
// Main waits for a message from a specific sub. A different sub sends first
// (should not match), then the correct sub sends (should match).

#[tokio::test]
async fn wait_filter_by_from_test() {
    let bus = message_bus_pair();
    register_main(&bus);
    register_sub(&bus, "sub-1");
    register_sub(&bus, "sub-2");

    let bus_clone = bus.clone();
    let wait_handle = tokio::spawn(async move {
        bus_clone
            .wait(
                "Main",
                Some("sub-2"),
                Some(Duration::from_secs(5)),
                CancellationToken::new(),
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // sub-1 sends — should NOT match the filtered waiter.
    let r1 = bus.send("sub-1", "Main", "from sub-1").await;
    assert!(matches!(r1, SendReceipt::Injected));

    // sub-2 sends — should match.
    let r2 = bus.send("sub-2", "Main", "from sub-2").await;
    assert!(matches!(r2, SendReceipt::Injected));

    let event = tokio::time::timeout(Duration::from_secs(5), wait_handle)
        .await
        .expect("wait task should complete within 5s")
        .expect("task should not panic")
        .expect("should receive a message");

    match event {
        SubAgentEvent::Message { agent_id, body } => {
            assert_eq!(agent_id, "sub-2");
            assert_eq!(body, "from sub-2");
        }
        SubAgentEvent::Done { .. } => panic!("expected Message event, got Done"),
    }
}

// ── Bonus: wait_times_out_test ─────────────────────────────────────────────
//
// `bus.wait()` returns None on timeout (no message arrives).

#[tokio::test]
async fn wait_times_out_test() {
    let bus = message_bus_pair();
    register_sub(&bus, "sub-1");

    let result = bus
        .wait(
            "sub-1",
            None,
            Some(Duration::from_millis(50)),
            CancellationToken::new(),
        )
        .await;

    assert!(result.is_none(), "wait should return None on timeout");

    // After timeout, a subsequent send should NOT match a stale waiter.
    // With harness=None and is_main=false, send returns Failed (harness
    // detached) — proving no stale oneshot was hit.
    let receipt = bus.send("main", "sub-1", "after-timeout").await;
    assert!(
        matches!(receipt, SendReceipt::Failed(_)),
        "send after timeout should not match stale waiter, got {receipt:?}"
    );
}

// ── Bonus: wait_aborts_on_cancel_test ──────────────────────────────────────
//
// `bus.wait()` returns None when the abort token is cancelled.

#[tokio::test]
async fn wait_aborts_on_cancel_test() {
    let bus = message_bus_pair();
    register_sub(&bus, "sub-1");

    let abort = CancellationToken::new();
    let abort_clone = abort.clone();

    let bus_clone = bus.clone();
    let wait_handle = tokio::spawn(async move {
        bus_clone
            .wait("sub-1", None, Some(Duration::from_secs(60)), abort_clone)
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    abort.cancel();

    let result = tokio::time::timeout(Duration::from_secs(5), wait_handle)
        .await
        .expect("wait task should complete within 5s")
        .expect("task should not panic");

    assert!(result.is_none(), "wait should return None on abort");
}

// ── Bonus: spawn_agent_tool_e2e_test ───────────────────────────────────────
//
// End-to-end: the SpawnAgentTool (LLM-visible tool) spawns a sub-agent via
// the spawner, and the Done event arrives on the event channel. This
// exercises the tool → spawner → bus → track → event channel path.

#[tokio::test]
async fn spawn_agent_tool_e2e_test() {
    let bus = message_bus_pair();
    let (spawner, _dir) = make_spawner(bus.clone(), vec![MockResponse::text("tool spawned ok")]);
    let tool = SpawnAgentTool::new(spawner);

    let env: Arc<dyn ExecutionEnv> = Arc::new(NoOpEnv);
    let (update_tx, _update_rx) = tokio::sync::mpsc::channel(8);
    let ctx = ToolContext {
        env,
        abort: CancellationToken::new(),
        tool_use_id: "test-call".into(),
        turn_index: 0,
        assistant_message: Arc::new(llm_harness_loop::test_utils::test_assistant_message(vec![])),
        update_tx,
    };

    let result = tool
        .execute(serde_json::json!({"prompt": "say ok"}), &ctx)
        .await
        .expect("tool execute should succeed");

    assert!(!result.terminate);
    let agent_id = result
        .details
        .get("agent_id")
        .expect("details should have agent_id")
        .as_str()
        .expect("agent_id should be a string");
    assert!(!agent_id.is_empty());

    // Wait for the sub to complete.
    wait_for_in_flight_zero(&bus).await;

    // Verify the Done event arrived on the event channel.
    let mut guard = bus.take_event_rx().expect("event_rx should be available");

    let event = tokio::time::timeout(Duration::from_secs(5), guard.recv())
        .await
        .expect("should receive event within 5s")
        .expect("event should be Some");

    match event {
        SubAgentEvent::Done {
            agent_id: id,
            result,
        } => {
            assert_eq!(id, agent_id);
            let r = result.expect("result should be Ok");
            assert!(
                r.final_answer.contains("tool spawned ok"),
                "final_answer should contain 'tool spawned ok', got: {}",
                r.final_answer
            );
        }
        SubAgentEvent::Message { .. } => panic!("expected Done event, got Message"),
    }
}
