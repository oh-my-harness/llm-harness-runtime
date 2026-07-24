//! Platform-layer trait abstractions implemented by backend crates.
//!
//! Each submodule defines a trait (and reference adapter) that a backend
//! crate (`runtime-sandbox-*`, `runtime-audit-jsonl`, `runtime-trace-otel`,
//! `runtime-mcp`, ...) implements. The runtime itself stays backend-agnostic.

pub mod prompt;
pub mod resource;
pub mod sandbox;
