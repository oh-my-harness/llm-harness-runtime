use std::path::Path;
use std::sync::Arc;

use llm_harness_types::{AgentError, ExecutionEnv};

/// `ExecutionEnv` 的抽象工厂，使 `HarnessSubAgentSpawner` 无需
/// 直接依赖 sandbox 后端 crate。
pub trait EnvFactory: Send + Sync {
    fn create(&self, cwd: &Path) -> Result<Arc<dyn ExecutionEnv>, AgentError>;
}
