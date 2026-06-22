//! coding-agent: a CLI coding assistant built on the llm-harness framework.
//!
//! Phase 1 provides 4 tools (read, bash, edit, write), one-shot / pipe / REPL
//! modes, and event-based terminal output rendering.

pub mod hooks;
pub mod plugin;
pub mod prompt;
pub mod provider;
pub mod tools;
pub mod ui;
