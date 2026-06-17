use std::sync::Arc;

use llm_adapter::types::{
    ImageSource as AdapterImageSource, RequestContent, ResponseContent, StopReason as AdapterStop,
    ToolDef, ToolInvocation, Usage,
};
use llm_harness_types::*;

pub(crate) fn convert_stop_reason(r: AdapterStop) -> StopReason {
    match r {
        AdapterStop::EndTurn => StopReason::EndTurn,
        AdapterStop::MaxTokens => StopReason::MaxTokens,
        AdapterStop::StopSequence => StopReason::StopSequence,
        AdapterStop::ToolUse => StopReason::ToolUse,
        AdapterStop::ContentFilter | AdapterStop::Other(_) => StopReason::Other,
    }
}

pub(crate) fn convert_usage(u: Usage) -> TokenUsage {
    TokenUsage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read_tokens: u.cached_input_tokens,
        cache_creation_tokens: u.cache_creation_input_tokens,
    }
}

pub(crate) fn tools_to_defs(tools: &[Arc<dyn Tool>]) -> Vec<ToolDef> {
    tools
        .iter()
        .map(|t| ToolDef::new(t.name(), t.description(), t.parameters_schema().clone()))
        .collect()
}

/// Convert a harness ContentBlock to adapter RequestContent (for User/Tool messages).
/// Skips Thinking and ToolUse blocks (not valid in user messages).
pub(crate) fn content_block_to_request(cb: &ContentBlock) -> Option<RequestContent> {
    match cb {
        ContentBlock::Text { text } => Some(RequestContent::Text(text.clone())),
        ContentBlock::Image { source } => {
            let src = match source {
                ImageSource::Base64 { media_type, data } => AdapterImageSource::Base64 {
                    media_type: media_type.clone(),
                    data: data.clone(),
                },
            };
            Some(RequestContent::Image(src))
        }
        ContentBlock::Thinking { .. } | ContentBlock::ToolUse { .. } => None,
    }
}

/// Convert a harness ContentBlock to adapter ResponseContent (for Assistant messages).
pub(crate) fn content_block_to_response(cb: &ContentBlock) -> Option<ResponseContent> {
    match cb {
        ContentBlock::Text { text } => Some(ResponseContent::Text(text.clone())),
        ContentBlock::Thinking {
            thinking,
            signature,
        } => Some(ResponseContent::Reasoning {
            text: thinking.clone(),
            signature: signature.clone(),
        }),
        ContentBlock::ToolUse { id, name, input } => {
            Some(ResponseContent::ToolInvocation(ToolInvocation {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            }))
        }
        ContentBlock::Image { .. } => Some(ResponseContent::Text(
            "[image omitted: assistant image content is not supported in provider history]".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_end_turn() {
        assert_eq!(
            convert_stop_reason(AdapterStop::EndTurn),
            StopReason::EndTurn
        );
    }

    #[test]
    fn stop_reason_tool_use() {
        assert_eq!(
            convert_stop_reason(AdapterStop::ToolUse),
            StopReason::ToolUse
        );
    }

    #[test]
    fn stop_reason_content_filter_maps_to_other() {
        assert_eq!(
            convert_stop_reason(AdapterStop::ContentFilter),
            StopReason::Other
        );
    }

    #[test]
    fn stop_reason_other_maps_to_other() {
        assert_eq!(
            convert_stop_reason(AdapterStop::Other("x".into())),
            StopReason::Other
        );
    }

    #[test]
    fn convert_usage_maps_fields() {
        let u = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: 3,
            cache_creation_input_tokens: 1,
            reasoning_tokens: 99,
        };
        let tu = convert_usage(u);
        assert_eq!(tu.input_tokens, 10);
        assert_eq!(tu.output_tokens, 5);
        assert_eq!(tu.cache_read_tokens, 3);
        assert_eq!(tu.cache_creation_tokens, 1);
    }

    #[test]
    fn tools_to_defs_maps_name_desc_schema() {
        use futures::future::BoxFuture;

        struct EchoTool;
        impl Tool for EchoTool {
            fn name(&self) -> &str {
                "echo"
            }
            fn description(&self) -> &str {
                "echoes input"
            }
            fn parameters_schema(&self) -> &serde_json::Value {
                static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
                SCHEMA.get_or_init(|| serde_json::json!({"type":"object","properties":{}}))
            }
            fn execute<'a>(
                &'a self,
                _: serde_json::Value,
                _: &'a ToolContext,
            ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
                Box::pin(async {
                    Ok(ToolResult {
                        content: vec![],
                        details: serde_json::Value::Null,
                        terminate: false,
                    })
                })
            }
        }
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool)];
        let defs = tools_to_defs(&tools);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "echo");
        assert_eq!(defs[0].description, "echoes input");
    }

    #[test]
    fn content_block_text_to_request() {
        let cb = ContentBlock::Text {
            text: "hello".into(),
        };
        assert!(
            matches!(content_block_to_request(&cb), Some(RequestContent::Text(s)) if s == "hello")
        );
    }

    #[test]
    fn content_block_thinking_skipped_in_request() {
        let cb = ContentBlock::Thinking {
            thinking: "hm".into(),
            signature: None,
        };
        assert!(content_block_to_request(&cb).is_none());
    }

    #[test]
    fn content_block_text_to_response() {
        let cb = ContentBlock::Text {
            text: "reply".into(),
        };
        assert!(
            matches!(content_block_to_response(&cb), Some(ResponseContent::Text(s)) if s == "reply")
        );
    }

    #[test]
    fn content_block_thinking_to_response() {
        let cb = ContentBlock::Thinking {
            thinking: "thoughts".into(),
            signature: Some("sig".into()),
        };
        let r = content_block_to_response(&cb);
        assert!(
            matches!(r, Some(ResponseContent::Reasoning { text, signature: Some(s) }) if text == "thoughts" && s == "sig")
        );
    }

    #[test]
    fn content_block_tool_use_to_response() {
        let cb = ContentBlock::ToolUse {
            id: "c1".into(),
            name: "bash".into(),
            input: serde_json::json!({"cmd":"ls"}),
        };
        let r = content_block_to_response(&cb);
        assert!(
            matches!(r, Some(ResponseContent::ToolInvocation(ti)) if ti.id == "c1" && ti.name == "bash")
        );
    }

    #[test]
    fn content_block_image_to_response_placeholder() {
        let cb = ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "AA==".into(),
            },
        };
        let r = content_block_to_response(&cb);
        assert!(
            matches!(r, Some(ResponseContent::Text(text)) if text.contains("image omitted")),
            "assistant image blocks should not be silently dropped"
        );
    }
}
