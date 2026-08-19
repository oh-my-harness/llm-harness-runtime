mod compaction_ops;
mod core;
mod cost;
pub(crate) mod cost_hook;
mod event;
mod follow_up;
mod hooks;
mod loop_driver;
mod options;
mod phase_guard;
mod prepare_next_turn;
mod state;

// Re-exports — public API of the harness module.
pub use core::AgentHarness;
pub use cost::UsageLedger;
pub use event::{AgentHarnessEvent, CompactionStats, HarnessToolCallResult};
pub use follow_up::FollowUpHandle;
pub use hooks::{AbortCascadeHook, AsyncSpawnHookHandle, IdleWatcherHook};
pub use options::AgentHarnessOptions;
pub use state::{HarnessHooks, HarnessState};

// Re-exports used by inline test modules via `use super::*`.
#[allow(unused_imports)]
pub(crate) use {
    crate::agent::ModelInfo,
    crate::compaction::CompactionModelConfig,
    crate::session::{Session, SessionRepo, repo::InMemorySessionRepo, types::*},
    crate::skills::{PromptTemplate, Skill},
    llm_harness_loop::{
        ChatRequest, ConvertToLlmHook, DefaultConvertToLlm, FinalAnswerMode, LlmClient, LlmMessage,
        RequestContent,
    },
    llm_harness_types::*,
    std::collections::HashSet,
    std::path::PathBuf,
    std::sync::{Arc, Mutex},
    tokio::sync::{broadcast, mpsc, watch},
    tokio_util::sync::CancellationToken,
};

#[cfg(test)]
#[cfg(feature = "test-utils")]
mod tests;

#[cfg(test)]
mod cost_state_tests {
    use super::*;
    #[test]
    fn usage_returns_default_when_no_accumulation() {
        // 用默认 cost 构造 options。
        let opts = AgentHarnessOptions::new("test-model");
        // 模拟 harness 持有同一 Arc（不跑 loop）。
        let cost_arc = opts.cost.clone();
        // 没有 harness 实例也能验证共享状态契约：
        // 修改 Arc 对持有同一 Arc 的读者可见。
        {
            let mut c = cost_arc.lock().unwrap();
            c.total_input_tokens = 42;
            c.total_cost = 1.5;
        }
        let snapshot = cost_arc.lock().unwrap().clone();
        assert_eq!(snapshot.total_input_tokens, 42);
        assert_eq!(snapshot.total_cost, 1.5);
    }

    #[test]
    fn options_new_initializes_empty_cost() {
        let opts = AgentHarnessOptions::new("test-model");
        let c = opts.cost.lock().unwrap().clone();
        assert_eq!(c.total_input_tokens, 0);
        assert_eq!(c.total_cost, 0.0);
        assert!(c.by_model.is_empty());
    }
}
