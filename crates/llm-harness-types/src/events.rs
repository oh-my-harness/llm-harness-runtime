use crate::{
    AgentError, AgentMessage, AssistantMessage, AssistantMessageKind, ToolError, ToolResult,
};

/// Agent 产生的完整事件流。
///
/// 消息级事件（`Message*`）与 token 级事件（`TextDelta` 等）并列，
/// 支持消息列表 UI 和字符流 UI 两种模式。
#[derive(Debug)]
pub enum AgentEvent {
    // === Agent 生命周期 ===
    /// Agent 开始一次完整运行。
    AgentStart,
    /// Agent 完成本次运行；携带本次新增的全部消息（Harness 的关键接口契约）。
    AgentEnd { new_messages: Vec<AgentMessage> },

    // === Turn 生命周期 ===
    /// 一次 turn 开始；`index` 从 0 开始递增。
    TurnStart { index: u32 },
    /// 一次 turn 结束；携带完整 assistant message 和全部 tool 执行结果。
    TurnEnd {
        /// Turn 编号（从 0 开始）。
        index: u32,
        /// 本轮 LLM 回复。
        message: AssistantMessage,
        /// 本轮所有 tool 执行结果；key 为 `tool_use_id`。
        tool_results: Vec<(String, Result<ToolResult, ToolError>)>,
    },

    // === 消息级（assistant message 边界） ===
    /// 一条新的 assistant message 开始流式生成。
    MessageStart { message_id: String },
    /// 流式期间 assistant message 的当前快照（覆盖之前的快照）。
    MessageUpdate {
        message_id: String,
        partial: AssistantMessage,
    },
    /// Assistant message 完整生成完毕，含 stop_reason 和 usage。
    MessageEnd {
        message_id: String,
        message: AssistantMessage,
    },

    // === Token 级（字符流） ===
    /// 文本增量。
    TextDelta { message_id: String, text: String },
    /// 推理/思考内容增量。
    ThinkingDelta {
        message_id: String,
        thinking: String,
        signature: Option<String>,
    },
    /// LLM 开始请求调用某个工具。
    ToolCallStart {
        message_id: String,
        tool_use_id: String,
        name: String,
    },
    /// LLM 工具调用参数的增量 JSON 片段。
    ToolCallArgsDelta {
        tool_use_id: String,
        partial_input: String,
    },
    /// LLM 工具调用参数完整到达，含解析后的完整参数。
    ToolCallEnd {
        tool_use_id: String,
        args: serde_json::Value,
    },

    // === 工具执行（Rust 层面执行 tool，区别于 LLM 发起的 ToolCall） ===
    /// Tool 开始执行。
    ToolExecutionStart {
        tool_use_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    /// 长时间运行的 tool 推送的中间结果。
    ToolExecutionUpdate {
        tool_use_id: String,
        partial: ToolResult,
    },
    /// Tool 执行完毕；携带 Rust 层面的执行结果。
    ToolExecutionEnd {
        tool_use_id: String,
        result: Result<ToolResult, ToolError>,
    },

    /// Loop 遇到不可恢复的错误；之后 `AgentEnd` 将立即到达。
    Error(AgentError),
    /// LLM call encountered a retryable error; emitted before the backoff sleep.
    ///
    /// `attempt` is 0-based: first retry = 0. UI should display `attempt + 1`.
    RetryAttempt {
        /// 0-based index of the current retry (first retry = 0).
        attempt: u32,
        /// Total allowed retries (`RetryConfig::max_retries`).
        max_retries: u32,
        /// Actual sleep duration in milliseconds (after `max_retry_delay_ms` cap).
        delay_ms: u64,
        /// `Display` string of the triggering `LlmError`.
        error: String,
    },
}

impl AgentEvent {
    /// 若此事件是最终回答（`MessageEnd` 且 `message.kind == FinalAnswer`），
    /// 返回 `(message_id, turn_id, text)`。
    pub fn as_final_answer(&self) -> Option<(&str, &str, String)> {
        if let AgentEvent::MessageEnd {
            message_id,
            message,
        } = self
            && message.kind == AssistantMessageKind::FinalAnswer
        {
            return Some((message_id, &message.turn_id, message.text_content()));
        }
        None
    }

    /// 若此事件是进度消息（`MessageEnd` 且 `message.kind == Progress`），
    /// 返回 `(message_id, turn_id, text)`。
    pub fn as_progress(&self) -> Option<(&str, &str, String)> {
        if let AgentEvent::MessageEnd {
            message_id,
            message,
        } = self
            && message.kind == AssistantMessageKind::Progress
        {
            return Some((message_id, &message.turn_id, message.text_content()));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentBlock, StopReason};

    fn make_assistant() -> AssistantMessage {
        AssistantMessage {
            kind: crate::AssistantMessageKind::FinalAnswer,
            message_id: "msg_1".into(),
            turn_id: "turn_1".into(),
            content: vec![ContentBlock::Text { text: "ok".into() }],
            stop_reason: Some(StopReason::EndTurn),
            timestamp: chrono::Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        }
    }

    #[test]
    fn agent_start_event() {
        let ev = AgentEvent::AgentStart;
        assert!(matches!(ev, AgentEvent::AgentStart));
    }

    #[test]
    fn agent_end_event_carries_new_messages() {
        let ev = AgentEvent::AgentEnd {
            new_messages: vec![],
        };
        assert!(matches!(ev, AgentEvent::AgentEnd { .. }));
    }

    #[test]
    fn turn_end_carries_tool_results() {
        let result: Result<ToolResult, crate::ToolError> = Ok(ToolResult {
            content: vec![],
            details: serde_json::Value::Null,
            terminate: false,
        });
        let ev = AgentEvent::TurnEnd {
            index: 0,
            message: make_assistant(),
            tool_results: vec![("call_1".into(), result)],
        };
        assert!(matches!(ev, AgentEvent::TurnEnd { index: 0, .. }));
    }

    #[test]
    fn error_event() {
        let ev = AgentEvent::Error(AgentError::Aborted);
        assert!(matches!(ev, AgentEvent::Error(_)));
    }

    #[test]
    fn retry_attempt_event_fields_are_accessible() {
        let ev = AgentEvent::RetryAttempt {
            attempt: 0,
            max_retries: 3,
            delay_ms: 2000,
            error: "rate limited".into(),
        };
        match ev {
            AgentEvent::RetryAttempt {
                attempt,
                max_retries,
                delay_ms,
                error,
            } => {
                assert_eq!(attempt, 0);
                assert_eq!(max_retries, 3);
                assert_eq!(delay_ms, 2000);
                assert_eq!(error, "rate limited");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn as_final_answer_extracts_text_from_message_end() {
        let msg = make_assistant();
        let ev = AgentEvent::MessageEnd {
            message_id: "msg_1".into(),
            message: msg,
        };
        let (id, turn, text) = ev.as_final_answer().unwrap();
        assert_eq!(id, "msg_1");
        assert_eq!(turn, "turn_1");
        assert_eq!(text, "ok");
    }

    #[test]
    fn as_final_answer_returns_none_for_progress_kind() {
        let mut msg = make_assistant();
        msg.kind = crate::AssistantMessageKind::Progress;
        let ev = AgentEvent::MessageEnd {
            message_id: "msg_1".into(),
            message: msg,
        };
        assert!(ev.as_final_answer().is_none());
        assert!(ev.as_progress().is_some());
    }

    #[test]
    fn as_progress_extracts_text_from_message_end() {
        let mut msg = make_assistant();
        msg.kind = crate::AssistantMessageKind::Progress;
        let ev = AgentEvent::MessageEnd {
            message_id: "msg_1".into(),
            message: msg,
        };
        let (id, turn, text) = ev.as_progress().unwrap();
        assert_eq!(id, "msg_1");
        assert_eq!(turn, "turn_1");
        assert_eq!(text, "ok");
    }

    #[test]
    fn as_final_answer_returns_none_for_non_message_end() {
        let ev = AgentEvent::TextDelta {
            message_id: "msg_1".into(),
            text: "hello".into(),
        };
        assert!(ev.as_final_answer().is_none());
        assert!(ev.as_progress().is_none());
    }
}
