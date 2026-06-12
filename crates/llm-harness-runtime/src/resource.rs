use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::AgentContext;

use crate::error::Result;

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

/// Resource injector — appends resource content to the system prompt tail
/// with cache-control-compatible markers.
pub struct ResourceInjector {
    _private: (),
}

impl ResourceInjector {
    /// Create a new injector.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Inject all resources from all providers into the system prompt tail.
    pub async fn inject_all(
        &self,
        providers: &[Arc<dyn ResourceProvider>],
        context: &mut AgentContext,
    ) -> Result<()> {
        for provider in providers {
            let descriptors = provider.list().await?;
            for desc in descriptors {
                self.inject_one(provider.as_ref(), &desc.uri, context)
                    .await?;
            }
        }
        Ok(())
    }

    /// Inject a single resource by URI.
    pub async fn inject(
        &self,
        provider: &dyn ResourceProvider,
        uri: &str,
        context: &mut AgentContext,
    ) -> Result<()> {
        self.inject_one(provider, uri, context).await
    }

    async fn inject_one(
        &self,
        provider: &dyn ResourceProvider,
        uri: &str,
        context: &mut AgentContext,
    ) -> Result<()> {
        let content = provider.read(uri).await?;
        let text = match content {
            ResourceContent::Text(t) => t,
            ResourceContent::Blob(b) => format!("[binary resource, {} bytes]", b.len()),
        };
        let injected = format!("<resource uri=\"{uri}\">\n{text}\n</resource>");
        match &mut context.system_prompt {
            Some(prompt) => {
                prompt.push_str("\n\n");
                prompt.push_str(&injected);
            }
            None => {
                context.system_prompt = Some(injected);
            }
        }
        Ok(())
    }
}

impl Default for ResourceInjector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use llm_harness_types::AgentContext;

    struct StubProvider {
        resources: Vec<ResourceDescriptor>,
    }

    impl ResourceProvider for StubProvider {
        fn list(&self) -> BoxFuture<'_, Result<Vec<ResourceDescriptor>>> {
            let res = self.resources.clone();
            Box::pin(async move { Ok(res) })
        }

        fn read<'a>(&'a self, uri: &'a str) -> BoxFuture<'a, Result<ResourceContent>> {
            let text = format!("content of {uri}");
            Box::pin(async move { Ok(ResourceContent::Text(text)) })
        }
    }

    #[tokio::test]
    async fn inject_resource_appends_to_system_prompt() {
        let provider = StubProvider {
            resources: vec![ResourceDescriptor {
                uri: "test://doc".into(),
                name: "Test Doc".into(),
                description: "A test resource".into(),
                mime_type: "text/plain".into(),
            }],
        };
        let injector = ResourceInjector::new();
        let mut ctx = AgentContext {
            system_prompt: Some("You are an assistant.".into()),
            messages: vec![],
        };

        injector
            .inject_one(&provider, "test://doc", &mut ctx)
            .await
            .unwrap();

        assert!(ctx.system_prompt.as_ref().unwrap().contains("test://doc"));
    }

    #[tokio::test]
    async fn inject_resource_creates_system_prompt_if_none() {
        let provider = StubProvider {
            resources: vec![ResourceDescriptor {
                uri: "test://doc".into(),
                name: "Test Doc".into(),
                description: "A test resource".into(),
                mime_type: "text/plain".into(),
            }],
        };
        let injector = ResourceInjector::new();
        let mut ctx = AgentContext {
            system_prompt: None,
            messages: vec![],
        };

        injector
            .inject_one(&provider, "test://doc", &mut ctx)
            .await
            .unwrap();

        assert!(ctx.system_prompt.as_ref().unwrap().contains("test://doc"));
    }
}
