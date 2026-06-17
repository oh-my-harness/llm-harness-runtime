use std::time::Duration;
use std::{collections::HashSet, sync::Arc};

use llm_adapter::LlmError;
use llm_harness_types::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::convert::ConvertToLlmHook;

// ── RetryConfig ───────────────────────────────────────────────────────────────

/// Static metadata about an LLM model.
///
/// Runtime and harness layers use this for context-window and compaction
/// decisions without depending directly on the provider adapter crate.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// Provider name, for example `"deepseek"` or `"openai"`.
    pub provider: Option<String>,
    /// Provider API family, for example `"openai-compatible"`.
    pub api: Option<String>,
    /// Provider-specific model identifier.
    pub model_id: Option<String>,
    /// Total context window size in tokens.
    pub context_window: u32,
    /// Maximum output tokens supported by this model.
    pub max_tokens: u32,
}

/// Retry configuration for transient LLM provider errors.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (not counting the original).
    pub max_retries: u32,
    /// Base delay in ms for exponential backoff (doubles each attempt).
    pub base_delay_ms: u64,
}

impl RetryConfig {
    /// Creates a new `RetryConfig` with the given max retry count and base backoff delay.
    pub fn new(max_retries: u32, base_delay_ms: u64) -> Self {
        Self {
            max_retries,
            base_delay_ms,
        }
    }

    pub(crate) fn can_retry(&self, attempt: u32) -> bool {
        attempt < self.max_retries
    }

    pub(crate) fn delay_for(&self, attempt: u32, e: &LlmError) -> Duration {
        let hint = match e {
            LlmError::RateLimit { retry_after } | LlmError::Overloaded { retry_after } => {
                *retry_after
            }
            _ => None,
        };
        hint.unwrap_or_else(|| {
            Duration::from_millis(self.base_delay_ms.saturating_mul(1u64 << attempt.min(10)))
        })
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 2_000,
        }
    }
}

/// Returns `true` for errors that are safe to retry (server-side transient).
pub(crate) fn is_retryable(e: &LlmError) -> bool {
    matches!(
        e,
        LlmError::RateLimit { .. } | LlmError::Overloaded { .. } | LlmError::Timeout
    )
}

/// Agent loop 的配置。`agent_loop` 会消费此结构（所有权转移）。
pub struct LoopConfig {
    // === LLM sampling ===
    /// 模型 ID（例如: `"claude-sonnet-4-6"`）。
    pub model: String,
    /// 最大生成 token 数。
    pub max_tokens: u32,
    /// 采样温度。
    pub temperature: Option<f32>,
    /// 推理深度级别（目前仅保存，adapter 映射未实现）。
    pub thinking_level: ThinkingLevel,

    // === Tools ===
    /// 活跃工具列表（通常接收已包装的 `HookedTool`）。
    pub tools: Vec<Arc<dyn Tool>>,
    /// Optional active-tool subset. `None` means all registered tools are available.
    pub active_tools: Option<HashSet<String>>,
    /// 工具的默认执行模式。
    pub default_execution_mode: ToolExecutionMode,

    // === Execution environment ===
    /// 工具执行时使用的执行环境。
    pub env: Arc<dyn ExecutionEnv>,
    /// 中止信号（用于 `ToolContext` 和 loop 的中止检查）。
    pub abort: CancellationToken,

    // === Stream options ===
    /// 传输层配置。
    ///
    /// 当前 loop 本地执行 `timeout_ms`、`max_retries` 和 `max_retry_delay_ms`。
    /// `headers`、`metadata` 和 `cache_config` 需要 provider adapter 暴露相应入口后
    /// 才能透传。
    pub stream_options: StreamOptions,

    // === Required hooks ===
    /// `AgentMessage` → adapter `Message` 转换（必需）。
    pub convert_to_llm: Arc<dyn ConvertToLlmHook>,

    // === Optional hooks ===
    /// LLM 调用前的上下文转换（例如 compaction）。
    pub transform_context: Option<Arc<dyn TransformContextHook>>,
    /// 每个转轮后决定下一转轮的配置。
    pub prepare_next_turn: Option<Arc<dyn PrepareNextTurnHook>>,
    /// 当 LLM 自然停止时，判断是否继续循环。
    pub should_stop: Option<Arc<dyn ShouldStopHook>>,
    /// 在 provider 调用前可编辑 `StreamOptions`。
    pub before_provider_request: Option<Arc<dyn BeforeProviderRequestHook>>,
    /// provider 响应后的观测钩子。
    pub after_provider_response: Option<Arc<dyn AfterProviderResponseHook>>,
    /// 动态身份验证（目前不直接传递给 adapter，但为将来保留）。
    pub auth: Option<Arc<dyn AuthHook>>,

    // === Reactive injection ===
    /// 在转轮间注入的消息（转向）。
    pub steer_rx: Option<mpsc::Receiver<AgentMessage>>,
    /// loop 自然停止后逐个处理的后续消息。
    pub follow_up_rx: Option<mpsc::Receiver<AgentMessage>>,

    // === Retry ===
    /// Retry config for transient provider errors; `None` disables retry.
    pub retry: Option<RetryConfig>,
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "test-utils")]
    fn loop_config_compiles() {
        use crate::LoopConfig;
        use crate::convert::DefaultConvertToLlm;
        use crate::test_utils::NoOpEnv;
        use llm_harness_types::{StreamOptions, ThinkingLevel, ToolExecutionMode};
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;
        let _cfg = LoopConfig {
            model: "test-model".into(),
            max_tokens: 1024,
            temperature: None,
            thinking_level: ThinkingLevel::Off,
            tools: vec![],
            active_tools: None,
            default_execution_mode: ToolExecutionMode::Parallel,
            env: Arc::new(NoOpEnv),
            abort: CancellationToken::new(),
            stream_options: StreamOptions::default(),
            convert_to_llm: Arc::new(DefaultConvertToLlm::new()),
            transform_context: None,
            prepare_next_turn: None,
            should_stop: None,
            before_provider_request: None,
            after_provider_response: None,
            auth: None,
            steer_rx: None,
            follow_up_rx: None,
            retry: None,
        };
    }
}
