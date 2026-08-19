use std::sync::Arc;

use llm_harness_agent::Plugin;
use llm_harness_types::Tool;

use crate::{MemoryForgetTool, MemoryService, MemoryWriteTool};

pub struct MemoryPlugin {
    service: Arc<MemoryService>,
}

impl MemoryPlugin {
    pub fn new(service: Arc<MemoryService>) -> Self {
        Self { service }
    }
}

impl Plugin for MemoryPlugin {
    fn name(&self) -> &str {
        "memory"
    }

    fn register_tools(&self, tools: &mut Vec<Arc<dyn Tool>>) {
        tools.push(Arc::new(MemoryWriteTool::new(self.service.clone())));
        tools.push(Arc::new(MemoryForgetTool::new(self.service.clone())));
    }
}
