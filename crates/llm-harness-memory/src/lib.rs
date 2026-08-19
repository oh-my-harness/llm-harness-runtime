//! Optional long-term memory composition for `llm-harness-runtime`.
//!
//! Memory is deliberately split into a read-only [`KnowledgeSource`] and an
//! independent [`MemoryStore`]. [`MemoryService`] joins them at the trusted
//! application boundary without requiring both sides to use the same backend.

mod mutation_gate;
mod plugin;
mod policy;
mod safe_policy;
mod service;
mod store;
mod tools;

#[cfg(any(test, feature = "test-utils"))]
pub mod contract;

pub use mutation_gate::*;
pub use plugin::*;
pub use policy::*;
pub use safe_policy::*;
pub use service::*;
pub use store::*;
pub use tools::*;

pub use llm_harness_knowledge::KnowledgeSource;
