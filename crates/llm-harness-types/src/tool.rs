use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{AssistantMessage, ContentBlock, ExecutionEnv, ToolError};

/// 工具执行结果。
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// 发送给 LLM 的内容块列表。
    pub content: Vec<ContentBlock>,
    /// 不发送给 LLM 的结构化数据，用于 UI 渲染或审计日志。
    pub details: serde_json::Value,
    /// 当 batch 中所有 tool 均返回 `true` 时，agent loop 提前停止。
    pub terminate: bool,
}

/// 工具执行模式——决定 tool 在同一 batch 中的并发策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    /// 与同 batch 中的其他 `Parallel` tool 并发执行（`join_all`）。
    Parallel,
    /// 作为子组分割点——前一子组结束后才开始新子组。
    Sequential,
}

/// Tool 执行上下文，每次调用时由 loop 构造后传入。
pub struct ToolContext {
    /// 执行环境（文件系统 + shell）。
    pub env: Arc<dyn ExecutionEnv>,
    /// 用户取消信号。
    pub abort: CancellationToken,
    /// 当前 tool call 在 LLM 输出中的唯一 ID。
    pub tool_use_id: String,
    /// 当前轮次索引（从 0 开始）。
    pub turn_index: u32,
    /// 触发本次 tool call 的完整 LLM 响应；同一消息的多个 tool call 共享同一 Arc。
    pub assistant_message: Arc<AssistantMessage>,
    /// 长时间运行的 tool 通过此 channel 推送部分结果；接收端转发为 `AgentEvent::ToolExecutionUpdate`。
    pub update_tx: mpsc::Sender<ToolResult>,
}

/// 工具 trait——框架调用工具的唯一接口。
pub trait Tool: Send + Sync {
    /// 工具的稳定程序标识符；在 session log 和 LLM tool definition 中使用。
    fn name(&self) -> &str;

    /// 工具的人类可读 UI 显示名；默认回退到 `name()`。
    fn label(&self) -> &str {
        self.name()
    }

    /// 工具功能的自然语言描述，用于 LLM tool definition。
    fn description(&self) -> &str;

    /// 工具参数的 JSON Schema。
    fn parameters_schema(&self) -> &serde_json::Value;

    /// 工具在同一 batch 中的执行模式；默认 `Parallel`。
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    /// 在 schema 校验前对 LLM 原始参数做兼容转换；默认 identity（不转换）。
    fn prepare_arguments(&self, raw: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        Ok(raw)
    }

    /// 执行工具；返回结果或错误。
    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContentBlock;

    #[test]
    fn tool_result_defaults_terminate_false() {
        let r = ToolResult {
            content: vec![ContentBlock::Text {
                text: "done".into(),
            }],
            details: serde_json::Value::Null,
            terminate: false,
        };
        assert!(!r.terminate);
    }

    #[test]
    fn tool_execution_mode_default_is_parallel() {
        struct MyTool;
        impl Tool for MyTool {
            fn name(&self) -> &str {
                "my_tool"
            }
            fn description(&self) -> &str {
                "does stuff"
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                &serde_json::Value::Null
            }
            fn execute<'a>(
                &'a self,
                _args: serde_json::Value,
                _ctx: &'a ToolContext,
            ) -> BoxFuture<'a, Result<ToolResult, crate::ToolError>> {
                Box::pin(async {
                    Ok(ToolResult {
                        content: vec![],
                        details: serde_json::Value::Null,
                        terminate: false,
                    })
                })
            }
        }
        let t = MyTool;
        assert!(matches!(t.execution_mode(), ToolExecutionMode::Parallel));
        assert_eq!(t.label(), "my_tool");
    }

    #[test]
    fn tool_is_object_safe() {
        fn _accepts(_: &dyn Tool) {}
    }
}
