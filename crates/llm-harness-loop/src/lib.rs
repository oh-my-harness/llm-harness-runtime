//! Advanced streaming loop API for `llm-harness-core`.
//!
//! This crate owns the stateless LLM loop: converting harness messages to
//! provider requests, consuming streaming responses, dispatching tool calls, and
//! emitting `AgentEvent` values.
//!
//! Most SDK users should start with `llm_harness_agent::Agent` or
//! `llm_harness_agent::AgentHarness`. Use this crate directly when building a custom
//! runtime, testing loop behavior, or integrating at the framework layer.

pub mod composite;
pub mod config;
pub mod convert;
pub(crate) mod dispatch;
pub mod final_answer;
pub mod hooked_tool;
pub mod loop_fn;
pub(crate) mod stream_state;

mod type_bridge;

#[cfg(feature = "test-utils")]
pub mod test_utils;

// Top-level re-exports
pub use composite::{
    CompositeAfterProviderResponseHook, CompositeAfterToolCallHook, CompositeAfterTurnHook,
    CompositeBeforeCompactHook, CompositeBeforeProviderRequestHook, CompositeBeforeRunHook,
    CompositeBeforeToolCallHook, CompositeBeforeTurnHook, CompositePrepareNextTurnHook,
    CompositeShouldStopHook, CompositeTransformContextHook,
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
pub use llm_adapter::openai::OpenAIProvider;
pub use llm_adapter::provider::Provider as LlmClient;
pub use llm_adapter::provider::ProviderCapabilities;
pub use llm_adapter::types::ChatRequest;
pub use llm_adapter::types::ChatResponse;
pub use llm_adapter::types::Message as LlmMessage;
pub use llm_adapter::types::RequestContent;
pub use llm_adapter::types::StreamEvent as AdapterStreamEvent;

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
