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
//! - Human approval (`HumanApprover`, `HumanApprovalWrapper`)
//! - Budget control (`BudgetExceededHook`, `BudgetControlAdapter`)
//! - Harness construction (`HarnessBuilder`)

pub mod builder;
pub mod workflow;

pub mod control;
pub mod lifecycle;
pub mod observability;
pub mod platform;
pub mod spawn;
