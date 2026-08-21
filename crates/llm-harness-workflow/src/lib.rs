//! Workflow orchestration engine + task persistence + rule-based approval.
//!
//! This crate combines three subsystems that together form the "Workflow"
//! prong of the three coordination mechanisms (Spawn / Workflow / Agent
//! Team):
//!
//! - `workflow` — DAG step orchestration engine (`WorkflowEngine` is the
//!   sole execution engine; a free task = single-step workflow).
//! - `lifecycle` — task persistence (`TaskStore`) and event-driven
//!   resumption. This is the persistence layer for `Workflow` execution
//!   state (step records, checkpoints).
//! - `rules` — pure rule-matching layer (`Predicate` trait, built-in
//!   predicates, `RuleChain`, `RuleBasedApprovalHook`). Consumed by
//!   `workflow::gate`.
//!
//! Depends on `llm-harness-subagents` for parallel step execution (step
//! agents call `spawn_agent` via `SessionFactory` / `HarnessSubAgentSpawner`).
//!
//! # Tracing / diagnostic logging
//!
//! This crate emits diagnostic warnings via `tracing::warn!` (session
//! parse failures, event-store I/O errors, etc.). These calls are no-ops
//! unless a `tracing_subscriber` is installed. Embedding applications must
//! initialize a subscriber.

pub mod lifecycle;
pub mod rules;
pub mod workflow;
