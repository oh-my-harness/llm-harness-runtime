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
    /// 消息内容块列表（含 Text、Thinking、ToolUse 等）。
    pub content: Vec<ContentBlock>,
    /// LLM 停止生成的原因。
    pub stop_reason: Option<StopReason>,
    /// 消息创建时间。
    pub timestamp: DateTime<Utc>,
    /// 生成此消息的 LLM provider 名（如 `"anthropic"`）。
    pub provider: Option<String>,
    /// 使用的 API 类型（如 `"messages"`、`"chat"`）。
    pub api: Option<String>,
    /// 使用的模型 ID。
    pub model: Option<String>,
    /// 本次调用的 token 用量；compaction 估算依赖此字段。
    pub usage: Option<TokenUsage>,
    /// LLM 返回错误时保存的错误文本快照。
    pub error_message: Option<String>,
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
}
