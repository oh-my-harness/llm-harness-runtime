//! Stable shared contracts for `llm-harness-core`.
//!
//! This crate defines the data model and extension traits used across the
//! harness stack: messages, content blocks, events, errors, `Tool`,
//! `ExecutionEnv`, hooks, compaction metadata, and common runtime options.
//!
//! Most downstream crates should treat these types and traits as the stable SDK
//! contract. Concrete tools, product settings, auth storage, model registries,
//! and UI concerns belong in upper layers.

pub mod compaction;
pub mod content;
pub mod env;
pub mod errors;
pub mod events;
pub mod hooks;
pub mod identity;
pub mod messages;
pub mod misc;
pub mod resources;
pub mod tool;

pub use compaction::*;
pub use content::*;
pub use env::*;
pub use errors::*;
pub use events::*;
pub use hooks::*;
pub use identity::*;
pub use messages::*;
pub use misc::*;
pub use resources::*;
pub use tool::*;
