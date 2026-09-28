//! Mobile agent control service.

mod auth;
mod config;
mod routes;
mod service;

pub use auth::AuthenticatedPrincipal;
pub use config::ControlServiceConfig;
pub use routes::build_router;
pub use service::{
    AgentControlManager, AgentSandbox, AgentSessionComponents, AgentSessionFactory, ControlEvent,
    ControlServiceError, RemoteSandboxResource, RemoteSandboxSessionFactory,
    SharedAgentControlManager,
};

/// Shared control-service state.
pub type SharedState = SharedAgentControlManager;
