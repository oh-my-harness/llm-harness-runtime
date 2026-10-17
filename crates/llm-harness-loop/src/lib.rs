//! # llm-harness-loop — Agent Loop Configuration Guide
//!
//! This crate owns the stateless LLM loop: converting harness messages to
//! provider requests, consuming streaming responses, dispatching tool calls,
//! and emitting `AgentEvent` values.
//!
//! Most SDK users should start with `llm_harness_agent::Agent` or
//! `llm_harness_agent::AgentHarness`. Use this crate directly when building a
//! custom runtime, testing loop behavior, or integrating at the framework layer.
//!
//! ## Quick Start
//!
//! ```no_run
//! use std::sync::Arc;
//! use llm_harness_loop::{
//!     agent_loop, LoopConfig, DefaultConvertToLlm, FinalAnswerMode, openai_compatible,
//! };
//! use llm_harness_types::*;
//!
//! let client = Arc::new(openai_compatible("sk-...", "https://api.openai.com"));
//! let run = Arc::new(RunContext::new(RunRequest::from_text("What is 2+2?")));
//! let config = LoopConfig {
//!     run,
//!     model: "gpt-4o".into(),
//!     max_tokens: 1024,
//!     temperature: None,
//!     thinking_level: ThinkingLevel::Off,
//!     tools: vec![],
//!     active_tools: None,
//!     default_execution_mode: ToolExecutionMode::Parallel,
//!     final_answer_mode: FinalAnswerMode::default(),
//!     env: Arc::new(UnsupportedEnv::new()),
//!     abort: tokio_util::sync::CancellationToken::new(),
//!     stream_options: StreamOptions::default(),
//!     convert_to_llm: Arc::new(DefaultConvertToLlm::new()),
//!     transform_context: None,
//!     prepare_next_turn: None,
//!     should_stop: None,
//!     before_provider_request: None,
//!     after_provider_response: None,
//!     final_answer_validator: None,
//!     provider_error: None,
//!     steer_rx: None,
//!     follow_up_rx: None,
//!     retry: None,
//!     response_format: None,
//! };
//! let ctx = AgentContext {
//!     system_prompt: None,
//!     messages: vec![AgentMessage::User(UserMessage {
//!         content: vec![ContentBlock::Text { text: "What is 2+2?".into() }],
//!         timestamp: chrono::Utc::now(),
//!     })],
//! };
//! // let stream = agent_loop(client, ctx, config);
//! // while let Some(event) = stream.next().await { /* handle AgentEvent */ }
//! ```
//!
//! ## LoopConfig Field Reference
//!
//! ### Required fields
//!
//! | Field | Type | Notes |
//! |-------|------|-------|
//! | `run` | `Arc<RunContext>` | Immutable run context; carries run ID, prompt, metadata. |
//! | `model` | `String` | Model ID, e.g. `"claude-sonnet-4-6"`, `"gpt-4o"`. |
//! | `max_tokens` | `u32` | Max generation tokens per turn. |
//! | `tools` | `Vec<Arc<dyn Tool>>` | Tool list; usually wrapped in `HookedTool` if hooks are used. |
//! | `env` | `Arc<dyn ExecutionEnv>` | Execution environment for tool calls. |
//! | `abort` | `CancellationToken` | Cancellation signal; checked by loop and passed to `ToolContext`. |
//! | `convert_to_llm` | `Arc<dyn ConvertToLlmHook>` | Converts `AgentMessage` → adapter `Message`. Use `DefaultConvertToLlm` unless you need custom message types. |
//!
//! ### Sampling
//!
//! | Field | Type | Default | Notes |
//! |-------|------|---------|-------|
//! | `thinking_level` | `ThinkingLevel` | `Off` | Reasoning depth. `Off` = disabled, `Minimal`→`XHigh` = increasing effort, `Budget(n)` = explicit token budget (Anthropic 4.6 only). Each provider translates to its own wire format. |
//! | `temperature` | `Option<f32>` | `None` | `None` = provider default. |
//! | `response_format` | `Option<ResponseFormat>` | `None` | Set to `JsonObject` for structured output. |
//!
//! ### Tool configuration
//!
//! | Field | Type | Default | Notes |
//! |-------|------|---------|-------|
//! | `active_tools` | `Option<HashSet<String>>` | `None` (all) | Subset of `tools` available this run. `None` = all registered tools. |
//! | `default_execution_mode` | `ToolExecutionMode` | `Parallel` | `Parallel` = concurrent `join_all`; `Sequential` = sub-group barrier. |
//! | `final_answer_mode` | `FinalAnswerMode` | `Heuristic` | See [Final Answer Modes](#final-answer-modes) below. |
//!
//! ### Stream options (`stream_options: StreamOptions`)
//!
//! | Field | Default | Notes |
//! |-------|---------|-------|
//! | `timeout_ms` | `None` | Overall request timeout. `None` = no timeout. |
//! | `stream_idle_timeout_ms` | `None` | Per-event idle watchdog. If no `StreamEvent` arrives within this window, the stream is dropped and retried. A partial `MessageEnd` is emitted before retry to close any open message. |
//! | `max_retries` | `None` | Overrides `RetryConfig.max_retries` if set. |
//! | `max_retry_delay_ms` | `None` | Caps retry delay. |
//! | `headers` / `metadata` / `cache_config` | empty | Require provider adapter support to pass through. |
//!
//! ### Retry (`retry: Option<RetryConfig>`)
//!
//! `None` disables retry. When set, the loop retries on transient provider
//! errors (5xx, connection, timeout) with exponential backoff (`base_delay_ms`
//! doubled per attempt). `stream_opts.max_retries` overrides
//! `RetryConfig.max_retries` at runtime.
//!
//! ### Provider error repair (`provider_error: Option<Arc<dyn ProviderErrorHook>>`)
//!
//! `None` (default) = non-transient provider errors surface unchanged, exactly
//! as before this hook existed. When set, the hook runs after transient-retry
//! is exhausted (or the error was never retryable, e.g. `InvalidRequest`) and
//! may repair history in place (both `ctx.context` and `ctx.new_messages`,
//! which are disjoint `&mut` views) before returning `Retry` (same-turn retry,
//! `turn_index` unchanged) or `Surface` (error propagates). See
//! `llm-harness-strategy`'s `VisionDegradeHook` for the canonical use.
//!
//! ### Reactive injection
//!
//! | Field | Type | Notes |
//! |-------|------|-------|
//! | `steer_rx` | `Option<Receiver<AgentMessage>>` | Injects messages between turns to redirect the agent mid-run. Drained at turn boundaries and at natural stop (leftovers run one more turn instead of being lost). |
//! | `follow_up_rx` | `Option<Receiver<AgentMessage>>` | Queued messages processed after the loop naturally stops. Each triggers a new run. |
//!
//! ## Final Answer Modes
//!
//! `final_answer_mode` controls how the loop decides a turn is the final
//! user-visible answer vs. intermediate progress.
//!
//! | Mode | Plain text ends | `final_answer` tool | `terminal_text_classification` |
//! |------|----------------|---------------------|-------------------------------|
//! | `Heuristic` (default) | `FinalAnswer` | Not registered | `FinalAnswer` |
//! | `Tool(Error)` | Error: "was required but not called" | Registered; calling it produces `FinalAnswer` | `Progress` |
//! | `Tool(Fallback)` | `FinalAnswer` (heuristic fallback) | Registered; calling it produces `FinalAnswer` | `FinalAnswer` |
//!
//! **When to use which:**
//!
//! - **`Heuristic`** — Simple chatbots, single-turn Q&A. Any non-tool stop is the answer.
//! - **`Tool(Error)`** — Multi-step agents where you need a hard guarantee: the
//!   model must explicitly call `final_answer` to end the run. Prevents the
//!   model from stopping prematurely and presenting intermediate text as final.
//! - **`Tool(Fallback)`** — Transition mode: the tool is available but plain
//!   text still works as a fallback. Useful when migrating from `Heuristic` to
//!   `Tool(Error)` incrementally.
//!
//! Construction helpers:
//!
//! ```ignore
//! use llm_harness_loop::FinalAnswerMode;
//!
//! FinalAnswerMode::default();                   // Heuristic
//! FinalAnswerMode::required_tool();             // Tool(Error)
//! FinalAnswerMode::tool_with_text_fallback();   // Tool(Fallback)
//! ```
//!
//! ## Hook System
//!
//! Hooks are optional trait objects inserted into the loop at well-defined
//! points. All hooks are `Send + Sync` and return `BoxFuture`.
//!
//! ### Hook call order (per turn)
//!
//! ```text
//! ┌─ BeforeTurnHook (notification, no return value)
//! ├─ TransformContextHook (can replace entire AgentContext)
//! ├─ BeforeProviderRequestHook (can modify StreamOptions in-place)
//! ├─ [LLM streaming — TextDelta / ToolCallDelta / ThinkingDelta / ...]
//! ├─ AfterProviderResponseHook (observation: status, usage, latency)
//! ├─ [If FinalAnswer] → FinalAnswerValidator → MessageEnd → break
//! ├─ [If ToolUse] → BeforeToolCallHook → execute → AfterToolCallHook
//! ├─ PrepareNextTurnHook (can override model, temperature, tools, etc.)
//! ├─ AfterTurnHook (notification, no return value)
//! └─ ShouldStopHook (only on non-tool natural stop; return true to stop)
//! ```
//!
//! Provider errors that survive transient retry invoke
//! `ProviderErrorHook` (before surfacing the terminal `Error` event):
//!
//! ```text
//! ┌─ [LLM call fails with non-transient error]
//! ├─ ProviderErrorHook (repair history; Retry = same-turn retry, Surface = propagate)
//! └─ [If Surface or unmounted] → AgentEvent::Error → AgentEnd
//! ```
//!
//! `BeforeRunHook` fires once before the first turn (can inject messages and
//! override system prompt). `BeforeCompactHook` fires when context compaction
//! is triggered (harness layer, not bare loop).
//!
//! ### Observation vs. decision hooks
//!
//! | Hook | Type | Can modify? |
//! |------|------|-------------|
//! | `BeforeRunHook` | Decision | Inject messages, override system prompt |
//! | `BeforeTurnHook` | Observation | No |
//! | `TransformContextHook` | Decision | Replace `AgentContext` entirely |
//! | `BeforeProviderRequestHook` | Decision | Mutate `StreamOptions` in-place |
//! | `AfterProviderResponseHook` | Observation | No (cost tracking, logging) |
//! | `BeforeToolCallHook` | Decision | `Allow` / `Modify(args)` / `Deny(failure)` |
//! | `AfterToolCallHook` | Decision | `Passthrough` / `Replace(result)` |
//! | `PrepareNextTurnHook` | Decision | Override model, temperature, tools, thinking, response_format |
//! | `AfterTurnHook` | Observation | No |
//! | `ShouldStopHook` | Decision | `true` = stop, `false` = force another turn |
//! | `FinalAnswerValidator` | Decision | `Ok` = commit, `Err` = reject (loop breaks) |
//! | `BeforeCompactHook` | Decision | `Proceed` / `Skip` / `Compact` / `Override(result)` |
//! | `ProviderErrorHook` | Decision | Repair `AgentContext` in place; `Retry` / `Surface` |
//!
//! ### Composite hooks
//!
//! When multiple hooks of the same type are needed, wrap them in the
//! corresponding `Composite*` type (e.g. `CompositeBeforeToolCallHook`).
//! Composites run hooks in order; decision hooks use "last wins" semantics
//! (each hook sees the previous hook's decision).
//!
//! ## Dynamic Configuration via PrepareNextTurnHook
//!
//! `PrepareNextTurnHook` returns a `NextTurnDirective` that can override
//! per-turn settings. Fields use double-`Option` where meaningful:
//!
//! | Field | `None` | `Some(None)` | `Some(Some(value))` |
//! |-------|--------|---------------|---------------------|
//! | `temperature` | Keep current | Clear (use provider default) | Set to `value` |
//! | `response_format` | Keep current | Clear (no format) | Set to `value` |
//! | `model` | Keep current | N/A | Switch to `value` |
//! | `thinking_level` | Keep current | N/A | Set to `value` |
//! | `tools` | Keep current | N/A | Replace entire tool list |
//! | `active_tools` | Keep current | N/A | Filter to this subset |
//! | `context` | Keep current | N/A | Replace entire `AgentContext` |
//!
//! Common use cases:
//!
//! - **Model switching** — Use a cheaper model for follow-up turns after an
//!   expensive reasoning model does the heavy lifting.
//! - **Tool phasing** — Restrict tools in early turns (planning), expand in
//!   later turns (execution).
//! - **Temperature ramp** — Start with `temperature = 0` for deterministic
//!   planning, increase for creative execution.
//!
//! ## Common Recipes
//!
//! ### Minimal chat loop (no tools)
//!
//! No tools, heuristic final answer, no hooks. The loop runs one turn and
//! stops on `EndTurn`.
//!
//! ### Agent with tools + safety
//!
//! Wrap each tool in `HookedTool` with a `BeforeToolCallHook` that validates
//! arguments and checks permissions (`Allow` / `Deny`). Use
//! `FinalAnswerMode::required_tool()` to force explicit completion. Add an
//! `AfterProviderResponseHook` for cost tracking.
//!
//! ### Long conversation with compaction
//!
//! Use `TransformContextHook` to check context size and compact when it
//! exceeds a threshold. The hook returns a new `AgentContext` with a
//! summarized message list. (The `AgentHarness` layer provides a built-in
//! compaction implementation with circuit breaker and context-aware prompts.)
//!
//! ### Structured output
//!
//! Set `response_format = Some(ResponseFormat::JsonObject)` and
//! `final_answer_mode = FinalAnswerMode::default()`. The model's plain-text
//! response is treated as the final answer and must be valid JSON.
//!
//! ### Truncation continuation
//!
//! When `stop_reason == MaxTokens`, the loop normally stops. Implement
//! `ShouldStopHook` returning `false` to force another turn, letting the
//! model continue from where it was cut off.

pub mod composite;
pub mod config;
pub mod convert;
pub(crate) mod dispatch;
pub mod final_answer;
pub mod hooked_tool;
pub mod loop_fn;
pub(crate) mod stream_state;
pub(crate) mod time;

mod type_bridge;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

// Top-level re-exports
pub use composite::{
    CompositeAfterProviderResponseHook, CompositeAfterRunHook, CompositeAfterToolCallHook,
    CompositeAfterTurnHook, CompositeBeforeProviderRequestHook, CompositeBeforeRunHook,
    CompositeBeforeToolCallHook, CompositeBeforeTurnHook, CompositeFinalAnswerValidator,
    CompositePrepareNextTurnHook, CompositeProviderErrorHook, CompositeShouldStopHook,
    CompositeTransformContextHook,
};
pub use config::{LoopConfig, RetryConfig};
pub use convert::{ConvertToLlmHook, CustomMessageConverter, DefaultConvertToLlm};
pub use final_answer::{
    DEFAULT_FINAL_ANSWER_TOOL_NAME, FinalAnswerMissingBehavior, FinalAnswerMode,
    FinalAnswerToolConfig,
};
pub use hooked_tool::HookedTool;
pub use loop_fn::{agent_loop, agent_loop_continue};

// Re-exports for downstream crates (downstream must not depend directly on
// llm_adapter). Includes the `Provider` trait, request/response types, and the
// concrete provider implementations so downstream crates can construct
// Anthropic / OpenAI-compatible clients without a direct llm_adapter dep.
pub use llm_adapter::LlmError;
pub use llm_adapter::StreamHandle;
pub use llm_adapter::anthropic::AnthropicProvider;
pub use llm_adapter::openai::{MaxTokensScheme, OpenAIProvider, ThinkingScheme};
pub use llm_adapter::provider::Provider as LlmClient;
pub use llm_adapter::provider::ProviderCapabilities;
pub use llm_adapter::types::ChatRequest;
pub use llm_adapter::types::ChatResponse;
pub use llm_adapter::types::DocumentSource as AdapterDocumentSource;
pub use llm_adapter::types::Message as LlmMessage;
pub use llm_adapter::types::RequestContent;
pub use llm_adapter::types::ResponseFormat;
pub use llm_adapter::types::StreamEvent as AdapterStreamEvent;
/// Factory for an OpenAI-compatible provider with reasoning parsing enabled.
///
/// Use this instead of `OpenAIProvider::builder(api_key).build()` when
/// connecting to OpenAI-compatible endpoints that emit `reasoning` /
/// `reasoning_content` fields — e.g. vLLM-hosted models (GLM, Qwen),
/// DeepSeek, or any endpoint whose streaming responses contain a
/// `delta.reasoning` field.
///
/// Enables:
/// - `parse_reasoning_content(true)` — parse `delta.reasoning` into reasoning blocks
/// - `tolerant_keepalive(true)` — tolerate keepalive bytes in SSE streams
/// - `documents(true)` — gateways commonly front vision/document models (GLM etc.)
///
/// For the official OpenAI API (`api.openai.com`), `OpenAIProvider::builder()`
/// is sufficient — reasoning models (o-series, GPT-5) use `reasoning_effort`,
/// not `reasoning_content`, and no keepalive is sent.
///
/// # Example
///
/// ```
/// # use llm_harness_loop::openai_compatible;
/// let client = openai_compatible("sk-your-key", "https://api.hyper-op.com");
/// ```
pub fn openai_compatible(
    api_key: impl Into<String>,
    base_url: impl Into<String>,
) -> OpenAIProvider {
    OpenAIProvider::builder(api_key)
        .base_url(base_url)
        .parse_reasoning_content(true)
        .tolerant_keepalive(true)
        .documents(true)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Downstream crates must be able to construct providers and implement the
    /// `Provider` trait via these re-exports alone, without a direct
    /// `llm_adapter` dependency. Guards the public re-export surface.
    #[test]
    fn provider_surface_is_re_exported() {
        // Concrete provider constructors.
        let anthropic = AnthropicProvider::builder("test-key").build();
        let openai = OpenAIProvider::builder("test-key").build();

        // `Provider` trait method return types reachable via re-exports.
        let _: ProviderCapabilities = anthropic.capabilities();
        let _: ProviderCapabilities = openai.capabilities();
        // `StreamHandle` is named in the `Provider` trait signature; reference
        // the type to ensure the re-export resolves.
        let _ = std::marker::PhantomData::<StreamHandle>;
    }
}
