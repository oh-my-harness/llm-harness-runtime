//! Sub-agent spawning, async delivery, and main↔sub communication.
//!
//! `spawner` builds a fresh `AgentHarness` per sub-agent; `message_bus`
//! provides unified event + message routing; `tools` exposes spawning and
//! communication to the LLM.
//!
//! This crate is the "Spawn" prong of the three coordination mechanisms
//! (Spawn / Workflow / Agent Team). It is self-contained — depends only on
//! the agent, types, loop, and platform crates — and does not reference
//! workflow, lifecycle, or rules.
//!
//! # Tracing / diagnostic logging
//!
//! This crate emits diagnostic warnings via `tracing::warn!` (SpawnPlugin
//! event drops, etc.). These calls are no-ops unless a
//! `tracing_subscriber` is installed. Embedding applications must
//! initialize a subscriber.

pub use llm_harness_platform::env::EnvFactory;

pub mod delivery;
pub mod message_bus;
pub mod plugin;
pub mod spawn_types;
pub mod spawner;
pub mod tools;
