//! wasm32 compile gate for `llm-harness-loop` (issue #180).
//!
//! Exercises the surface senza-wasm consumes: `agent_loop` bound through the
//! `MockLlmClient` path (test-utils feature). Compile-only acceptance:
//! `cargo check --target wasm32-unknown-unknown` must pass with no `Send`
//! bound errors. Runtime wiring (wasm-bindgen / JS event loop) belongs to
//! the senza-wasm repo — tokio has no time driver on wasm32-unknown-unknown
//! (`Instant::now` panics), so `run_loop` cannot complete a turn there until
//! senza-wasm supplies a time-substitution layer.

use std::sync::Arc;

use llm_harness_loop::loop_fn::agent_loop;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse, NoOpEnv};
use llm_harness_loop::{DefaultConvertToLlm, LoopConfig};
use llm_harness_types::{AgentContext, RunContext, RunRequest, StreamOptions};
use tokio_util::sync::CancellationToken;


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

