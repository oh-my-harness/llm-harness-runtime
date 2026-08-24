//! axum server lifecycle and public Inspector API.

use crate::state::{AuthToken, InspectorState};
use llm_harness_agent::AgentHarness;
use std::sync::Arc;

pub struct Inspector {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
    bound_addr: Option<std::net::SocketAddr>,
}

impl Inspector {
    pub fn bound_addr(&self) -> Option<std::net::SocketAddr> {
        self.bound_addr
    }

    pub fn builder(harness: Arc<AgentHarness>) -> InspectorBuilder {
        InspectorBuilder {
            harness,
            port: 8080,
        }
    }

    pub fn from_env(harness: Arc<AgentHarness>) -> Option<InspectorBuilder> {
        let port_str = std::env::var("SENZA_INSPECT").ok()?;
        let port: u16 = port_str.parse().unwrap_or(8080);
        Some(InspectorBuilder { harness, port })
    }

    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            let _ = h.await;
        }
    }
}

impl Drop for Inspector {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

pub struct InspectorBuilder {
    harness: Arc<AgentHarness>,
    port: u16,
}

impl InspectorBuilder {
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub async fn mount(self) -> Result<Inspector, InspectorError> {
        let token = AuthToken::generate();
        let token_str = token.as_str().to_string();
        let state = InspectorState {
            harness: self.harness,
            token,
        };
        let app = crate::routes::build_router(state);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", self.port))
            .await
            .map_err(|e| InspectorError::Bind(e.to_string()))?;
        let addr = listener.local_addr().ok();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .ok();
        });
        tracing::info!(
            "Agent Inspector listening on http://127.0.0.1:{}  (ws token: {})",
            self.port,
            token_str,
        );
        Ok(Inspector {
            shutdown: Some(shutdown_tx),
            handle: Some(handle),
            bound_addr: addr,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InspectorError {
    #[error("bind failed: {0}")]
    Bind(String),
}
