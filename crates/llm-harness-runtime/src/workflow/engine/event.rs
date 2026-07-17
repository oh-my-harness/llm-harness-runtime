//! 引擎级事件类型 + AgentEvent → StepProgress 裁剪。

use llm_harness_types::{AgentEvent, AssistantMessageKind};

use crate::workflow::model::{StepId, StepResult};

/// broadcast channel 容量。步骤级事件频率低，overflow 时丢弃旧事件（比阻塞 run 好）。
pub(super) const EVENT_CHANNEL_CAPACITY: usize = 64;

/// LLM step 内部的粗粒度进度事件。由 `step_progress_from` 从 `AgentEvent` 裁剪而来。
/// 只携带前端展示必需的字段，丢弃 `AssistantMessage` / 完整 `ToolResult` 等大对象。
#[derive(Debug, Clone)]
pub enum StepProgress {
    /// LLM 开始请求调用某个工具。
    ToolCallStart { tool_use_id: String, name: String },
    /// LLM 工具调用参数完整到达。
    ToolCallEnd {
        tool_use_id: String,
        args: serde_json::Value,
    },
    /// Tool 开始执行（Rust 层面）。
    ToolExecutionStart {
        tool_use_id: String,
        tool_name: String,
    },
    /// Tool 执行完毕。
    ToolExecutionEnd {
        tool_use_id: String,
        ok: bool,
        error: Option<String>,
    },
    /// 一次 turn 结束。
    TurnEnd { index: u32 },
    /// Assistant message 完整生成完毕。
    MessageEnd {
        message_id: String,
        kind: AssistantMessageKind,
    },
}

/// 引擎级事件。subscribe() 返回的 Receiver 推送此类型。
/// 步骤级事件必须对外可见——长周期任务才能做监控/UI。
///
/// broadcast channel 容量由 `EVENT_CHANNEL_CAPACITY` 控制。
/// overflow 时丢弃旧事件（比阻塞 run 好）。订阅者应容忍丢事件。
#[derive(Debug, Clone)]
pub enum WorkflowEvent {
    StepStarted {
        step_id: StepId,
        step_name: String,
    },
    StepFinished {
        step_id: StepId,
        result: StepResult,
    },
    Paused {
        reason: String,
    },
    Resumed,
    Cancelled {
        reason: String,
    },
    Failed {
        error: String,
    },
    /// LLM step 内部进度（工具调用边界、turn 边界）。
    /// 高频事件，overflow 时丢弃旧事件——订阅者应容忍丢事件。
    StepProgress {
        step_id: StepId,
        progress: StepProgress,
    },
}

/// 从 `AgentEvent` 裁剪出粗粒度进度事件。
/// 返回 `None` 的事件（TextDelta、ThinkingDelta 等）不透传到 `WorkflowEvent`。
pub(super) fn step_progress_from(ev: &AgentEvent) -> Option<StepProgress> {
    Some(match ev {
        AgentEvent::ToolCallStart {
            tool_use_id, name, ..
        } => StepProgress::ToolCallStart {
            tool_use_id: tool_use_id.clone(),
            name: name.clone(),
        },
        AgentEvent::ToolCallEnd { tool_use_id, args } => StepProgress::ToolCallEnd {
            tool_use_id: tool_use_id.clone(),
            args: args.clone(),
        },
        AgentEvent::ToolExecutionStart {
            tool_use_id,
            tool_name,
            ..
        } => StepProgress::ToolExecutionStart {
            tool_use_id: tool_use_id.clone(),
            tool_name: tool_name.clone(),
        },
        AgentEvent::ToolExecutionEnd {
            tool_use_id,
            result,
        } => StepProgress::ToolExecutionEnd {
            tool_use_id: tool_use_id.clone(),
            ok: result.is_ok(),
            error: result.as_ref().err().map(|e| e.to_string()),
        },
        AgentEvent::TurnEnd { index, .. } => StepProgress::TurnEnd { index: *index },
        AgentEvent::MessageEnd {
            message_id,
            message,
        } => StepProgress::MessageEnd {
            message_id: message_id.clone(),
            kind: message.kind,
        },
        _ => return None,
    })
}
