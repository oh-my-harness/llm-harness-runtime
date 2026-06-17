//! Advanced streaming loop API for `llm-harness-core`.
//!
//! This crate owns the stateless LLM loop: converting harness messages to
//! provider requests, consuming streaming responses, dispatching tool calls, and
//! emitting `AgentEvent` values.
//!
//! Most SDK users should start with `llm_harness_agent::Agent` or
//! `llm_harness_agent::AgentHarness`. Use this crate directly when building a custom
//! runtime, testing loop behavior, or integrating at the framework layer.

pub mod config;
pub mod convert;
pub(crate) mod dispatch;
pub mod hooked_tool;
pub mod loop_fn;
pub(crate) mod stream_state;

mod type_bridge;

#[cfg(feature = "test-utils")]
pub mod test_utils;

// Top-level re-exports
pub use config::{LoopConfig, ModelInfo, RetryConfig};
pub use convert::{ConvertToLlmHook, CustomMessageConverter, DefaultConvertToLlm};
pub use hooked_tool::HookedTool;
pub use loop_fn::{agent_loop, agent_loop_continue};

// Re-exports for downstream crates (harness must not depend directly on llm_adapter)
pub use llm_adapter::LlmError;
pub use llm_adapter::provider::Provider as LlmClient;
pub use llm_adapter::types::ChatRequest;
pub use llm_adapter::types::ChatResponse;
pub use llm_adapter::types::Message as LlmMessage;
pub use llm_adapter::types::RequestContent;
pub use llm_adapter::types::StreamEvent as AdapterStreamEvent;
