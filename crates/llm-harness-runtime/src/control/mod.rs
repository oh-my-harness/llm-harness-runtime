//! Runtime control flow: cost accumulation and budget enforcement.
//!
//! `cost` re-exports cost types from `llm-harness-types`; `cost_hook`
//! accumulates per-call cost into a shared `CostAggregate`; `budget`
//! wraps it in a `ShouldStopHook`.

pub mod budget;
pub mod cost;
pub mod cost_hook;
