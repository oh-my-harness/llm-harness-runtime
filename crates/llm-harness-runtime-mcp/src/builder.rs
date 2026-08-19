use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use llm_harness_agent::HarnessBuilder;
use llm_harness_agent::{AgentHarness, HarnessHooks, Session};
use llm_harness_types::{AgentError, BeforeRunCtx, BeforeRunHook, BeforeRunResult, ExecutionEnv};
use parking_lot::RwLock;

use crate::config::{McpConfigFile, McpServerConfig};
use crate::manager::McpManager;
use crate::types::{McpContent, McpPromptArguments};
use crate::utils::sanitize_name;
use crate::{McpSession, McpToolSource};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpResourceSelector {
    Exact { server: String, uri: String },
    Prefix { server: String, uri_prefix: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpResourceInjectionPolicy {
    pub selectors: Vec<McpResourceSelector>,
    pub max_resources: usize,
    pub max_bytes_per_resource: usize,
    pub max_total_bytes: usize,
    pub max_total_chars: usize,
    pub strict: bool,
}

impl Default for McpResourceInjectionPolicy {
    fn default() -> Self {
        Self {
            selectors: Vec::new(),
            max_resources: 16,
            max_bytes_per_resource: 256 * 1024,
            max_total_bytes: 1024 * 1024,
            max_total_chars: 200_000,
            strict: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpManagerOwnership {
    Owned,
    Shared,
}

pub struct McpRuntimeHandle {
    manager: Arc<McpManager>,
    ownership: McpManagerOwnership,
}

impl McpRuntimeHandle {
    pub(crate) fn new(manager: Arc<McpManager>, ownership: McpManagerOwnership) -> Self {
        Self { manager, ownership }
    }

    pub fn manager(&self) -> &Arc<McpManager> {
        &self.manager
    }

    pub fn ownership(&self) -> McpManagerOwnership {
        self.ownership
    }

    pub async fn shutdown_if_owned(&self) -> Result<()> {
        if self.ownership == McpManagerOwnership::Owned {
            self.manager.disconnect_all().await?;
        }
        Ok(())
    }
}

impl Drop for McpRuntimeHandle {
    fn drop(&mut self) {
        if self.ownership == McpManagerOwnership::Owned {
            self.manager.cancel_background_tasks();
        }
    }
}

pub struct McpHarnessBuilder {
    inner: HarnessBuilder,
    mcp_servers: Vec<(String, McpServerConfig)>,
    mcp_config_files: Vec<PathBuf>,
    mcp_manager: Option<Arc<McpManager>>,
    resource_policy: Option<McpResourceInjectionPolicy>,
    lock_file: Option<PathBuf>,
}

pub trait HarnessBuilderMcpExt {
    fn with_mcp(self) -> McpHarnessBuilder;
}

impl HarnessBuilderMcpExt for HarnessBuilder {
    fn with_mcp(self) -> McpHarnessBuilder {
        McpHarnessBuilder::new(self)
    }
}
impl McpHarnessBuilder {
    pub fn new(inner: HarnessBuilder) -> Self {
        Self {
            inner,
            mcp_servers: Vec::new(),
            mcp_config_files: Vec::new(),
            mcp_manager: None,
            resource_policy: None,
            lock_file: None,
        }
    }

    pub fn mcp_server(mut self, name: impl Into<String>, config: McpServerConfig) -> Self {
        self.mcp_servers.push((name.into(), config));
        self
    }

    pub fn mcp_config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.mcp_config_files.push(path.into());
        self
    }

    pub fn with_mcp_manager(mut self, manager: Arc<McpManager>) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    pub fn resource_injection(mut self, policy: McpResourceInjectionPolicy) -> Self {
        self.resource_policy = Some(policy);
        self
    }

    /// Set the MCP lock file path. When set, the builder constructs
    /// McpManager with this path. When None, defaults to ".mcp-lock.json".
    pub fn lock_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.lock_file = Some(path.into());
        self
    }

    /// Set version_policy for a specific server. The server must already
    /// have been added via mcp_server(). This updates the config in-place.
    pub fn version_policy(mut self, server: &str, policy: crate::config::VersionPolicy) -> Self {
        for (name, config) in &mut self.mcp_servers {
            if name == server {
                match config {
                    McpServerConfig::Stdio {
                        version_policy: vp, ..
                    }
                    | McpServerConfig::HttpOrSse {
                        version_policy: vp, ..
                    } => {
                        *vp = policy;
                    }
                }
                break;
            }
        }
        self
    }

    pub async fn build(self, env: Arc<dyn ExecutionEnv>) -> Result<McpAgentHarness> {
        let PreparedMcpBuild {
            inner,
            runtime,
            prompts,
        } = self.prepare().await?;
        match inner.build(env).await {
            Ok(harness) => Ok(McpAgentHarness {
                harness,
                runtime,
                prompts,
            }),
            Err(error) => {
                runtime.shutdown_if_owned().await?;
                Err(error.into())
            }
        }
    }

    pub async fn build_with_session(
        self,
        env: Arc<dyn ExecutionEnv>,
        session: Session,
    ) -> Result<McpAgentHarness> {
        let PreparedMcpBuild {
            inner,
            runtime,
            prompts,
        } = self.prepare().await?;
        match inner.build_with_session(env, session) {
            Ok(harness) => Ok(McpAgentHarness {
                harness,
                runtime,
                prompts,
            }),
            Err(error) => {
                runtime.shutdown_if_owned().await?;
                Err(error.into())
            }
        }
    }

    async fn prepare(self) -> Result<PreparedMcpBuild> {
        let McpHarnessBuilder {
            mut inner,
            mcp_servers,
            mcp_config_files,
            mcp_manager,
            resource_policy,
            lock_file,
        } = self;

        let mut configs = HashMap::new();
        for path in mcp_config_files {
            let file = McpConfigFile::from_file(&path).await?;
            for (name, config) in file.mcp_servers {
                if configs.insert(name.clone(), config).is_some() {
                    tracing::warn!(server = name, file = %path.display(), "overriding duplicate MCP server config");
                }
            }
        }
        for (name, config) in mcp_servers {
            if configs.insert(name.clone(), config).is_some() {
                tracing::warn!(
                    server = name,
                    "programmatic MCP server overrides file config"
                );
            }
        }
        let lock_path = lock_file.or(Some(PathBuf::from(".mcp-lock.json")));
        let (manager, ownership) = match mcp_manager {
            Some(manager) => (manager, McpManagerOwnership::Shared),
            None => {
                let manager = McpManager::with_credential_provider(
                    None,
                    Arc::new(crate::manager::default_connector),
                    lock_path,
                    Arc::new(crate::credential::EnvCredentialProvider),
                );
                (Arc::new(manager), McpManagerOwnership::Owned)
            }
        };
        if !configs.is_empty() {
            manager.discover_and_connect(configs).await?;
        }
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
        for adapter in adapters {
            inner = inner.tool(Arc::new(adapter));
        }

        if let Some(policy) = resource_policy {
            let mut hooks = HarnessHooks::none();
            hooks
                .before_run
                .push(Arc::new(McpResourceInjectionHook::new(
                    Arc::clone(&manager),
                    policy,
                )));
            inner = inner.hooks(hooks);
        }

        let prompts = match McpPromptRegistry::discover(Arc::clone(&manager)).await {
            Ok(prompts) => Arc::new(prompts),
            Err(error) => {
                runtime.shutdown_if_owned().await?;
                return Err(error);
            }
        };
        Ok(PreparedMcpBuild {
            inner,
            runtime,
            prompts,
        })
    }
}

struct PreparedMcpBuild {
    inner: HarnessBuilder,
    runtime: Arc<McpRuntimeHandle>,
    prompts: Arc<McpPromptRegistry>,
}

pub struct McpAgentHarness {
    harness: AgentHarness,
    runtime: Arc<McpRuntimeHandle>,
    prompts: Arc<McpPromptRegistry>,
}

impl McpAgentHarness {
    pub fn list_mcp_prompts(&self) -> Vec<McpPromptMeta> {
        self.prompts.list()
    }

    pub async fn prompt_from_mcp(&self, name: &str, arguments: McpPromptArguments) -> Result<()> {
        let text = self.prompts.invoke(name, arguments).await?;
        self.harness.prompt(text).await?;
        Ok(())
    }

    pub fn mcp_manager(&self) -> &Arc<McpManager> {
        self.runtime.manager()
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.runtime.shutdown_if_owned().await
    }
}

impl Deref for McpAgentHarness {
    type Target = AgentHarness;

    fn deref(&self) -> &Self::Target {
        &self.harness
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPromptMeta {
    pub server: String,
    pub name: String,
    pub full_name: String,
    pub description: String,
    pub arguments: Vec<crate::types::McpPromptArgument>,
}

struct PromptEntry {
    meta: McpPromptMeta,
    session: Arc<McpSession>,
}

pub struct McpPromptRegistry {
    prompts: RwLock<HashMap<String, PromptEntry>>,
}

impl McpPromptRegistry {
    pub async fn discover(manager: Arc<McpManager>) -> Result<Self> {
        let registry = Self {
            prompts: RwLock::new(HashMap::new()),
        };
        for (server, session) in manager.sessions() {
            if session.capabilities().prompts.is_none() {
                continue;
            }
            for prompt in session.list_prompts().await? {
                let full_name = format!(
                    "mcp__{}_{}",
                    sanitize_name(&server),
                    sanitize_name(&prompt.name)
                );
                let meta = McpPromptMeta {
                    server: server.clone(),
                    name: prompt.name,
                    full_name: full_name.clone(),
                    description: prompt.description,
                    arguments: prompt.arguments,
                };
                registry.prompts.write().insert(
                    full_name,
                    PromptEntry {
                        meta,
                        session: Arc::clone(&session),
                    },
                );
            }
        }
        Ok(registry)
    }

    pub fn list(&self) -> Vec<McpPromptMeta> {
        let mut prompts = self
            .prompts
            .read()
            .values()
            .map(|entry| entry.meta.clone())
            .collect::<Vec<_>>();
        prompts.sort_by(|left, right| left.full_name.cmp(&right.full_name));
        prompts
    }

    pub async fn invoke(&self, name: &str, arguments: McpPromptArguments) -> Result<String> {
        let (session, prompt_name) = {
            let prompts = self.prompts.read();
            let entry = prompts
                .get(name)
                .ok_or_else(|| anyhow!("unknown MCP prompt '{name}'"))?;
            (Arc::clone(&entry.session), entry.meta.name.clone())
        };
        let result = session.get_prompt(&prompt_name, arguments).await?;
        Ok(format_prompt_messages(result.messages))
    }
}

pub struct McpResourceInjectionHook {
    manager: Arc<McpManager>,
    policy: McpResourceInjectionPolicy,
}

impl McpResourceInjectionHook {
    pub fn new(manager: Arc<McpManager>, policy: McpResourceInjectionPolicy) -> Self {
        Self { manager, policy }
    }

    async fn collect_resources(&self) -> Result<Vec<InjectedResource>> {
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        for (selector_index, selector) in self.policy.selectors.iter().enumerate() {
            match selector {
                McpResourceSelector::Exact { server, uri } => {
                    if seen.insert((server.clone(), uri.clone())) {
                        candidates.push((selector_index, server.clone(), uri.clone()));
                    }
                }
                McpResourceSelector::Prefix { server, uri_prefix } => {
                    let resources = async {
                        let session = self
                            .manager
                            .session(server)
                            .ok_or_else(|| anyhow!("MCP server '{server}' is not connected"))?;
                        Ok::<_, anyhow::Error>(session.list_resources().await?)
                    }
                    .await;
                    let mut resources = match resources {
                        Ok(resources) => resources
                            .into_iter()
                            .filter(|resource| resource.uri.starts_with(uri_prefix))
                            .map(|resource| resource.uri)
                            .collect::<Vec<_>>(),
                        Err(error) if self.policy.strict => return Err(error),
                        Err(error) => {
                            tracing::warn!(server, uri_prefix, %error, "skipping MCP resource prefix");
                            continue;
                        }
                    };
                    resources.sort();
                    for uri in resources {
                        if seen.insert((server.clone(), uri.clone())) {
                            candidates.push((selector_index, server.clone(), uri));
                        }
                    }
                }
            }
        }
        candidates.sort();
        candidates.truncate(self.policy.max_resources);

        let mut injected = Vec::new();
        let mut total_bytes = 0_usize;
        let mut total_chars = 0_usize;
        for (_, server, uri) in candidates {
            let result = async {
                let session = self
                    .manager
                    .session(&server)
                    .ok_or_else(|| anyhow!("MCP server '{server}' is not connected"))?;
                let contents = session.read_resource(&uri).await?.contents;
                let mut byte_count = 0_usize;
                let mut parts = Vec::new();
                for content in contents {
                    if let Some(text) = content.text {
                        byte_count = byte_count.saturating_add(text.len());
                        parts.push(text);
                    } else if let Some(blob) = content.blob {
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(blob)
                            .with_context(|| format!("invalid base64 in MCP resource '{uri}'"))?;
                        byte_count = byte_count.saturating_add(bytes.len());
                        parts.push(format!("[binary resource, {} bytes]", bytes.len()));
                    }
                }
                if byte_count > self.policy.max_bytes_per_resource {
                    return Err(anyhow!(
                        "MCP resource '{uri}' is {byte_count} bytes; limit is {}",
                        self.policy.max_bytes_per_resource
                    ));
                }
                let body = parts.join("\n");
                Ok((byte_count, body))
            }
            .await;

            let (byte_count, body) = match result {
                Ok(resource) => resource,
                Err(error) if self.policy.strict => return Err(error),
                Err(error) => {
                    tracing::warn!(server, uri, %error, "skipping MCP resource");
                    continue;
                }
            };
            let block = format!(
                "<resource server=\"{}\" uri=\"{}\">\n{}\n</resource>",
                escape_attribute(&server),
                escape_attribute(&uri),
                body
            );
            let char_count = block.chars().count();
            if total_bytes.saturating_add(byte_count) > self.policy.max_total_bytes
                || total_chars.saturating_add(char_count) > self.policy.max_total_chars
            {
                let error = anyhow!("MCP resource injection total limit reached before '{uri}'");
                if self.policy.strict {
                    return Err(error);
                }
                tracing::warn!(server, uri, %error, "skipping MCP resource");
                continue;
            }
            total_bytes += byte_count;
            total_chars += char_count;
            injected.push(InjectedResource { block });
        }
        Ok(injected)
    }
}

impl BeforeRunHook for McpResourceInjectionHook {
    fn before_run<'a>(
        &'a self,
        ctx: BeforeRunCtx<'a>,
    ) -> futures::future::BoxFuture<'a, Result<BeforeRunResult, AgentError>> {
        Box::pin(async move {
            let resources = self
                .collect_resources()
                .await
                .map_err(|error| AgentError::ResourceLimitExceeded(error.to_string()))?;
            if !resources.is_empty() {
                let prompt = ctx.system_prompt.get_or_insert_with(String::new);
                for resource in resources {
                    if !prompt.is_empty() {
                        prompt.push_str("\n\n");
                    }
                    prompt.push_str(&resource.block);
                }
            }
            Ok(BeforeRunResult {
                additional_messages: Vec::new(),
                system_prompt: None,
            })
        })
    }
}

#[derive(Debug)]
struct InjectedResource {
    block: String,
}

fn format_prompt_messages(messages: Vec<crate::types::McpPromptMessage>) -> String {
    messages
        .into_iter()
        .map(|message| {
            let content = match message.content {
                McpContent::Text { text, .. } => text,
                McpContent::Image { mime_type, .. } => format!("[Image: {mime_type}]"),
                McpContent::Audio { mime_type, .. } => format!("[Audio: {mime_type}]"),
                McpContent::ResourceLink { uri, name, .. } => {
                    format!("[Resource link: {name} ({uri})]")
                }
                McpContent::Resource { resource, .. } => match resource.text {
                    Some(text) => format!("[Resource: {}]\n{text}", resource.uri),
                    None => format!("[Resource: {}]", resource.uri),
                },
            };
            format!("{}: {content}", message.role)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn escape_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::manager::SessionConnector;
    use crate::transport::{
        CloseHandler, McpTransport, NotificationHandler, RequestHandler, TransportResult,
    };
    use crate::types::McpPromptMessage;

    struct FeatureTransport {
        connected: AtomicBool,
    }

    #[async_trait]
    impl McpTransport for FeatureTransport {
        async fn request(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> TransportResult<serde_json::Value> {
            Ok(match method {
                "initialize" => json!({
                    "protocolVersion": crate::session::PREFERRED_PROTOCOL_VERSION,
                    "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                    "serverInfo": {"name": "fixture", "version": "1"}
                }),
                "tools/list" => json!({"tools": []}),
                "resources/list" => json!({
                    "resources": [
                        {"uri": "doc:b", "name": "b"},
                        {"uri": "other:c", "name": "c"},
                        {"uri": "doc:a", "name": "a"}
                    ]
                }),
                "resources/read" => {
                    let uri = params["uri"].as_str().unwrap();
                    json!({"contents": [{"uri": uri, "text": format!("body:{uri}")}]})
                }
                "prompts/list" => json!({
                    "prompts": [{
                        "name": "review",
                        "description": "Review",
                        "arguments": [{"name": "subject", "required": true}]
                    }]
                }),
                "prompts/get" => json!({
                    "messages": [{
                        "role": "user",
                        "content": {
                            "type": "text",
                            "text": format!("review {}", params["arguments"]["subject"].as_str().unwrap())
                        }
                    }]
                }),
                other => panic!("unexpected method {other}"),
            })
        }

        async fn notify(&self, _method: &str, _params: serde_json::Value) -> TransportResult<()> {
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.connected.load(Ordering::Acquire)
        }

        async fn close(&self) -> TransportResult<()> {
            self.connected.store(false, Ordering::Release);
            Ok(())
        }

        fn set_protocol_version(&self, _version: &str) {}

        fn on_request(&self, _handler: RequestHandler) {}

        fn on_notification(&self, _handler: NotificationHandler) {}

        fn on_close(&self, _handler: CloseHandler) {}

        fn clear_on_close(&self) {}
    }

    async fn feature_manager() -> Arc<McpManager> {
        let connector: SessionConnector = Arc::new(|_config| {
            Box::pin(async move {
                let transport: Arc<dyn McpTransport> = Arc::new(FeatureTransport {
                    connected: AtomicBool::new(true),
                });
                Ok(Arc::new(McpSession::connect(transport).await?))
            })
        });
        let manager = Arc::new(McpManager::with_connector(None, connector));
        manager
            .discover_and_connect(HashMap::from([(
                "fixture".to_owned(),
                McpServerConfig::Stdio {
                    command: "fixture".to_owned(),
                    args: Vec::new(),
                    env: HashMap::new(),
                    cwd: None,
                    timeout: Some(5_000),
                    version_policy: Default::default(),
                    credentials: None,
                },
            )]))
            .await
            .unwrap();
        manager
    }

    #[test]
    fn resource_policy_defaults_are_bounded_and_disabled() {
        let policy = McpResourceInjectionPolicy::default();
        assert!(policy.selectors.is_empty());
        assert_eq!(policy.max_resources, 16);
        assert_eq!(policy.max_bytes_per_resource, 256 * 1024);
        assert_eq!(policy.max_total_bytes, 1024 * 1024);
        assert_eq!(policy.max_total_chars, 200_000);
        assert!(!policy.strict);
    }

    #[test]
    fn formats_dynamic_prompt_content() {
        let messages: Vec<McpPromptMessage> = serde_json::from_value(json!([
            {"role": "user", "content": {"type": "text", "text": "hello"}},
            {
                "role": "assistant",
                "content": {"type": "image", "data": "AA==", "mimeType": "image/png"}
            }
        ]))
        .unwrap();

        assert_eq!(
            format_prompt_messages(messages),
            "user: hello\nassistant: [Image: image/png]"
        );
    }

    #[test]
    fn escapes_resource_selector_metadata() {
        assert_eq!(escape_attribute("a&\"<>"), "a&amp;&quot;&lt;&gt;");
    }

    #[tokio::test]
    async fn resource_selectors_are_explicit_stable_and_bounded() {
        let hook = McpResourceInjectionHook::new(
            feature_manager().await,
            McpResourceInjectionPolicy {
                selectors: vec![
                    McpResourceSelector::Exact {
                        server: "fixture".to_owned(),
                        uri: "special:z".to_owned(),
                    },
                    McpResourceSelector::Prefix {
                        server: "fixture".to_owned(),
                        uri_prefix: "doc:".to_owned(),
                    },
                ],
                max_resources: 2,
                ..McpResourceInjectionPolicy::default()
            },
        );

        let resources = hook.collect_resources().await.unwrap();

        assert_eq!(resources.len(), 2);
        assert!(resources[0].block.contains("special:z"));
        assert!(resources[1].block.contains("doc:a"));
        assert!(
            resources
                .iter()
                .all(|resource| !resource.block.contains("doc:b"))
        );
    }

    #[tokio::test]
    async fn strict_resource_limits_fail_instead_of_injecting() {
        let hook = McpResourceInjectionHook::new(
            feature_manager().await,
            McpResourceInjectionPolicy {
                selectors: vec![McpResourceSelector::Exact {
                    server: "fixture".to_owned(),
                    uri: "doc:a".to_owned(),
                }],
                max_bytes_per_resource: 1,
                strict: true,
                ..McpResourceInjectionPolicy::default()
            },
        );

        let error = hook.collect_resources().await.unwrap_err();

        assert!(error.to_string().contains("limit is 1"));
    }

    #[tokio::test]
    async fn prompt_registry_discovers_namespaced_prompts_and_invokes_them() {
        let registry = McpPromptRegistry::discover(feature_manager().await)
            .await
            .unwrap();

        let prompts = registry.list();
        assert_eq!(prompts[0].full_name, "mcp__fixture_review");
        let text = registry
            .invoke(
                "mcp__fixture_review",
                HashMap::from([("subject".to_owned(), "Rust".to_owned())]),
            )
            .await
            .unwrap();
        assert_eq!(text, "user: review Rust");
    }
}
