//! Reusable studio process lifecycle.

use crate::routes::{AppState, team_router};
use crate::settings::Settings;
use crate::team_manager::{TeamManager, list_persisted};
use llm_harness_agent_team::AgentId;
use llm_harness_inspector::Inspector;
use llm_harness_inspector::state::HarnessRegistry;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, thiserror::Error)]
pub enum StudioError {
    #[error("data root preparation failed: {0}")]
    DataRoot(#[source] std::io::Error),
    #[error("studio singleton is already running for data root {root}")]
    AlreadyRunning { root: PathBuf },
    #[error("settings load failed: {0}")]
    Settings(#[from] crate::settings::SettingsError),
    #[error("workspace grants load failed: {0}")]
    Workspace(#[from] crate::workspace::WorkspaceError),
    #[error("inspector mount failed: {0}")]
    Inspector(#[from] llm_harness_inspector::InspectorError),
}

pub struct StudioConfig {
    pub data_root: PathBuf,
    pub port: u16,
    provider_factory: Option<crate::env_cfg::ProviderFactory>,
}

impl StudioConfig {
    pub fn from_env() -> Self {
        let port = std::env::var("STUDIO_PORT")
            .or_else(|_| std::env::var("CT_PORT"))
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or(0);
        Self {
            data_root: default_data_root(),
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

    pub fn with_provider_factory(
        mut self,
        provider_factory: Option<crate::env_cfg::ProviderFactory>,
    ) -> Self {
        self.provider_factory = provider_factory;
        self
    }
}

pub struct StudioRuntime {
    inspector: Inspector,
    pm: Arc<RwLock<TeamManager>>,
    _singleton: StudioSingleton,
}

impl StudioRuntime {
    pub async fn start(config: StudioConfig) -> Result<Self, StudioError> {
        prepare_data_root(&config.data_root).map_err(StudioError::DataRoot)?;
        let singleton = StudioSingleton::acquire(&config.data_root)?;
        let data_root = config.data_root;
        let injected_provider_factory = config.provider_factory;

        let settings = Settings::load(&data_root)?.unwrap_or_default();
        let grants = Arc::new(RwLock::new(crate::workspace::WorkspaceGrants::load(
            &data_root,
        )?));
        let api_key = crate::settings::load_api_key(&data_root).unwrap_or_default();
        let base_url = std::env::var("OPENAI_API_BASE").unwrap_or_default();
        let models = settings.models.clone();
        let key_for_factory = api_key.clone();
        let base_for_factory = base_url.clone();
        match injected_provider_factory {
            Some(provider_factory) => crate::env_cfg::install_provider_factory(provider_factory),
            None => crate::env_cfg::install_provider_factory(Arc::new(move || {
                crate::config::build_providers(&key_for_factory, &base_for_factory, &models)
            })),
        }

        let mut tm = TeamManager::new(&data_root);
        for spec in list_persisted(&data_root) {
            let repo_path = spec.repo_hint();
            if let Some(repo_path) = &repo_path {
                let grants = grants.read().await;
                match grants.contains(repo_path) {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(
                            "team `{}` restore skipped: workspace {} is not authorized",
                            spec.id,
                            repo_path.display()
                        );
                        continue;
                    }
                    Err(error) => {
                        tracing::error!(
                            "team `{}` restore failed to check workspace {}: {error}",
                            spec.id,
                            repo_path.display()
                        );
                        continue;
                    }
                }
            }
            match tm
                .create(spec.clone(), repo_path, &settings, &api_key)
                .await
            {
                Ok(_) => tracing::info!("team `{}` restored", spec.id),
                Err(e) => tracing::error!("team `{}` restore failed: {e}", spec.id),
            }
        }

        let registry = HarnessRegistry::new("default");
        for id in tm.ids() {
            let Some(entry) = tm.get(&id) else {
                continue;
            };
            for member in &entry.spec.members {
                if let Some(harness) = entry.team.harness(&AgentId::new(&member.id)) {
                    registry.register(format!("{id}/{}", member.id), harness);
                }
            }
        }

        let tm_shared = Arc::new(RwLock::new(tm));
        let events = Arc::new(crate::events::EventBus::new());
        let operator_pumps = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        {
            let tm = tm_shared.read().await;
            for id in tm.ids() {
                let Some(entry) = tm.get(&id) else {
                    continue;
                };
                let Some(ctx) = entry.team.social_context().cloned() else {
                    continue;
                };
                crate::routes::spawn_operator_pump(
                    events.clone(),
                    operator_pumps.clone(),
                    id.clone(),
                    ctx,
                );
            }
        }

        let state = AppState {
            pm: tm_shared.clone(),
            data_root: data_root.clone(),
            tier: Arc::new(RwLock::new(settings.models)),
            scout_interval_secs: Arc::new(RwLock::new(settings.scout_interval_secs)),
            events,
            registry: registry.clone(),
            operator_pumps,
            grants,
        };
        let inspector = Inspector::registry(registry)
            .extra_routes(team_router(state))
            .port(config.port)
            .require_auth()
            .mount()
            .await?;

        Ok(Self {
            inspector,
            pm: tm_shared,
            _singleton: singleton,
        })
    }

    pub fn bound_addr(&self) -> Option<std::net::SocketAddr> {
        self.inspector.bound_addr()
    }

    pub fn auth_token(&self) -> Option<&str> {
        self.inspector.auth_token()
    }

    pub fn panel_url(&self) -> Option<String> {
        let addr = self.bound_addr()?;
        let token = self.auth_token()?;
        Some(format!("http://{addr}/app/team.html#token={token}"))
    }

    pub async fn shutdown(self) {
        self.inspector.shutdown().await;
        self.pm.write().await.shutdown_all().await;
    }
}

struct StudioSingleton {
    _guard: fd_lock::RwLockWriteGuard<'static, std::fs::File>,
}

impl StudioSingleton {
    fn acquire(data_root: &Path) -> Result<Self, StudioError> {
        let path = data_root.join("studio.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(StudioError::DataRoot)?;
        let lock: &'static mut fd_lock::RwLock<std::fs::File> =
            Box::leak(Box::new(fd_lock::RwLock::new(file)));
        match lock.try_write() {
            Ok(guard) => Ok(Self { _guard: guard }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Err(StudioError::AlreadyRunning {
                    root: data_root.to_path_buf(),
                })
            }
            Err(error) => Err(StudioError::DataRoot(error)),
        }
    }
}

fn prepare_data_root(data_root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(data_root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(data_root, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn default_data_root() -> PathBuf {
    std::env::var("STUDIO_DATA_ROOT")
        .or_else(|_| std::env::var("CT_DATA_ROOT"))
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| PathBuf::from(home).join(".harness/agent-team-studio"))
                .unwrap_or_else(|| PathBuf::from(".agent-team-studio"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(test: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agent-team-studio-runtime-{test}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn singleton_lock_is_held_for_runtime_lifetime() {
        let root = temp_root("singleton");
        std::fs::create_dir_all(&root).unwrap();
        let first = StudioSingleton::acquire(&root).unwrap();
        assert!(matches!(
            StudioSingleton::acquire(&root),
            Err(StudioError::AlreadyRunning { .. })
        ));
        drop(first);
        let second = StudioSingleton::acquire(&root).unwrap();
        drop(second);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn start_binds_loopback_with_auth_and_panel_url() {
        let root = temp_root("start");
        let _factory = crate::env_cfg::install_test_provider_factory(Arc::new(|| {
            vec![(
                "default".to_string(),
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                    as Arc<dyn llm_harness_loop::LlmClient>,
            )]
        }));
        let runtime = StudioRuntime::start(StudioConfig::from_env().with_data_root(&root))
            .await
            .unwrap();
        let addr = runtime.bound_addr().unwrap();
        assert!(addr.ip().is_loopback());
        let token = runtime.auth_token().unwrap();
        assert!(!token.is_empty());
        let url = runtime.panel_url().unwrap();
        assert!(url.starts_with(&format!("http://{addr}/app/team.html#token=")));
        runtime.shutdown().await;
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn startup_authorizes_restored_workspaces() {
        let root = temp_root("startup-grants");
        let repo = root.join("workspace/repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let spec = crate::team_spec::TeamSpec {
            id: "startup-grants".into(),
            name: "Startup Grants".into(),
            repo_path: Some(repo.to_string_lossy().into_owned()),
            members: vec![crate::team_spec::MemberSpec {
                id: "planner".into(),
                persona: "test".into(),
                role_label: "Planner".into(),
                model: crate::team_spec::ModelRef::Explicit("default".into()),
                toolkits: vec!["fs".into()],
                behaviors: vec![],
                provider_override: None,
                persona_file: None,
            }],
            template: None,
            template_baseline: None,
        };
        crate::team_manager::persist_spec(&root, &spec);
        let provider_factory = Arc::new(move || {
            vec![(
                "default".to_string(),
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                    as Arc<dyn llm_harness_loop::LlmClient>,
            )]
        }) as crate::env_cfg::ProviderFactory;
        let _factory_guard =
            crate::env_cfg::install_test_provider_factory(provider_factory.clone());

        let runtime = StudioRuntime::start(
            StudioConfig::from_env()
                .with_data_root(&root)
                .with_provider_factory(Some(provider_factory.clone())),
        )
        .await
        .unwrap();
        assert!(runtime.pm.read().await.ids().is_empty());
        runtime.shutdown().await;

        crate::workspace::WorkspaceGrants::load(&root)
            .unwrap()
            .add(&repo, None)
            .unwrap();
        let runtime = StudioRuntime::start(
            StudioConfig::from_env()
                .with_data_root(&root)
                .with_provider_factory(Some(provider_factory)),
        )
        .await
        .unwrap();
        assert_eq!(runtime.pm.read().await.ids(), vec!["startup-grants"]);
        runtime.shutdown().await;
        std::fs::remove_dir_all(&root).unwrap();
    }
}
