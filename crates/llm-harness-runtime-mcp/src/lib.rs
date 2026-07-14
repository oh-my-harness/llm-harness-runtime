use std::sync::Arc;

use anyhow::Result;
use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_runtime::platform::mcp::{McpClientHandle, McpError};
use llm_harness_runtime::platform::prompt::{PromptSource, PromptSourceTemplate};

use llm_harness_types::{ContentBlock, Tool, ToolContext, ToolError, ToolResult};
use serde_json::Value;

/// Discovers tools from an MCP server and adapts them as core `Tool` objects.
///
/// Call `into_adapters()` to get `McpToolAdapter` instances that hold the
/// `McpClientHandle` and dispatch `execute()` to `call_tool()`.
pub struct McpToolSource {
    client: Arc<dyn McpClientHandle>,
}

impl McpToolSource {
    /// Create a new `McpToolSource` backed by the given MCP client.
    pub fn new(client: Arc<dyn McpClientHandle>) -> Self {
        Self { client }
    }

    /// Discover tools and return them as `McpToolAdapter` instances.
    pub async fn into_adapters(&self) -> Result<Vec<McpToolAdapter>> {
        let tools = self.client.list_tools().await?;
        Ok(tools
            .into_iter()
            .map(|t| McpToolAdapter {
                tool_name: t.name.clone(),
                description: t.description.clone(),
                parameters_schema: t.input_schema.clone(),
                client: self.client.clone(),
            })
            .collect())
    }
}

/// Adapts a single MCP tool + client as a core `Tool` trait object.
///
/// Holds the `McpClientHandle` and dispatches `execute()` to `call_tool()`.
pub struct McpToolAdapter {
    tool_name: String,
    description: String,
    parameters_schema: Value,
    client: Arc<dyn McpClientHandle>,
}

impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> &Value {
        &self.parameters_schema
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, std::result::Result<ToolResult, ToolError>> {
        Box::pin(async move {
            match self.client.call_tool(&self.tool_name, args).await {
                Ok(mcp_result) => {
                    let content = mcp_result
                        .content
                        .into_iter()
                        .filter_map(|c| c.text.map(|t| ContentBlock::Text { text: t }))
                        .collect();
                    Ok(ToolResult {
                        content,
                        details: Value::Null,
                        terminate: false,
                    })
                }
                Err(McpError::Protocol { source }) => Err(ToolError::Execution(source.to_string())),
                Err(McpError::ToolExecution { content }) => {
                    let text = content
                        .into_iter()
                        .filter_map(|c| c.text)
                        .collect::<Vec<_>>()
                        .join("\n");
                    Ok(ToolResult {
                        content: vec![ContentBlock::Text { text }],
                        details: Value::Null,
                        terminate: false,
                    })
                }
            }
        })
    }
}

/// Adapts an `McpClientHandle` as a `PromptSource`.
///
/// v0.2 stub — MCP `prompts/list` is not implemented yet.
/// Returns an empty list; full implementation is deferred to v0.3.
pub struct McpPromptSource {
    _client: Arc<dyn McpClientHandle>,
}

impl McpPromptSource {
    /// Create a new `McpPromptSource`.
    pub fn new(client: Arc<dyn McpClientHandle>) -> Self {
        Self { _client: client }
    }
}

impl PromptSource for McpPromptSource {
    fn discover(&self) -> BoxFuture<'_, Result<Vec<PromptSourceTemplate>>> {
        async move { Ok(vec![]) }.boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_runtime::platform::mcp::{McpContent, McpResult, McpToolDef};
    use std::sync::Mutex;

    struct StubMcpClient {
        tools: Mutex<Vec<McpToolDef>>,
    }

    impl McpClientHandle for StubMcpClient {
        fn list_tools(&self) -> BoxFuture<'_, Result<Vec<McpToolDef>>> {
            let tools = self.tools.lock().unwrap().clone();
            async move { Ok(tools) }.boxed()
        }

        fn call_tool(
            &self,
            name: &str,
            _arguments: Value,
        ) -> BoxFuture<'_, std::result::Result<McpResult, McpError>> {
            let name = name.to_owned();
            async move {
                Ok(McpResult {
                    content: vec![McpContent {
                        content_type: "text".into(),
                        text: Some(format!("result from {name}")),
                        data: None,
                        mime_type: None,
                    }],
                    is_error: false,
                })
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn mcp_tool_source_into_adapters() {
        let client = Arc::new(StubMcpClient {
            tools: Mutex::new(vec![McpToolDef {
                name: "read".into(),
                description: "Read a file".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }]),
        });

        let source = McpToolSource::new(client);
        let adapters = source.into_adapters().await.unwrap();
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].name(), "read");
    }

    #[tokio::test]
    async fn mcp_tool_adapter_execute_returns_text() {
        let client = Arc::new(StubMcpClient {
            tools: Mutex::new(vec![McpToolDef {
                name: "greet".into(),
                description: "Greet".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }]),
        });

        let source = McpToolSource::new(client);
        let adapters = source.into_adapters().await.unwrap();
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].name(), "greet");

        // Build minimal ToolContext.
        use llm_harness_types::UnsupportedEnv;
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;

        let ctx = ToolContext {
            env: Arc::new(UnsupportedEnv::new()),
            abort: CancellationToken::new(),
            tool_use_id: "t1".into(),
            turn_index: 0,
            assistant_message: Arc::new(llm_harness_loop::test_utils::test_assistant_message(
                vec![],
            )),
            update_tx: tokio::sync::mpsc::channel(1).0,
        };

        let result = adapters[0]
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(!result.terminate);
        assert_eq!(result.content.len(), 1);
    }

    #[tokio::test]
    async fn mcp_prompt_source_returns_empty() {
        let client = Arc::new(StubMcpClient {
            tools: Mutex::new(vec![]),
        });
        let source = McpPromptSource::new(client);
        let templates = source.discover().await.unwrap();
        assert!(templates.is_empty());
    }
}
