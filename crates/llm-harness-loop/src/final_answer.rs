use std::sync::Arc;

use llm_harness_types::{
    DataBlock, Tool, ToolContext, ToolError, ToolFailure, ToolLevel, ToolResult,
};

use futures::future::BoxFuture;

/// Default built-in tool name for opt-in final-answer delivery.
pub const DEFAULT_FINAL_ANSWER_TOOL_NAME: &str = llm_harness_types::FINAL_ANSWER_TOOL_NAME;

/// Controls how the loop identifies durable final answers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FinalAnswerMode {
    /// Backward-compatible heuristic: a non-tool terminal assistant message is
    /// treated as the final answer.
    #[default]
    Heuristic,
    /// Inject a built-in tool and accept its `text` argument as the final
    /// answer. Normal terminal text is handled according to
    /// [`FinalAnswerToolConfig::missing_behavior`].
    Tool(FinalAnswerToolConfig),
}

impl FinalAnswerMode {
    /// Require the model to call the default `final_answer` tool before a turn
    /// is accepted as final.
    pub fn required_tool() -> Self {
        Self::Tool(FinalAnswerToolConfig::default())
    }

    /// Enable the default `final_answer` tool but preserve heuristic fallback
    /// when the model ends with plain text.
    pub fn tool_with_text_fallback() -> Self {
        Self::Tool(FinalAnswerToolConfig {
            missing_behavior: FinalAnswerMissingBehavior::FallbackToText,
        })
    }
}

/// Built-in final-answer tool configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalAnswerToolConfig {
    /// Behavior when the model stops without calling the final-answer tool.
    pub missing_behavior: FinalAnswerMissingBehavior,
}

impl Default for FinalAnswerToolConfig {
    fn default() -> Self {
        Self {
            missing_behavior: FinalAnswerMissingBehavior::Error,
        }
    }
}

/// Behavior when a final-answer tool contract is enabled but the model ends
/// without calling that tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalAnswerMissingBehavior {
    /// Emit `AgentEvent::Error` and do not accept the turn as final.
    Error,
    /// Preserve the legacy heuristic and treat terminal text as final.
    FallbackToText,
}

pub(crate) fn builtin_final_answer_tool() -> Arc<dyn Tool> {
    static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    let schema = SCHEMA.get_or_init(|| {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The complete final answer text to present to the user. e.g. \"The refund window is 30 days from the purchase date.\""
                }
            },
            "required": ["text"],
            "additionalProperties": false
        })
    });
    Arc::new(BuiltinFinalAnswerTool { schema })
}

struct BuiltinFinalAnswerTool {
    schema: &'static serde_json::Value,
}
impl Tool for BuiltinFinalAnswerTool {
    fn level(&self) -> ToolLevel {
        ToolLevel::BuiltIn
    }

    fn name(&self) -> &str {
        DEFAULT_FINAL_ANSWER_TOOL_NAME
    }

    fn label(&self) -> &str {
        "Final answer"
    }

    fn description(&self) -> &str {
        "Deliver the final answer to the user. Calling this tool ends the agent loop.\n\n\
         Use when: all tool calls are complete and the task is fully resolved; you have the complete answer ready to present.\n\
         Don't use for: intermediate results or progress updates (use plain text instead); partial answers while tools are still running.\n\
         Returns: The text you provide is shown to the user as the final response. The agent loop terminates immediately.\n\n\
         Examples:\n\
         {\"text\": \"The refund window is 30 days from the purchase date.\"}\n\
         {\"text\": \"I've updated the configuration file and verified the service restarts correctly.\"}"
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        self.schema
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let text = args
                .get("text")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ToolError::InvalidArguments("missing string field 'text'".into()))?
                .to_string();

            let details = serde_json::json!({
                "kind": "final_answer",
                "text": text,
            });
            Ok(ToolResult::full(vec![DataBlock::text(text)], details, true))
        })
    }
}

llm_harness_types::assert_builtin_registered!(BuiltinFinalAnswerTool => DEFAULT_FINAL_ANSWER_TOOL_NAME);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_follows_unified_format() {
        let tool = builtin_final_answer_tool();
        let desc = tool.description();
        assert!(desc.contains("Use when:"), "missing Use when section");
        assert!(
            desc.contains("Don't use for:"),
            "missing Don't use for section"
        );
        assert!(desc.contains("Returns:"), "missing Returns section");
        assert!(desc.contains("Examples:"), "missing Examples section");
    }

    #[test]
    fn text_parameter_has_concrete_example() {
        let tool = builtin_final_answer_tool();
        let schema = tool.parameters_schema();
        let text_desc = schema
            .get("properties")
            .and_then(|p| p.get("text"))
            .and_then(|t| t.get("description"))
            .and_then(|d| d.as_str())
            .expect("text parameter must have a description");
        assert!(
            text_desc.contains("e.g."),
            "text parameter description must contain a concrete e.g. example, got: {text_desc}"
        );
    }
}
