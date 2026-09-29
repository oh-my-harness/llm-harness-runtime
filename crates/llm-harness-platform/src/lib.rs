//! llm-harness-platform — Backend-agnostic trait contracts for the runtime platform layer.
//!
//! Sandbox, EnvFactory, PromptSource, TraceExporter 等 trait 定义。
//! 后端 crate (runtime-sandbox, runtime-trace-otel, runtime-mcp) 只依赖本 crate，
//! 不引入 workflow / spawn / lifecycle 编排逻辑。

pub mod env;
pub mod prompt;
pub mod sandbox;
pub mod tracer;
pub mod tracing_adapter;
pub mod trust;

pub use llm_harness_types::audit;
