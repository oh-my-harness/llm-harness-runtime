use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpServerCapabilities {
    #[serde(default)]
    pub tools: Option<ToolsCapability>,
    #[serde(default)]
    pub resources: Option<ResourcesCapability>,
    #[serde(default)]
    pub prompts: Option<PromptsCapability>,
    #[serde(default)]
    pub logging: Option<Value>,
    #[serde(default)]
    pub completions: Option<Value>,
    #[serde(default)]
    pub tasks: Option<Value>,
    #[serde(default)]
    pub experimental: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolsCapability {
    #[serde(default, rename = "listChanged")]
    pub list_changed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcesCapability {
    #[serde(default)]
    pub subscribe: bool,
    #[serde(default, rename = "listChanged")]
    pub list_changed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptsCapability {
    #[serde(default, rename = "listChanged")]
    pub list_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpIcon {
    pub src: String,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub sizes: Option<Vec<String>>,
    #[serde(default)]
    pub theme: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpImplementation {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icons: Vec<McpIcon>,
    #[serde(default, rename = "websiteUrl")]
    pub website_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpInitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: McpServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: McpImplementation,
    #[serde(default)]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolAnnotations {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, rename = "readOnlyHint")]
    pub read_only_hint: Option<bool>,
    #[serde(default, rename = "destructiveHint")]
    pub destructive_hint: Option<bool>,
    #[serde(default, rename = "idempotentHint")]
    pub idempotent_hint: Option<bool>,
    #[serde(default, rename = "openWorldHint")]
    pub open_world_hint: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolDefinition {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(default, rename = "outputSchema")]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub annotations: Option<McpToolAnnotations>,
    #[serde(default)]
    pub icons: Vec<McpIcon>,
    #[serde(default, rename = "_meta")]
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpAnnotations {
    #[serde(default)]
    pub audience: Option<Vec<String>>,
    #[serde(default)]
    pub priority: Option<f64>,
    #[serde(default, rename = "lastModified")]
    pub last_modified: Option<String>,
}

/// Content returned by MCP tools and prompts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpContent {
    Text {
        text: String,
        #[serde(default)]
        annotations: Option<McpAnnotations>,
        #[serde(default, rename = "_meta")]
        metadata: Option<Map<String, Value>>,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(default)]
        annotations: Option<McpAnnotations>,
        #[serde(default, rename = "_meta")]
        metadata: Option<Map<String, Value>>,
    },
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(default)]
        annotations: Option<McpAnnotations>,
        #[serde(default, rename = "_meta")]
        metadata: Option<Map<String, Value>>,
    },
    ResourceLink {
        uri: String,
        name: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default, rename = "mimeType")]
        mime_type: Option<String>,
        #[serde(default)]
        size: Option<u64>,
        #[serde(default)]
        annotations: Option<McpAnnotations>,
        #[serde(default, rename = "_meta")]
        metadata: Option<Map<String, Value>>,
    },
    Resource {
        resource: McpResourceContent,
        #[serde(default)]
        annotations: Option<McpAnnotations>,
        #[serde(default, rename = "_meta")]
        metadata: Option<Map<String, Value>>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpResourceContent {
    pub uri: String,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub blob: Option<String>,
    #[serde(default, rename = "_meta")]
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolCallResult {
    pub content: Vec<McpContent>,
    #[serde(default, rename = "structuredContent")]
    pub structured_content: Option<Value>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
    #[serde(default, rename = "_meta")]
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpListToolsResult {
    #[serde(default)]
    pub tools: Vec<McpToolDefinition>,
    #[serde(default, rename = "nextCursor")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpResource {
    pub uri: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub icons: Vec<McpIcon>,
    #[serde(default)]
    pub annotations: Option<McpAnnotations>,
    #[serde(default, rename = "_meta")]
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpListResourcesResult {
    #[serde(default)]
    pub resources: Vec<McpResource>,
    #[serde(default, rename = "nextCursor")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpResourceReadResult {
    #[serde(default)]
    pub contents: Vec<McpResourceContent>,
    #[serde(default, rename = "_meta")]
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptArgument {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPrompt {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub arguments: Vec<McpPromptArgument>,
    #[serde(default)]
    pub icons: Vec<McpIcon>,
    #[serde(default, rename = "_meta")]
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpListPromptsResult {
    #[serde(default)]
    pub prompts: Vec<McpPrompt>,
    #[serde(default, rename = "nextCursor")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPromptMessage {
    pub role: String,
    pub content: McpContent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpGetPromptResult {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub messages: Vec<McpPromptMessage>,
    #[serde(default, rename = "_meta")]
    pub metadata: Option<Map<String, Value>>,
}

pub type McpPromptArguments = HashMap<String, String>;

pub mod notification_methods {
    pub const TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";
    pub const RESOURCES_LIST_CHANGED: &str = "notifications/resources/list_changed";
    pub const RESOURCES_UPDATED: &str = "notifications/resources/updated";
    pub const PROMPTS_LIST_CHANGED: &str = "notifications/prompts/list_changed";
    pub const LOGGING: &str = "notifications/message";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn initialize_result_uses_camel_case_wire_fields() {
        let result: McpInitializeResult = serde_json::from_value(json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {
                "tools": {"listChanged": true},
                "resources": {"subscribe": true, "listChanged": false}
            },
            "serverInfo": {
                "name": "fixture",
                "version": "1.0.0"
            }
        }))
        .unwrap();

        assert_eq!(result.protocol_version, "2025-11-25");
        assert!(result.capabilities.tools.unwrap().list_changed);
        assert!(result.capabilities.resources.unwrap().subscribe);
    }

    #[test]
    fn tool_definition_preserves_latest_optional_fields() {
        let tool: McpToolDefinition = serde_json::from_value(json!({
            "name": "lookup",
            "description": "Lookup a value",
            "inputSchema": {"type": "object"},
            "outputSchema": {"type": "object"},
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false
            },
            "_meta": {"source": "fixture"}
        }))
        .unwrap();

        assert!(tool.output_schema.is_some());
        assert_eq!(tool.annotations.unwrap().read_only_hint, Some(true));
        assert_eq!(
            tool.metadata.unwrap().get("source"),
            Some(&json!("fixture"))
        );
    }

    #[test]
    fn content_supports_all_current_variants() {
        let values = [
            json!({"type": "text", "text": "hello"}),
            json!({"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"}),
            json!({"type": "audio", "data": "aGVsbG8=", "mimeType": "audio/wav"}),
            json!({
                "type": "resource_link",
                "uri": "file:///tmp/a",
                "name": "a"
            }),
            json!({
                "type": "resource",
                "resource": {
                    "uri": "file:///tmp/a",
                    "text": "hello"
                }
            }),
        ];

        let parsed = values
            .into_iter()
            .map(serde_json::from_value::<McpContent>)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(matches!(parsed[0], McpContent::Text { .. }));
        assert!(matches!(parsed[1], McpContent::Image { .. }));
        assert!(matches!(parsed[2], McpContent::Audio { .. }));
        assert!(matches!(parsed[3], McpContent::ResourceLink { .. }));
        assert!(matches!(parsed[4], McpContent::Resource { .. }));
    }

    #[test]
    fn tool_result_preserves_structured_content_and_error_flag() {
        let result: McpToolCallResult = serde_json::from_value(json!({
            "content": [{"type": "text", "text": "failed"}],
            "structuredContent": {"code": "E_FIXTURE"},
            "isError": true
        }))
        .unwrap();

        assert!(result.is_error);
        assert_eq!(
            result.structured_content,
            Some(json!({"code": "E_FIXTURE"}))
        );
    }

    #[test]
    fn paginated_results_read_next_cursor() {
        let result: McpListToolsResult = serde_json::from_value(json!({
            "tools": [],
            "nextCursor": "page-2"
        }))
        .unwrap();

        assert_eq!(result.next_cursor.as_deref(), Some("page-2"));
    }
}
