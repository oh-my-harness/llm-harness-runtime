use std::path::Path;
use std::sync::Arc;

use llm_harness_runtime::spawner::EnvFactory;
use llm_harness_types::{AgentError, ExecutionEnv};

use crate::OsEnv;

/// 基于真实 OS 的 `EnvFactory` 实现，使用 `OsEnv`。
pub struct OsEnvFactory;

impl EnvFactory for OsEnvFactory {
    fn create(&self, cwd: &Path) -> Result<Arc<dyn ExecutionEnv>, AgentError> {
        Ok(Arc::new(OsEnv::new(cwd)))
    }
}
