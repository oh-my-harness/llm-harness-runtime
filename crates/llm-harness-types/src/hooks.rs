use std::{collections::HashSet, sync::Arc};

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::{
    AgentContext, AgentError, AgentHarnessResources, AgentMessage, AssistantMessage,
    CompactionResult, EntryId, RunContext, StopReason, StreamOptions, Tool, ToolFailure,
    ToolResult, TurnSnapshot,
};

// ── FinalAnswerValidator ──────────────────────────────────────────────────────

/// Context presented before a candidate final answer becomes a committed
/// `MessageEnd`.
///
/// Streaming deltas are provisional. A rejected candidate is never appended to
/// the agent context or persisted by `AgentHarness`.
pub struct FinalAnswerValidationCtx<'a> {
    /// Immutable context for the current run.
    pub run: &'a RunContext,
    /// Candidate final answer.
    pub candidate: &'a AssistantMessage,
    /// Current turn index.
    pub turn_index: u32,
}

/// Public, sanitized reason for rejecting a candidate final answer.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[error("{code}: {message}")]
pub struct FinalAnswerValidationError {
    /// Stable machine-readable rejection code.
    pub code: String,
    /// Safe message that may be exposed to runtime consumers.
    pub message: String,
}

impl FinalAnswerValidationError {
    /// Creates a sanitized validation error.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Validates a candidate final answer before it crosses the committed answer
/// boundary.
pub trait FinalAnswerValidator: Send + Sync {
    /// Returns `Ok(())` to commit the answer, or a sanitized rejection.
    fn validate<'a>(
        &'a self,
        ctx: FinalAnswerValidationCtx<'a>,
    ) -> BoxFuture<'a, Result<(), FinalAnswerValidationError>>;
}

// ── TransformContextHook ──────────────────────────────────────────────────────

/// 传递给 `TransformContextHook::transform` 的上下文。
pub struct TransformContextCtx<'a> {
    /// 当前 run 上下文。
    pub run: &'a RunContext,
    /// 当前 turn 编号（从 0 开始）。
    pub turn_index: u32,
    /// Cancellation for the current run; long-running transforms must observe it.
    pub abort: CancellationToken,
    /// 待转换的结构化 Agent 上下文。
    pub context: AgentContext,
}

/// 每次 LLM 调用前对上下文做转换。
pub trait TransformContextHook: Send + Sync {
    /// 对 `ctx` 做变换后返回新的上下文。
    fn transform<'a>(
        &'a self,
        ctx: TransformContextCtx<'a>,
    ) -> BoxFuture<'a, Result<AgentContext, AgentError>>;
}

// ── PrepareNextTurnHook ───────────────────────────────────────────────────────

/// 传递给 `PrepareNextTurnHook::prepare` 的上下文。
pub struct PrepareNextTurnCtx<'a> {
    /// 当前 run 上下文。
    pub run: &'a RunContext,
    /// 当前 turn 编号（从 0 开始）。
    pub turn_index: u32,
    /// 上一轮的完整 LLM 回复。
    pub last_message: &'a AssistantMessage,
    /// 上一轮所有 tool 执行结果；key 为 `tool_use_id`。
    pub last_tool_results: &'a [(String, Result<ToolResult, ToolFailure>)],
}

/// `PrepareNextTurnHook::prepare` 的返回值；`None` 字段表示沿用当前值。
pub struct NextTurnDirective {
    /// 替换下一轮的完整上下文；`None` 表示沿用当前上下文。
    pub context: Option<AgentContext>,
    /// 替换下一轮使用的模型 ID；`None` 表示沿用当前模型。
    pub model: Option<String>,
    /// 替换下一轮的推理深度；`None` 表示沿用当前级别。
    pub thinking_level: Option<crate::ThinkingLevel>,
    /// 替换下一轮的采样温度；`None` 表示沿用当前值。
    /// `Some(None)` 表示清除温度（走 provider 默认），`Some(Some(t))` 表示设为 `t`。
    pub temperature: Option<Option<f32>>,
    /// 替换全部工具列表；`None` 表示沿用当前工具列表。
    pub tools: Option<Vec<Arc<dyn Tool>>>,
    /// 仅控制激活工具子集（在当前或已替换的全集中过滤）；`None` 表示激活全部。
    pub active_tools: Option<HashSet<String>>,
    /// 替换下一轮的 response_format；`None` 表示沿用当前值。
    /// `Some(Some(fmt))` = 设为 fmt，`Some(None)` = 清除，`None` = 沿用。
    pub response_format: Option<Option<llm_adapter::types::ResponseFormat>>,
}

/// 每个 turn 结束后调用，返回下一轮的配置。
pub trait PrepareNextTurnHook: Send + Sync {
    /// 根据上一轮结果决定下一轮配置。
    fn prepare<'a>(
        &'a self,
        ctx: PrepareNextTurnCtx<'a>,
    ) -> BoxFuture<'a, Result<NextTurnDirective, AgentError>>;
}

// ── BeforeToolCallHook ────────────────────────────────────────────────────────

/// 传递给 `BeforeToolCallHook::on_call` 的上下文。
pub struct BeforeToolCallCtx<'a> {
    /// 当前 run 上下文。
    pub run: &'a RunContext,
    /// 触发本次 tool call 的 LLM 回复。
    pub assistant_message: &'a AssistantMessage,
    /// 当前 tool call 的唯一 ID。
    pub tool_use_id: &'a str,
    /// 工具名称。
    pub tool_name: &'a str,
    /// 工具调用参数。
    pub args: &'a serde_json::Value,
    /// 当前轮次索引。
    pub turn_index: u32,
}

/// `BeforeToolCallHook::on_call` 的返回决策。
pub enum BeforeToolCallDecision {
    /// 允许工具按原参数执行。
    Allow,
    /// 以修改后的参数执行工具。
    Modify(serde_json::Value),
    /// 拒绝执行，返回经过脱敏且可投影的公开失败。
    Deny(ToolFailure),
}

/// 工具执行前的拦截 hook。
pub trait BeforeToolCallHook: Send + Sync {
    /// 在工具执行前决定是否允许、修改参数或拒绝执行。
    fn on_call<'a>(&'a self, ctx: BeforeToolCallCtx<'a>) -> BoxFuture<'a, BeforeToolCallDecision>;
}

// ── AfterToolCallHook ─────────────────────────────────────────────────────────

/// 传递给 `AfterToolCallHook::on_complete` 的上下文。
pub struct AfterToolCallCtx<'a> {
    /// 当前 run 上下文。
    pub run: &'a RunContext,
    /// 触发本次 tool call 的 LLM 回复。
    pub assistant_message: &'a AssistantMessage,
    /// 当前 tool call 的唯一 ID。
    pub tool_use_id: &'a str,
    /// 工具名称。
    pub tool_name: &'a str,
    /// 工具调用参数。
    pub args: &'a serde_json::Value,
    /// 工具执行结果。
    pub result: &'a Result<ToolResult, ToolFailure>,
    /// 当前轮次索引。
    pub turn_index: u32,
}

/// `AfterToolCallHook::on_complete` 的返回决策。
pub enum AfterToolCallDecision {
    /// 照常使用工具执行结果，不做修改。
    Passthrough,
    /// 原子替换整个结果，保持 model content、extensions 和 projection 一致。
    Replace(Result<ToolResult, ToolFailure>),
}

/// 工具执行后的结果拦截 hook。
pub trait AfterToolCallHook: Send + Sync {
    /// 在工具执行完成后决定是否覆盖结果。
    fn on_complete<'a>(&'a self, ctx: AfterToolCallCtx<'a>)
    -> BoxFuture<'a, AfterToolCallDecision>;
}

// ── ShouldStopHook ────────────────────────────────────────────────────────────

/// 传递给 `ShouldStopHook::should_stop` 的上下文。
pub struct ShouldStopCtx<'a> {
    /// 当前 run 上下文。
    pub run: &'a RunContext,
    /// 最后一条 LLM 回复。
    pub last_assistant: &'a AssistantMessage,
    /// LLM 停止原因。
    pub stop_reason: StopReason,
    /// 当前轮次索引。
    pub turn_index: u32,
}

/// LLM 自然停止后的继续决策 hook。
///
/// 返回 `true` 停止 loop；返回 `false` 强制再跑一轮（适用于 `MaxTokens` 等截断场景）。
/// 不能用于中断进行中的 turn——中断走 `abort()`。
pub trait ShouldStopHook: Send + Sync {
    /// 仅在 LLM 自然停止时调用；返回 `true` 才停止。
    fn should_stop<'a>(&'a self, ctx: ShouldStopCtx<'a>) -> BoxFuture<'a, bool>;
}

// ── Provider Request/Response Hooks ───────────────────────────────────────────

/// 传递给 `BeforeProviderRequestHook::before_request` 的上下文。
pub struct BeforeProviderRequestCtx<'a> {
    /// 当前 run 上下文。
    pub run: &'a RunContext,
    /// 当前 turn 编号（从 0 开始）。
    pub turn_index: u32,
    /// 当前请求使用的传输层配置，可由 Hook 原地修改。
    pub options: &'a mut StreamOptions,
}

/// LLM provider 请求前拦截 hook；可原地修改传输层配置。
pub trait BeforeProviderRequestHook: Send + Sync {
    /// 在 LLM 调用前修改 `StreamOptions`。
    ///
    /// 当前 loop 直接应用 timeout/retry 字段；headers、metadata 和 cache 配置需要
    /// provider adapter 支持 per-request 传输配置后才能透传。
    fn before_request<'a>(&'a self, ctx: BeforeProviderRequestCtx<'a>) -> BoxFuture<'a, ()>;
}

/// Provider 响应的元数据信息。
pub struct ProviderResponseInfo {
    /// HTTP 状态码；`None` 表示流式请求未携带状态码。
    pub status_code: Option<u16>,
    /// HTTP 响应头。
    pub response_headers: Vec<(String, String)>,
    /// Token 用量；`None` 表示 provider 未返回。
    pub usage: Option<crate::TokenUsage>,
    /// 请求延迟（毫秒）。
    pub latency_ms: u64,
    /// 本次响应的模型 ID（从 `AssistantMessage.model` 透传）。
    pub model: Option<String>,
    /// 生成本次响应的 provider 名（从 `AssistantMessage.provider` 透传）。
    pub provider: Option<String>,
}

/// 传递给 `AfterProviderResponseHook::after_response` 的上下文。
pub struct AfterProviderResponseCtx<'a> {
    /// 当前 run 上下文。
    pub run: &'a RunContext,
    /// 当前 turn 编号（从 0 开始）。
    pub turn_index: u32,
    /// Provider 响应元数据。
    pub info: &'a ProviderResponseInfo,
}

/// LLM provider 响应后的观测 hook；用于配额追踪、成本监控等。
pub trait AfterProviderResponseHook: Send + Sync {
    /// 在收到 provider 响应后调用（纯观测，无返回值）。
    fn after_response<'a>(&'a self, ctx: AfterProviderResponseCtx<'a>) -> BoxFuture<'a, ()>;
}

// ── Turn 边界 Hooks ───────────────────────────────────────────────────────────

/// 传递给 `BeforeRunHook::before_run` 的上下文。
pub struct BeforeRunCtx<'a> {
    /// 当前 run 上下文。
    pub run: &'a RunContext,
    /// 用户输入的提示文本。
    pub prompt_text: &'a str,
    /// 本次运行注入的初始消息列表（可修改）。
    pub initial_messages: &'a mut Vec<AgentMessage>,
    /// 系统提示（可修改）。
    pub system_prompt: &'a mut Option<String>,
    /// Harness 运行时资源。
    pub resources: &'a AgentHarnessResources,
}

/// `BeforeRunHook::before_run` 的返回值。
pub struct BeforeRunResult {
    /// 追加到 `initial_messages` 末尾的额外消息。
    pub additional_messages: Vec<AgentMessage>,
    /// 覆盖系统提示；`None` 表示沿用 `BeforeRunCtx` 中可能已修改的值。
    pub system_prompt: Option<String>,
}

/// Harness 专属：一次完整 agent 运行（prompt 调用）开始前的 hook。
pub trait BeforeRunHook: Send + Sync {
    /// 在 agent 开始运行前调用；可注入额外消息或修改系统提示。
    fn before_run<'a>(
        &'a self,
        ctx: BeforeRunCtx<'a>,
    ) -> BoxFuture<'a, Result<BeforeRunResult, AgentError>>;
}

/// 传递给 `BeforeTurnHook::before_turn` 的上下文。
pub struct BeforeTurnCtx<'a> {
    /// 当前 run 上下文。
    pub run: &'a RunContext,
    /// 当前 turn 编号（从 0 开始）。
    pub turn_index: u32,
    /// Turn 开始时的配置快照。
    pub snapshot: &'a TurnSnapshot,
}

/// 传递给 `AfterTurnHook::after_turn` 的上下文。
pub struct AfterTurnCtx<'a> {
    /// 当前 run 上下文。
    pub run: &'a RunContext,
    /// 当前 turn 编号（从 0 开始）。
    pub turn_index: u32,
    /// 本 turn 新增的消息列表。
    pub new_messages: &'a [AgentMessage],
}

/// Harness 专属：turn 开始前通知 hook（纯通知，无返回值）。
pub trait BeforeTurnHook: Send + Sync {
    /// Turn 开始前调用。
    fn before_turn<'a>(&'a self, ctx: BeforeTurnCtx<'a>) -> BoxFuture<'a, ()>;
}

/// Harness 专属：turn 结束后通知 hook（纯通知，无返回值）。
pub trait AfterTurnHook: Send + Sync {
    /// Turn 结束后调用。
    fn after_turn<'a>(&'a self, ctx: AfterTurnCtx<'a>) -> BoxFuture<'a, ()>;
}

// ── BeforeCompactHook ─────────────────────────────────────────────────────────

/// 传递给 `BeforeCompactHook::before_compact` 的上下文。
pub struct BeforeCompactCtx<'a> {
    /// 当前估算的 token 数。
    pub estimated_tokens: usize,
    /// 当前消息列表。
    pub messages: &'a [AgentMessage],
    /// 当前 active path 中所有 entry 的 ID（与 messages 一一对应之前的完整 path）。
    /// Override 决策需要从中选取 `first_kept_entry`。
    pub entry_ids: &'a [EntryId],
    /// 模型上下文窗口大小（token 数）。
    pub context_window: u32,
    /// 压缩时预留给 LLM 回复的 token 数。
    pub reserve_tokens: usize,
    /// 压缩时保留的近期历史 token 数。
    pub keep_recent_tokens: usize,
}

/// `BeforeCompactHook::before_compact` 的返回决策。
#[allow(clippy::large_enum_variant)]
pub enum BeforeCompactDecision {
    /// 继续执行框架默认的 compaction 流程。
    Proceed,
    /// 跳过本次 compaction（由 hook 决定暂不压缩）。
    Skip,
    /// 强制走框架默认压缩流程，跳过阈值检查。
    /// 框架算 cut point + LLM 摘要，hook 不需要提供任何东西。
    Compact,
    /// 使用 hook 提供的 `CompactionResult` 替代框架生成的摘要。
    Override(CompactionResult),
}

/// Compaction 执行前的决策 hook。
pub trait BeforeCompactHook: Send + Sync {
    /// 在 compaction 执行前决定是继续、跳过或使用自定义摘要。
    fn before_compact<'a>(
        &'a self,
        ctx: BeforeCompactCtx<'a>,
    ) -> BoxFuture<'a, BeforeCompactDecision>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn before_tool_call_decision_allow() {
        let d = BeforeToolCallDecision::Allow;
        assert!(matches!(d, BeforeToolCallDecision::Allow));
    }

    #[test]
    fn after_tool_call_decision_passthrough() {
        let d = AfterToolCallDecision::Passthrough;
        assert!(matches!(d, AfterToolCallDecision::Passthrough));
    }

    #[test]
    fn before_compact_decision_skip() {
        let d = BeforeCompactDecision::Skip;
        assert!(matches!(d, BeforeCompactDecision::Skip));
    }

    #[test]
    fn compact_decision_is_constructible() {
        let d = BeforeCompactDecision::Compact;
        assert!(!matches!(d, BeforeCompactDecision::Proceed));
        assert!(!matches!(d, BeforeCompactDecision::Skip));
    }

    #[test]
    fn all_hook_traits_are_object_safe() {
        fn _final(_: &dyn FinalAnswerValidator) {}
        fn _a(_: &dyn TransformContextHook) {}
        fn _b(_: &dyn PrepareNextTurnHook) {}
        fn _c(_: &dyn BeforeToolCallHook) {}
        fn _d(_: &dyn AfterToolCallHook) {}
        fn _e(_: &dyn ShouldStopHook) {}
        fn _f(_: &dyn BeforeProviderRequestHook) {}
        fn _g(_: &dyn AfterProviderResponseHook) {}
        fn _i(_: &dyn BeforeRunHook) {}
        fn _j(_: &dyn BeforeTurnHook) {}
        fn _k(_: &dyn AfterTurnHook) {}
        fn _l(_: &dyn BeforeCompactHook) {}
    }

    #[test]
    fn next_turn_directive_response_format_field_exists() {
        let directive = NextTurnDirective {
            context: None,
            model: None,
            thinking_level: None,
            temperature: None,
            tools: None,
            active_tools: None,
            response_format: Some(Some(llm_adapter::types::ResponseFormat::JsonObject)),
        };
        assert!(directive.response_format.is_some());
    }
}
