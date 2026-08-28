//! Agent Team: declarative, decentralized multi-agent collaboration.
//!
//! [`IndividualSpec`]s are combined into an [`AgentTeam`], whose `build`
//! materializes each member into a self-driving [`AgentRunner`] over a shared
//! [`SocialContext`]. Agents collaborate peer-to-peer through the social
//! tools ([`tools::SocialToolPlugin`]); the human operator interacts via
//! [`AgentTeam::submit`] / [`AgentTeam::receive`].

mod agent_id;
mod agent_runner;
mod contact;
mod error;
mod hooks;
mod inbox;
mod individual_spec;
mod memory_store;
mod relation;
mod social_context;
mod team;
mod timeout_manager;
pub mod tools;
pub use agent_id::AgentId;

pub use agent_runner::{AgentRunner, ReflectionTrigger, build_reflection_task, format_transcript};
pub use contact::{ContactIndex, ContactRecord, ContactStore};
pub use error::{Result, TeamError};
pub use hooks::TeamBeforeRunHook;
pub use inbox::{InboxMessage, InboxQueue, InboxStore, MessageType};
pub use individual_spec::IndividualSpec;
pub use memory_store::{
    TEAM_MEMORY_SOURCE_ID, TeamSqliteMemorySource, TeamSqliteMemoryStore,
    team_memory_access_context,
};
pub use relation::{RelationConfig, RelationEngine, RelationEvent};
pub use social_context::{
    AgentInfo, AgentRegistry, AgentStatus, CancelledTimeout, SocialContext, TimeoutCancel,
};
pub use team::{AgentTeam, OperatorMessage, TeamConfig};
pub use timeout_manager::TimeoutManager;
pub use tools::SocialToolPlugin;
