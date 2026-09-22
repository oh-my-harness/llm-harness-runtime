//! Reusable studio process lifecycle.

use crate::routes::{AppState, team_router};
use crate::settings::Settings;
use crate::team_manager::TeamManager;
use axum::Router;
use llm_harness_inspector::Inspector;
use llm_harness_inspector::state::HarnessRegistry;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupStatus {
    Healthy,
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryFailureReason {
    InvalidSpec,
    WorkspaceUnauthorized,
    WorkspaceCheck,
    TeamCreate,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RecoveryFailure {
    pub team_id: String,
    pub reason: RecoveryFailureReason,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StartupRecovery {
    pub status: StartupStatus,
    pub persisted_teams: usize,
    pub restored_teams: usize,
    pub failed_teams: Vec<RecoveryFailure>,
}

impl Default for StartupRecovery {
    fn default() -> Self {
        Self {
            status: StartupStatus::Healthy,
            persisted_teams: 0,
            restored_teams: 0,
            failed_teams: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StudioError {
    #[error("data root preparation failed: {0}")]
    DataRoot(#[source] std::io::Error),
    #[error("lifecycle journal write failed: {0}")]
    LifecycleLog(#[from] std::io::Error),
    #[error("panel descriptor preparation failed: {0}")]
    PanelDescriptor(#[source] std::io::Error),
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
    panel_path: &'static str,
    provider_factory: Option<crate::env_cfg::ProviderFactory>,
    extra_routes: Option<Router>,
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
            panel_path: "/app/team.html",
            provider_factory: None,
            extra_routes: None,
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

    pub fn with_panel_path(mut self, panel_path: &'static str) -> Self {
        self.panel_path = panel_path;
        self
    }

    pub fn with_provider_factory(
        mut self,
        provider_factory: Option<crate::env_cfg::ProviderFactory>,
    ) -> Self {
        self.provider_factory = provider_factory;
        self
    }

    pub fn with_extra_routes(mut self, extra_routes: Router) -> Self {
        self.extra_routes = Some(extra_routes);
        self
    }
}

pub struct StudioRuntime {
    data_root: PathBuf,
    inspector: Inspector,
    pm: Arc<RwLock<TeamManager>>,
    recovery: StartupRecovery,
    panel_path: &'static str,
    _panel_descriptor: PanelDescriptorGuard,
    _singleton: StudioSingleton,
}

impl StudioRuntime {
    pub async fn start(config: StudioConfig) -> Result<Self, StudioError> {
        let mut config = config;
        let extra_routes = config.extra_routes.take();
        prepare_data_root(&config.data_root).map_err(StudioError::DataRoot)?;
        let singleton = StudioSingleton::acquire(&config.data_root)?;
        clear_panel_descriptor(&config.data_root).map_err(StudioError::PanelDescriptor)?;
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

        let (specs, mut failures) = load_persisted_specs(&data_root);
        let persisted_teams = specs.len() + failures.len();
        let mut restored_teams = 0;
        let mut tm = TeamManager::new(&data_root);
        for spec in specs {
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
                        failures.push(RecoveryFailure {
                            team_id: spec.id,
                            reason: RecoveryFailureReason::WorkspaceUnauthorized,
                            detail: repo_path.display().to_string(),
                        });
                        continue;
                    }
                    Err(error) => {
                        tracing::error!(
                            "team `{}` restore failed to check workspace {}: {error}",
                            spec.id,
                            repo_path.display()
                        );
                        failures.push(RecoveryFailure {
                            team_id: spec.id,
                            reason: RecoveryFailureReason::WorkspaceCheck,
                            detail: error.to_string(),
                        });
                        continue;
                    }
                }
            }
            match tm
                .create(spec.clone(), repo_path, &settings, &api_key)
                .await
            {
                Ok(_) => {
                    tracing::info!("team `{}` restored", spec.id);
                    restored_teams += 1;
                }
                Err(e) => {
                    tracing::error!("team `{}` restore failed: {e}", spec.id);
                    failures.push(RecoveryFailure {
                        team_id: spec.id,
                        reason: RecoveryFailureReason::TeamCreate,
                        detail: e.to_string(),
                    });
                }
            }
        }

        let recovery = StartupRecovery {
            status: if failures.is_empty() {
                StartupStatus::Healthy
            } else {
                StartupStatus::Degraded
            },
            persisted_teams,
            restored_teams,
            failed_teams: failures,
        };

        let registry = HarnessRegistry::new("default");

        let tm_shared = Arc::new(RwLock::new(tm));
        let events = Arc::new(crate::events::EventBus::new());
        let operator_pumps = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let thought_pumps = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));

        let state = AppState {
            pm: tm_shared.clone(),
            data_root: data_root.clone(),
            tier: Arc::new(RwLock::new(settings.models)),
            scout_interval_secs: Arc::new(RwLock::new(settings.scout_interval_secs)),
            events,
            registry: registry.clone(),
            operator_pumps,
            thought_pumps,
            grants,
            startup_recovery: Arc::new(RwLock::new(recovery.clone())),
        };
        {
            let tm = tm_shared.read().await;
            for id in tm.ids() {
                crate::routes::register_team_side_effects(&state, &tm, &id);
            }
        }
        let mut routes = team_router(state);
        if let Some(extra_routes) = extra_routes {
            routes = routes.merge(extra_routes);
        }

        let inspector = Inspector::registry(registry)
            .extra_routes(routes)
            .port(config.port)
            .require_auth()
            .mount()
            .await?;

        let runtime = Self {
            data_root: data_root.clone(),
            inspector,
            pm: tm_shared,
            recovery: recovery.clone(),
            panel_path: config.panel_path,
            _panel_descriptor: PanelDescriptorGuard {
                path: panel_descriptor_path(&data_root),
            },
            _singleton: singleton,
        };
        if let Some(url) = runtime.panel_url()
            && let Err(error) = write_panel_descriptor(&data_root, &url)
        {
            runtime.shutdown().await;
            return Err(StudioError::PanelDescriptor(error));
        }
        if let Err(error) = crate::diagnostics::append_lifecycle_event(
            &data_root,
            serde_json::json!({
                "event": "startup",
                "recovery": recovery,
                "bound_address": runtime.bound_addr().map(|addr| addr.to_string()),
            }),
        ) {
            runtime.shutdown().await;
            return Err(StudioError::LifecycleLog(error));
        }
        Ok(runtime)
    }

    pub fn panel_descriptor_path(&self) -> PathBuf {
        panel_descriptor_path(&self.data_root)
    }

    pub fn bound_addr(&self) -> Option<std::net::SocketAddr> {
        self.inspector.bound_addr()
    }

    pub fn auth_token(&self) -> Option<&str> {
        self.inspector.auth_token()
    }

    pub fn recovery(&self) -> StartupRecovery {
        self.recovery.clone()
    }

    pub fn panel_url(&self) -> Option<String> {
        let addr = self.bound_addr()?;
        let token = self.auth_token()?;
        Some(format!("http://{addr}{}#token={token}", self.panel_path))
    }

    pub async fn shutdown(self) {
        self.inspector.shutdown().await;
        self.pm.write().await.shutdown_all().await;
        if let Err(error) = crate::diagnostics::append_lifecycle_event(
            &self.data_root,
            serde_json::json!({
                "event": "shutdown",
                "reason": "normal",
            }),
        ) {
            eprintln!("agent-team-studio: failed to persist shutdown event: {error}");
        }
    }
}

struct PanelDescriptorGuard {
    path: PathBuf,
}

impl Drop for PanelDescriptorGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let mut temporary = self.path.clone().into_os_string();
        temporary.push(".tmp");
        let _ = std::fs::remove_file(std::path::PathBuf::from(temporary));
    }
}

const PANEL_DESCRIPTOR_SCHEMA: &str = "llm-harness.studio.panel-descriptor.v1";

fn panel_descriptor_path(data_root: &Path) -> PathBuf {
    data_root.join("panel.json")
}

fn panel_descriptor_temp_path(data_root: &Path) -> PathBuf {
    data_root.join("panel.json.tmp")
}

fn clear_panel_descriptor(data_root: &Path) -> std::io::Result<()> {
    for path in [
        panel_descriptor_path(data_root),
        panel_descriptor_temp_path(data_root),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn write_panel_descriptor(data_root: &Path, panel_url: &str) -> std::io::Result<()> {
    clear_panel_descriptor(data_root)?;
    let temporary = panel_descriptor_temp_path(data_root);
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    #[cfg(unix)]
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({
            "schema": PANEL_DESCRIPTOR_SCHEMA,
            "url": panel_url,
        }),
    )?;
    file.flush()?;
    file.sync_all()?;
    set_private_file_permissions(&temporary)?;
    std::fs::rename(&temporary, panel_descriptor_path(data_root))?;
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

struct StudioSingleton {
    _file: std::fs::File,
}

impl StudioSingleton {
    fn acquire(data_root: &Path) -> Result<Self, StudioError> {
        let path = data_root.join("studio.lock");
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        let file = options.open(path).map_err(|error| {
            #[cfg(windows)]
            if error.raw_os_error() == Some(32) {
                return StudioError::AlreadyRunning {
                    root: data_root.to_path_buf(),
                };
            }
            StudioError::DataRoot(error)
        })?;

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    return Err(StudioError::AlreadyRunning {
                        root: data_root.to_path_buf(),
                    });
                }
                return Err(StudioError::DataRoot(error));
            }
        }

        #[cfg(not(unix))]
        let _ = &file;
        Ok(Self { _file: file })
    }
}

fn load_persisted_specs(
    data_root: &Path,
) -> (Vec<crate::team_spec::TeamSpec>, Vec<RecoveryFailure>) {
    let mut specs = Vec::new();
    let mut failures = Vec::new();
    let Ok(entries) = std::fs::read_dir(data_root.join("teams")) else {
        return (specs, failures);
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let team_id = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path().join("team.json");
        match std::fs::read_to_string(&path).and_then(|text| {
            serde_json::from_str::<crate::team_spec::TeamSpec>(&text).map_err(Into::into)
        }) {
            Ok(spec) => specs.push(spec),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => failures.push(RecoveryFailure {
                team_id,
                reason: RecoveryFailureReason::InvalidSpec,
                detail: error.to_string(),
            }),
        }
    }
    specs.sort_by(|a, b| a.id.cmp(&b.id));
    failures.sort_by(|a, b| a.team_id.cmp(&b.team_id));
    (specs, failures)
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
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("panel.json"), "{\"url\":\"stale\"}").unwrap();
        std::fs::write(root.join("panel.json.tmp"), "stale").unwrap();
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
        let token = runtime.auth_token().unwrap().to_owned();
        assert!(!token.is_empty());
        let url = runtime.panel_url().unwrap();
        assert!(url.starts_with(&format!("http://{addr}/app/team.html#token=")));
        let descriptor_path = runtime.panel_descriptor_path();
        let descriptor: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&descriptor_path).unwrap()).unwrap();
        assert_eq!(
            descriptor["schema"],
            "llm-harness.studio.panel-descriptor.v1"
        );
        assert_eq!(descriptor["url"], url);
        assert!(!root.join("panel.json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&descriptor_path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        runtime.shutdown().await;
        assert!(!descriptor_path.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn startup_and_shutdown_are_recorded_without_secrets() {
        let root = temp_root("lifecycle");
        std::fs::create_dir_all(&root).unwrap();
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
        let token = runtime.auth_token().unwrap().to_owned();

        let lifecycle = read_lifecycle_event(&root, "startup");
        assert_eq!(lifecycle["schema"], "llm-harness.studio.lifecycle-event.v1");
        assert_eq!(lifecycle["data"]["event"], "startup");
        assert_eq!(lifecycle["data"]["recovery"]["status"], "healthy");
        assert_eq!(
            lifecycle["data"]["bound_address"],
            runtime.bound_addr().unwrap().to_string()
        );
        assert!(!serde_json::to_string(&lifecycle).unwrap().contains(&token));
        runtime.shutdown().await;

        let lifecycle = read_lifecycle_event(&root, "shutdown");
        assert_eq!(lifecycle["data"]["event"], "shutdown");
        assert_eq!(lifecycle["data"]["reason"], "normal");
        assert!(!serde_json::to_string(&lifecycle).unwrap().contains(&token));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn degraded_recovery_reasons_are_recorded() {
        let root = temp_root("lifecycle-degraded");
        let repo = root.join("unauthorized");
        let spec = recovery_spec("unauthorized", Some(&repo));
        write_spec(
            &root,
            "unauthorized",
            &serde_json::to_string(&spec).unwrap(),
        );
        write_spec(&root, "invalid", "{\"id\":\"invalid\"}");
        let provider_factory = Arc::new(|| {
            vec![(
                "default".to_string(),
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                    as Arc<dyn llm_harness_loop::LlmClient>,
            )]
        }) as crate::env_cfg::ProviderFactory;
        let _factory = crate::env_cfg::install_test_provider_factory(provider_factory.clone());

        let runtime = StudioRuntime::start(
            StudioConfig::from_env()
                .with_data_root(&root)
                .with_provider_factory(Some(provider_factory)),
        )
        .await
        .unwrap();
        let recovery = runtime.recovery();
        runtime.shutdown().await;

        let lifecycle = read_lifecycle_event(&root, "startup");
        assert_eq!(lifecycle["data"]["event"], "startup");
        assert_eq!(lifecycle["data"]["recovery"]["status"], "degraded");
        assert_eq!(
            lifecycle["data"]["recovery"]["failed_teams"],
            serde_json::to_value(recovery.failed_teams).unwrap()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn read_lifecycle_event(root: &Path, event: &str) -> serde_json::Value {
        std::fs::read_to_string(root.join("logs/runtime.jsonl"))
            .unwrap()
            .lines()
            .map(|line| {
                let value: serde_json::Value = line.parse().unwrap();
                value
            })
            .find(|value| value["data"]["event"] == event)
            .unwrap()
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

    fn recovery_spec(id: &str, repo_path: Option<&Path>) -> crate::team_spec::TeamSpec {
        crate::team_spec::TeamSpec {
            id: id.into(),
            name: "Startup Recovery".into(),
            repo_path: repo_path.map(|path| path.to_string_lossy().into_owned()),
            members: vec![crate::team_spec::MemberSpec {
                id: "planner".into(),
                persona: "test".into(),
                role_label: "Planner".into(),
                model: crate::team_spec::ModelRef::Explicit("default".into()),
                toolkits: vec![],
                behaviors: vec![],
                provider_override: None,
                persona_file: None,
            }],
            template: None,
            template_baseline: None,
        }
    }

    fn write_spec(root: &Path, id: &str, value: &str) {
        let team_dir = root.join("teams").join(id);
        std::fs::create_dir_all(&team_dir).unwrap();
        std::fs::write(team_dir.join("team.json"), value).unwrap();
    }

    #[tokio::test]
    async fn startup_reports_degraded_recovery() {
        let root = temp_root("startup-degraded");
        let repo = root.join("unauthorized");
        let spec = recovery_spec("unauthorized", Some(&repo));
        write_spec(
            &root,
            "unauthorized",
            &serde_json::to_string(&spec).unwrap(),
        );
        write_spec(&root, "invalid", "{\"id\":\"invalid\"}");
        let provider_factory = Arc::new(|| {
            vec![(
                "default".to_string(),
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                    as Arc<dyn llm_harness_loop::LlmClient>,
            )]
        }) as crate::env_cfg::ProviderFactory;
        let _factory = crate::env_cfg::install_test_provider_factory(provider_factory.clone());

        let runtime = StudioRuntime::start(
            StudioConfig::from_env()
                .with_data_root(&root)
                .with_provider_factory(Some(provider_factory)),
        )
        .await
        .unwrap();
        let recovery = runtime.recovery();
        assert_eq!(recovery.status, StartupStatus::Degraded);
        assert_eq!(recovery.persisted_teams, 2);
        assert_eq!(recovery.restored_teams, 0);
        assert_eq!(
            recovery
                .failed_teams
                .iter()
                .map(|failure| (failure.team_id.as_str(), failure.reason))
                .collect::<Vec<_>>(),
            vec![
                ("invalid", RecoveryFailureReason::InvalidSpec),
                ("unauthorized", RecoveryFailureReason::WorkspaceUnauthorized),
            ]
        );
        runtime.shutdown().await;
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn startup_reports_healthy_recovery() {
        let root = temp_root("startup-healthy");
        let spec = recovery_spec("healthy", None);
        write_spec(&root, "healthy", &serde_json::to_string(&spec).unwrap());
        let provider_factory = Arc::new(|| {
            vec![(
                "default".to_string(),
                Arc::new(llm_harness_loop::test_utils::MockLlmClient::new(vec![]))
                    as Arc<dyn llm_harness_loop::LlmClient>,
            )]
        }) as crate::env_cfg::ProviderFactory;
        let _factory = crate::env_cfg::install_test_provider_factory(provider_factory.clone());

        let runtime = StudioRuntime::start(
            StudioConfig::from_env()
                .with_data_root(&root)
                .with_provider_factory(Some(provider_factory)),
        )
        .await
        .unwrap();
        let recovery = runtime.recovery();
        assert_eq!(recovery.status, StartupStatus::Healthy);
        assert_eq!(recovery.persisted_teams, 1);
        assert_eq!(recovery.restored_teams, 1);
        assert!(recovery.failed_teams.is_empty());
        runtime.shutdown().await;
        std::fs::remove_dir_all(&root).unwrap();
    }
}
