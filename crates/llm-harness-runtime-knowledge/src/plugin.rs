use std::sync::Arc;

use llm_harness_agent::Plugin;
use llm_harness_types::Tool;
use thiserror::Error;

use crate::{
    EvidenceAuthority, EvidenceProviderId, KnowledgeReadTool, KnowledgeRegistry,
    KnowledgeSearchTool, KnowledgeToolConfig,
};

#[derive(Debug, Error)]
pub enum KnowledgePluginBuildError {
    #[error("evidence provider is not registered with the evidence authority")]
    UnregisteredEvidenceProvider,
    #[error("knowledge tool limits must be greater than zero")]
    InvalidToolLimits,
}

pub struct KnowledgePlugin {
    registry: Arc<KnowledgeRegistry>,
    authority: Arc<EvidenceAuthority>,
    provider_id: EvidenceProviderId,
    tool_config: KnowledgeToolConfig,
}

impl KnowledgePlugin {
    pub fn new(
        registry: Arc<KnowledgeRegistry>,
        authority: Arc<EvidenceAuthority>,
        provider_id: EvidenceProviderId,
        tool_config: KnowledgeToolConfig,
    ) -> Result<Self, KnowledgePluginBuildError> {
        if !authority.is_registered(&provider_id) {
            return Err(KnowledgePluginBuildError::UnregisteredEvidenceProvider);
        }
        if tool_config.max_search_results == 0 || tool_config.max_read_bytes == 0 {
            return Err(KnowledgePluginBuildError::InvalidToolLimits);
        }

        Ok(Self {
            registry,
            authority,
            provider_id,
            tool_config,
        })
    }
}

impl Plugin for KnowledgePlugin {
    fn name(&self) -> &str {
        "knowledge"
    }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(KnowledgeSearchTool::new(
            self.registry.clone(),
            self.tool_config.clone(),
        )));
        tools.push(Arc::new(KnowledgeReadTool::new(
            self.registry.clone(),
            self.authority.clone(),
            self.provider_id.clone(),
            self.tool_config.clone(),
        )));
    }
}
