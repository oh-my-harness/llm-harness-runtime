//! Observability + security audit.
//!
//! `audit` re-exports sink traits from `llm-harness-types`; `tracer` defines
//! a span exporter; `tracing_adapter` bridges core hooks into span events
//! consumed by a `TraceExporter`.

pub use llm_harness_types::audit;
pub mod tracer;
pub mod tracing_adapter;
