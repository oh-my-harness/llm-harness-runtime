use futures::future::BoxFuture;
use llm_harness_types::{ContentBlock, Tool, ToolContext, ToolError, ToolResult};
use serde_json::Value;

use crate::tool_registry::{ToolDefinition, ToolExecutionMode as RuntimeExecMode};

/// Adapts a `ToolDefinition` (from a `ToolSource`) into a core `Tool` trait object.
///
/// For MCP-originated tools, prefer `McpToolAdapter` from the `mcp` crate,
/// which holds `McpClientHandle` and can dispatch `call_tool()` on `execute()`.
///
/// `ToolSourceAdapter` is for non-MCP tools where the definition carries a
/// `custom_converter` that pre-processes arguments, or as a metadata-only
/// placeholder that returns the converted args as output.
pub struct ToolSourceAdapter {
    definition: ToolDefinition,
}

impl ToolSourceAdapter {
    /// Create an adapter from a `ToolDefinition`.
    pub fn from_definition(definition: ToolDefinition) -> Self {
        Self { definition }
    }
}

impl Tool for ToolSourceAdapter {
    fn name(&self) -> &str {
        &self.definition.name
    }

    fn description(&self) -> &str {
        &self.definition.description
    }

    fn parameters_schema(&self) -> &Value {
        &self.definition.parameters_schema
    }

    fn execution_mode(&self) -> llm_harness_types::ToolExecutionMode {
        match self.definition.execution_mode {
            RuntimeExecMode::Parallel => llm_harness_types::ToolExecutionMode::Parallel,
            RuntimeExecMode::Sequential => llm_harness_types::ToolExecutionMode::Sequential,
        }
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, std::result::Result<ToolResult, ToolError>> {
        Box::pin(async move {
            let converted = if let Some(ref converter) = self.definition.custom_converter {
                converter(args).map_err(ToolError::InvalidArguments)?
            } else {
                args
            };
            Ok(ToolResult {
                content: vec![ContentBlock::Text {
                    text: format!(
                        "tool '{}' args: {}",
                        self.definition.name,
                        serde_json::to_string(&converted).unwrap_or_default()
                    ),
                }],
                details: converted,
                terminate: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_registry::ToolExecutionMode as RuntimeExecMode;
    use std::sync::Arc;

    #[test]
    fn tool_source_adapter_metadata() {
        let def = ToolDefinition {
            name: "hello".into(),
            description: "Say hello".into(),
            parameters_schema: serde_json::json!({"type": "object"}),
            execution_mode: RuntimeExecMode::Sequential,
            custom_converter: None,
        };
        let adapter = ToolSourceAdapter::from_definition(def);
        assert_eq!(adapter.name(), "hello");
        assert_eq!(adapter.description(), "Say hello");
        assert!(matches!(
            adapter.execution_mode(),
            llm_harness_types::ToolExecutionMode::Sequential
        ));
    }

    #[test]
    fn tool_source_adapter_parallel_execution_mode() {
        let def = ToolDefinition {
            name: "search".into(),
            description: "Search".into(),
            parameters_schema: serde_json::Value::Null,
            execution_mode: RuntimeExecMode::Parallel,
            custom_converter: None,
        };
        let adapter = ToolSourceAdapter::from_definition(def);
        assert!(matches!(
            adapter.execution_mode(),
            llm_harness_types::ToolExecutionMode::Parallel
        ));
    }

    #[tokio::test]
    async fn tool_source_adapter_custom_converter_applied() {
        let def = ToolDefinition {
            name: "convert".into(),
            description: "Convert args".into(),
            parameters_schema: serde_json::Value::Null,
            execution_mode: RuntimeExecMode::Parallel,
            custom_converter: Some(Arc::new(|mut v: Value| {
                if let Value::Object(ref mut m) = v {
                    m.insert("converted".into(), Value::Bool(true));
                }
                Ok(v)
            })),
        };
        let adapter = ToolSourceAdapter::from_definition(def);
        let dummy_ctx = build_dummy_ctx();
        let result = adapter
            .execute(serde_json::json!({"key": "val"}), &dummy_ctx)
            .await
            .unwrap();
        assert!(result.details.get("converted").is_some());
    }

    fn build_dummy_ctx() -> ToolContext {
        use llm_harness_types::{AssistantMessage, UnsupportedEnv};
        use tokio_util::sync::CancellationToken;

        ToolContext {
            env: Arc::new(UnsupportedEnv::new()),
            abort: CancellationToken::new(),
            tool_use_id: "t1".into(),
            turn_index: 0,
            assistant_message: Arc::new(AssistantMessage {
                content: vec![],
                stop_reason: None,
                timestamp: chrono::Utc::now(),
                provider: None,
                api: None,
                model: None,
                usage: None,
                error_message: None,
            }),
            update_tx: tokio::sync::mpsc::channel(1).0,
        }
    }
}
