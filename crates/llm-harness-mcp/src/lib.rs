pub mod builder;
pub mod config;
pub mod credential;
pub mod json_rpc;
pub mod manager;
pub mod session;
pub mod transport;
pub mod types;
mod utils;
pub mod version_lock;

pub use builder::{
    HarnessBuilderMcpExt, McpAgentHarness, McpHarnessBuilder, McpManagerOwnership, McpPromptMeta,
    McpPromptRegistry, McpResourceInjectionHook, McpResourceInjectionPolicy, McpResourceSelector,
    McpRuntimeHandle,
};
pub use manager::{ConnectionStatus, ExposedTool, McpManager, McpManagerError, McpToolCache};
pub use session::{McpSession, McpSessionError, PREFERRED_PROTOCOL_VERSION};

use std::sync::Arc;

use anyhow::{Result, anyhow};
use base64::Engine;
use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_platform::prompt::{ArgumentDef, PromptSource, PromptSourceTemplate};
use llm_harness_types::{DataBlock, ImageSource, Tool, ToolContext, ToolFailure, ToolResult};
use llm_harness_types::{ResourceContent, ResourceDescriptor, ResourceProvider};
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

    /// Build a credential lookup function for the credential guard hook.
    /// Tools on servers with scoped_env declarations but not in any
    /// allowed_tools list are marked as Unauthorized.
    pub fn build_credential_lookup(&self) -> llm_harness_types::CredentialLookupFn {
        let tools = self.manager.get_tools();
        let scopes = self.manager.credential_scopes_snapshot();

        let mut authorized: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let mut unauthorized: std::collections::HashSet<String> = std::collections::HashSet::new();

        for tool in &tools {
            let creds = self
                .manager
                .authorized_credentials_for(&tool.server_name, &tool.tool_def.name);
            if creds.is_empty() {
                if scopes.get(&tool.server_name).is_some_and(|s| !s.is_empty()) {
                    unauthorized.insert(tool.full_name.clone());
                }
            } else {
                authorized.insert(tool.full_name.clone(), creds);
            }
        }

        llm_harness_types::build_credential_lookup(authorized, unauthorized)
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
    ) -> BoxFuture<'a, std::result::Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let result = self
                .manager
                .call_tool(&self.full_name, args)
                .await
                .map_err(|_| {
                    ToolFailure::new("mcp_call_failed", "MCP tool communication failed.")
                })?;
            let details = result.structured_content.unwrap_or(Value::Null);
            let content = format_mcp_content(result.content);
            if result.is_error {
                let message = content
                    .iter()
                    .filter_map(|block| match block {
                        DataBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut failure = ToolFailure::new(
                    "mcp_tool_error",
                    if message.is_empty() {
                        "MCP tool execution failed.".to_owned()
                    } else {
                        message
                    },
                );
                failure.details = details;
                return Err(failure);
            }
            Ok(ToolResult::full(content, details, false))
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

/// Upper bound for a single MCP-provided image (base64 payload length),
/// matching the read tool's inline-multimodal cap. MCP servers are
/// third-party code: an unbounded payload would be injected straight into
/// the model context and persisted to the session while token estimation
/// counts it as a fixed 1024.
const MAX_MCP_IMAGE_BYTES: usize = 4 * 1024 * 1024; // 4 MiB of base64 payload

fn format_mcp_content(content: Vec<McpContent>) -> Vec<DataBlock> {
    content
        .into_iter()
        .map(|content| match content {
            // Images pass through as real content blocks — a multimodal model
            // can consume them directly. Oversized payloads degrade to the
            // placeholder instead of silently bloating the context.
            McpContent::Image {
                data, mime_type, ..
            } if data.len() <= MAX_MCP_IMAGE_BYTES => DataBlock::Image {
                source: ImageSource::Base64 {
                    media_type: mime_type,
                    data,
                },
            },
            McpContent::Image { mime_type, .. } => {
                DataBlock::text(format!("[Image: {mime_type} — payload exceeds 4 MiB cap]"))
            }
            other => {
                let text = match other {
                    McpContent::Text { text, .. } => text,
                    // Audio stays a placeholder: audio modality is out of
                    // scope for the multimodal pipeline.
                    McpContent::Audio { mime_type, .. } => format!("[Audio: {mime_type}]"),
                    McpContent::ResourceLink { uri, name, .. } => {
                        format!("[Resource link: {name} ({uri})]")
                    }
                    McpContent::Resource { resource, .. } => match resource.text {
                        Some(text) => format!("[Resource: {}]\n{text}", resource.uri),
                        None => format!("[Resource: {}]", resource.uri),
                    },
                    McpContent::Image { .. } => unreachable!("handled above"),
                };
                DataBlock::text(text)
            }
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

    fn text(block: &DataBlock) -> &str {
        match block {
            DataBlock::Text { text, .. } => text,
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
        // Image passes through as a real content block.
        assert!(matches!(
            &formatted[1],
            DataBlock::Image {
                source: ImageSource::Base64 { media_type, data }
            } if media_type == "image/png" && data == "AA=="
        ));
        assert_eq!(text(&formatted[2]), "[Audio: audio/wav]");
        assert_eq!(text(&formatted[3]), "[Resource link: x (file:///x)]");
        assert_eq!(text(&formatted[4]), "[Resource: file:///x]\nbody");
    }

    #[test]
    fn oversized_image_degrades_to_placeholder() {
        let content = vec![McpContent::Image {
            data: "A".repeat(4 * 1024 * 1024 + 1),
            mime_type: "image/png".to_owned(),
            annotations: None,
            metadata: None,
        }];
        let blocks = format_mcp_content(content);
        assert!(
            matches!(&blocks[0], DataBlock::Text { text, .. } if text.contains("exceeds 4 MiB cap"))
        );
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

// ── MCP workflow assembly ─────────────────────────────────────────────────

use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::{McpConfigFile, McpServerConfig};

/// MCP 工具装配结果——消费方据此注入 `WorkflowEngineConfig` 或 `HarnessBuilder`。
///
/// 不依赖 runtime crate。字段类型只来自 `llm-harness-types` 和 mcp 内部类型。
pub struct McpParts {
    /// MCP manager（共享或独占）。
    pub manager: Arc<McpManager>,
    /// manager 所有权。
    pub ownership: McpManagerOwnership,
    /// 已发现的 MCP 工具，转为 `Arc<dyn Tool>`。
    pub tools: Vec<Arc<dyn Tool>>,
    /// 资源注入 hook（仅当配置了 resource_policy 时存在）。
    pub resource_hook: Option<Arc<McpResourceInjectionHook>>,
    /// 已发现的 MCP prompt registry。
    pub prompts: Arc<McpPromptRegistry>,
}

#[derive(Default)]
pub struct McpWorkflowConfig {
    servers: Vec<(String, McpServerConfig)>,
    config_files: Vec<PathBuf>,
    manager: Option<Arc<McpManager>>,
    resource_policy: Option<McpResourceInjectionPolicy>,
}

impl McpWorkflowConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mcp_server(mut self, name: impl Into<String>, config: McpServerConfig) -> Self {
        self.servers.push((name.into(), config));
        self
    }

    pub fn mcp_config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_files.push(path.into());
        self
    }

    pub fn with_mcp_manager(mut self, manager: Arc<McpManager>) -> Self {
        self.manager = Some(manager);
        self
    }

    pub fn resource_injection(mut self, policy: McpResourceInjectionPolicy) -> Self {
        self.resource_policy = Some(policy);
        self
    }

    /// 连接 MCP server、发现工具和 prompt、构建 resource hook。
    /// 返回消费方自行注入 runtime 引擎所需的全部零件。
    pub async fn prepare(self) -> Result<McpParts> {
        let (manager, ownership) = prepare_manager(&self).await?;
        let runtime = Arc::new(McpRuntimeHandle::new(Arc::clone(&manager), ownership));

        let adapters = match McpToolSource::new(Arc::clone(&manager))
            .into_adapters()
            .await
        {
            Ok(adapters) => adapters,
            Err(error) => {
                runtime.shutdown_if_owned().await?;
                return Err(error);
            }
        };
        let tools: Vec<Arc<dyn Tool>> = adapters
            .into_iter()
            .map(|adapter| Arc::new(adapter) as Arc<dyn Tool>)
            .collect();
        let resource_hook = self
            .resource_policy
            .map(|policy| Arc::new(McpResourceInjectionHook::new(Arc::clone(&manager), policy)));
        let prompts = match McpPromptRegistry::discover(Arc::clone(&manager)).await {
            Ok(prompts) => Arc::new(prompts),
            Err(error) => {
                runtime.shutdown_if_owned().await?;
                return Err(error);
            }
        };
        Ok(McpParts {
            manager,
            ownership,
            tools,
            resource_hook,
            prompts,
        })
    }
}

async fn prepare_manager(
    mcp: &McpWorkflowConfig,
) -> Result<(Arc<McpManager>, McpManagerOwnership)> {
    let mut configs = HashMap::new();
    for path in &mcp.config_files {
        let file = McpConfigFile::from_file(path).await?;
        for (name, config) in file.mcp_servers {
            if configs.insert(name.clone(), config).is_some() {
                tracing::warn!(server = name, file = %path.display(), "overriding duplicate MCP server config");
            }
        }
    }
    for (name, config) in &mcp.servers {
        if configs.insert(name.clone(), config.clone()).is_some() {
            tracing::warn!(
                server = name,
                "programmatic MCP server overrides file config"
            );
        }
    }
    let (manager, ownership) = match &mcp.manager {
        Some(manager) => (Arc::clone(manager), McpManagerOwnership::Shared),
        None => (Arc::new(McpManager::default()), McpManagerOwnership::Owned),
    };
    if !configs.is_empty() {
        manager.discover_and_connect(configs).await?;
    }
    Ok((manager, ownership))
}
