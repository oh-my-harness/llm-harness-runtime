//! Cost types re-exported from `llm-harness-types`.
//!
//! Originally defined here; moved to `llm-harness-types` so the agent crate
//! (which sits below runtime in the dependency chain) can hold
//! `CostAggregate` / `PricingProvider` without reversing the dependency
//! direction.

use std::sync::{Arc, Mutex};

pub use llm_harness_types::{CostAggregate, ModelCost, PricingProvider, TokenPrice};

/// Caller-owned accounting state that survives cancellation of a harness run.
///
/// The runtime writes each completed provider response into this ledger before
/// later response hooks, session persistence, or output extraction. Clones
/// share the same state; callers can retain one outside the execution future
/// and obtain an immutable snapshot after cancellation or timeout.
#[derive(Clone, Debug, Default)]
pub struct UsageLedger {
    state: Arc<Mutex<CostAggregate>>,
}

impl UsageLedger {
    /// Returns the current completed-call accounting snapshot.
    pub fn snapshot(&self) -> CostAggregate {
        self.state.lock().unwrap().clone()
    }

    /// Returns the shared cost state, the same `Arc` held by
    /// `CostAccumulatorHook` and `AgentHarness`.
    ///
    /// Callers can use this to construct `BudgetControlAdapter` (from
    /// `llm-harness-strategy`) and wire it as a `should_stop` hook.
    pub fn shared_state(&self) -> Arc<Mutex<CostAggregate>> {
        self.state.clone()
    }
}
