//! llm-harness-runtime — Platform layer for agent runtime infrastructure.
//!
//! This crate defines traits and core orchestration logic for:
//! - Sandbox abstraction (`Sandbox` trait)
//! - MCP adapter (`McpClientHandle`)
//! - Resource injection (`ResourceProvider`, `ResourceInjector`)
//! - Prompt template integration (`PromptSource`, `PromptTemplateCompiler`)
//! - Task lifecycle management (`WorkflowEngine`)
//! - Tracing (`TraceExporter`)
//! - Cost aggregation (`PricingProvider`, `CostAggregate`)
//! - Audit logging (`AuditSink`)
//! - Budget control (`BudgetExceededHook`, `BudgetControlAdapter`)
//! - Harness construction (`HarnessBuilder`)
//!
//! # Tracing / diagnostic logging
//!
//! This crate emits diagnostic warnings via `tracing::warn!` (session parse
//! failures, event-store I/O errors, IdleWatcher event drops, etc.).  These
//! calls are no-ops unless a `tracing_subscriber` is installed.
//!
//! **Embedding applications must initialize a subscriber.**  For example:
//!
//! ```ignore
//! tracing_subscriber::fmt()
//!     .with_writer(std::io::stderr)
//!     .with_env_filter(
//!         tracing_subscriber::EnvFilter::builder()
//!             .with_default_directive(tracing::level_filters::LevelFilter::WARN.into())
//!             .from_env_lossy(),
//!     )
//!     .try_init();
//! ```
//!
//! The Python bindings crate (`llm-harness-py`) does this automatically in its
//! `#[pymodule]` entry point.

pub mod builder;
pub mod rules;
pub mod workflow;

pub mod control;
pub mod lifecycle;
pub mod observability;
pub mod platform;
pub mod spawn;
