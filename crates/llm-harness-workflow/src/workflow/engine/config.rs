//! WorkflowEngine 构造器共用参数 + builder 定制闭包。

use std::path::PathBuf;
use std::sync::Arc;

use llm_harness_loop::LlmClient;

use llm_harness_agent::HarnessBuilder;
use llm_harness_platform::env::EnvFactory;
use llm_harness_subagents::spawner::SessionFactory;

/// Builder 定制闭包。接收已预设 model + provider 的 HarnessBuilder，
/// 返回定制后的 builder。每步构造 harness 时调用一次。
pub type BuilderCustomize = Arc<dyn Fn(HarnessBuilder) -> HarnessBuilder + Send + Sync>;

/// 打包 WorkflowEngine 构造器的共用参数，避免 new/new_free_task/restore 传散参。
#[derive(Clone)]
pub struct WorkflowEngineConfig {
    pub client: Arc<dyn LlmClient>,
    pub model: String,
    pub env_factory: Arc<dyn EnvFactory>,
    pub session_factory: Arc<dyn SessionFactory>,
    pub session_base_dir: PathBuf,
    /// 每个 LLM step 构造 harness 时调用。model + provider 已预设，
    /// 闭包在其之上叠加配置。None = 不定制。
    pub customize_builder: Option<BuilderCustomize>,
}

impl WorkflowEngineConfig {
    /// 设置共享 builder 定制闭包。链式便捷方法。
    #[must_use]
    pub fn with_builder_customize(
        mut self,
        customize: impl Fn(HarnessBuilder) -> HarnessBuilder + Send + Sync + 'static,
    ) -> Self {
        self.customize_builder = Some(Arc::new(customize));
        self
    }
}
