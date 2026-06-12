use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::Tool;
use serde_json::Value;

use crate::error::Result;

/// Argument converter function type used in `ToolDefinition`.
pub type ConverterFn = Arc<dyn Fn(Value) -> std::result::Result<Value, String> + Send + Sync>;

/// Human-readable tool metadata for UI display.
#[derive(Debug, Clone)]
pub struct ToolMetadata {
    /// Tool identifier (matches the `Tool::name()` value).
    pub name: String,
    /// Short display label for UI.
    pub label: String,
    /// Human-readable description.
    pub description: String,
}

/// Tool registry — manages the full set of known tools.
pub trait ToolRegistry: Send + Sync {
    /// Register a tool.
    fn register(&self, tool: Arc<dyn Tool>);

    /// Unregister a tool by name.
    fn unregister(&self, name: &str);

    /// Look up a tool by name.
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>>;

    /// List all registered tools (metadata only).
    fn list(&self) -> Vec<ToolMetadata>;

    /// Return a subset of tools by name list (management-plane filter).
    fn subset(&self, names: &[&str]) -> Vec<Arc<dyn Tool>>;
}

/// Source of tool definitions (MCP server, local file, HTTP API, etc.).
pub trait ToolSource: Send + Sync {
    /// Discover and return available tool definitions.
    fn discover(&self) -> BoxFuture<'_, Result<Vec<ToolDefinition>>>;
}

/// Raw tool definition produced by a `ToolSource`, to be adapted into a `Tool`
/// by the registry.
pub struct ToolDefinition {
    /// Tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for tool parameters.
    pub parameters_schema: Value,
    /// Execution mode hint.
    pub execution_mode: ToolExecutionMode,
    /// Optional argument converter applied before execution.
    pub custom_converter: Option<ConverterFn>,
}

/// Tool execution mode — mirrors `llm_harness_types::ToolExecutionMode`
/// for `ToolSource`-originated definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    /// Tool may run concurrently with other parallel-mode tools.
    Parallel,
    /// Tool acts as a sequential barrier.
    Sequential,
}

// ── InMemoryToolRegistry ──────────────────────────────────────────────────────

use std::collections::HashMap;
use std::sync::Mutex;

/// In-memory `ToolRegistry` — default implementation for single-process use.
pub struct InMemoryToolRegistry {
    tools: Mutex<HashMap<String, Arc<dyn Tool>>>,
}

impl InMemoryToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            tools: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry for InMemoryToolRegistry {
    fn register(&self, tool: Arc<dyn Tool>) {
        self.tools
            .lock()
            .unwrap()
            .insert(tool.name().to_string(), tool);
    }

    fn unregister(&self, name: &str) {
        self.tools.lock().unwrap().remove(name);
    }

    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.lock().unwrap().get(name).cloned()
    }

    fn list(&self) -> Vec<ToolMetadata> {
        let tools = self.tools.lock().unwrap();
        tools
            .values()
            .map(|t| ToolMetadata {
                name: t.name().to_string(),
                label: t.label().to_string(),
                description: t.description().to_string(),
            })
            .collect()
    }

    fn subset(&self, names: &[&str]) -> Vec<Arc<dyn Tool>> {
        let tools = self.tools.lock().unwrap();
        names
            .iter()
            .filter_map(|n| tools.get(*n).cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use llm_harness_types::{ToolContext, ToolError, ToolResult};

    struct StubTool {
        name: &'static str,
        desc: &'static str,
    }

    impl Tool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.desc
        }
        fn parameters_schema(&self) -> &Value {
            &Value::Null
        }
        fn execute<'a>(
            &'a self,
            _args: Value,
            _ctx: &'a ToolContext,
        ) -> BoxFuture<'a, std::result::Result<ToolResult, ToolError>> {
            Box::pin(async move {
                Ok(ToolResult {
                    content: vec![],
                    details: Value::Null,
                    terminate: false,
                })
            })
        }
    }

    #[test]
    fn tool_registry_is_object_safe() {
        fn _accept(_r: &dyn ToolRegistry) {}
    }

    #[test]
    fn tool_source_is_object_safe() {
        fn _accept(_s: &dyn ToolSource) {}
    }

    #[test]
    fn tool_definition_construction() {
        let def = ToolDefinition {
            name: "read_file".into(),
            description: "Read a file".into(),
            parameters_schema: serde_json::json!({"type": "object"}),
            execution_mode: ToolExecutionMode::Parallel,
            custom_converter: None,
        };
        assert_eq!(def.name, "read_file");
    }

    #[test]
    fn register_and_retrieve_tool() {
        let registry = InMemoryToolRegistry::new();
        registry.register(Arc::new(StubTool { name: "read", desc: "Read a file" }));
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.get("read").unwrap().name(), "read");
    }

    #[test]
    fn unregister_removes_tool() {
        let registry = InMemoryToolRegistry::new();
        registry.register(Arc::new(StubTool { name: "t1", desc: "Tool 1" }));
        assert_eq!(registry.list().len(), 1);
        registry.unregister("t1");
        assert_eq!(registry.list().len(), 0);
        assert!(registry.get("t1").is_none());
    }

    #[test]
    fn subset_filters_by_name() {
        let registry = InMemoryToolRegistry::new();
        registry.register(Arc::new(StubTool { name: "read", desc: "..." }));
        registry.register(Arc::new(StubTool { name: "write", desc: "..." }));
        registry.register(Arc::new(StubTool { name: "search", desc: "..." }));

        let subset = registry.subset(&["read", "write"]);
        assert_eq!(subset.len(), 2);
        let names: Vec<&str> = subset.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"write"));
        assert!(!names.contains(&"search"));
    }
}

