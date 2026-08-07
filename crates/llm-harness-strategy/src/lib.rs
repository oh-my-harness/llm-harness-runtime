//! llm-harness-strategy — 用户可选注入的策略实现。
//!
//! 依赖 `llm-harness-types` 的公开 API 与 `llm-harness-agent` 的 Plugin/Hooks trait。
//! 默认实现在此 crate。用户按需引入，runtime 不依赖此 crate。

pub mod audit;
pub use audit::{AuditPlugin, ToolCallAuditHook};
pub mod budget;
pub mod credential_guard;
pub mod resource;
pub mod safety;
pub mod status_panel;
