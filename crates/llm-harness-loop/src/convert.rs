use std::sync::Arc;

use futures::future::BoxFuture;
use llm_adapter::types::{Message as AdapterMessage, RequestContent};
use llm_harness_types::*;

/// AgentMessage → `llm_adapter::types::Message` 的转换钩子。
pub trait ConvertToLlmHook: Send + Sync {
    /// Convert a slice of `AgentMessage` to adapter `Message` list.
    fn convert<'a>(
        &'a self,
        messages: &'a [AgentMessage],
    ) -> BoxFuture<'a, Result<Vec<AdapterMessage>, AgentError>>;
}

/// 自定义消息转换器 — `DefaultConvertToLlm` 用于 `Custom` 变体。
pub trait CustomMessageConverter: Send + Sync {
    /// Convert a single `CustomMessage` to an adapter `Message`.
    fn convert<'a>(
        &'a self,
        msg: &'a CustomMessage,
    ) -> BoxFuture<'a, Result<AdapterMessage, AgentError>>;
}

/// 框架提供的默认转换器。
pub struct DefaultConvertToLlm {
    /// Optional handler for `AgentMessage::Custom` variants.
    pub custom_handler: Option<Arc<dyn CustomMessageConverter>>,
}

impl DefaultConvertToLlm {
    /// Create with no custom handler.
    pub fn new() -> Self {
        Self {
            custom_handler: None,
        }
    }

    /// Set a custom message converter for `AgentMessage::Custom` variants.
    pub fn with_custom_converter(mut self, c: Arc<dyn CustomMessageConverter>) -> Self {
        self.custom_handler = Some(c);
        self
    }
}

impl Default for DefaultConvertToLlm {
    fn default() -> Self {
        Self::new()
    }
}

impl ConvertToLlmHook for DefaultConvertToLlm {
    fn convert<'a>(
        &'a self,
        messages: &'a [AgentMessage],
    ) -> BoxFuture<'a, Result<Vec<AdapterMessage>, AgentError>> {
        Box::pin(async move {
            use crate::type_bridge::{content_block_to_request, content_block_to_response};
            let mut out = Vec::with_capacity(messages.len());
            for msg in messages {
                match msg {
                    AgentMessage::User(u) => {
                        let content =
                            u.content.iter().filter_map(content_block_to_request).collect();
                        out.push(AdapterMessage::User(content));
                    }
                    AgentMessage::Assistant(a) => {
                        let content =
                            a.content.iter().filter_map(content_block_to_response).collect();
                        out.push(AdapterMessage::Assistant(content));
                    }
                    AgentMessage::ToolResult(t) => {
                        let content =
                            t.content.iter().filter_map(content_block_to_request).collect();
                        out.push(AdapterMessage::Tool {
                            invocation_id: t.tool_use_id.clone(),
                            content,
                            is_error: t.is_error,
                        });
                    }
                    AgentMessage::BranchSummary(b) => {
                        let text = format!("<summary>\n{}\n</summary>", b.summary);
                        out.push(AdapterMessage::User(vec![RequestContent::Text(text)]));
                    }
                    AgentMessage::CompactionSummary(c) => {
                        let text = format!("<summary>\n{}\n</summary>", c.summary);
                        out.push(AdapterMessage::User(vec![RequestContent::Text(text)]));
                    }
                    AgentMessage::Custom(c) => match &self.custom_handler {
                        Some(h) => out.push(h.convert(c).await?),
                        None => {
                            return Err(AgentError::Internal(
                                "custom message has no converter; provide CustomMessageConverter via DefaultConvertToLlm::with_custom_converter".into(),
                            ))
                        }
                    },
                }
            }
            // Post-process: ensure user/assistant alternation for providers
            // (notably Anthropic) that require it.  A Tool message is encoded
            // as a user-role message in Anthropic's wire format, so Tool
            // followed by User would violate alternation.
            ensure_alternation(&mut out);
            Ok(out)
        })
    }
}

/// Check whether the message will be sent with a user-like role to providers
/// that enforce user/assistant alternation (e.g. Anthropic).
///
/// Returns `true` if `msg` is a [`Message::User`] or [`Message::Tool`], since
/// both are encoded as `role: "user"` in Anthropic's wire format.
fn is_user_role(msg: &AdapterMessage) -> bool {
    matches!(msg, AdapterMessage::User(_) | AdapterMessage::Tool { .. })
}

/// Ensure message alternation by inserting empty `Assistant` messages between
/// consecutive user-role messages.
///
/// Some providers (notably Anthropic) require messages to strictly alternate
/// between `user` and `assistant` roles.  A `Tool` message is encoded as a
/// `user`-role message (containing a `tool_result` block), so a `Tool` followed
/// by a `User` (or two `Tool` messages, or a `User` followed by a `User`)
/// would violate the alternation requirement and cause a 400 error.
///
/// This function inserts an empty `Assistant` message as a separator wherever
/// alternation would otherwise be violated.  Empty assistant messages are
/// silently consumed by Anthropic without observable effect.
fn ensure_alternation(messages: &mut Vec<AdapterMessage>) {
    let mut i = 1;
    while i < messages.len() {
        if is_user_role(&messages[i - 1]) && is_user_role(&messages[i]) {
            messages.insert(i, AdapterMessage::Assistant(vec![]));
            i += 1; // Skip the inserted assistant message
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn text_block(s: &str) -> ContentBlock {
        ContentBlock::Text { text: s.into() }
    }

    #[tokio::test]
    async fn user_message_converts_text() {
        let conv = DefaultConvertToLlm::new();
        let msg = AgentMessage::User(UserMessage {
            content: vec![text_block("hi")],
            timestamp: Utc::now(),
        });
        let result = conv.convert(&[msg]).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], AdapterMessage::User(c) if c.len() == 1));
    }

    #[tokio::test]
    async fn assistant_message_with_tool_use_and_thinking() {
        let conv = DefaultConvertToLlm::new();
        let msg = AgentMessage::Assistant(AssistantMessage {
            kind: AssistantMessageKind::Progress,
            message_id: None,
            turn_id: None,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "plan".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "doing it".into(),
                },
                ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                },
            ],
            stop_reason: Some(StopReason::ToolUse),
            timestamp: Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        });
        let result = conv.convert(&[msg]).await.unwrap();
        assert_eq!(result.len(), 1);
        if let AdapterMessage::Assistant(content) = &result[0] {
            assert_eq!(content.len(), 3); // Thinking, Text, ToolInvocation
        } else {
            panic!("expected Assistant message");
        }
    }

    #[tokio::test]
    async fn tool_result_converts() {
        let conv = DefaultConvertToLlm::new();
        let msg = AgentMessage::ToolResult(ToolResultMessage {
            tool_use_id: "c1".into(),
            content: vec![text_block("done")],
            is_error: false,
            timestamp: Utc::now(),
        });
        let result = conv.convert(&[msg]).await.unwrap();
        assert!(
            matches!(&result[0], AdapterMessage::Tool { invocation_id, is_error, .. }
                if invocation_id == "c1" && !is_error)
        );
    }

    #[tokio::test]
    async fn branch_summary_becomes_user_message() {
        let conv = DefaultConvertToLlm::new();
        let msg = AgentMessage::BranchSummary(BranchSummaryMessage {
            summary: "previous branch did X".into(),
            timestamp: Utc::now(),
        });
        let result = conv.convert(&[msg]).await.unwrap();
        assert!(matches!(&result[0], AdapterMessage::User(c) if {
            if let RequestContent::Text(t) = &c[0] {
                t.contains("previous branch did X")
            } else {
                false
            }
        }));
    }

    #[tokio::test]
    async fn compaction_summary_becomes_user_message() {
        let conv = DefaultConvertToLlm::new();
        let msg = AgentMessage::CompactionSummary(CompactionSummaryMessage {
            summary: "compact summary".into(),
            timestamp: Utc::now(),
        });
        let result = conv.convert(&[msg]).await.unwrap();
        assert!(matches!(&result[0], AdapterMessage::User(c) if {
            if let RequestContent::Text(t) = &c[0] {
                t.contains("compact summary")
            } else {
                false
            }
        }));
    }

    #[tokio::test]
    async fn custom_without_handler_returns_err() {
        let conv = DefaultConvertToLlm::new();
        let msg = AgentMessage::Custom(CustomMessage {
            r#type: "artifact".into(),
            data: serde_json::Value::Null,
            timestamp: Utc::now(),
        });
        assert!(conv.convert(&[msg]).await.is_err());
    }

    #[tokio::test]
    async fn custom_with_handler_delegates() {
        struct FixedHandler;
        impl CustomMessageConverter for FixedHandler {
            fn convert<'a>(
                &'a self,
                _: &'a CustomMessage,
            ) -> BoxFuture<'a, Result<AdapterMessage, AgentError>> {
                Box::pin(async {
                    Ok(AdapterMessage::User(vec![RequestContent::Text(
                        "custom".into(),
                    )]))
                })
            }
        }
        let conv = DefaultConvertToLlm::new().with_custom_converter(Arc::new(FixedHandler));
        let msg = AgentMessage::Custom(CustomMessage {
            r#type: "artifact".into(),
            data: serde_json::Value::Null,
            timestamp: chrono::Utc::now(),
        });
        let result = conv.convert(&[msg]).await.unwrap();
        assert!(matches!(&result[0], AdapterMessage::User(c) if {
            if let RequestContent::Text(t) = &c[0] {
                t == "custom"
            } else {
                false
            }
        }));
    }

    #[tokio::test]
    async fn tool_result_followed_by_user_inserts_empty_assistant() {
        let conv = DefaultConvertToLlm::new();
        let msgs = vec![
            AgentMessage::ToolResult(ToolResultMessage {
                tool_use_id: "c1".into(),
                content: vec![text_block("done")],
                is_error: false,
                timestamp: Utc::now(),
            }),
            AgentMessage::User(UserMessage {
                content: vec![text_block("continue")],
                timestamp: Utc::now(),
            }),
        ];
        let result = conv.convert(&msgs).await.unwrap();
        assert_eq!(result.len(), 3);
        assert!(matches!(&result[0], AdapterMessage::Tool { .. }));
        assert!(matches!(&result[1], AdapterMessage::Assistant(c) if c.is_empty()));
        assert!(matches!(&result[2], AdapterMessage::User(c) if {
            matches!(&c[0], RequestContent::Text(t) if t == "continue")
        }));
    }

    #[tokio::test]
    async fn two_tools_in_a_row_separated_by_assistant() {
        let conv = DefaultConvertToLlm::new();
        let msgs = vec![
            AgentMessage::ToolResult(ToolResultMessage {
                tool_use_id: "c1".into(),
                content: vec![text_block("first")],
                is_error: false,
                timestamp: Utc::now(),
            }),
            AgentMessage::ToolResult(ToolResultMessage {
                tool_use_id: "c2".into(),
                content: vec![text_block("second")],
                is_error: false,
                timestamp: Utc::now(),
            }),
        ];
        let result = conv.convert(&msgs).await.unwrap();
        assert_eq!(result.len(), 3);
        assert!(matches!(&result[0], AdapterMessage::Tool { .. }));
        assert!(matches!(&result[1], AdapterMessage::Assistant(c) if c.is_empty()));
        assert!(matches!(&result[2], AdapterMessage::Tool { .. }));
    }

    #[tokio::test]
    async fn user_followed_by_user_inserts_empty_assistant() {
        let conv = DefaultConvertToLlm::new();
        let msgs = vec![
            AgentMessage::User(UserMessage {
                content: vec![text_block("first")],
                timestamp: Utc::now(),
            }),
            AgentMessage::User(UserMessage {
                content: vec![text_block("second")],
                timestamp: Utc::now(),
            }),
        ];
        let result = conv.convert(&msgs).await.unwrap();
        assert_eq!(result.len(), 3);
        assert!(matches!(&result[0], AdapterMessage::User(_)));
        assert!(matches!(&result[1], AdapterMessage::Assistant(c) if c.is_empty()));
        assert!(matches!(&result[2], AdapterMessage::User(_)));
    }

    #[tokio::test]
    async fn user_assistant_user_unchanged() {
        let conv = DefaultConvertToLlm::new();
        let msgs = vec![
            AgentMessage::User(UserMessage {
                content: vec![text_block("hi")],
                timestamp: Utc::now(),
            }),
            AgentMessage::Assistant(AssistantMessage {
                kind: AssistantMessageKind::FinalAnswer,
                message_id: None,
                turn_id: None,
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                stop_reason: Some(StopReason::EndTurn),
                timestamp: Utc::now(),
                provider: None,
                api: None,
                model: None,
                usage: None,
                error_message: None,
            }),
            AgentMessage::User(UserMessage {
                content: vec![text_block("again")],
                timestamp: Utc::now(),
            }),
        ];
        let result = conv.convert(&msgs).await.unwrap();
        assert_eq!(result.len(), 3);
    }

    #[tokio::test]
    async fn branch_summary_followed_by_user_inserts_assistant() {
        let conv = DefaultConvertToLlm::new();
        let msgs = vec![
            AgentMessage::BranchSummary(BranchSummaryMessage {
                summary: "sum".into(),
                timestamp: Utc::now(),
            }),
            AgentMessage::User(UserMessage {
                content: vec![text_block("continue")],
                timestamp: Utc::now(),
            }),
        ];
        let result = conv.convert(&msgs).await.unwrap();
        assert_eq!(result.len(), 3);
        assert!(matches!(&result[0], AdapterMessage::User(_)));
        assert!(matches!(&result[1], AdapterMessage::Assistant(c) if c.is_empty()));
        assert!(matches!(&result[2], AdapterMessage::User(_)));
    }

    #[test]
    fn ensure_alternation_noop_on_alternating_messages() {
        let mut msgs = vec![
            AdapterMessage::User(vec![]),
            AdapterMessage::Assistant(vec![]),
            AdapterMessage::User(vec![]),
        ];
        let before = msgs.clone();
        ensure_alternation(&mut msgs);
        assert_eq!(msgs, before);
    }

    #[test]
    fn ensure_alternation_single_message_unchanged() {
        let mut msgs = vec![AdapterMessage::User(vec![])];
        let before = msgs.clone();
        ensure_alternation(&mut msgs);
        assert_eq!(msgs, before);
    }

    #[test]
    fn ensure_alternation_empty_list_unchanged() {
        let mut msgs: Vec<AdapterMessage> = vec![];
        ensure_alternation(&mut msgs);
        assert!(msgs.is_empty());
    }

    #[test]
    fn ensure_alternation_three_users_adds_two_assistants() {
        let mut msgs = vec![
            AdapterMessage::User(vec![]),
            AdapterMessage::User(vec![]),
            AdapterMessage::User(vec![]),
        ];
        ensure_alternation(&mut msgs);
        assert_eq!(msgs.len(), 5);
        // Positions: 0:User, 1:Assistant, 2:User, 3:Assistant, 4:User
        assert!(matches!(&msgs[0], AdapterMessage::User(_)));
        assert!(matches!(&msgs[1], AdapterMessage::Assistant(c) if c.is_empty()));
        assert!(matches!(&msgs[2], AdapterMessage::User(_)));
        assert!(matches!(&msgs[3], AdapterMessage::Assistant(c) if c.is_empty()));
        assert!(matches!(&msgs[4], AdapterMessage::User(_)));
    }
}
