use llm_harness_agent_team_studio::env_cfg::ProviderFactory;
use llm_harness_agent_team_studio::runtime::{StudioConfig, StudioError, StudioRuntime};
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;

pub struct RuntimeAppConfig {
    pub data_root: PathBuf,
    pub port: u16,
    provider_factory: Option<ProviderFactory>,
}

impl RuntimeAppConfig {
    pub fn from_env() -> Self {
        let data_root = std::env::var("RUNTIME_APP_DATA_ROOT")
            .or_else(|_| std::env::var("STUDIO_DATA_ROOT"))
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(llm_harness_agent_team_studio::runtime::default_data_root);
        let port = std::env::var("RUNTIME_APP_PORT")
            .or_else(|_| std::env::var("STUDIO_PORT"))
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or(0);
        Self {
            data_root,
            port,
            provider_factory: None,
        }
    }

    pub fn with_data_root(mut self, data_root: impl Into<PathBuf>) -> Self {
        self.data_root = data_root.into();
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_provider_factory(mut self, provider_factory: Option<ProviderFactory>) -> Self {
        self.provider_factory = provider_factory;
        self
    }
}

pub struct RuntimeApp {
    runtime: StudioRuntime,
}

impl RuntimeApp {
    pub async fn start(config: RuntimeAppConfig) -> Result<Self, StudioError> {
        let startup_recovery = Arc::new(RwLock::new(None));
        let state = crate::routes::AppState {
            data_root: config.data_root.clone(),
            startup_recovery: startup_recovery.clone(),
        };
        let extra_routes = crate::routes::app_router(state);
        let studio_config = StudioConfig::from_env()
            .with_data_root(&config.data_root)
            .with_port(config.port)
            .with_panel_path("/app/agent-team")
            .with_provider_factory(config.provider_factory)
            .with_extra_routes(extra_routes);
        let runtime = StudioRuntime::start(studio_config).await?;
        *startup_recovery.write() = Some(runtime.recovery());
        Ok(Self { runtime })
    }

    pub fn panel_descriptor_path(&self) -> PathBuf {
        self.runtime.panel_descriptor_path()
    }

    pub fn bound_addr(&self) -> Option<std::net::SocketAddr> {
        self.runtime.bound_addr()
    }

    pub fn auth_token(&self) -> Option<&str> {
        self.runtime.auth_token()
    }

    pub fn panel_url(&self) -> Option<String> {
        self.runtime.panel_url()
    }

    pub async fn shutdown(self) {
        self.runtime.shutdown().await;
    }
}
