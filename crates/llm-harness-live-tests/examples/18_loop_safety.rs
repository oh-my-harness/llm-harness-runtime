//! 18 — Loop Safety: LoopSafetyPlugin with four guards.
//!
//! Demonstrates:
//! - Part 1: MaxTurns — hard cap on total turns (max_turns: 2)
//! - Part 2: Repetition guard — denies repeated identical tool calls
//! - Part 3: Failure circuit breaker — trips after N consecutive tool failures
//! - Part 4: Death spiral guard — halts after N consecutive error turns
//!
//! The `LoopSafetyPlugin` bundles five hooks behind a single config. Each
//! sub-config is `Option<T>`: `Some` enables that guard, `None` disables it.
//! When a guard denies a tool call, the harness feeds a `ToolFailure` result
//! back to the LLM. When a guard triggers `should_stop` or `prepare` returns
//! an error, the loop halts.
//!
//! Run:
//!   source .env
//!   cargo run -p llm-harness-live-tests --example 18_loop_safety

use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_live_tests::{
    EventCapture, LiveProvider, SINGLE_TURN, make_harness_with, with_timeout,
};
use llm_harness_strategy::{
    DeathSpiralGuardConfig, FailureCircuitBreakerConfig, LoopSafetyConfig, LoopSafetyPlugin,
    MaxTurnsConfig, RepetitionGuardConfig,
};
use llm_harness_types::{DataBlock, Tool, ToolContext, ToolFailure, ToolResult};

// ── Echo tool ────────────────────────────────────────────────────────────────

struct EchoTool;

static ECHO_SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": { "type": "string", "description": "The message to echo back" }
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

// ── Failing tool — always returns ToolFailure ───────────────────────────────

struct FailingTool;

static FAILING_SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "input": { "type": "string", "description": "Some input to process" }
        },
        "required": ["input"]
    })
});

impl Tool for FailingTool {
    fn name(&self) -> &str {
        "failing_tool"
    }
    fn description(&self) -> &str {
        "A tool that always fails. Useful for testing error handling."
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        &FAILING_SCHEMA
    }
    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            Err(ToolFailure::new(
                "execution_failed",
                "This tool always fails by design.",
            ))
        })
    }
}

// ── Helper: count captured events of a given discriminant ────────────────────

fn count_errors(events: &[llm_harness_live_tests::CapturedEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, llm_harness_live_tests::CapturedEvent::Error(_)))
        .count()
}

fn count_tool_calls(events: &[llm_harness_live_tests::CapturedEvent], name: &str) -> usize {
    events
        .iter()
        .filter(|e| match e {
            llm_harness_live_tests::CapturedEvent::ToolCallStart { tool_name } => tool_name == name,
            _ => false,
        })
        .count()
}

#[tokio::main]
async fn main() {
    println!("=== 18: Loop Safety (LoopSafetyPlugin) ===\n");

    let provider = LiveProvider::from_env();
    let entry = provider
        .openai
        .as_ref()
        .or(provider.anthropic.as_ref())
        .expect("set OPENAI_API_KEY or ANTHROPIC_API_KEY to run this example");

    println!("Provider: {} | Model: {}\n", entry.name, entry.model);

    part1_max_turns(entry).await;
    println!();
    part2_repetition_guard(entry).await;
    println!();
    part3_failure_circuit_breaker(entry).await;
    println!();
    part4_death_spiral_guard(entry).await;

    println!("\n--- Summary ---");
    println!("MaxTurns (max 2):           loop stops after 2 turns with error event");
    println!("Repetition (max 2):         3rd identical echo call denied");
    println!("Circuit breaker (max 2):    failing_tool blocked after 2 failures");
    println!("Death spiral (max 2):       loop stops after 2 error turns");
}

// ── Part 1: MaxTurns — hard cap on turns ────────────────────────────────────

async fn part1_max_turns(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 1: MaxTurns (hard cap = 2 turns) ---\n");

    let config = LoopSafetyConfig {
        max_turns: Some(MaxTurnsConfig { max_turns: 2 }),
        ..LoopSafetyConfig::default()
    };

    let (harness, _tmp) = make_harness_with(entry, |b| {
        b.install(&LoopSafetyPlugin::new(config))
            .tool(Arc::new(EchoTool) as Arc<dyn Tool>)
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    // Prompt that encourages multiple tool-call turns; the cap should stop it.
    let prompt = "Use the echo tool to echo the message 'turn one', then echo 'turn two', \
                  then echo 'turn three'. Call echo once per turn, waiting for each result \
                  before calling it again.";
    println!("LLM prompt: {}", prompt);

    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let events = cap.events();
    let echo_count = count_tool_calls(&events, "echo");
    let error_count = count_errors(&events);
    let has_settled = events
        .iter()
        .any(|e| matches!(e, llm_harness_live_tests::CapturedEvent::Settled));

    println!("\nObservation:");
    println!("  echo ToolCallStart events: {}", echo_count);
    println!("  Error events:              {}", error_count);
    println!("  Settled emitted:           {}", has_settled);
    println!(
        "  Final text:                {}",
        cap.texts().chars().take(120).collect::<String>()
    );

    // With max_turns=2, the loop runs at most 2 turns. The 3rd turn's
    // prepare_next_turn returns an error ("max_turns limit exceeded"),
    // which surfaces as an Error event.
    println!("\n  Expected: at most 2 turns execute; the loop halts with a");
    println!("            'max_turns limit (2) exceeded' error event.");
    if error_count > 0 {
        let error_text = events
            .iter()
            .find_map(|e| match e {
                llm_harness_live_tests::CapturedEvent::Error(msg) => Some(msg.clone()),
                _ => None,
            })
            .unwrap_or_default();
        println!(
            "  Error detail: {}",
            error_text.chars().take(100).collect::<String>()
        );
    }
}

// ── Part 2: Repetition guard — repeated tool calls detected ─────────────────

async fn part2_repetition_guard(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 2: Repetition guard (max_repetitions = 2) ---\n");

    let config = LoopSafetyConfig {
        repetition_guard: Some(RepetitionGuardConfig {
            max_repetitions: 2,
            ..RepetitionGuardConfig::default()
        }),
        ..LoopSafetyConfig::default()
    };

    let (harness, _tmp) = make_harness_with(entry, |b| {
        b.install(&LoopSafetyPlugin::new(config))
            .tool(Arc::new(EchoTool) as Arc<dyn Tool>)
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    let prompt = "Call the echo tool 3 times with the same message 'repeat'. \
                  Wait for each result before calling again.";
    println!("LLM prompt: {}", prompt);

    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let events = cap.events();
    let echo_count = count_tool_calls(&events, "echo");

    println!("\nObservation:");
    println!("  echo ToolCallStart events: {}", echo_count);
    println!(
        "  Final text:                {}",
        cap.texts().chars().take(140).collect::<String>()
    );

    // The repetition guard fingerprints tool_name + canonical args. After
    // max_repetitions (2) identical calls, the 3rd is denied with a
    // ToolFailure("repetition_detected"). The LLM receives the failure
    // and typically reports it.
    println!("\n  Expected: the first 2 identical echo calls succeed; the 3rd is");
    println!("            denied with 'repetition_detected: tool=echo, count=3'.");
    if echo_count >= 3 {
        println!(
            "  NOTE: all {} calls appear as ToolCallStart (the LLM attempted them),",
            echo_count
        );
        println!("        but the 3rd execution is denied by the guard; the LLM");
        println!("        receives a ToolFailure and typically reports the denial.");
    }
}

// ── Part 3: Failure circuit breaker — consecutive failures ──────────────────

async fn part3_failure_circuit_breaker(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 3: Failure circuit breaker (max_consecutive_failures = 2) ---\n");

    let config = LoopSafetyConfig {
        failure_circuit_breaker: Some(FailureCircuitBreakerConfig {
            max_consecutive_failures: 2,
            ..FailureCircuitBreakerConfig::default()
        }),
        ..LoopSafetyConfig::default()
    };

    let (harness, _tmp) = make_harness_with(entry, |b| {
        b.install(&LoopSafetyPlugin::new(config))
            .tool(Arc::new(FailingTool) as Arc<dyn Tool>)
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    let prompt = "Use the failing_tool repeatedly with input 'test'. \
                  Keep calling it until you get a success or are told to stop.";
    println!("LLM prompt: {}", prompt);

    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let events = cap.events();
    let fail_count = count_tool_calls(&events, "failing_tool");

    println!("\nObservation:");
    println!("  failing_tool ToolCallStart events: {}", fail_count);
    println!(
        "  Final text:                        {}",
        cap.texts().chars().take(140).collect::<String>()
    );

    // The circuit breaker tracks per-tool consecutive failures. After
    // max_consecutive_failures (2) failures in a row, subsequent calls are
    // denied with ToolFailure("circuit_broken"). The should_stop hook also
    // halts the loop once the counter reaches the limit.
    println!("\n  Expected: the first 2 calls execute and fail; the 3rd call is");
    println!("            denied with 'circuit_broken: tool=failing_tool, failures=2'.");
    println!("            The loop then stops via the should_stop hook.");
    if fail_count >= 2 {
        println!(
            "  NOTE: {} call(s) attempted. The first 2 fail by design;",
            fail_count
        );
        println!("        the circuit breaker denies any further calls and halts the loop.");
    }
}

// ── Part 4: Death spiral guard — consecutive error turns ────────────────────

async fn part4_death_spiral_guard(entry: &llm_harness_live_tests::LiveProviderEntry) {
    println!("--- Part 4: Death spiral guard (max_error_turns = 2) ---\n");

    let config = LoopSafetyConfig {
        death_spiral_guard: Some(DeathSpiralGuardConfig { max_error_turns: 2 }),
        ..LoopSafetyConfig::default()
    };

    let (harness, _tmp) = make_harness_with(entry, |b| {
        b.install(&LoopSafetyPlugin::new(config))
            .tool(Arc::new(FailingTool) as Arc<dyn Tool>)
    })
    .await;

    let cap = EventCapture::subscribe(&harness);

    let prompt = "Use the failing_tool with input 'spiral'. \
                  If it fails, try again in the next turn. Keep trying.";
    println!("LLM prompt: {}", prompt);

    with_timeout(SINGLE_TURN, harness.prompt(prompt))
        .await
        .expect("prompt failed");
    cap.settle().await;

    let events = cap.events();
    let fail_count = count_tool_calls(&events, "failing_tool");

    println!("\nObservation:");
    println!("  failing_tool ToolCallStart events: {}", fail_count);
    println!(
        "  Final text:                        {}",
        cap.texts().chars().take(140).collect::<String>()
    );

    // The death spiral guard counts consecutive turns that produced any tool
    // failure (via PrepareNextTurnHook). Once the counter reaches
    // max_error_turns (2), should_stop halts the loop to avoid an
    // unproductive error spiral.
    println!("\n  Expected: after 2 consecutive error turns (each containing a failed");
    println!("            failing_tool call), the loop halts via should_stop.");
    if fail_count >= 2 {
        println!(
            "  NOTE: {} call(s) across error turns. The guard detected the",
            fail_count
        );
        println!("        error spiral and stopped the loop after 2 consecutive");
        println!("        error-producing turns.");
    }
}
