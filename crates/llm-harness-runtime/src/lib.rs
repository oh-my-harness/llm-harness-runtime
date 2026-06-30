//! llm-harness-runtime — Platform layer for agent runtime infrastructure.
//!
//! This crate defines traits and core orchestration logic for:
//! - Sandbox abstraction (`Sandbox` trait)
//! - Tool registry and discovery (`ToolRegistry`, `ToolSource`)
//! - MCP adapter (`McpClientHandle`)
//! - Resource injection (`ResourceProvider`, `ResourceInjector`)
//! - Prompt template integration (`PromptSource`, `PromptTemplateCompiler`)
//! - Task lifecycle management (`WorkflowEngine`)
//! - Tracing (`TraceExporter`)
//! - Cost aggregation (`PricingProvider`, `CostAggregate`)
//! - Audit logging (`AuditSink`)
//! - Authentication (see `llm-harness-types` and `llm-harness-runtime-auth`)
//! - Human approval (`HumanApprover`, `HumanApprovalWrapper`)
//! - Budget control (`BudgetExceededHook`, `BudgetControlAdapter`)
//! - Harness construction (`HarnessBuilder`)

pub mod async_spawn;
pub mod audit;
pub mod budget;
pub mod builder;
pub mod cost;
pub mod cost_hook;
pub mod error;
pub mod human_approval;
pub mod mcp;
pub mod peer;
pub mod prompt;
pub mod resource;
pub mod sandbox;
pub mod spawn_tool;
pub mod spawner;
pub mod task;
pub mod task_store;
pub mod event;
pub mod tool_adapter;
pub mod tool_registry;
pub mod tracer;
pub mod tracing_adapter;
pub mod workflow;
