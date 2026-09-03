//! axum server lifecycle and public Inspector API.

use crate::state::{AuthToken, HarnessRegistry, InspectorState};
use llm_harness_agent::AgentHarness;
use std::sync::Arc;

pub struct Inspector {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
    bound_addr: Option<std::net::SocketAddr>,
    auth_token: Option<String>,
}

impl Inspector {
    pub fn bound_addr(&self) -> Option<std::net::SocketAddr> {
        self.bound_addr
    }

    /// Return the token accepted by WebSocket event requests.
    ///
    /// `require_auth` also accepts this token for all `/api/*` requests. The
    /// token is intentionally not written to logs; hosts that need to hand it
    /// to a local UI should pass it through this accessor instead.
    pub fn auth_token(&self) -> Option<&str> {
        self.auth_token.as_deref()
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
            require_auth: false,
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
            require_auth: false,
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
    require_auth: bool,
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

    /// Require a per-process bearer token for all `/api/*` requests.
    ///
    /// Desktop and other multi-user-capable hosts must enable this. Static
    /// assets remain unauthenticated so the bundled UI can bootstrap; every
    /// endpoint that can expose or mutate agent state is protected.
    pub fn require_auth(mut self) -> Self {
        self.require_auth = true;
        self
    }

    pub async fn mount(self) -> Result<Inspector, InspectorError> {
        let token = AuthToken::generate();
        let token_str = token.as_str().to_string();
        let state = InspectorState {
            registry: self.registry,
            token: token.clone(),
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
        if self.require_auth {
            app = app.layer(axum::middleware::from_fn_with_state(
                token.clone(),
                local_api_auth,
            ));
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
        let bound_addr = addr.ok_or(InspectorError::Bind(
            "listener did not expose its bound address".to_string(),
        ))?;
        tracing::info!(
            "Inspector (headless) ready: http://{bound_addr} (auth token available via Inspector::auth_token; no bundled UI)"
        );
        Ok(Inspector {
            shutdown: Some(shutdown_tx),
            handle: Some(handle),
            bound_addr: addr,
            auth_token: Some(token_str),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InspectorError {
    #[error("bind failed: {0}")]
    Bind(String),
}

fn authority_host(authority: &str) -> &str {
    let authority = if let Some((_, host_port)) = authority.rsplit_once('@') {
        host_port
    } else {
        authority
    };
    if authority.starts_with('[')
        && let Some(end) = authority.find(']')
    {
        return &authority[..=end];
    }
    authority.split(':').next().unwrap_or(authority)
}

fn is_local_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

fn origin_host(origin: &str) -> Option<&str> {
    let (scheme, rest) = origin.split_once("://")?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    Some(authority_host(authority))
}

async fn local_api_auth(
    axum::extract::State(token): axum::extract::State<AuthToken>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    if !request.uri().path().starts_with("/api/") {
        return next.run(request).await;
    }

    let headers = request.headers();
    let host_valid = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(|authority| is_local_host(authority_host(authority)))
        .unwrap_or(false);
    if !host_valid {
        return StatusCode::FORBIDDEN.into_response();
    }

    if let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        match origin_host(origin) {
            Some(host) if is_local_host(host) => {}
            _ => return StatusCode::FORBIDDEN.into_response(),
        }
    }

    let header_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let query_token = request.uri().query().and_then(|query| {
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == "token").then(|| value.to_owned())
        })
    });
    let provided = header_token
        .map(str::to_owned)
        .or(query_token)
        .unwrap_or_default();
    if provided.is_empty() || !token.matches(&provided) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    next.run(request).await
}
