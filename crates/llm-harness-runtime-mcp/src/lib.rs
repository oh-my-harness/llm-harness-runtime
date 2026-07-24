pub mod builder;
pub mod config;
pub mod json_rpc;
pub mod manager;
pub mod session;
pub mod transport;
pub mod types;
pub mod workflow;

pub use builder::{
    HarnessBuilderMcpExt, McpAgentHarness, McpHarnessBuilder, McpManagerOwnership, McpPromptMeta,
    McpPromptRegistry, McpResourceInjectionHook, McpResourceInjectionPolicy, McpResourceSelector,
    McpRuntimeHandle,
};
pub use manager::{ConnectionStatus, ExposedTool, McpManager, McpManagerError, McpToolCache};
pub use session::{McpSession, McpSessionError, PREFERRED_PROTOCOL_VERSION};
pub use workflow::{McpWorkflowConfig, McpWorkflowEngine};

use std::sync::Arc;

use anyhow::{Result, anyhow};
use base64::Engine;
use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_runtime::platform::prompt::{ArgumentDef, PromptSource, PromptSourceTemplate};
use llm_harness_runtime::platform::resource::{
    ResourceContent, ResourceDescriptor, ResourceProvider,
};
use llm_harness_types::{ContentBlock, Tool, ToolContext, ToolError, ToolResult};
use serde_json::Value;

use crate::types::{McpContent, McpResourceContent};

/// Discovers the manager's current tool view and adapts it to core `Tool`s.
pub struct McpToolSource {
    manager: Arc<McpManager>,
}

impl McpToolSource {
    pub fn new(manager: Arc<McpManager>) -> Self {
        Self { manager }
    }

    pub async fn into_adapters(&self) -> Result<Vec<McpToolAdapter>> {
        Ok(self
            .manager
            .get_tools()
            .into_iter()
            .map(|tool| McpToolAdapter {
                full_name: tool.full_name,
                description: tool.tool_def.description,
                parameters_schema: tool.tool_def.input_schema,
                manager: Arc::clone(&self.manager),
            })
            .collect())
    }
}

/// A single manager-routed MCP tool.
pub struct McpToolAdapter {
    full_name: String,
    description: String,
    parameters_schema: Value,
    manager: Arc<McpManager>,
}

impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.full_name
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
            let result = self
                .manager
                .call_tool(&self.full_name, args)
                .await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            let details = result.structured_content.unwrap_or(Value::Null);
            let mut content = format_mcp_content(result.content);
            if result.is_error {
                for block in &mut content {
                    if let ContentBlock::Text { text } = block {
                        *text = format!("Error: {text}");
                    }
                }
            }
            Ok(ToolResult {
                content,
                details,
                terminate: false,
            })
        })
    }
}

/// Discovers MCP prompt metadata from one initialized session.
///
/// MCP prompts return messages at invocation time rather than a static template.
/// The runtime builder decides how those dynamic prompts are exposed.
pub struct McpPromptSource {
    session: Arc<McpSession>,
}

impl McpPromptSource {
    pub fn new(session: Arc<McpSession>) -> Self {
        Self { session }
    }
}

impl PromptSource for McpPromptSource {
    fn discover(&self) -> BoxFuture<'_, Result<Vec<PromptSourceTemplate>>> {
        let session = Arc::clone(&self.session);
        async move {
            Ok(session
                .list_prompts()
                .await?
                .into_iter()
                .map(|prompt| PromptSourceTemplate {
                    name: prompt.name,
                    description: prompt.description,
                    template: String::new(),
                    arguments: prompt
                        .arguments
                        .into_iter()
                        .map(|argument| ArgumentDef {
                            name: argument.name,
                            description: argument.description,
                            required: argument.required,
                        })
                        .collect(),
                })
                .collect())
        }
        .boxed()
    }
}

/// Bridges one MCP session's resources into the runtime resource abstraction.
pub struct McpResourceProvider {
    session: Arc<McpSession>,
}

impl McpResourceProvider {
    pub fn new(session: Arc<McpSession>) -> Self {
        Self { session }
    }
}

impl ResourceProvider for McpResourceProvider {
    fn list(&self) -> BoxFuture<'_, Result<Vec<ResourceDescriptor>>> {
        let session = Arc::clone(&self.session);
        async move {
            Ok(session
                .list_resources()
                .await?
                .into_iter()
                .map(|resource| ResourceDescriptor {
                    uri: resource.uri,
                    name: resource.name,
                    description: resource.description,
                    mime_type: resource.mime_type.unwrap_or_default(),
                })
                .collect())
        }
        .boxed()
    }

    fn read<'a>(&'a self, uri: &'a str) -> BoxFuture<'a, Result<ResourceContent>> {
        let session = Arc::clone(&self.session);
        async move {
            let result = session.read_resource(uri).await?;
            let item = result
                .contents
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("MCP resource '{uri}' returned no content"))?;
            resource_content(item)
        }
        .boxed()
    }
}

fn format_mcp_content(content: Vec<McpContent>) -> Vec<ContentBlock> {
    content
        .into_iter()
        .map(|content| {
            let text = match content {
                McpContent::Text { text, .. } => text,
                McpContent::Image { mime_type, .. } => {
                    format!("[Image: {mime_type}]")
                }
                McpContent::Audio { mime_type, .. } => {
                    format!("[Audio: {mime_type}]")
                }
                McpContent::ResourceLink { uri, name, .. } => {
                    format!("[Resource link: {name} ({uri})]")
                }
                McpContent::Resource { resource, .. } => match resource.text {
                    Some(text) => format!("[Resource: {}]\n{text}", resource.uri),
                    None => format!("[Resource: {}]", resource.uri),
                },
            };
            ContentBlock::Text { text }
        })
        .collect()
}

fn resource_content(item: McpResourceContent) -> Result<ResourceContent> {
    if let Some(text) = item.text {
        return Ok(ResourceContent::Text(text));
    }
    if let Some(blob) = item.blob {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(blob)
            .map_err(|error| anyhow!("invalid base64 for MCP resource '{}': {error}", item.uri))?;
        return Ok(ResourceContent::Blob(bytes));
    }
    Err(anyhow!(
        "MCP resource '{}' has neither text nor blob content",
        item.uri
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn text(block: &ContentBlock) -> &str {
        match block {
            ContentBlock::Text { text } => text,
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn formats_every_mcp_content_variant_without_dropping_order() {
        let content = vec![
            McpContent::Text {
                text: "hello".to_owned(),
                annotations: None,
                metadata: None,
            },
            McpContent::Image {
                data: "AA==".to_owned(),
                mime_type: "image/png".to_owned(),
                annotations: None,
                metadata: None,
            },
            McpContent::Audio {
                data: "AA==".to_owned(),
                mime_type: "audio/wav".to_owned(),
                annotations: None,
                metadata: None,
            },
            McpContent::ResourceLink {
                uri: "file:///x".to_owned(),
                name: "x".to_owned(),
                title: None,
                description: None,
                mime_type: None,
                size: None,
                annotations: None,
                metadata: None,
            },
            McpContent::Resource {
                resource: McpResourceContent {
                    uri: "file:///x".to_owned(),
                    mime_type: Some("text/plain".to_owned()),
                    text: Some("body".to_owned()),
                    blob: None,
                    metadata: None,
                },
                annotations: None,
                metadata: None,
            },
        ];

        let formatted = format_mcp_content(content);

        assert_eq!(formatted.len(), 5);
        assert_eq!(text(&formatted[0]), "hello");
        assert_eq!(text(&formatted[1]), "[Image: image/png]");
        assert_eq!(text(&formatted[2]), "[Audio: audio/wav]");
        assert_eq!(text(&formatted[3]), "[Resource link: x (file:///x)]");
        assert_eq!(text(&formatted[4]), "[Resource: file:///x]\nbody");
    }

    #[test]
    fn decodes_resource_blob_and_rejects_empty_content() {
        let blob = resource_content(McpResourceContent {
            uri: "memory://blob".to_owned(),
            mime_type: None,
            text: None,
            blob: Some("aGVsbG8=".to_owned()),
            metadata: None,
        })
        .unwrap();
        assert!(matches!(blob, ResourceContent::Blob(bytes) if bytes == b"hello"));

        let error = resource_content(McpResourceContent {
            uri: "memory://empty".to_owned(),
            mime_type: None,
            text: None,
            blob: None,
            metadata: None,
        })
        .unwrap_err();
        assert!(error.to_string().contains("neither text nor blob"));
    }

    #[test]
    fn structured_content_shape_is_preserved_by_protocol_type() {
        let result: crate::types::McpToolCallResult = serde_json::from_value(json!({
            "content": [{"type": "text", "text": "ok"}],
            "structuredContent": {"value": 1}
        }))
        .unwrap();

        assert_eq!(result.structured_content, Some(json!({"value": 1})));
    }
}
