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
#[cfg(test)]
mod env;
pub mod harness;
pub mod plugin;
pub mod session;
pub mod skills;
pub mod sub_agent;

pub use agent::{Agent, AgentOptions, AgentPhase, AgentState, ModelInfo};
pub use compaction::{CompactionPreparation, CompactionSettings, compact, prepare_compaction};
pub use harness::{
    AgentHarness, AgentHarnessEvent, AgentHarnessOptions, AsyncSpawnHookHandle, CompactionStats,
    FollowUpHandle, HarnessHooks, HarnessState, HarnessToolCallResult, IdleWatcherHook,
};
pub use plugin::{Plugin, StateProvider};
pub use session::{
    BuiltContext, InMemorySessionRepo, JsonlSessionRepo, Session, SessionRepo, SessionStorage,
};
pub use skills::{
    PromptTemplate, Skill, SkillDiagnostic, SourcedSkill, format_skill_invocation,
    format_skills_for_system_prompt, invoke_template, load_prompt_templates, load_skills,
    load_sourced_skills, parse_command_args,
};
pub use sub_agent::{
    ContextMode, ProviderSelection, SpawnOptions, SubAgentHandle, SubAgentResult, SubAgentSpawner,
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
        AgentState, BuiltContext, CompactionPreparation, CompactionSettings, CompactionStats,
        ContextMode, FollowUpHandle, InMemorySessionRepo, JsonlSessionRepo, ModelInfo, Plugin,
        PromptTemplate, ProviderSelection, Session, SessionRepo, SessionStorage, Skill,
        SkillDiagnostic, SourcedSkill, SpawnOptions, StateProvider, SubAgentHandle, SubAgentResult,
        SubAgentSpawner,
    };
    pub use llm_harness_types::{
        AgentError, AgentEvent, AgentMessage, AssistantMessage, AuthHook, BranchSummaryMessage,
        CompactionError, CompactionSummaryMessage, ContentBlock, DiagnosticLevel, EntryId,
        EnvError, ExecutionEnv, FileInfo, HarnessError, HarnessPhase, ImageSource, ShellOptions,
        ShellOutput, StopReason, StreamOptions, ThinkingLevel, TokenUsage, Tool, ToolContext,
        ToolError, ToolExecutionMode, ToolResult, UnsupportedEnv, UserMessage,
    };
}
