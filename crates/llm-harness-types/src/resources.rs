use anyhow::Result;
use futures::future::BoxFuture;

/// Description of a context resource.
#[derive(Debug, Clone)]
pub struct ResourceDescriptor {
    /// Resource URI (e.g., `file:///path/to/doc`, `mcp://server/resource`).
    pub uri: String,
    /// Display name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// MIME type.
    pub mime_type: String,
}

/// Content of a context resource.
#[derive(Debug, Clone)]
pub enum ResourceContent {
    /// UTF-8 text content.
    Text(String),
    /// Raw binary content.
    Blob(Vec<u8>),
}

/// Context resource provider (MCP resources, local files, etc.).
pub trait ResourceProvider: Send + Sync {
    /// List available resources.
    fn list(&self) -> BoxFuture<'_, Result<Vec<ResourceDescriptor>>>;

    /// Read a specific resource by URI.
    fn read<'a>(&'a self, uri: &'a str) -> BoxFuture<'a, Result<ResourceContent>>;
}

use crate::{PromptTemplate, Skill};

/// Harness runtime resources, provided via `BeforeRunHook` context.
pub struct AgentHarnessResources {
    /// Loaded skills (metadata only; full content via `skill_read` tool).
    pub skills: Vec<Skill>,
    /// Loaded prompt templates.
    pub templates: Vec<PromptTemplate>,
}
