//! Stable shared contracts for `llm-harness-core`.
//!
//! This crate defines the data model and extension traits used across the
//! harness stack: messages, content blocks, events, errors, `Tool`,
//! `ExecutionEnv`, hooks, compaction metadata, and common runtime options.
//!
//! Most downstream crates should treat these types and traits as the stable SDK
//! contract. Concrete tools, product settings, auth storage, model registries,
//! and UI concerns belong in upper layers.

pub mod audit;
pub mod compaction;
pub mod content;
pub mod cost;
pub mod env;
pub mod errors;
pub mod event_stream;
pub mod events;
pub mod hooks;
pub mod identity;
pub mod messages;
pub mod misc;
pub mod resources;
pub mod run;
pub mod skills;
pub mod tool;

pub use audit::*;
pub use compaction::*;
pub use content::*;
pub use cost::*;
pub use env::*;
pub use errors::*;
pub use event_stream::*;
pub use events::*;
pub use hooks::*;
pub use identity::*;
pub use messages::*;
pub use misc::*;
pub use resources::*;
pub use run::*;
pub use skills::*;
pub use tool::*;
