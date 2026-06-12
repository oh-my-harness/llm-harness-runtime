use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{ExecutionEnv, TokenUsage};

use crate::error::Result;
use crate::sandbox::ResourceLimits;

/// Configuration for spawning a sub-agent.
#[derive(Debug, Clone)]
pub struct SubAgentSpawnOpts {
    /// Initial prompt for the sub-agent.
    pub prompt: String,
    /// Model ID override. `None` means inherit from parent configuration.
    pub model: Option<String>,
    /// Tool subset by name from the registry. `None` means no tools.
    pub tools: Option<Vec<String>>,
    /// Maximum number of turns. `None` means no limit.
    pub max_turns: Option<usize>,
    /// Wall-clock timeout. `None` means no limit.
    pub timeout: Option<std::time::Duration>,
    /// Resource limits independent of parent agent.
    pub resource_limits: Option<ResourceLimits>,
}

/// Result of a sub-agent execution.
#[derive(Debug, Clone)]
pub struct SubAgentResult {
    /// 1–2K token text summary — the only information channel from
    /// sub-agent back to the parent agent.
    pub summary: String,
    /// Token usage of the sub-agent session.
    pub token_usage: Option<TokenUsage>,
    /// Whether the sub-agent was truncated due to `max_turns`/timeout/resource_limits.
    pub truncated: bool,
}

/// Sub-agent spawning extension point.
///
/// ## Isolation guarantees
/// - **Context-independent**: sub-agent starts with a fresh empty context window.
/// - **Tools-independent**: tools are reassembled from registry by name; parent's
///   tool set is not automatically inherited.
/// - **Resources-independent**: `resource_limits` is independently configured.
/// - **Information one-way**: sub-agent returns only `SubAgentResult.summary`.
///
/// v0.2 default: `None` (sub-agent spawning is not enabled by default).
pub trait SubAgentSpawner: Send + Sync {
    /// Spawn a sub-agent and await its completion.
    fn spawn(
        &self,
        opts: SubAgentSpawnOpts,
        env: Arc<dyn ExecutionEnv>,
    ) -> BoxFuture<'_, Result<SubAgentResult>>;
}
