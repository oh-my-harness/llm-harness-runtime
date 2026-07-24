use std::sync::Arc;

use llm_harness_agent::{HarnessHooks, Plugin};
use llm_harness_types::Tool;
use thiserror::Error;

use crate::{
    EvidenceAuthority, EvidenceProviderId, KnowledgeCitationPolicy, KnowledgeFinalAnswerValidator,
    KnowledgeReadTool, KnowledgeRegistry, KnowledgeSearchTool, KnowledgeToolConfig,
};

#[derive(Debug, Clone, Default)]
pub struct KnowledgePluginConfig {
    pub tools: KnowledgeToolConfig,
    pub citation_policy: KnowledgeCitationPolicy,
}

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
    config: KnowledgePluginConfig,
}

impl KnowledgePlugin {
    pub fn new(
        registry: Arc<KnowledgeRegistry>,
        authority: Arc<EvidenceAuthority>,
        provider_id: EvidenceProviderId,
        config: KnowledgePluginConfig,
    ) -> Result<Self, KnowledgePluginBuildError> {
        if !authority.is_registered(&provider_id) {
            return Err(KnowledgePluginBuildError::UnregisteredEvidenceProvider);
        }
        if config.tools.max_search_results == 0 || config.tools.max_read_bytes == 0 {
            return Err(KnowledgePluginBuildError::InvalidToolLimits);
        }

        Ok(Self {
            registry,
            authority,
            provider_id,
            config,
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
            self.config.tools.clone(),
        )));
        tools.push(Arc::new(KnowledgeReadTool::new(
            self.registry.clone(),
            self.authority.clone(),
            self.provider_id.clone(),
            self.config.tools.clone(),
        )));
    }

    fn register_hooks(&self, hooks: &mut HarnessHooks) {
        if self.config.citation_policy != KnowledgeCitationPolicy::Disabled {
            hooks
                .final_answer_validator
                .push(Arc::new(KnowledgeFinalAnswerValidator::new(
                    self.config.citation_policy,
                )));
        }
    }
}
