//! Sub-agent spawning, async delivery, and peer-to-peer communication.
//!
//! `spawner` builds a fresh `AgentHarness` per sub-agent; `async_spawn`
//! delivers completions back to the parent via a registry; `peer` provides
//! sub↔sub messaging; `spawn_tool` exposes spawning to the LLM.

pub mod async_spawn;
pub mod peer;
pub mod spawn_tool;
pub mod spawner;
