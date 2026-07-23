//! Optional long-term memory composition for `llm-harness-runtime`.
//!
//! Memory is deliberately split into a read-only [`KnowledgeSource`] and an
//! independent [`MemoryStore`]. [`MemoryService`] joins them at the trusted
//! application boundary without requiring both sides to use the same backend.

mod policy;
mod service;
mod store;

#[cfg(any(test, feature = "test-utils"))]
pub mod contract;

pub use policy::*;
pub use service::*;
pub use store::*;

pub use llm_harness_runtime_knowledge::KnowledgeSource;
