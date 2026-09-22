use thiserror::Error;

/// Errors raised by team construction and social-infrastructure operations.
#[derive(Debug, Error)]
pub enum TeamError {
    /// No agent with this id is registered in the social context.
    #[error("agent '{0}' not found in registry")]
    AgentNotFound(String),
    /// A contact list is at capacity and the incoming record is too weak
    /// to evict an existing contact.
    #[error("contact list is full (max {0})")]
    ContactListFull(usize),
    /// Team construction was requested with an empty member list.
    #[error("no members in team")]
    NoMembers,
    /// Two members share the same agent id.
    #[error("duplicate agent id '{0}' in team")]
    DuplicateMember(String),
    /// The id is reserved for system use (e.g. the operator).
    #[error("agent id '{0}' is reserved for system use")]
    ReservedAgentId(String),
    /// No LLM providers were configured, so sub-agents cannot spawn.
    #[error(
        "no LLM providers configured — at least one provider is required for sub-agent spawning"
    )]
    NoProviders,
    /// The team was used before it was built.
    #[error("team has not been built yet")]
    NotBuilt,
    /// The team was built twice.
    #[error("team already built")]
    AlreadyBuilt,
    /// Building the underlying per-agent harness failed.
    #[error("harness build error: {0}")]
    HarnessBuild(#[from] llm_harness_agent::HarnessBuildError),
    /// A social tool plugin failed during setup.
    #[error("plugin setup failed: {0}")]
    PluginSetup(String),
    /// A harness runtime error surfaced through a tool or runner.
    #[error("harness error: {0}")]
    Harness(#[from] llm_harness_types::HarnessError),
    /// The durable inbox journal could not be opened or recovered.
    #[error(transparent)]
    InboxJournal(#[from] crate::inbox_journal::InboxJournalError),
    /// A durable timer journal could not be opened.
    #[error(transparent)]
    TimerJournal(#[from] crate::timer_journal::TimerJournalError),
    /// A durable chat transcript could not be opened or read.
    #[error(transparent)]
    ChatHistoryJournal(#[from] crate::chat_history_journal::ChatHistoryJournalError),
}

/// Crate-wide result alias with [`TeamError`] as the default error type.
pub type Result<T, E = TeamError> = std::result::Result<T, E>;
