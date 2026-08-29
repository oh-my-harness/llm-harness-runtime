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
    pub(crate) custom_handler: Option<Arc<dyn CustomMessageConverter>>,
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
            use crate::type_bridge::{
                content_block_to_request, content_block_to_response, data_block_to_request,
            };
            let mut out = Vec::with_capacity(messages.len());
            for msg in messages {
                match msg {
                    AgentMessage::User(u) => {
                        let mut content = Vec::with_capacity(u.content.len());
                        for cb in &u.content {
                            if let Some(rc) = content_block_to_request(cb)? {
                                content.push(rc);
                            }
                        }
                        out.push(AdapterMessage::User(content));
                    }
                    AgentMessage::Assistant(a) => {
                        let content =
                            a.content.iter().filter_map(content_block_to_response).collect();
                        out.push(AdapterMessage::Assistant(content));
                    }
                    AgentMessage::ToolResult(t) => {
                        let content = t
                            .content
                            .iter()
                            .map(data_block_to_request)
                            .collect::<Result<Vec<_>, _>>()?;
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
            // Keep this conversion provider-neutral. Provider adapters own
            // wire-format constraints such as Anthropic user/assistant
            // alternation and OpenAI tool-call adjacency.
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn text_block(s: &str) -> ContentBlock {
        ContentBlock::Text { text: s.into() }
    }

    fn persisted_full() -> PersistedToolProjection {
        PersistedToolProjection {
            kind: ToolProjectionKind::Full,
            metadata: ProjectionMetadata::default(),
        }
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
        let msg = AgentMessage::Assistant({
            let mut m = crate::test_utils::test_assistant_message(vec![
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
            ]);
            m.kind = AssistantMessageKind::Progress;
            m.stop_reason = Some(StopReason::ToolUse);
            m
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
            content: vec![DataBlock::text("done")],
            is_error: false,
            projection: persisted_full(),
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
    async fn tool_result_followed_by_user_preserves_messages() {
        let conv = DefaultConvertToLlm::new();
        let msgs = vec![
            AgentMessage::ToolResult(ToolResultMessage {
                tool_use_id: "c1".into(),
                content: vec![DataBlock::text("done")],
                is_error: false,
                projection: persisted_full(),
                timestamp: Utc::now(),
            }),
            AgentMessage::User(UserMessage {
                content: vec![text_block("continue")],
                timestamp: Utc::now(),
            }),
        ];
        let result = conv.convert(&msgs).await.unwrap();
        assert_eq!(result.len(), 2);
        assert!(matches!(&result[0], AdapterMessage::Tool { .. }));
        assert!(matches!(&result[1], AdapterMessage::User(c) if {
            matches!(&c[0], RequestContent::Text(t) if t == "continue")
        }));
    }

    #[tokio::test]
    async fn two_tools_in_a_row_remain_adjacent() {
        let conv = DefaultConvertToLlm::new();
        let msgs = vec![
            AgentMessage::ToolResult(ToolResultMessage {
                tool_use_id: "c1".into(),
                content: vec![DataBlock::text("first")],
                is_error: false,
                projection: persisted_full(),
                timestamp: Utc::now(),
            }),
            AgentMessage::ToolResult(ToolResultMessage {
                tool_use_id: "c2".into(),
                content: vec![DataBlock::text("second")],
                is_error: false,
                projection: persisted_full(),
                timestamp: Utc::now(),
            }),
        ];
        let result = conv.convert(&msgs).await.unwrap();
        assert_eq!(result.len(), 2);
        assert!(matches!(&result[0], AdapterMessage::Tool { .. }));
        assert!(matches!(&result[1], AdapterMessage::Tool { .. }));
    }

    #[tokio::test]
    async fn assistant_tool_calls_and_results_preserve_openai_adjacency() {
        let conv = DefaultConvertToLlm::new();
        let msgs = vec![
            AgentMessage::Assistant({
                let mut m = crate::test_utils::test_assistant_message(vec![
                    ContentBlock::ToolUse {
                        id: "c1".into(),
                        name: "first".into(),
                        input: serde_json::json!({}),
                    },
                    ContentBlock::ToolUse {
                        id: "c2".into(),
                        name: "second".into(),
                        input: serde_json::json!({}),
                    },
                ]);
                m.stop_reason = Some(StopReason::ToolUse);
                m
            }),
            AgentMessage::ToolResult(ToolResultMessage {
                tool_use_id: "c1".into(),
                content: vec![DataBlock::text("first done")],
                is_error: false,
                projection: persisted_full(),
                timestamp: Utc::now(),
            }),
            AgentMessage::ToolResult(ToolResultMessage {
                tool_use_id: "c2".into(),
                content: vec![DataBlock::text("second done")],
                is_error: false,
                projection: persisted_full(),
                timestamp: Utc::now(),
            }),
        ];
        let result = conv.convert(&msgs).await.unwrap();
        assert_eq!(result.len(), 3);
        assert!(matches!(&result[0], AdapterMessage::Assistant(_)));
        assert!(matches!(
            &result[1],
            AdapterMessage::Tool { invocation_id, .. } if invocation_id == "c1"
        ));
        assert!(matches!(
            &result[2],
            AdapterMessage::Tool { invocation_id, .. } if invocation_id == "c2"
        ));
    }

    #[tokio::test]
    async fn user_followed_by_user_preserves_messages() {
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
        assert_eq!(result.len(), 2);
        assert!(matches!(&result[0], AdapterMessage::User(_)));
        assert!(matches!(&result[1], AdapterMessage::User(_)));
    }

    #[tokio::test]
    async fn user_assistant_user_unchanged() {
        let conv = DefaultConvertToLlm::new();
        let msgs = vec![
            AgentMessage::User(UserMessage {
                content: vec![text_block("hi")],
                timestamp: Utc::now(),
            }),
            AgentMessage::Assistant({
                let mut m = crate::test_utils::test_assistant_message(vec![ContentBlock::Text {
                    text: "hello".into(),
                }]);
                m.stop_reason = Some(StopReason::EndTurn);
                m
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
    async fn branch_summary_followed_by_user_preserves_messages() {
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
        assert_eq!(result.len(), 2);
        assert!(matches!(&result[0], AdapterMessage::User(_)));
        assert!(matches!(&result[1], AdapterMessage::User(_)));
    }
}
