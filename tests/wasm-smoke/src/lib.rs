//! wasm32 gate for `llm-harness-loop` (issue #180): compile + runtime.
//!
//! Exercises the surface senza-wasm consumes: `agent_loop` driven by the
//! `MockLlmClient` path (test-utils feature). Acceptance:
//! 1. `cargo check --target wasm32-unknown-unknown` passes with no `Send`
//!    bound errors (compile gate)
//! 2. the exported `run_mock_turn_wasm` completes a full agent turn under
//!    wasm-bindgen + Node (runtime gate) — this exercises the crate::time
//!    bridge (futures-timer / gloo-timers) that replaces tokio's missing
//!    wasm time driver, including the idle-watchdog re-arm path.

use std::sync::Arc;

use llm_harness_loop::loop_fn::agent_loop;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse, NoOpEnv};
use llm_harness_loop::{DefaultConvertToLlm, LoopConfig};
use llm_harness_types::{AgentContext, RunContext, RunRequest, StreamOptions};
use tokio_util::sync::CancellationToken;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

fn mock_config() -> LoopConfig {
    LoopConfig {
        run: Arc::new(RunContext::new(RunRequest::default())),
        model: "mock-model".into(),
        max_tokens: 256,
        temperature: None,
        thinking_level: llm_harness_types::ThinkingLevel::Off,
        tools: vec![],
        active_tools: None,
        default_execution_mode: llm_harness_types::ToolExecutionMode::Parallel,
        final_answer_mode: llm_harness_loop::FinalAnswerMode::default(),
        env: Arc::new(NoOpEnv),
        abort: CancellationToken::new(),
        stream_options: StreamOptions::default(),
        convert_to_llm: Arc::new(DefaultConvertToLlm::new()),
        transform_context: None,
        prepare_next_turn: None,
        should_stop: None,
        before_provider_request: None,
        after_provider_response: None,
        final_answer_validator: None,
        provider_error: None,
        max_provider_error_retries: None,
        steer_rx: None,
        follow_up_rx: None,
        retry: None,
        response_format: None,
    }
}

/// Bind one `agent_loop` against `MockLlmClient` without driving it.
///
/// Native build asserts the stream itself is `Send` (regression guard for the
/// cfg-forked signature); wasm build asserts the signature binds WITHOUT a
/// `Send` bound.
fn run_mock_turn(client: Arc<MockLlmClient>, ctx: AgentContext, config: LoopConfig) {
    let stream = agent_loop(client, ctx, config);
    #[cfg(not(target_arch = "wasm32"))]
    {
        fn assert_send<T: Send>(_: T) {}
        assert_send(stream);
    }
    #[cfg(target_arch = "wasm32")]
    let _ = stream;
}

#[allow(dead_code)]
fn build_deps() {
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
    let ctx = AgentContext {
        system_prompt: None,
        messages: vec![],
    };
    run_mock_turn(client, ctx, mock_config());
}

/// Runtime gate: drive one full agent turn on wasm (gloo-timers clock).
///
/// `stream_idle_timeout_ms = Some(5000)` arms the idle watchdog so the
/// hoisted-sleep + `reset_for` path runs against the futures-timer bridge,
/// not just the plain `sleep` path.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub async fn run_mock_turn_wasm() -> Result<(), wasm_bindgen::JsValue> {
    let stream_opts = StreamOptions {
        stream_idle_timeout_ms: Some(5000),
        ..StreamOptions::default()
    };
    let mut config = mock_config();
    config.stream_options = stream_opts;
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
    let ctx = AgentContext {
        system_prompt: None,
        messages: vec![],
    };
    let stream = agent_loop(client, ctx, config);
    futures::pin_mut!(stream);
    use futures::StreamExt;
    let mut events = 0;
    while let Some(event) = stream.next().await {
        if matches!(event, llm_harness_types::AgentEvent::Error(_)) {
            return Err(
                js_sys::Error::new("agent_loop emitted AgentEvent::Error").into(),
            );
        }
        events += 1;
    }
    if events == 0 {
        return Err(js_sys::Error::new("agent_loop emitted no events").into());
    }
    Ok(())
}
