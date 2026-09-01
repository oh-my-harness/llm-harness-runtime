//! Agent Team Studio — generic agent-team assembly from declarative
//! [`TeamSpec`]s, with templates (the built-in `coding-team` is the first
//! official template), a web operation panel, issue intake, and patrol
//! behaviors. One team = one [`AgentTeam`](llm_harness_agent_team::AgentTeam),
//! fully isolated (memory root, issue store, working dir). Mounted into the
//! inspector server via its `extra_routes` / `extra_static` extension
//! points. See `docs/design/2026-08-30-agent-team-studio-design.md`.
pub mod config;
pub mod engine;
pub mod env_cfg;
pub mod events;
pub mod issue;
pub mod pulse;
pub mod roles;
pub mod routes;
pub mod scout;
pub mod settings;
pub mod team_manager;
pub mod team_spec;
pub mod template_store;
