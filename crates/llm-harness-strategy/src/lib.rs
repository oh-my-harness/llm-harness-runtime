//! llm-harness-strategy — 用户可选注入的策略实现。
//!
//! 依赖 `llm-harness-types` 的公开 API 与 `llm-harness-agent` 的 Plugin/Hooks trait。
//! 默认实现在此 crate。用户按需引入，runtime 不依赖此 crate。

pub mod event_stream;
pub use event_stream::{
    EventContext, EventDetails, FilterStream, HeartbeatHandle, HeartbeatStream, TimerKind,
    TimerStream, by_channel, by_field, by_source, pass_all,
};
pub mod audit;
pub use audit::{AuditPlugin, ToolCallAuditHook};
pub mod budget;
pub mod credential_guard;
pub mod injection_filter;
pub use injection_filter::{
    InjectionAction, InjectionDetector, InjectionFilterHook, InjectionFilterPlugin,
    InjectionPattern,
};
pub mod memory_defense;
pub use memory_defense::{
    MemoryDefenseAction, MemoryDefenseHook, MemoryDefensePlugin, MemoryDefensePluginBuilder,
    MemoryFileSet,
};
pub mod loop_safety;
pub use loop_safety::{
    DeathSpiralGuardConfig, FailureCircuitBreakerConfig, LoopSafetyConfig, LoopSafetyPlugin,
    MaxTurnsConfig, RepetitionGuardConfig, TruncationContinuationConfig,
};
pub mod notify;
pub mod resource;
pub mod safety;
pub mod source_tag;
pub use source_tag::{SourceTagEntry, SourceTagHook, SourceTagPlugin};
pub mod project_instruction;
pub mod status_panel;
pub use notify::{
    NotificationChannel, NotificationError, NotificationMessage, NotifyPlugin, NotifyUserTool,
    Urgency,
};
pub use project_instruction::{
    ProjectInstructionConfig, ProjectInstructionHook, ProjectInstructionPlugin,
};
