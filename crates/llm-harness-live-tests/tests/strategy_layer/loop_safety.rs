//! Tests: LoopSafetyPlugin guards — max_turns hard cap, repetition guard,
//! and failure circuit breaker. Each test installs a single guard via
//! `LoopSafetyPlugin::new(config)` and exercises it against a live LLM.

use std::sync::Arc;
use std::sync::LazyLock;

use futures::future::BoxFuture;

use llm_harness_live_tests::skip_if_none;
use llm_harness_live_tests::{
    CapturedEvent, EventCapture, LiveProvider, SINGLE_TURN, assert_tool_was_called,
    make_harness_with, with_timeout,
};
use llm_harness_strategy::{
    FailureCircuitBreakerConfig, LoopSafetyConfig, LoopSafetyPlugin, MaxTurnsConfig,
    RepetitionGuardConfig,
};
use llm_harness_types::{
    AgentMessage, DataBlock, HarnessPhase, Tool, ToolContext, ToolFailure, ToolResult,
};

// ── Echo tool ──────────────────────────────────────────────────────────────

struct EchoTool;

static ECHO_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": { "type": "string", "description": "Message to echo back" }
        },
        "required": ["message"]
    })
});

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echo back the provided message verbatim."
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        &ECHO_SCHEMA
    }
    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let msg = args
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("(no message)");
            Ok(ToolResult::full(
                vec![DataBlock::text(msg)],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

// ── Failing tool (always returns ToolFailure) ──────────────────────────────

struct FailingTool;

static FAIL_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "input": { "type": "string", "description": "Input that will fail" }
        },
        "required": ["input"]
    })
});

impl Tool for FailingTool {
    fn name(&self) -> &str {
        "fail_tool"
    }
    fn description(&self) -> &str {
        "A tool that always fails. Use it when asked to demonstrate failure handling."
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        &FAIL_SCHEMA
    }
    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move { Err(ToolFailure::new("always_fails", "this tool always fails")) })
    }
}

// ── Helper: count assistant turns from the session ─────────────────────────

/// Count assistant messages in the active session path — each corresponds
/// to one completed turn.
async fn count_assistant_turns(harness: &llm_harness_agent::AgentHarness) -> usize {
    let path = harness.read_active_path().await.expect("read_active_path");
    path.iter()
        .filter(|e| {
            matches!(
                &e.payload,
                llm_harness_agent::session::SessionEntryPayload::Message(AgentMessage::Assistant(
                    _
                ))
            )
        })
        .count()
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// Max-turns guard: with `max_turns = 2`, the loop stops after at most 2
/// turns even if the model would naturally continue.
#[tokio::test]
async fn max_turns_stops_loop() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let config = LoopSafetyConfig {
        max_turns: Some(MaxTurnsConfig { max_turns: 2 }),
        ..LoopSafetyConfig::default()
    };

    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.install(&LoopSafetyPlugin::new(config))
            .system_prompt(Some(
                "You are a helpful assistant. Use the echo tool whenever possible.".into(),
            ))
            .tool(Arc::new(EchoTool) as Arc<dyn Tool>)
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    // A prompt that invites multiple tool calls / turns.
    with_timeout(
        SINGLE_TURN,
        harness.prompt("Use the echo tool to say 'hello', then 'world', then 'done'."),
    )
    .await
    .expect("prompt should complete (either settled or stopped by max_turns)");

    cap.settle().await;

    // The conversation completed: either Settled naturally or stopped by
    // the max_turns guard (which emits an Error event then breaks).
    let events = cap.events();
    let completed = events
        .iter()
        .any(|e| matches!(e, CapturedEvent::Settled | CapturedEvent::Error(_)));
    assert!(
        completed,
        "conversation should have completed; events: {events:?}"
    );

    // At most 2 turns were executed.
    let turns = count_assistant_turns(&harness).await;
    assert!(
        turns <= 2,
        "max_turns=2 should cap turns at 2, but got {turns}"
    );

    // Harness returned to Idle.
    assert_eq!(harness.state().phase, HarnessPhase::Idle);
}

/// Repetition guard: with `max_repetitions = 2`, the third identical echo
/// call is denied. The conversation still completes.
#[tokio::test]
async fn repetition_detected() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let config = LoopSafetyConfig {
        repetition_guard: Some(RepetitionGuardConfig {
            max_repetitions: 2,
            ..Default::default()
        }),
        ..LoopSafetyConfig::default()
    };

    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.install(&LoopSafetyPlugin::new(config))
            .system_prompt(Some(
                "You are a helpful assistant. When asked to call a tool, call it.".into(),
            ))
            .tool(Arc::new(EchoTool) as Arc<dyn Tool>)
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    with_timeout(
        SINGLE_TURN,
        harness.prompt("Call the echo tool 3 times with the same message 'repeat'."),
    )
    .await
    .expect("prompt should complete (repetition guard denies the 3rd call)");

    cap.settle().await;

    let events = cap.events();

    // Echo was called at least once before the guard tripped.
    assert_tool_was_called(&events, "echo");

    // Conversation completed (Settled or Error from the guard's should_stop).
    let completed = events
        .iter()
        .any(|e| matches!(e, CapturedEvent::Settled | CapturedEvent::Error(_)));
    assert!(
        completed,
        "conversation should have completed; events: {events:?}"
    );

    assert_eq!(harness.state().phase, HarnessPhase::Idle);
}

/// Failure circuit breaker: with `max_consecutive_failures = 2`, the loop
/// stops after 2 consecutive failures of the same tool.
#[tokio::test]
async fn consecutive_failure_breaker() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let config = LoopSafetyConfig {
        failure_circuit_breaker: Some(FailureCircuitBreakerConfig {
            max_consecutive_failures: 2,
            ..Default::default()
        }),
        ..LoopSafetyConfig::default()
    };

    let (harness, _inspector, _tmp) = make_harness_with(entry, |b| {
        b.install(&LoopSafetyPlugin::new(config))
            .system_prompt(Some(
                "You are a helpful assistant. When asked to use a tool, use it.".into(),
            ))
            .tool(Arc::new(FailingTool) as Arc<dyn Tool>)
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    with_timeout(
        SINGLE_TURN,
        harness.prompt("Use the fail_tool 3 times with any input."),
    )
    .await
    .expect("prompt should complete (circuit breaker stops after 2 failures)");

    cap.settle().await;

    let events = cap.events();

    // fail_tool was called at least once before the breaker tripped.
    assert_tool_was_called(&events, "fail_tool");

    // Conversation completed (Settled or Error from the breaker's should_stop).
    let completed = events
        .iter()
        .any(|e| matches!(e, CapturedEvent::Settled | CapturedEvent::Error(_)));
    assert!(
        completed,
        "conversation should have completed; events: {events:?}"
    );

    assert_eq!(harness.state().phase, HarnessPhase::Idle);
}
