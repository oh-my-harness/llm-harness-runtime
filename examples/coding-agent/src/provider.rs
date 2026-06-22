//! LLM provider resolution for coding-agent.
//!
//! Builds an [`AnthropicProvider`] or [`OpenAIProvider`] based on environment
//! variables, and resolves the effective model name.

use std::sync::Arc;

use llm_adapter::anthropic::AnthropicProvider;
use llm_adapter::openai::OpenAIProvider;
use llm_harness_loop::LlmClient;

const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// A resolved provider along with the model name to use.
pub struct ResolvedProvider {
    pub client: Arc<dyn LlmClient>,
    pub model: String,
}

/// Build a provider from environment variables.
///
/// Resolution order:
/// 1. `OPENAI_API_KEY` + `OPENAI_API_BASE` → OpenAI-compatible provider
/// 2. `ANTHROPIC_API_KEY` → Anthropic provider
/// 3. Neither → error message printed to stderr, returns `None`
pub fn resolve_provider() -> Option<ResolvedProvider> {
    let openai_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    let openai_base = std::env::var("OPENAI_API_BASE")
        .ok()
        .filter(|s| !s.is_empty());

    let (client, default_model): (Arc<dyn LlmClient>, String) =
        if let (Some(key), Some(base)) = (openai_key, openai_base) {
            let base_trimmed = base.trim_end_matches('/').to_string();
            let (base_url, chat_path) = if base_trimmed.ends_with("/v1") {
                (base_trimmed, "/chat/completions".to_string())
            } else {
                (base_trimmed, "/v1/chat/completions".to_string())
            };
            let provider = OpenAIProvider::builder(key)
                .base_url(base_url)
                .chat_path(chat_path)
                .build();
            let model = std::env::var("OPENAI_MODEL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "gpt-4o".to_string());
            (Arc::new(provider), model)
        } else {
            let api_key = match std::env::var("ANTHROPIC_API_KEY") {
                Ok(k) if !k.is_empty() => k,
                _ => {
                    eprintln!("Error: set ANTHROPIC_API_KEY or OPENAI_API_KEY + OPENAI_API_BASE");
                    return None;
                }
            };

            let anthropic_base = std::env::var("ANTHROPIC_API_BASE")
                .ok()
                .filter(|s| !s.is_empty());
            let anthropic_model = std::env::var("ANTHROPIC_MODEL")
                .ok()
                .filter(|s| !s.is_empty());

            let mut builder = AnthropicProvider::builder(api_key);
            if let Some(base) = anthropic_base {
                builder = builder.base_url(base);
            }
            let provider = builder.build();

            let model = anthropic_model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
            (Arc::new(provider), model)
        };

    let model = std::env::var("CODING_AGENT_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or(default_model);

    Some(ResolvedProvider { client, model })
}
