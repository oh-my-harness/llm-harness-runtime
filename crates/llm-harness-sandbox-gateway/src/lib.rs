//! Authenticated Linux gateway for the remote sandbox protocol.
//!
//! The gateway uses [`BwrapSandbox`] as its execution backend. It rejects
//! policies that bubblewrap cannot enforce instead of downgrading them.

mod auth;
mod config;
mod observability;
mod routes;
mod state;

pub use auth::{FirecrackerBackendConfig, GatewayBackend, GatewayConfig, GatewayPrincipalConfig};
pub use config::GatewayServiceConfig;
pub use routes::serve;

use std::sync::Arc;

/// A running gateway and its in-memory sandbox registry.
#[derive(Clone)]
pub struct SandboxGateway {
    state: Arc<state::GatewayState>,
    router: axum::Router,
}

impl SandboxGateway {
    /// Build an authenticated gateway router.
    pub fn new(config: GatewayConfig) -> anyhow::Result<Self> {
        let state = Arc::new(state::GatewayState::new(config)?);
        let router = routes::build_router(Arc::clone(&state));
        Ok(Self { state, router })
    }

    /// Return the Axum router.
    pub fn router(&self) -> axum::Router {
        self.router.clone()
    }

    /// Delete every sandbox currently owned by this gateway process.
    pub async fn shutdown_all(&self) -> anyhow::Result<()> {
        self.state.shutdown_all().await
    }
}
