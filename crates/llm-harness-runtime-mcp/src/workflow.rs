use std::collections::HashMap;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use llm_harness_agent::HarnessHooks;
use llm_harness_runtime::workflow::engine::{
    WorkflowEngine, WorkflowEngineConfig, WorkflowRunRequest,
};
use llm_harness_runtime::workflow::error::WorkflowError;
use llm_harness_runtime::workflow::judge::StepTransitionJudge;
use llm_harness_runtime::workflow::model::Workflow;
use llm_harness_types::Tool;

use crate::builder::{
    McpManagerOwnership, McpPromptRegistry, McpResourceInjectionHook, McpResourceInjectionPolicy,
    McpRuntimeHandle,
};
use crate::config::{McpConfigFile, McpServerConfig};
use crate::{McpManager, McpToolSource};

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
}

pub struct McpWorkflowEngine {
    inner: WorkflowEngine,
    runtime: Arc<McpRuntimeHandle>,
    prompts: Arc<McpPromptRegistry>,
}

impl McpWorkflowEngine {
    pub async fn new(
        workflow: Workflow,
        mut config: WorkflowEngineConfig,
        judge: Arc<dyn StepTransitionJudge>,
        mcp: McpWorkflowConfig,
    ) -> Result<Self> {
        let (manager, ownership) = prepare_manager(&mcp).await?;
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
        let resource_hook = mcp
            .resource_policy
            .map(|policy| Arc::new(McpResourceInjectionHook::new(Arc::clone(&manager), policy)));
        let previous = config.customize_builder.take();
        config.customize_builder = Some(Arc::new(move |builder| {
            let mut builder = match &previous {
                Some(customize) => customize(builder),
                None => builder,
            };
            for tool in &tools {
                builder = builder.tool(Arc::clone(tool));
            }
            if let Some(hook) = &resource_hook {
                let mut hooks = HarnessHooks::none();
                hooks.before_run.push(hook.clone());
                builder = builder.hooks(hooks);
            }
            builder
        }));
        let prompts = match McpPromptRegistry::discover(Arc::clone(runtime.manager())).await {
            Ok(prompts) => Arc::new(prompts),
            Err(error) => {
                runtime.shutdown_if_owned().await?;
                return Err(error);
            }
        };
        match WorkflowEngine::new(workflow, config, judge) {
            Ok(inner) => Ok(Self {
                inner,
                runtime,
                prompts,
            }),
            Err(error) => {
                runtime.shutdown_if_owned().await?;
                Err(error.into())
            }
        }
    }

    pub async fn run(
        &self,
    ) -> std::result::Result<llm_harness_runtime::lifecycle::task::TaskResult, WorkflowError> {
        self.inner.run().await
    }

    pub async fn run_with_request(
        &self,
        request: WorkflowRunRequest,
    ) -> std::result::Result<llm_harness_runtime::lifecycle::task::TaskResult, WorkflowError> {
        self.inner.run_with_request(request).await
    }

    pub fn prompts(&self) -> &Arc<McpPromptRegistry> {
        &self.prompts
    }

    pub fn mcp_manager(&self) -> &Arc<McpManager> {
        self.runtime.manager()
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.runtime.shutdown_if_owned().await
    }
}

impl Deref for McpWorkflowEngine {
    type Target = WorkflowEngine;

    fn deref(&self) -> &Self::Target {
        &self.inner
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
