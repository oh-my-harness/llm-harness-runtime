use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{ContentBlock, Tool, ToolContext, ToolError, ToolResult};

/// Default built-in tool name for opt-in final-answer delivery.
pub const DEFAULT_FINAL_ANSWER_TOOL_NAME: &str = "final_answer";

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
            ..FinalAnswerToolConfig::default()
        })
    }
}

/// Built-in final-answer tool configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalAnswerToolConfig {
    /// Tool name exposed to the provider.
    pub name: String,
    /// Behavior when the model stops without calling the final-answer tool.
    pub missing_behavior: FinalAnswerMissingBehavior,
}

impl Default for FinalAnswerToolConfig {
    fn default() -> Self {
        Self {
            name: DEFAULT_FINAL_ANSWER_TOOL_NAME.to_string(),
            missing_behavior: FinalAnswerMissingBehavior::Error,
        }
    }
}

impl FinalAnswerToolConfig {
    /// Create a strict final-answer tool contract with a custom tool name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
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

pub(crate) fn builtin_final_answer_tool(cfg: &FinalAnswerToolConfig) -> Arc<dyn Tool> {
    Arc::new(BuiltinFinalAnswerTool {
        name: cfg.name.clone(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "Final answer text to show to the user."
                }
            },
            "required": ["text"],
            "additionalProperties": false
        }),
    })
}

struct BuiltinFinalAnswerTool {
    name: String,
    schema: serde_json::Value,
}

impl Tool for BuiltinFinalAnswerTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn label(&self) -> &str {
        "Final answer"
    }

    fn description(&self) -> &str {
        "Use this only when ready to give the final answer to the user."
    }

    fn parameters_schema(&self) -> &serde_json::Value {
        &self.schema
    }

    fn execute<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let text = args
                .get("text")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ToolError::InvalidArguments("missing string field 'text'".into()))?
                .to_string();

            Ok(ToolResult {
                content: vec![ContentBlock::Text { text: text.clone() }],
                details: serde_json::json!({
                    "kind": "final_answer",
                    "text": text,
                }),
                terminate: true,
            })
        })
    }
}
