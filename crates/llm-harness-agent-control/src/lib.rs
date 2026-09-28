//! Mobile agent control service.

mod auth;
mod config;
mod persistence;
mod routes;
mod service;

pub use auth::AuthenticatedPrincipal;
pub use config::{ControlServiceConfig, GatewayConfig, LlmConfig, PrincipalConfig};
pub use persistence::{PersistedSessions, SessionPersistence, reconcile_orphaned_sandboxes};
pub use routes::build_router;
pub use service::{
    AgentControlManager, AgentSandbox, AgentSessionComponents, AgentSessionFactory,
    AgentSessionStatus, ControlEvent, ControlServiceError, CreateAgentSessionRequest,
    PromptRequest, RemoteSandboxResource, RemoteSandboxSessionFactory, SessionEvent,
    SharedAgentControlManager, SteerRequest,
};

/// Shared control-service state.
pub type SharedState = SharedAgentControlManager;
