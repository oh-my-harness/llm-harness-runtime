//! llm-harness-strategy — 用户可选注入的策略实现。
//!
//! 只依赖 `llm-harness-types` 的公开 API。trait 定义在 types，
//! 默认实现在此 crate。用户按需引入，runtime 不依赖此 crate。

pub mod audit;
pub use audit::{AuditPlugin, ToolCallAuditHook};
pub mod budget;
pub mod credential_guard;
pub mod resource;
pub mod skills;
