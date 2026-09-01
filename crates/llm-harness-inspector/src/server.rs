//! axum server lifecycle and public Inspector API.

use crate::state::{AuthToken, HarnessRegistry, InspectorState};
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

    /// Builder for a single-harness inspector (legacy-compatible).
    /// The harness is registered under the default key `"default"`.
    pub fn builder(harness: Arc<AgentHarness>) -> InspectorBuilder {
        let registry = HarnessRegistry::new("default");
        registry.register("default", harness);
        InspectorBuilder {
            registry,
            port: 8080,
            shared: None,
            extra_routes: None,
            extra_static: None,
        }
    }

    /// Builder over a pre-populated multi-agent registry.
    pub fn registry(registry: Arc<HarnessRegistry>) -> InspectorBuilder {
        InspectorBuilder {
            registry,
            port: 8080,
            shared: None,
            extra_routes: None,
            extra_static: None,
        }
    }

    pub fn from_env(harness: Arc<AgentHarness>) -> Option<InspectorBuilder> {
        let port_str = std::env::var("SENZA_INSPECT").ok()?;
        let port: u16 = port_str.parse().unwrap_or(8080);
        Some(Self::builder(harness).port(port))
    } // shared remains None for the legacy single-harness path

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
    registry: Arc<HarnessRegistry>,
    port: u16,
    shared: Option<Arc<crate::state::SharedEvents>>,
    extra_routes: Option<axum::Router>,
    extra_static: Option<(String, std::path::PathBuf)>,
}

impl InspectorBuilder {
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Attach an external-events channel (virtual agents).
    pub fn shared(mut self, shared: Arc<crate::state::SharedEvents>) -> Self {
        self.shared = Some(shared);
        self
    }

    /// Merge additional domain routes into the served router.
    ///
    /// Upper-layer applications (coding-team, bench, ...) mount their own
    /// axum routers here instead of forking the inspector. These routes sit
    /// behind the same security model as the built-in REST endpoints
    /// (loopback bind); the WS token check is per-handler and unaffected.
    pub fn extra_routes(mut self, router: axum::Router) -> Self {
        self.extra_routes = Some(router);
        self
    }

    /// Serve a static directory under `/{prefix}` (e.g. `("app", dir)` →
    /// `/app/*`). Minimal built-in file server: GET only, path-traversal
    /// safe, small MIME whitelist — no `tower-http` dependency.
    pub fn extra_static(mut self, prefix: &str, dir: impl Into<std::path::PathBuf>) -> Self {
        self.extra_static = Some((prefix.to_string(), dir.into()));
        self
    }

    pub async fn mount(self) -> Result<Inspector, InspectorError> {
        let token = AuthToken::generate();
        let token_str = token.as_str().to_string();
        let state = InspectorState {
            registry: self.registry,
            token,
            shared: self.shared,
        };
        let mut app = crate::routes::build_router(state);
        if let Some(extra) = self.extra_routes {
            app = app.merge(extra);
        }
        if let Some((prefix, dir)) = self.extra_static {
            app = app
                .route(
                    &format!("/{prefix}/{{*rest}}"),
                    axum::routing::get(crate::static_files::serve(dir.clone())),
                )
                .route(
                    &format!("/{prefix}"),
                    axum::routing::get(crate::static_files::serve(dir)),
                );
        }
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
            "Inspector (headless) ready: http://127.0.0.1:{}  — token: {} (API auth; no bundled UI)",
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
