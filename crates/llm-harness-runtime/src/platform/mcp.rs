use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::error::Error;

use anyhow::Result;

/// MCP client handle — trait that the business layer implements.
/// Runtime does NOT bind to any specific MCP SDK.
pub trait McpClientHandle: Send + Sync {
    /// List tools from the MCP server (called once at startup).
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<McpToolDef>>>;

    /// Call a tool on the MCP server.
    fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> BoxFuture<'_, std::result::Result<McpResult, McpError>>;
}

/// MCP tool definition as returned by `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDef {
    /// Tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for tool input parameters.
    pub input_schema: Value,
}

/// Result of an MCP tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResult {
    /// Content blocks returned by the tool.
    pub content: Vec<McpContent>,
    /// Whether the tool reported an error.
    pub is_error: bool,
}

/// Content block within an MCP result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpContent {
    /// Content type (e.g., `"text"`, `"image"`, `"resource"`).
    #[serde(rename = "type")]
    pub content_type: String,
    /// Text content (for text type).
    pub text: Option<String>,
    /// Base64-encoded data (for image/blob types).
    pub data: Option<String>,
    /// MIME type for non-text content.
    pub mime_type: Option<String>,
}

/// MCP error — distinguishes protocol errors from tool execution errors.
#[derive(Debug)]
pub enum McpError {
    /// Protocol-level error: connection lost, timeout, etc. → escalate to runtime.
    Protocol { source: Box<dyn Error + Send> },
    /// Tool execution failure → return to agent for retry.
    ToolExecution { content: Vec<McpContent> },
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Protocol { source } => write!(f, "MCP protocol error: {source}"),
            McpError::ToolExecution { .. } => write!(f, "MCP tool execution error"),
        }
    }
}

impl Error for McpError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            McpError::Protocol { source } => Some(source.as_ref()),
            McpError::ToolExecution { .. } => None,
        }
    }
}
