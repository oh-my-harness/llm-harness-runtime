//! Agent Team: declarative, decentralized multi-agent collaboration.
//!
//! [`IndividualSpec`]s are combined into an [`AgentTeam`], whose `build`
//! materializes each member into a self-driving [`AgentRunner`] over a shared
//! [`SocialContext`]. Agents collaborate peer-to-peer through the social
//! tools ([`tools::SocialToolPlugin`]); the human operator interacts via
//! [`AgentTeam::submit`] / [`AgentTeam::receive`].

mod agent_id;
mod agent_runner;
mod chat_history_journal;
mod contact;
mod error;
mod group_reply;
mod hooks;
mod inbox;
mod inbox_journal;
mod individual_spec;
mod memory_store;
mod relation;
mod social_context;
mod team;
mod timeout_manager;
mod timer_journal;
pub mod tools;
pub use agent_id::AgentId;

pub use agent_runner::{
    AgentRunner, NO_RESPONSE, ReflectionTrigger, build_reflection_task, format_transcript,
};
pub use chat_history_journal::{ChatHistoryEntry, ChatHistoryJournal, ChatHistoryJournalError};
pub use contact::{ContactIndex, ContactRecord, ContactStore};
pub use error::{Result, TeamError};
pub use group_reply::GroupReplyContext;
pub use hooks::TeamBeforeRunHook;
pub use inbox::priority;
pub use inbox::{ChatView, InboxMessage, InboxQueue, InboxStore, MessageType};
pub use inbox_journal::{InboxJournal, InboxJournalError};
pub use individual_spec::IndividualSpec;
pub use memory_store::{
    TEAM_MEMORY_SOURCE_ID, TeamSqliteMemorySource, TeamSqliteMemoryStore,
    team_memory_access_context,
};
pub use relation::{RelationConfig, RelationEngine, RelationEvent};
pub use social_context::{
    AgentInfo, AgentRegistry, AgentStatus, BroadcastMessageIds, CancelledTimeout, MessageObserver,
    SocialContext, TimeoutCancel,
};
pub use team::{AgentTeam, OperatorMessage, TeamConfig};
pub use timeout_manager::TimeoutManager;
pub use timeout_manager::reserved;
pub use timer_journal::{TimeoutJournal, TimeoutKind, TimeoutRequest, TimerJournalError};
pub use tools::SocialToolPlugin;
