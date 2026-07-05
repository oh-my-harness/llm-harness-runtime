//! Runtime control flow: cost accumulation, budget enforcement, human approval.
//!
//! `cost` re-exports cost types from `llm-harness-types`; `cost_hook`
//! accumulates per-call cost into a shared `CostAggregate`; `budget`
//! wraps it in a `ShouldStopHook`; `human_approval` wraps a `HumanApprover`
//! into a `BeforeToolCallHook`.

pub mod budget;
pub mod cost;
pub mod cost_hook;
pub mod human_approval;
