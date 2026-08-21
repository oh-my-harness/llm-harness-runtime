//! Primary framework SDK facade — the user-facing entry point for building
//! and running LLM agents.
//!
//! This crate provides:
//!
//! - **[`HarnessBuilder`]** — fluent builder for constructing an
//!   [`AgentHarness`] with plugins, LLM providers, tools, hooks, skills,
//!   and cost tracking. This is the main entry point for most users.
//! - **[`AgentHarness`]** — session-backed agent runtime: manages the
//!   turn loop, tool dispatch, compaction, streaming, and event emission.
//! - **[`Agent`]** — lightweight stateful runner for single-turn or
//!   few-shot use cases without full session persistence.
//! - **Sessions** — `Session`, `SessionRepo`, `JsonlSessionRepo`,
//!   `InMemorySessionRepo` for conversation persistence and branching.
//! - **Compaction** — `compact()`, `CompactionModelConfig`,
//!   `CompactionPromptSpec` for context-window management.
//! - **Skills & Templates** — `Skill`, `PromptTemplate`, `SkillReadTool`
//!   for loading and invoking markdown-based skills.
//! - **Cost tracking** — `UsageLedger` for caller-owned cost accounting
//!   that survives run cancellation; `CostAggregate` (via `types`) for
//!   token/cost snapshots.
//! - **Hooks** — `HarnessHooks`, `Plugin` trait for composable
//!   before/after-run, before/after-turn, before/after-tool-call,
//!   should-stop, and transform-context extensions.
//!
//! Concrete platform infrastructure (sandboxes, workflow engine, spawn,
//! tracing, audit) lives in `llm-harness-runtime`. Strategy plugins
//! (safety, budget, audit) live in `llm-harness-strategy`.

pub mod agent;
pub mod builder;
pub mod compaction;
pub mod harness;
pub mod plugin;
pub mod session;
pub mod skills;

pub use agent::{Agent, AgentOptions, AgentPhase, AgentState, ModelInfo};
pub use builder::{HarnessBuildError, HarnessBuilder};
pub use compaction::{
    CompactionModelConfig, CompactionPreparation, CompactionPromptSpec, CompactionSummaryConfig,
    CompactionThresholdConfig, compact,
};
pub use harness::{
    AgentHarness, AgentHarnessEvent, AgentHarnessOptions, CompactionStats, FollowUpHandle,
    HarnessHooks, HarnessState, HarnessToolCallResult, UsageLedger,
};
pub use llm_harness_loop::{
    DEFAULT_FINAL_ANSWER_TOOL_NAME, FinalAnswerMissingBehavior, FinalAnswerMode,
    FinalAnswerToolConfig,
};
pub use plugin::Plugin;
pub use session::{
    BuiltContext, InMemorySessionRepo, JsonlSessionRepo, ObservedSessionRepo, Session,
    SessionDurability, SessionLifecycle, SessionMutation, SessionMutationKind,
    SessionMutationObserver, SessionRecallPolicy, SessionRecallScope, SessionRecallSettings,
    SessionRepo, SessionStorage,
};
pub use skills::{
    PromptTemplate, Skill, SkillDiagnostic, SkillMetadataInjectionHook, SkillReadTool,
    SourcedSkill, format_skill_invocation, format_skills_metadata, invoke_template,
    load_prompt_templates, load_skills, parse_command_args, parse_frontmatter, parse_skill_file,
    resolve_skill_resource,
};

/// Recommended imports for most `llm-harness` SDK users.
///
/// This module intentionally exposes the stable framework surface: `Agent`,
/// `AgentHarness`, sessions, tools, messages, events, hooks, skills, and the
/// default OS execution environment. Advanced loop APIs and direct adapter
/// types are not included here.
pub mod prelude {
    pub use crate::{
        Agent, AgentHarness, AgentHarnessEvent, AgentHarnessOptions, AgentOptions, AgentPhase,
        AgentState, BuiltContext, CompactionPreparation, CompactionStats, CompactionSummaryConfig,
        CompactionThresholdConfig, DEFAULT_FINAL_ANSWER_TOOL_NAME, FinalAnswerMissingBehavior,
        FinalAnswerMode, FinalAnswerToolConfig, FollowUpHandle, InMemorySessionRepo,
        JsonlSessionRepo, ModelInfo, ObservedSessionRepo, Plugin, PromptTemplate, Session,
        SessionDurability, SessionLifecycle, SessionMutation, SessionMutationKind,
        SessionMutationObserver, SessionRecallPolicy, SessionRecallScope, SessionRecallSettings,
        SessionRepo, SessionStorage, Skill, SkillDiagnostic, SourcedSkill,
    };
    pub use llm_harness_types::{
        AgentError, AgentEvent, AgentMessage, AssistantMessage, BinarySource, BranchSummaryMessage,
        CompactionError, CompactionSummaryMessage, ContentBlock, DataBlock, DiagnosticLevel,
        EntryId, EnvError, ExecutionEnv, FileInfo, HarnessError, HarnessPhase, ImageSource,
        PersistedEnvelope, PersistedToolProjection, ProjectionMetadata, ShellOptions, ShellOutput,
        StopReason, StreamOptions, ThinkingLevel, TokenUsage, Tool, ToolContext, ToolError,
        ToolExecutionMode, ToolFailure, ToolProgress, ToolProjectionKind, ToolResult,
        ToolResultExtensions, ToolSessionProjection, UnsupportedEnv, UserMessage,
    };
}
