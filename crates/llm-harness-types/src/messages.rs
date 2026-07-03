use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ContentBlock, StopReason};

/// 单次 LLM 调用的 token 用量。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenUsage {
    /// Prompt token 数。
    pub input_tokens: u32,
    /// Completion token 数。
    pub output_tokens: u32,
    /// Anthropic prompt cache 命中的 token 数。
    pub cache_read_tokens: u32,
    /// Anthropic prompt cache 新写入的 token 数。
    pub cache_creation_tokens: u32,
}

impl TokenUsage {
    /// 所有 token 的合计。
    pub fn total_tokens(&self) -> u32 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_creation_tokens
    }
}

/// Assistant message display and persistence semantics.
///
/// UIs should render `FinalAnswer` as the durable assistant response for a
/// user turn. `Progress` is model-visible context and may be shown in progress
/// views, but should not be restored as the final assistant bubble.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssistantMessageKind {
    /// Intermediate assistant text, usually followed by tool calls or another turn.
    Progress,
    /// Durable user-visible answer for the turn.
    #[default]
    FinalAnswer,
}

/// 用户消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    /// 消息内容块列表。
    pub content: Vec<ContentBlock>,
    /// 消息创建时间。
    pub timestamp: DateTime<Utc>,
}

/// LLM 助手消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// UI/display semantics for this assistant message.
    #[serde(default)]
    pub kind: AssistantMessageKind,
    /// Stable ID for this assistant message when produced by the runtime loop.
    pub message_id: String,
    /// Stable ID for the user turn that produced this assistant message.
    pub turn_id: String,
    /// 消息内容块列表（含 Text、Thinking、ToolUse 等）。
    pub content: Vec<ContentBlock>,
    /// LLM 停止生成的原因。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// 消息创建时间。
    pub timestamp: DateTime<Utc>,
    /// 生成此消息的 LLM provider 名（如 `"anthropic"`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// 使用的 API 类型（如 `"messages"`、`"chat"`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// 使用的模型 ID。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 本次调用的 token 用量；compaction 估算依赖此字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    /// LLM 返回错误时保存的错误文本快照。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

impl AssistantMessage {
    /// Concatenate user-visible text blocks, excluding thinking and tool-use blocks.
    pub fn text_content(&self) -> String {
        let mut text = String::new();
        for block in &self.content {
            if let ContentBlock::Text { text: block_text } = block {
                text.push_str(block_text);
            }
        }
        text
    }
}

/// 工具执行结果消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultMessage {
    /// 对应的 LLM tool call ID。
    pub tool_use_id: String,
    /// 发送给 LLM 的结果内容块。
    pub content: Vec<ContentBlock>,
    /// 工具执行是否失败。
    pub is_error: bool,
    /// 消息创建时间。
    pub timestamp: DateTime<Utc>,
}

/// Compaction 生成的摘要消息；由框架在 compaction 时插入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionSummaryMessage {
    /// 摘要文本。
    pub summary: String,
    /// 消息创建时间。
    pub timestamp: DateTime<Utc>,
}

/// 分支导航时生成的摘要消息；由框架在 navigate_tree 时插入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchSummaryMessage {
    /// 摘要文本。
    pub summary: String,
    /// 消息创建时间。
    pub timestamp: DateTime<Utc>,
}

/// 应用层自定义消息；必须由 `ConvertToLlmHook` 提供转换器才能进入 LLM 上下文。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomMessage {
    /// 应用层自定义的消息类别标签（如 `"artifact"`、`"notification"`）。
    pub r#type: String,
    /// 任意 JSON 负载。
    pub data: serde_json::Value,
    /// 消息创建时间。
    pub timestamp: DateTime<Utc>,
}

/// Agent 内部消息联合体；会进入 session log 并经 compaction 处理。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AgentMessage {
    /// 用户消息。
    User(UserMessage),
    /// LLM 助手消息。
    Assistant(AssistantMessage),
    /// 工具执行结果消息。
    ToolResult(ToolResultMessage),
    /// 框架生成的分支摘要消息。
    BranchSummary(BranchSummaryMessage),
    /// 框架生成的 compaction 摘要消息。
    CompactionSummary(CompactionSummaryMessage),
    /// 应用层自定义消息。
    Custom(CustomMessage),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContentBlock;

    fn text_block(s: &str) -> ContentBlock {
        ContentBlock::Text { text: s.into() }
    }

    #[test]
    fn agent_message_serde_user() {
        let msg = AgentMessage::User(UserMessage {
            content: vec![text_block("hello")],
            timestamp: chrono::Utc::now(),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let msg2: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(msg2, AgentMessage::User(_)));
    }

    #[test]
    fn agent_message_serde_assistant() {
        let msg = AgentMessage::Assistant(AssistantMessage {
            kind: AssistantMessageKind::FinalAnswer,
            message_id: "test_msg".into(),
            turn_id: "test_turn".into(),
            content: vec![text_block("response")],
            stop_reason: Some(crate::StopReason::EndTurn),
            timestamp: chrono::Utc::now(),
            provider: Some("anthropic".into()),
            api: Some("messages".into()),
            model: Some("claude-3".into()),
            usage: Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }),
            error_message: None,
        });
        let json = serde_json::to_string(&msg).unwrap();
        let msg2: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(msg2, AgentMessage::Assistant(_)));
    }

    #[test]
    fn tool_result_message_serde() {
        let msg = AgentMessage::ToolResult(ToolResultMessage {
            tool_use_id: "call_1".into(),
            content: vec![text_block("result")],
            is_error: false,
            timestamp: chrono::Utc::now(),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let msg2: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(msg2, AgentMessage::ToolResult(_)));
    }

    #[test]
    fn custom_message_serde() {
        let msg = AgentMessage::Custom(CustomMessage {
            r#type: "artifact".into(),
            data: serde_json::json!({"url": "https://example.com"}),
            timestamp: chrono::Utc::now(),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let msg2: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(msg2, AgentMessage::Custom(_)));
    }

    #[test]
    fn token_usage_total() {
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 20,
            cache_creation_tokens: 10,
        };
        assert_eq!(u.total_tokens(), 180);
    }

    #[test]
    fn assistant_message_none_fields_omitted_in_json() {
        let msg = AssistantMessage {
            kind: AssistantMessageKind::FinalAnswer,
            message_id: "test_msg".into(),
            turn_id: "test_turn".into(),
            content: vec![text_block("hi")],
            stop_reason: None,
            timestamp: chrono::Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        // 逐字段断言 None 值被省略，而非用 "null" 子串匹配（防误报）
        assert!(
            !json.contains(r#""stop_reason""#),
            "stop_reason should be omitted: {json}"
        );
        assert!(
            !json.contains(r#""provider""#),
            "provider should be omitted: {json}"
        );
        assert!(!json.contains(r#""api""#), "api should be omitted: {json}");
        assert!(
            !json.contains(r#""model""#),
            "model should be omitted: {json}"
        );
        assert!(
            !json.contains(r#""usage""#),
            "usage should be omitted: {json}"
        );
        assert!(
            !json.contains(r#""error_message""#),
            "error_message should be omitted: {json}"
        );
        // round-trip
        let msg2: AssistantMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg2.kind, AssistantMessageKind::FinalAnswer);
        assert!(msg2.stop_reason.is_none());
        assert!(msg2.provider.is_none());
    }

    #[test]
    fn assistant_message_kind_and_ids_roundtrip() {
        let msg = AssistantMessage {
            kind: AssistantMessageKind::Progress,
            message_id: "msg_1".into(),
            turn_id: "turn_1".into(),
            content: vec![text_block("checking")],
            stop_reason: Some(crate::StopReason::ToolUse),
            timestamp: chrono::Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        };

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""kind":"progress""#), "{json}");
        assert!(json.contains(r#""message_id":"msg_1""#), "{json}");
        assert!(json.contains(r#""turn_id":"turn_1""#), "{json}");

        let decoded: AssistantMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.kind, AssistantMessageKind::Progress);
        assert_eq!(decoded.message_id, "msg_1");
        assert_eq!(decoded.turn_id, "turn_1");
    }

    #[test]
    fn assistant_message_missing_kind_defaults_to_final_answer() {
        let json = serde_json::json!({
            "content": [{"type": "text", "text": "legacy"}],
            "timestamp": chrono::Utc::now(),
            "message_id": "msg_1",
            "turn_id": "turn_1",
        });

        let decoded: AssistantMessage = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.kind, AssistantMessageKind::FinalAnswer);
    }

    #[test]
    fn assistant_message_text_content_concatenates_visible_text_only() {
        let msg = AssistantMessage {
            kind: AssistantMessageKind::FinalAnswer,
            message_id: "test_msg".into(),
            turn_id: "test_turn".into(),
            content: vec![
                text_block("hello "),
                ContentBlock::Thinking {
                    thinking: "hidden".into(),
                    signature: None,
                },
                text_block("world"),
            ],
            stop_reason: Some(crate::StopReason::EndTurn),
            timestamp: chrono::Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        };

        assert_eq!(msg.text_content(), "hello world");
    }
}
