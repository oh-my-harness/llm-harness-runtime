//! llm-harness-runtime — Platform layer for agent runtime infrastructure.
//!
//! This crate defines traits and core orchestration logic for:
//! - Sandbox abstraction (`Sandbox` trait)
//! - Tool registry and discovery (`ToolRegistry`, `ToolSource`)
//! - MCP adapter (`McpClientHandle`)
//! - Resource injection (`ResourceProvider`, `ResourceInjector`)
//! - Prompt template integration (`PromptSource`, `PromptTemplateCompiler`)
//! - Task lifecycle management (`TaskRunner`)
//! - Tracing (`TraceExporter`)
//! - Cost aggregation (`PricingProvider`, `CostAggregate`)
//! - Audit logging (`AuditSink`)
//! - Authentication (see `llm-harness-types` and `llm-harness-runtime-auth`)
//! - Human approval (`HumanApprover`, `HumanApprovalWrapper`)
//! - Budget control (`BudgetExceededHook`, `BudgetControlAdapter`)
//! - Harness construction (`HarnessBuilder`)

pub mod audit;
pub mod budget;
pub mod builder;
pub mod cost;
pub mod error;
pub mod human_approval;
pub mod mcp;
pub mod prompt;
pub mod resource;
pub mod sandbox;
pub mod spawner;
pub mod task;
pub mod tool_adapter;
pub mod tool_registry;
pub mod tracer;
pub mod tracing_adapter;

pub use audit::{AuditDecision, AuditEntry, AuditEventType, AuditSink, CostDelta};
pub use budget::{BudgetControlAdapter, BudgetExceededHook};
pub use builder::{HarnessBuildError, HarnessBuilder};
pub use cost::{CostAggregate, ModelCost, PricingProvider, TokenPrice};
pub use human_approval::{
    ApprovalDecision, ApprovalRequest, HumanApprovalWrapper, HumanApprover, TimeoutPolicy,
};
pub use llm_harness_loop::composite::{
    CompositeAfterProviderResponseHook, CompositeAfterToolCallHook, CompositeAfterTurnHook,
    CompositeBeforeProviderRequestHook, CompositeBeforeToolCallHook,
};
pub use mcp::{McpClientHandle, McpContent, McpError, McpResult, McpToolDef};
pub use prompt::{
    ArgumentDef, InMemoryPromptRegistry, InvokeError, PromptSource, PromptSourceTemplate,
    PromptTemplateCompiler,
};
pub use resource::{ResourceContent, ResourceDescriptor, ResourceInjector, ResourceProvider};
pub use sandbox::{NetRule, ResourceLimits, Sandbox, SandboxConfig};
pub use spawner::{
    EnvFactory, HarnessSubAgentSpawner, JsonlSessionFactory, PluginFactory, SessionCreateResult,
    SessionFactory,
};
pub use task::{
    Checkpoint, RetryPolicy, TaskError, TaskId, TaskResult, TaskRunner, TaskRunnerImpl, TaskState,
    TaskTrace, TaskVerdict, TaskVerifier,
};
pub use tool_adapter::ToolSourceAdapter;
pub use tool_registry::{
    InMemoryToolRegistry, ToolDefinition, ToolExecutionMode, ToolMetadata, ToolRegistry, ToolSource,
};
pub use tracer::{
    AttributeValue, CaptureLevel, SpanCaptureConfig, SpanEvent, SpanId, SpanKind, SpanRedactor,
    SpanStatus, TraceExporter, TraceId,
};
pub use tracing_adapter::TracingHookAdapter;
