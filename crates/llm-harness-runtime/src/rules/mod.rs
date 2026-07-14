//! 纯规则匹配层。
//!
//! 提供 `Predicate` trait、内置 predicate、`RuleChain`、`RuleBasedApprovalHook`。
//! 可被任何实现 `BeforeToolCallHook` 的场景复用。

pub mod chain;
pub mod hook;
pub mod predicate;
pub mod predicates;
pub mod rule;

pub use chain::{RuleChain, RuleChainBuilder};
pub use hook::RuleBasedApprovalHook;
pub use predicate::Predicate;
pub use predicates::{Contains, NumberRangeField, RateLimit, RegexField};
pub use rule::{Decision, Rule};
