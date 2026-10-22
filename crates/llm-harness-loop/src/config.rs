use std::time::Duration;
use std::{collections::HashSet, sync::Arc};

use llm_adapter::LlmError;
use llm_harness_types::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::convert::ConvertToLlmHook;
use crate::final_answer::FinalAnswerMode;

/// Retry configuration for explicit rate-limit rejections (HTTP 429) — the
/// only error class safe to replay without re-running accepted work.
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
        hint.unwrap_or_else(|| self.delay_for_attempt(attempt))
    }

    /// Full-jitter exponential backoff for callers that only have an
    /// `AgentError` (e.g. a mid-stream transport drop).
    pub(crate) fn delay_for_attempt(&self, attempt: u32) -> Duration {
        // Full jitter: delay = random(0, base * 2^attempt]
        let exp = self.base_delay_ms.saturating_mul(1u64 << attempt.min(10));
        let jittered = fastrand::u64(0..=exp);
        Duration::from_millis(jittered)
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

/// Returns `true` for errors that are safe to replay the generation for.
/// Only an explicit `RateLimit` (HTTP 429) qualifies: the provider rejected
/// the request without running it. Connection/timeout/5xx/stream failures are
/// deliberately excluded — the request may have been accepted upstream, so
/// resending it could duplicate a generation or tool side effects.
pub(crate) fn is_retryable(e: &LlmError) -> bool {
    matches!(e, LlmError::RateLimit { .. })
}

/// Cap a retry delay to `max_retry_delay_ms` if set.
pub(crate) fn cap_delay(delay: Duration, max_retry_delay_ms: Option<u64>) -> Duration {
    match max_retry_delay_ms {
        Some(max_ms) => delay.min(Duration::from_millis(max_ms)),
        None => delay,
    }
}

/// Agent loop 的配置。`agent_loop` 会消费此结构（所有权转移）。
pub struct LoopConfig {
    // === Run context ===
    /// 当前 loop 所属的不可变 run 上下文。
    pub run: Arc<RunContext>,

    // === LLM sampling ===
    /// 模型 ID（例如: `"claude-sonnet-4-6"`）。
    pub model: String,
    /// 最大生成 token 数。
    pub max_tokens: u32,
    /// 采样温度。
    pub temperature: Option<f32>,
    /// 推理深度级别；通过 canonical `thinking` 字段传递，由各 provider converter 翻译。
    pub thinking_level: ThinkingLevel,

    // === Tools ===
    /// 活跃工具列表（通常接收已包装的 `HookedTool`）。
    pub tools: Vec<Arc<dyn Tool>>,
    /// Optional active-tool subset. `None` means all registered tools are available.
    pub active_tools: Option<HashSet<String>>,
    /// 工具的默认执行模式。
    pub default_execution_mode: ToolExecutionMode,
    /// Final-answer classification mode. Defaults to the backward-compatible
    /// heuristic unless callers opt into the built-in final-answer tool.
    pub final_answer_mode: FinalAnswerMode,

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
    /// 每个 turn 后决定下一 turn 的配置。
    pub prepare_next_turn: Option<Arc<dyn PrepareNextTurnHook>>,
    /// 当 LLM 自然停止时，判断是否继续循环。
    pub should_stop: Option<Arc<dyn ShouldStopHook>>,
    /// 在 provider 调用前可编辑 `StreamOptions`。
    pub before_provider_request: Option<Arc<dyn BeforeProviderRequestHook>>,
    /// provider 响应后的观测钩子。
    pub after_provider_response: Option<Arc<dyn AfterProviderResponseHook>>,
    /// Candidate final-answer validation before committed `MessageEnd`.
    pub final_answer_validator: Option<Arc<dyn FinalAnswerValidator>>,
    /// Provider 非瞬态错误（重试耗尽后仍失败）的修复 hook；`None` 时错误原样上抛。
    ///
    /// 典型用途见 strategy crate 的 `VisionDegradeHook`：文本-only provider
    /// 拒绝携带 image/document 块的请求时剥离视觉块并同轮重试。
    ///
    /// 安全约束：hook 返回 `Retry` 会重新发送同轮请求。对结果未知的失败
    /// （网关 5xx 等，adapter 可能折叠进 `InvalidRequest`）重发即重放，因此
    /// 同一个 turn 内 hook 触发的重发次数有硬上限（`max_provider_error_repairs`）。
    pub provider_error: Option<Arc<dyn ProviderErrorHook>>,

    /// 单 turn 内 `ProviderErrorHook` 触发的同轮修复重发上限。`None` = 1。
    /// 这是对"仅凭错误类别无法判断请求是否已被 provider 接受"的兜底：上限
    /// 与类别无关，任何 hook 驱动的重发都不能超过该次数。
    pub max_provider_error_retries: Option<u32>,

    // === Reactive injection ===
    /// 在 turn 间注入的消息（转向）。turn 边界与自然停止时都会 drain；
    /// 停止时剩余消息会触发额外一轮，而不是随 channel 一起丢失。
    pub steer_rx: Option<mpsc::Receiver<AgentMessage>>,
    /// loop 自然停止后逐个处理的后续消息；每条触发一次新的 run。
    pub follow_up_rx: Option<mpsc::Receiver<AgentMessage>>,

    // === Retry ===
    /// Retry config for transient provider errors; `None` disables retry.
    pub retry: Option<RetryConfig>,
    // === Structured output ===
    /// Optional `response_format` for structured output (e.g. `JsonObject`).
    pub response_format: Option<crate::ResponseFormat>,
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(feature = "test-utils")]
    fn loop_config_compiles() {
        use crate::convert::DefaultConvertToLlm;
        use crate::test_utils::NoOpEnv;
        use crate::{FinalAnswerMode, LoopConfig};
        use llm_harness_types::{
            RunContext, RunRequest, StreamOptions, ThinkingLevel, ToolExecutionMode,
        };
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;
        let _cfg = LoopConfig {
            run: Arc::new(RunContext::new(RunRequest::default())),
            model: "test-model".into(),
            max_tokens: 1024,
            temperature: None,
            thinking_level: ThinkingLevel::Off,
            tools: vec![],
            active_tools: None,
            default_execution_mode: ToolExecutionMode::Parallel,
            final_answer_mode: FinalAnswerMode::default(),
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
        };
    }
    #[test]
    #[cfg(feature = "test-utils")]
    fn loop_config_accepts_response_format() {
        use crate::convert::DefaultConvertToLlm;
        use crate::test_utils::NoOpEnv;
        use crate::{FinalAnswerMode, LoopConfig, ResponseFormat};
        use llm_harness_types::{
            RunContext, RunRequest, StreamOptions, ThinkingLevel, ToolExecutionMode,
        };
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;
        let cfg = LoopConfig {
            run: Arc::new(RunContext::new(RunRequest::default())),
            model: "test-model".into(),
            max_tokens: 1024,
            temperature: None,
            thinking_level: ThinkingLevel::Off,
            tools: vec![],
            active_tools: None,
            default_execution_mode: ToolExecutionMode::Parallel,
            final_answer_mode: FinalAnswerMode::default(),
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
            response_format: Some(ResponseFormat::JsonObject),
        };
        assert!(cfg.response_format.is_some());
        assert_eq!(
            cfg.response_format.as_ref().unwrap(),
            &ResponseFormat::JsonObject
        );
    }

    #[test]
    fn delay_for_jitter_within_range() {
        use crate::config::RetryConfig;
        use llm_adapter::LlmError;

        let cfg = RetryConfig::new(3, 1000);
        // attempt=0 → exp = 1000 * 2^0 = 1000; jittered must be in [0, 1000]
        for _ in 0..100 {
            let d = cfg.delay_for(0, &LlmError::Timeout);
            assert!(
                d.as_millis() <= 1000,
                "delay {d:?} exceeded base for attempt 0"
            );
        }
        // attempt=2 → exp = 1000 * 2^2 = 4000; jittered must be in [0, 4000]
        for _ in 0..100 {
            let d = cfg.delay_for(2, &LlmError::Timeout);
            assert!(
                d.as_millis() <= 4000,
                "delay {d:?} exceeded 4x base for attempt 2"
            );
        }
    }

    #[test]
    fn delay_for_retry_after_no_jitter() {
        use crate::config::RetryConfig;
        use llm_adapter::LlmError;
        use std::time::Duration;

        let cfg = RetryConfig::new(3, 1000);
        let err = LlmError::RateLimit {
            retry_after: Some(Duration::from_millis(5000)),
        };
        // With retry_after hint, delay must be exactly the hint — no jitter
        for _ in 0..20 {
            let d = cfg.delay_for(0, &err);
            assert_eq!(
                d,
                Duration::from_millis(5000),
                "retry_after should not be jittered"
            );
        }
    }

    #[test]
    fn delay_for_jitter_is_seeded_deterministic() {
        use crate::config::RetryConfig;
        use llm_adapter::LlmError;

        let cfg = RetryConfig::new(3, 1000);
        fastrand::seed(42);
        let d1 = cfg.delay_for(0, &LlmError::Timeout);
        fastrand::seed(42);
        let d2 = cfg.delay_for(0, &LlmError::Timeout);
        assert_eq!(d1, d2, "same seed must produce same delay");
    }

    #[test]
    fn delay_for_zero_base_returns_zero() {
        use crate::config::RetryConfig;
        use llm_adapter::LlmError;

        let cfg = RetryConfig::new(3, 0);
        // exp = 0 * 2^attempt = 0; fastrand::u64(0..=0) = 0
        for _ in 0..20 {
            let d = cfg.delay_for(0, &LlmError::Timeout);
            assert_eq!(
                d,
                std::time::Duration::from_millis(0),
                "zero base must yield zero delay"
            );
        }
    }
}
