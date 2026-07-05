//! Observability + security audit.
//!
//! `audit` and `tracer` define sink traits; `tracing_adapter` bridges core
//! hooks into span events consumed by a `TraceExporter`.

pub mod audit;
pub mod tracer;
pub mod tracing_adapter;
