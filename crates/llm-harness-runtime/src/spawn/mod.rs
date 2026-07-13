//! Sub-agent spawning, async delivery, and main↔sub communication.
//!
//! `spawner` builds a fresh `AgentHarness` per sub-agent; `message_bus`
//! provides unified event + message routing; `tools` exposes spawning and
//! communication to the LLM.

pub mod delivery;
pub mod message_bus;
pub mod spawn_types;
pub mod spawner;
pub mod tools;
