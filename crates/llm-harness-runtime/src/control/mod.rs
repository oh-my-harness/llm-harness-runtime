//! Runtime control flow: cost accumulation.
//!
//! `cost` re-exports cost types from `llm-harness-types`; `cost_hook`
//! accumulates per-call cost into a shared `CostAggregate`.

pub mod cost;
pub mod cost_hook;
