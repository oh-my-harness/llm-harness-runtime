//! Primary framework SDK facade for `llm-harness-core`.
//!
//! This crate exposes the user-facing core runtime: `Agent` for lightweight
//! stateful runs, `AgentHarness` for session-backed agents, session
//! repositories, compaction, and skills/templates.
//!
//! Concrete tools, settings/auth/model registries, product prompts, CLI/TUI
//! entrypoints, and extension runtimes belong above core, for example in
//! `llm-harness-runtime` or a domain-agent repository.
//!
//! For common imports, use [`prelude`].

pub mod agent;
pub mod compaction;
pub mod harness;
pub mod plugin;
pub mod session;
pub mod skills;

pub use agent::{Agent, AgentOptions, AgentPhase, AgentState, ModelInfo};
pub use compaction::{
    CompactionModelConfig, CompactionPreparation, CompactionPromptSpec, CompactionSummaryConfig,
    CompactionThresholdConfig, compact,
};
pub use harness::{
    AbortCascadeHook, AgentHarness, AgentHarnessEvent, AgentHarnessOptions, AsyncSpawnHookHandle,
    CompactionStats, FollowUpHandle, HarnessHooks, HarnessState, HarnessToolCallResult,
    IdleWatcherHook,
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
    PromptTemplate, Skill, SkillDiagnostic, SkillReadTool, SourcedSkill, format_skill_invocation,
    invoke_template, load_prompt_templates, load_skills, parse_command_args, parse_frontmatter,
    parse_skill_file, resolve_skill_resource,
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
