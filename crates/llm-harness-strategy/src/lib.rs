//! llm-harness-strategy — 用户可选注入的策略实现。
//!
//! 依赖 `llm-harness-types` 的公开 API 与 `llm-harness-agent` 的 Plugin/Hooks trait。
//! 默认实现在此 crate。用户按需引入，runtime 不依赖此 crate。

pub mod audit;
pub use audit::{AuditPlugin, ToolCallAuditHook};
pub mod budget;
pub mod credential_guard;
pub mod injection_filter;
pub use injection_filter::{
    InjectionAction, InjectionFilterHook, InjectionFilterPlugin, InjectionPattern,
};
pub mod notify;
pub mod resource;
pub mod safety;
pub mod source_tag;
pub use source_tag::{SourceTagEntry, SourceTagHook, SourceTagPlugin};
pub mod status_panel;
pub use notify::{
    NotificationChannel, NotificationError, NotificationMessage, NotifyPlugin, NotifyUserTool,
    Urgency,
};
