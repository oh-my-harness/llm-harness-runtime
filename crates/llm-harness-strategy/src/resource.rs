//! Resource injector — appends resource content to the system prompt tail.

use std::sync::Arc;

use anyhow::Result;
use llm_harness_types::{
    AgentContext, ResourceContent, ResourceProvider,
};

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
    use llm_harness_types::{ResourceDescriptor, ResourceContent, ResourceProvider};

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
    async fn inject_appends_to_existing_system_prompt() {
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
            .inject_all(&[Arc::new(provider)], &mut ctx)
            .await
            .unwrap();
        let prompt = ctx.system_prompt.as_ref().unwrap();
        assert!(prompt.contains("You are an assistant."));
        assert!(prompt.contains("<resource uri=\"test://doc\">"));
        assert!(prompt.contains("content of test://doc"));
    }

    #[tokio::test]
    async fn inject_creates_system_prompt_when_none() {
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
            .inject_all(&[Arc::new(provider)], &mut ctx)
            .await
            .unwrap();
        assert!(ctx.system_prompt.is_some());
        assert!(ctx
            .system_prompt
            .as_ref()
            .unwrap()
            .contains("<resource uri=\"test://doc\">"));
    }
}
