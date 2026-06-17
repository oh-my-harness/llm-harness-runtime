use std::{collections::HashSet, sync::Arc};

use futures::future::BoxFuture;

use crate::{
    AgentContext, AgentError, AgentHarnessResources, AgentMessage, AssistantMessage,
    CompactionResult, ContentBlock, StopReason, StreamOptions, Tool, ToolError, ToolResult,
    TurnSnapshot,
};

// ── TransformContextHook ──────────────────────────────────────────────────────

/// 每次 LLM 调用前对上下文做转换；compaction 通过此 hook 接入。
pub trait TransformContextHook: Send + Sync {
    /// 对 `ctx` 做变换后返回新的上下文。
    fn transform<'a>(
        &'a self,
        ctx: AgentContext,
    ) -> BoxFuture<'a, Result<AgentContext, AgentError>>;
}

// ── PrepareNextTurnHook ───────────────────────────────────────────────────────

/// 传递给 `PrepareNextTurnHook::prepare` 的上下文。
pub struct PrepareNextTurnCtx<'a> {
    /// 当前 turn 编号（从 0 开始）。
    pub turn_index: u32,
    /// 上一轮的完整 LLM 回复。
    pub last_message: &'a AssistantMessage,
    /// 上一轮所有 tool 执行结果；key 为 `tool_use_id`。
    pub last_tool_results: &'a [(String, Result<ToolResult, ToolError>)],
}

/// `PrepareNextTurnHook::prepare` 的返回值；`None` 字段表示沿用当前值。
pub struct NextTurnDirective {
    /// 替换下一轮的完整上下文；`None` 表示沿用当前上下文。
    pub context: Option<AgentContext>,
    /// 替换下一轮使用的模型 ID；`None` 表示沿用当前模型。
    pub model: Option<String>,
    /// 替换下一轮的推理深度；`None` 表示沿用当前级别。
    pub thinking_level: Option<crate::ThinkingLevel>,
    /// 替换全部工具列表；`None` 表示沿用当前工具列表。
    pub tools: Option<Vec<Arc<dyn Tool>>>,
    /// 仅控制激活工具子集（在当前或已替换的全集中过滤）；`None` 表示激活全部。
    pub active_tools: Option<HashSet<String>>,
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
    /// 拒绝执行，直接返回指定的 `ToolResult`。
    Deny(ToolResult),
}

/// 工具执行前的拦截 hook。
pub trait BeforeToolCallHook: Send + Sync {
    /// 在工具执行前决定是否允许、修改参数或拒绝执行。
    fn on_call<'a>(&'a self, ctx: BeforeToolCallCtx<'a>) -> BoxFuture<'a, BeforeToolCallDecision>;
}

// ── AfterToolCallHook ─────────────────────────────────────────────────────────

/// 传递给 `AfterToolCallHook::on_complete` 的上下文。
pub struct AfterToolCallCtx<'a> {
    /// 触发本次 tool call 的 LLM 回复。
    pub assistant_message: &'a AssistantMessage,
    /// 当前 tool call 的唯一 ID。
    pub tool_use_id: &'a str,
    /// 工具名称。
    pub tool_name: &'a str,
    /// 工具调用参数。
    pub args: &'a serde_json::Value,
    /// 工具执行结果。
    pub result: &'a Result<ToolResult, ToolError>,
    /// 当前轮次索引。
    pub turn_index: u32,
}

/// `ToolResult` 的部分覆盖补丁；`None` 字段表示保持原值。
pub struct ToolResultPatch {
    /// 覆盖内容块列表。
    pub content: Option<Vec<ContentBlock>>,
    /// 覆盖扩展数据。
    pub details: Option<serde_json::Value>,
    /// 覆盖错误标志。
    pub is_error: Option<bool>,
    /// 覆盖终止标志。
    pub terminate: Option<bool>,
}

/// `AfterToolCallHook::on_complete` 的返回决策。
pub enum AfterToolCallDecision {
    /// 照常使用工具执行结果，不做修改。
    Passthrough,
    /// 部分覆盖执行结果。
    Patch(ToolResultPatch),
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

/// LLM provider 请求前拦截 hook；可原地修改传输层配置。
pub trait BeforeProviderRequestHook: Send + Sync {
    /// 在 LLM 调用前修改 `StreamOptions`。
    ///
    /// 当前 loop 直接应用 timeout/retry 字段；headers、metadata 和 cache 配置需要
    /// provider adapter 支持 per-request 传输配置后才能透传。
    fn before_request<'a>(&'a self, opts: &'a mut StreamOptions) -> BoxFuture<'a, ()>;
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
}

/// LLM provider 响应后的观测 hook；用于配额追踪、成本监控等。
pub trait AfterProviderResponseHook: Send + Sync {
    /// 在收到 provider 响应后调用（纯观测，无返回值）。
    fn after_response<'a>(&'a self, info: &'a ProviderResponseInfo) -> BoxFuture<'a, ()>;
}

// ── AuthHook ──────────────────────────────────────────────────────────────────

/// 动态认证信息。
pub struct AuthInfo {
    /// API key；`None` 表示使用 provider 默认配置。
    pub api_key: Option<String>,
    /// 附加的认证 HTTP 头（如 OAuth token）。
    pub headers: Vec<(String, String)>,
}

/// 动态认证 hook；每次 LLM 调用前解析最新凭据（适用于 OAuth token 过期等场景）。
pub trait AuthHook: Send + Sync {
    /// 返回当前有效的认证信息。
    fn resolve<'a>(&'a self) -> BoxFuture<'a, Result<AuthInfo, AgentError>>;
}

// ── Turn 边界 Hooks ───────────────────────────────────────────────────────────

/// 传递给 `BeforeRunHook::before_run` 的上下文。
pub struct BeforeRunCtx<'a> {
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
    /// 当前 turn 编号（从 0 开始）。
    pub turn_index: u32,
    /// Turn 开始时的配置快照。
    pub snapshot: &'a TurnSnapshot,
}

/// 传递给 `AfterTurnHook::after_turn` 的上下文。
pub struct AfterTurnCtx<'a> {
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
}

/// `BeforeCompactHook::before_compact` 的返回决策。
#[allow(clippy::large_enum_variant)]
pub enum BeforeCompactDecision {
    /// 继续执行框架默认的 compaction 流程。
    Proceed,
    /// 跳过本次 compaction（由 hook 决定暂不压缩）。
    Skip,
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
    fn tool_result_patch_all_none() {
        let p = ToolResultPatch {
            content: None,
            details: None,
            is_error: None,
            terminate: None,
        };
        assert!(p.content.is_none());
    }

    #[test]
    fn before_compact_decision_skip() {
        let d = BeforeCompactDecision::Skip;
        assert!(matches!(d, BeforeCompactDecision::Skip));
    }

    #[test]
    fn auth_info_fields() {
        let a = AuthInfo {
            api_key: Some("sk-test".into()),
            headers: vec![("X-Custom".into(), "val".into())],
        };
        assert!(a.api_key.is_some());
        assert_eq!(a.headers.len(), 1);
    }

    #[test]
    fn all_hook_traits_are_object_safe() {
        fn _a(_: &dyn TransformContextHook) {}
        fn _b(_: &dyn PrepareNextTurnHook) {}
        fn _c(_: &dyn BeforeToolCallHook) {}
        fn _d(_: &dyn AfterToolCallHook) {}
        fn _e(_: &dyn ShouldStopHook) {}
        fn _f(_: &dyn BeforeProviderRequestHook) {}
        fn _g(_: &dyn AfterProviderResponseHook) {}
        fn _h(_: &dyn AuthHook) {}
        fn _i(_: &dyn BeforeRunHook) {}
        fn _j(_: &dyn BeforeTurnHook) {}
        fn _k(_: &dyn AfterTurnHook) {}
        fn _l(_: &dyn BeforeCompactHook) {}
    }
}
