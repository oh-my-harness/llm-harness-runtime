use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use llm_harness_platform::sandbox::{NetRule, ResourceLimits, Sandbox, SandboxConfig};
#[cfg(target_os = "linux")]
use llm_harness_sandbox::BwrapSandbox;
#[cfg(target_os = "macos")]
use llm_harness_sandbox::SeatbeltSandbox;
use llm_harness_sandbox_remote::RemoteSandboxConfig;
use llm_harness_sandbox_remote::protocol::RemoteSandboxState;
use llm_harness_types::ExecutionEnv;
#[cfg(target_os = "linux")]
use llm_harness_types::{EnvError, FileInfo, ShellOptions, ShellOutput};
#[cfg(target_os = "linux")]
use llm_harness_vm_agent_env::GuestAgentEnv;
#[cfg(target_os = "linux")]
use llm_harness_vm_firecracker::{FirecrackerLifecycle, FirecrackerLifecycleConfig};
use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use crate::audit::{AuditEvent, GatewayAudit};
use crate::auth::AuthenticatedPrincipal;
use crate::credentials::CredentialStore;
use crate::observability::{RateLimiter, RequestMetrics};

pub(crate) const GUEST_WORK_DIR: &str = "/workspace";

pub(crate) type SharedState = Arc<GatewayState>;

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct GatewayState {
    config: crate::auth::GatewayConfig,
    work_root: PathBuf,
    _work_root_lock: File,
    audit: Option<GatewayAudit>,
    registry: std::sync::Mutex<Connection>,
    sandboxes: std::sync::Mutex<HashMap<String, SandboxRecord>>,
    executions: std::sync::Mutex<HashMap<String, ExecutionRecord>>,
    lifecycle_locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    metrics: RequestMetrics,
    rate_limiter: RateLimiter,
    audit_failure_mode: crate::auth::AuditFailureMode,
    credentials: CredentialStore,
}

#[derive(Debug)]
pub(crate) enum GatewayStateError {
    InvalidConfig(anyhow::Error),
    Storage(anyhow::Error),
    Audit(anyhow::Error),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Capacity,
    Busy,
    NotFound,
}

impl GatewayStateError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::InvalidConfig(error) => error.to_string(),
            Self::Storage(_) => "sandbox registry operation failed".to_owned(),
            Self::Audit(_) => "gateway audit sink is unavailable".to_owned(),
            Self::Capacity => "gateway sandbox limit reached".to_owned(),
            Self::Busy => "sandbox is busy; retry after active requests complete".to_owned(),
            Self::NotFound => "sandbox not found".to_owned(),
        }
    }

    pub(crate) fn detail(&self) -> String {
        match self {
            Self::Storage(error) => error.to_string(),
            Self::Audit(error) => error.to_string(),
            other => other.message(),
        }
    }
}

pub(crate) struct SandboxRecord {
    pub(crate) owner: Arc<str>,
    pub(crate) sandbox: Arc<GatewaySandbox>,
    pub(crate) env: Arc<dyn ExecutionEnv>,
    pub(crate) work_dir: PathBuf,
    pub(crate) config: RemoteSandboxConfig,
    pub(crate) state: RemoteSandboxState,
}

#[cfg(target_os = "linux")]
pub(crate) enum GatewaySandbox {
    Bwrap(Box<BwrapSandbox>),
    Firecracker(Box<FirecrackerSandbox>),
}

/// macOS backend: macOS `sandbox-exec` isolation via `SeatbeltSandbox`.
#[cfg(target_os = "macos")]
pub(crate) enum GatewaySandbox {
    Seatbelt(SeatbeltSandbox),
}

#[cfg(target_os = "macos")]
impl Sandbox for GatewaySandbox {
    fn start(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        match self {
            GatewaySandbox::Seatbelt(sandbox) => sandbox.start(),
        }
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        match self {
            GatewaySandbox::Seatbelt(sandbox) => sandbox.env(),
        }
    }

    fn reset(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        match self {
            GatewaySandbox::Seatbelt(sandbox) => sandbox.reset(),
        }
    }

    fn shutdown(self: Box<Self>) -> BoxFuture<'static, anyhow::Result<()>> {
        match *self {
            GatewaySandbox::Seatbelt(sandbox) => Sandbox::shutdown(Box::new(sandbox)),
        }
    }

    fn config(&self) -> &SandboxConfig {
        match self {
            GatewaySandbox::Seatbelt(sandbox) => sandbox.config(),
        }
    }

    fn is_running(&self) -> bool {
        match self {
            GatewaySandbox::Seatbelt(sandbox) => sandbox.is_running(),
        }
    }
}

#[cfg(target_os = "linux")]
impl Sandbox for GatewaySandbox {
    fn start(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        match self {
            GatewaySandbox::Bwrap(sandbox) => Sandbox::start(&**sandbox),
            GatewaySandbox::Firecracker(sandbox) => Box::pin(sandbox.start()),
        }
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        match self {
            GatewaySandbox::Bwrap(sandbox) => Sandbox::env(&**sandbox),
            GatewaySandbox::Firecracker(sandbox) => FirecrackerSandbox::env(sandbox),
        }
    }

    fn reset(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        match self {
            GatewaySandbox::Bwrap(sandbox) => Sandbox::reset(&**sandbox),
            GatewaySandbox::Firecracker(sandbox) => Box::pin(sandbox.reset()),
        }
    }

    fn shutdown(self: Box<Self>) -> BoxFuture<'static, anyhow::Result<()>> {
        match *self {
            GatewaySandbox::Bwrap(sandbox) => Sandbox::shutdown(sandbox),
            GatewaySandbox::Firecracker(sandbox) => Box::pin(sandbox.shutdown_owned()),
        }
    }

    fn config(&self) -> &SandboxConfig {
        match self {
            GatewaySandbox::Bwrap(sandbox) => Sandbox::config(&**sandbox),
            GatewaySandbox::Firecracker(sandbox) => FirecrackerSandbox::config(sandbox),
        }
    }

    fn is_running(&self) -> bool {
        match self {
            GatewaySandbox::Bwrap(sandbox) => Sandbox::is_running(&**sandbox),
            GatewaySandbox::Firecracker(sandbox) => FirecrackerSandbox::is_running(sandbox),
        }
    }
}

/// Other non-Linux platforms: no sandbox backend is wired; construction
/// paths fail closed before reaching this type.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[derive(Debug)]
pub(crate) enum GatewaySandbox {}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl Sandbox for GatewaySandbox {
    fn start(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        unreachable!("no sandbox backend is available on this platform")
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        unreachable!("no sandbox backend is available on this platform")
    }

    fn reset(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        unreachable!("no sandbox backend is available on this platform")
    }

    fn shutdown(self: Box<Self>) -> BoxFuture<'static, anyhow::Result<()>> {
        unreachable!("no sandbox backend is available on this platform")
    }

    fn config(&self) -> &SandboxConfig {
        unreachable!("no sandbox backend is available on this platform")
    }

    fn is_running(&self) -> bool {
        false
    }
}

#[cfg(target_os = "linux")]
pub(crate) struct FirecrackerSandboxConfig {
    backend: crate::auth::FirecrackerBackendConfig,
    run_directory: PathBuf,
    vsock_guest_cid: u32,
}

#[cfg(target_os = "linux")]
struct PendingGuestAgentEnv {
    env: Arc<std::sync::RwLock<Option<Arc<dyn ExecutionEnv>>>>,
}

#[cfg(target_os = "linux")]
impl PendingGuestAgentEnv {
    fn new() -> Self {
        Self {
            env: Arc::new(std::sync::RwLock::new(None)),
        }
    }
    fn set(&self, env: Arc<dyn ExecutionEnv>) {
        *self
            .env
            .write()
            .expect("firecracker guest env lock is poisoned") = Some(env);
    }

    fn clear(&self) {
        *self
            .env
            .write()
            .expect("firecracker guest env lock is poisoned") = None;
    }

    fn env(&self) -> Result<Arc<dyn ExecutionEnv>, EnvError> {
        self.env
            .read()
            .expect("firecracker guest env lock is poisoned")
            .clone()
            .ok_or_else(|| EnvError::Other("firecracker sandbox is not ready".to_owned()))
    }
}

#[cfg(target_os = "linux")]
impl ExecutionEnv for PendingGuestAgentEnv {
    fn working_dir(&self) -> &Path {
        Path::new(GUEST_WORK_DIR)
    }

    fn read_text_file<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<String, EnvError>> {
        Box::pin(async move { self.env()?.read_text_file(path, abort).await })
    }

    fn read_text_lines<'a>(
        &'a self,
        path: &'a Path,
        max_lines: Option<usize>,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<String>, EnvError>> {
        Box::pin(async move { self.env()?.read_text_lines(path, max_lines, abort).await })
    }

    fn read_binary_file<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<u8>, EnvError>> {
        Box::pin(async move { self.env()?.read_binary_file(path, abort).await })
    }

    fn write_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        Box::pin(async move { self.env()?.write_file(path, content, abort).await })
    }

    fn append_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        Box::pin(async move { self.env()?.append_file(path, content, abort).await })
    }

    fn file_info<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<FileInfo, EnvError>> {
        Box::pin(async move { self.env()?.file_info(path, abort).await })
    }

    fn list_dir<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<FileInfo>, EnvError>> {
        Box::pin(async move { self.env()?.list_dir(path, abort).await })
    }

    fn exists<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<bool, EnvError>> {
        Box::pin(async move { self.env()?.exists(path, abort).await })
    }

    fn create_dir<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        Box::pin(async move { self.env()?.create_dir(path, recursive, abort).await })
    }

    fn remove<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        force: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        Box::pin(async move { self.env()?.remove(path, recursive, force, abort).await })
    }

    fn create_temp_dir<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<PathBuf, EnvError>> {
        Box::pin(async move { self.env()?.create_temp_dir(prefix).await })
    }

    fn execute_shell<'a>(
        &'a self,
        cmd: &'a str,
        opts: ShellOptions<'a>,
    ) -> BoxFuture<'a, Result<ShellOutput, EnvError>> {
        Box::pin(async move { self.env()?.execute_shell(cmd, opts).await })
    }

    fn cleanup<'a>(&'a self) -> BoxFuture<'a, Result<(), EnvError>> {
        Box::pin(async move { self.env()?.cleanup().await })
    }
}

#[cfg(target_os = "linux")]
pub(crate) struct FirecrackerSandbox {
    config: SandboxConfig,
    firecracker_config: Option<FirecrackerSandboxConfig>,
    lifecycle: tokio::sync::Mutex<Option<FirecrackerLifecycle>>,
    env: Arc<PendingGuestAgentEnv>,
    running: AtomicBool,
}

#[cfg(target_os = "linux")]
impl FirecrackerSandbox {
    fn new(config: SandboxConfig, firecracker_config: FirecrackerSandboxConfig) -> Self {
        Self {
            config,
            firecracker_config: Some(firecracker_config),
            lifecycle: tokio::sync::Mutex::new(None),
            env: Arc::new(PendingGuestAgentEnv::new()),
            running: AtomicBool::new(false),
        }
    }

    async fn start(&self) -> anyhow::Result<()> {
        let mut lifecycle = self.lifecycle.lock().await;
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.running.store(false, Ordering::SeqCst);
        self.env.clear();
        let current = lifecycle.take();
        if let Some(mut current) = current {
            current.stop().await?;
        }
        let config = self.lifecycle_config()?;
        let started = FirecrackerLifecycle::start(&config).await?;
        let Some(client) = started.guest_agent_shared() else {
            let mut failed = started;
            let stop_error = failed.stop().await.err();
            anyhow::bail!(
                "firecracker guest agent is unavailable{}",
                stop_error
                    .map(|error| format!("; cleanup failed: {error}"))
                    .unwrap_or_default()
            );
        };
        let guest_env = GuestAgentEnv::from_shared(client);
        self.env.set(Arc::new(guest_env));
        *lifecycle = Some(started);
        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn reset(&self) -> anyhow::Result<()> {
        if !self.running.load(Ordering::SeqCst) {
            anyhow::bail!("sandbox is not running; cannot reset");
        }
        self.running.store(false, Ordering::SeqCst);
        let mut lifecycle = self.lifecycle.lock().await;
        if let Some(mut current) = lifecycle.take() {
            current
                .graceful_stop(
                    self.config
                        .resource_limits
                        .timeout
                        .unwrap_or(std::time::Duration::from_secs(5)),
                )
                .await?;
        }
        self.env.clear();
        let config = self.lifecycle_config()?;
        let started = FirecrackerLifecycle::start(&config).await?;
        let Some(client) = started.guest_agent_shared() else {
            let mut failed = started;
            let stop_error = failed.stop().await.err();
            anyhow::bail!(
                "firecracker guest agent is unavailable{}",
                stop_error
                    .map(|error| format!("; cleanup failed: {error}"))
                    .unwrap_or_default()
            );
        };
        let guest_env = GuestAgentEnv::from_shared(client);
        self.env.set(Arc::new(guest_env));
        *lifecycle = Some(started);
        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn shutdown_owned(self: Box<Self>) -> anyhow::Result<()> {
        self.running.store(false, Ordering::SeqCst);
        let mut lifecycle = self.lifecycle.lock().await;
        if let Some(mut current) = lifecycle.take() {
            current
                .graceful_stop(
                    self.config
                        .resource_limits
                        .timeout
                        .unwrap_or(std::time::Duration::from_secs(5)),
                )
                .await?;
        }
        self.env.clear();
        if let Some(config) = self.firecracker_config.as_ref() {
            tokio::fs::remove_dir_all(&config.run_directory).await?;
        }
        Ok(())
    }

    fn lifecycle_config(&self) -> anyhow::Result<FirecrackerLifecycleConfig> {
        let FirecrackerSandboxConfig {
            backend,
            run_directory,
            vsock_guest_cid,
        } = self
            .firecracker_config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("firecracker sandbox is not configured"))?;
        let limits = &self.config.resource_limits;
        Ok(FirecrackerLifecycleConfig::new(
            &backend.binary_path,
            &backend.kernel_image_path,
            &backend.base_image_path,
            run_directory,
            limits
                .max_cpus
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(backend.max_vcpu_count),
            limits
                .max_memory_mb
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(backend.max_memory_mib),
            *vsock_guest_cid,
        )
        .with_guest_agent_port(backend.guest_agent_port)
        .with_image_capacity(
            limits
                .max_disk_mb
                .and_then(|value| u64::try_from(value).ok())
                .map(|value| value.saturating_mul(1024 * 1024)),
        )
        .with_readiness_timeout(backend.readiness_timeout))
    }

    fn config(&self) -> &SandboxConfig {
        &self.config
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        Arc::clone(&self.env) as Arc<dyn ExecutionEnv>
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

pub(crate) struct ExecutionRecord {
    pub(crate) sandbox_id: String,
    pub(crate) abort: CancellationToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistryState {
    Creating,
    Running,
    Deleting,
}

impl RegistryState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Running => "running",
            Self::Deleting => "deleting",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "creating" => Ok(Self::Creating),
            "running" => Ok(Self::Running),
            "deleting" => Ok(Self::Deleting),
            other => anyhow::bail!("invalid sandbox registry state {other:?}"),
        }
    }
}

#[derive(Debug)]
struct PersistedSandbox {
    sandbox_id: String,
    owner: String,
    config: RemoteSandboxConfig,
    vsock_guest_cid: Option<u32>,
    state: RegistryState,
}

impl GatewayState {
    pub(crate) fn new(mut config: crate::auth::GatewayConfig) -> anyhow::Result<Self> {
        config.validate()?;
        let audit_failure_mode = config.audit_failure_mode();
        let work_root = config.prepare_work_root()?;
        let sandbox_root = work_root.join("sandboxes");
        create_private_directory(&sandbox_root)?;
        let work_root_lock = lock_work_root(&work_root)?;
        let registry = std::sync::Mutex::new(open_registry(&work_root.join("registry.sqlite"))?);
        let credentials = CredentialStore::new(
            &work_root.join("credentials.sqlite"),
            &config.credential_seeds(),
        )?;
        let audit_path = config
            .audit_path()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| work_root.join("audit.jsonl"));
        let audit = GatewayAudit::new(audit_path)?;
        let persisted = reconcile_registry(&registry, &sandbox_root, &config)?;
        if persisted.len() > config.max_sandboxes {
            anyhow::bail!(
                "persisted sandbox count {} exceeds configured maximum {}",
                persisted.len(),
                config.max_sandboxes
            );
        }
        let mut sandboxes = HashMap::with_capacity(persisted.len());
        for record in persisted {
            let sandbox_id = record.sandbox_id.clone();
            let record = restore_sandbox(&config, record)?;
            #[cfg(target_os = "linux")]
            let registry_state =
                if matches!(record.sandbox.as_ref(), GatewaySandbox::Firecracker(_)) {
                    RegistryState::Creating
                } else {
                    RegistryState::Running
                };
            #[cfg(not(target_os = "linux"))]
            let registry_state = RegistryState::Running;
            set_registry_state(&registry, &sandbox_id, registry_state)?;
            sandboxes.insert(sandbox_id, record);
        }
        Ok(Self {
            config,
            work_root,
            _work_root_lock: work_root_lock,
            audit: Some(audit),
            registry,
            sandboxes: std::sync::Mutex::new(sandboxes),
            executions: std::sync::Mutex::new(HashMap::new()),
            lifecycle_locks: std::sync::Mutex::new(HashMap::new()),
            metrics: RequestMetrics::default(),
            rate_limiter: RateLimiter::default(),
            audit_failure_mode,
            credentials,
        })
    }

    pub(crate) fn authenticate(&self, bearer_token: &str) -> Option<AuthenticatedPrincipal> {
        let principal_id = self.credentials.authenticate(bearer_token)?;
        self.config.principal(&principal_id)
    }

    pub(crate) fn rotate_credential(
        &self,
        principal_id: &str,
        bearer_token: &str,
    ) -> Result<(), GatewayStateError> {
        self.credentials
            .rotate(principal_id, bearer_token)
            .map_err(GatewayStateError::Storage)
    }

    pub(crate) fn revoke_credential(&self, principal_id: &str) -> Result<(), GatewayStateError> {
        self.credentials
            .revoke(principal_id)
            .map_err(GatewayStateError::Storage)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn audit_request(
        &self,
        event: AuditEvent,
        request_id: &str,
        principal: Option<&str>,
        method: &str,
        path: &str,
        status: Option<u16>,
        sandbox_id: Option<&str>,
        duration_ms: Option<u64>,
        detail: serde_json::Value,
    ) -> Result<(), GatewayStateError> {
        let Some(audit) = self.audit.as_ref() else {
            return Ok(());
        };
        if let Err(error) = audit.record(
            event,
            request_id,
            principal.unwrap_or("anonymous"),
            method,
            crate::observability::metric_route(path),
            path,
            status,
            sandbox_id,
            duration_ms,
            detail,
        ) {
            return match self.audit_failure_mode {
                crate::auth::AuditFailureMode::FailOpen => {
                    tracing::warn!(
                        request_id = %request_id,
                        error = %error,
                        "failed to write gateway audit record"
                    );
                    Ok(())
                }
                crate::auth::AuditFailureMode::FailClosed => Err(GatewayStateError::Audit(error)),
            };
        }
        Ok(())
    }

    pub(crate) fn config(&self) -> &crate::auth::GatewayConfig {
        &self.config
    }

    pub(crate) fn backend_label(&self) -> &'static str {
        match self.config.backend() {
            crate::auth::GatewayBackend::Bwrap => "linux-bwrap",
            #[cfg(target_os = "macos")]
            crate::auth::GatewayBackend::Seatbelt => "macos-seatbelt",
            crate::auth::GatewayBackend::Firecracker(_) => "linux-firecracker",
        }
    }

    pub(crate) fn record_request(
        &self,
        method: &str,
        route: &str,
        principal: Option<&str>,
        status: u16,
        duration: std::time::Duration,
    ) {
        self.metrics
            .record(method, route, principal, status, duration);
    }

    pub(crate) fn render_metrics(&self) -> String {
        self.metrics
            .render(self.backend_label(), self.active_sandbox_count())
    }

    pub(crate) fn check_rate_limit(
        &self,
        principal: &str,
        requests_per_minute: Option<usize>,
    ) -> Option<std::time::Duration> {
        self.rate_limiter.check(principal, requests_per_minute)
    }

    fn active_sandbox_count(&self) -> usize {
        self.sandboxes
            .lock()
            .map(|sandboxes| sandboxes.len())
            .unwrap_or(0)
    }

    /// Trust profile actually granted to a sandbox (from its stored config).
    pub(crate) fn requested_profile(
        &self,
        sandbox_id: &str,
    ) -> llm_harness_platform::trust::TrustProfile {
        match self.sandboxes.lock() {
            Ok(sandboxes) => sandboxes
                .get(sandbox_id)
                .map(|record| record.config.trust_profile)
                .unwrap_or_default(),
            Err(_) => Default::default(),
        }
    }

    pub(crate) async fn create_sandbox(
        &self,
        principal: &AuthenticatedPrincipal,
        request: RemoteSandboxConfig,
    ) -> Result<String, GatewayStateError> {
        if request.trust_profile > principal.max_trust_profile {
            return Err(GatewayStateError::InvalidConfig(anyhow::anyhow!(
                "trust profile {:?} exceeds principal cap {:?}",
                request.trust_profile,
                principal.max_trust_profile
            )));
        }
        #[cfg(target_os = "linux")]
        if let crate::auth::GatewayBackend::Firecracker(backend) = self.config.backend() {
            return self
                .create_firecracker_sandbox(principal, request, backend)
                .await;
        }
        #[cfg(target_os = "linux")]
        if let crate::auth::GatewayBackend::Bwrap = self.config.backend() {
            return self.create_bwrap_sandbox(principal, request);
        }
        #[cfg(target_os = "macos")]
        if let crate::auth::GatewayBackend::Seatbelt = self.config.backend() {
            return self.create_seatbelt_sandbox(principal, request);
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let _ = (principal, request);
            Err(GatewayStateError::InvalidConfig(anyhow::anyhow!(
                "no sandbox backend is available on this platform"
            )))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (principal, request);
            Err(GatewayStateError::InvalidConfig(anyhow::anyhow!(
                "no sandbox backend is available on this platform"
            )))
        }
    }

    #[cfg(target_os = "linux")]
    fn create_bwrap_sandbox(
        &self,
        principal: &AuthenticatedPrincipal,
        request: RemoteSandboxConfig,
    ) -> Result<String, GatewayStateError> {
        let mut sandboxes = self.lock_sandboxes();
        if sandboxes.len() >= self.config.max_sandboxes {
            return Err(GatewayStateError::Capacity);
        }
        let request = normalize_request_config(principal, request)
            .map_err(GatewayStateError::InvalidConfig)?;
        let sandbox_config =
            build_sandbox_config(&request).map_err(GatewayStateError::InvalidConfig)?;
        let sandbox_id = uuid::Uuid::new_v4().to_string();
        let work_dir = self.work_root.join("sandboxes").join(&sandbox_id);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&work_dir)
            .map_err(|error| GatewayStateError::InvalidConfig(error.into()))?;
        let sandbox =
            match BwrapSandbox::new_mapped(sandbox_config, work_dir, PathBuf::from(GUEST_WORK_DIR))
            {
                Ok(sandbox) => GatewaySandbox::Bwrap(Box::new(sandbox)),
                Err(error) => {
                    let orphan_path = self.work_root.join("sandboxes").join(&sandbox_id);
                    if let Err(cleanup_error) = std::fs::remove_dir_all(&orphan_path) {
                        tracing::warn!(
                            sandbox_id = %sandbox_id,
                            error = %cleanup_error,
                            "failed to remove work root after sandbox construction failure"
                        );
                    }
                    return Err(GatewayStateError::InvalidConfig(error));
                }
            };
        let config_json = serde_json::to_string(&request)
            .map_err(|error| GatewayStateError::Storage(error.into()))?;
        let now_ms = unix_now_ms().map_err(GatewayStateError::Storage)?;
        insert_registry_record(
            &self.registry,
            &sandbox_id,
            &principal.id,
            &config_json,
            None,
            now_ms,
        )
        .map_err(|error| {
            let orphan_path = self.work_root.join("sandboxes").join(&sandbox_id);
            if let Err(cleanup_error) = std::fs::remove_dir_all(&orphan_path) {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    error = %cleanup_error,
                    "failed to remove work root after registry failure"
                );
            }
            GatewayStateError::Storage(error)
        })?;
        let env = sandbox.env();
        let record = SandboxRecord {
            owner: Arc::clone(&principal.id),
            sandbox: Arc::new(sandbox),
            env,
            work_dir: PathBuf::from(GUEST_WORK_DIR),
            config: request,
            state: RemoteSandboxState::Creating,
        };

        sandboxes.insert(sandbox_id.clone(), record);
        Ok(sandbox_id)
    }

    #[cfg(target_os = "macos")]
    fn create_seatbelt_sandbox(
        &self,
        principal: &AuthenticatedPrincipal,
        request: RemoteSandboxConfig,
    ) -> Result<String, GatewayStateError> {
        let mut sandboxes = self.lock_sandboxes();
        if sandboxes.len() >= self.config.max_sandboxes {
            return Err(GatewayStateError::Capacity);
        }
        let request = normalize_request_config(principal, request)
            .map_err(GatewayStateError::InvalidConfig)?;
        let sandbox_config =
            build_sandbox_config(&request).map_err(GatewayStateError::InvalidConfig)?;
        let sandbox_id = uuid::Uuid::new_v4().to_string();
        let work_dir = self.work_root.join("sandboxes").join(&sandbox_id);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&work_dir)
            .map_err(|error| GatewayStateError::InvalidConfig(error.into()))?;
        // Pin the sandbox to the gateway-created work dir so the shell cwd
        // and the guest-coordinate fs mapping (/workspace) resolve to the
        // same host directory.
        let mut sandbox_config = sandbox_config;
        sandbox_config.work_dir = Some(work_dir.clone());
        // Trusted profiles bind the principal's host project directories at
        // guest /workspace/<basename>. Ignored by restricted (floors hold).
        for dir in principal.host_project_dirs.iter() {
            let base = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or_else(|| {
                    GatewayStateError::InvalidConfig(anyhow::anyhow!(
                        "host project dir {dir:?} has no file name"
                    ))
                })?;
            sandbox_config
                .host_binds
                .push(llm_harness_platform::sandbox::HostBind {
                    host: dir.clone(),
                    guest: std::path::PathBuf::from(GUEST_WORK_DIR).join(base),
                });
        }
        let sandbox = match llm_harness_sandbox::SeatbeltSandbox::new(sandbox_config) {
            Ok(sandbox) => GatewaySandbox::Seatbelt(sandbox),
            Err(error) => {
                let orphan_path = self.work_root.join("sandboxes").join(&sandbox_id);
                if let Err(cleanup_error) = std::fs::remove_dir_all(&orphan_path) {
                    tracing::warn!(
                        sandbox_id = %sandbox_id,
                        error = %cleanup_error,
                        "failed to remove work root after sandbox construction failure"
                    );
                }
                return Err(GatewayStateError::InvalidConfig(error));
            }
        };
        let config_json = serde_json::to_string(&request)
            .map_err(|error| GatewayStateError::Storage(error.into()))?;
        let now_ms = unix_now_ms().map_err(GatewayStateError::Storage)?;
        insert_registry_record(
            &self.registry,
            &sandbox_id,
            &principal.id,
            &config_json,
            None,
            now_ms,
        )
        .map_err(|error| {
            let orphan_path = self.work_root.join("sandboxes").join(&sandbox_id);
            if let Err(cleanup_error) = std::fs::remove_dir_all(&orphan_path) {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    error = %cleanup_error,
                    "failed to remove work root after registry failure"
                );
            }
            GatewayStateError::Storage(error)
        })?;
        let env = sandbox.env();
        let record = SandboxRecord {
            owner: Arc::clone(&principal.id),
            sandbox: Arc::new(sandbox),
            env,
            work_dir: PathBuf::from(GUEST_WORK_DIR),
            config: request,
            state: RemoteSandboxState::Creating,
        };

        sandboxes.insert(sandbox_id.clone(), record);
        Ok(sandbox_id)
    }

    #[cfg(target_os = "linux")]
    async fn create_firecracker_sandbox(
        &self,
        principal: &AuthenticatedPrincipal,
        request: RemoteSandboxConfig,
        backend: &crate::auth::FirecrackerBackendConfig,
    ) -> Result<String, GatewayStateError> {
        let mut sandboxes = self.lock_sandboxes();
        if sandboxes.len() >= self.config.max_sandboxes {
            return Err(GatewayStateError::Capacity);
        }
        let request = normalize_request_config(principal, request)
            .map_err(GatewayStateError::InvalidConfig)?;
        validate_firecracker_policies(&request, backend)
            .map_err(GatewayStateError::InvalidConfig)?;
        let sandbox_config =
            build_sandbox_config(&request).map_err(GatewayStateError::InvalidConfig)?;
        let sandbox_id = uuid::Uuid::new_v4().to_string();
        let run_directory = self.work_root.join("sandboxes").join(&sandbox_id);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&run_directory)
            .map_err(|error| GatewayStateError::InvalidConfig(error.into()))?;
        let vsock_guest_cid =
            allocate_vsock_guest_cid(&self.registry, backend.vsock_guest_cid_base).map_err(
                |error| {
                    if let Err(cleanup_error) = std::fs::remove_dir_all(&run_directory) {
                        tracing::warn!(
                            sandbox_id = %sandbox_id,
                            error = %cleanup_error,
                            "failed to remove work root after CID allocation failure"
                        );
                    }
                    GatewayStateError::Storage(error)
                },
            )?;
        let sandbox = FirecrackerSandbox::new(
            sandbox_config,
            FirecrackerSandboxConfig {
                backend: backend.clone(),
                run_directory,
                vsock_guest_cid,
            },
        );
        let config_json = serde_json::to_string(&request)
            .map_err(|error| GatewayStateError::Storage(error.into()))?;
        let now_ms = unix_now_ms().map_err(GatewayStateError::Storage)?;
        insert_registry_record(
            &self.registry,
            &sandbox_id,
            &principal.id,
            &config_json,
            Some(vsock_guest_cid),
            now_ms,
        )
        .map_err(|error| {
            let orphan_path = self.work_root.join("sandboxes").join(&sandbox_id);
            if let Err(cleanup_error) = std::fs::remove_dir_all(&orphan_path) {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    error = %cleanup_error,
                    "failed to remove work root after registry failure"
                );
            }
            GatewayStateError::Storage(error)
        })?;
        let env = sandbox.env();
        let record = SandboxRecord {
            owner: Arc::clone(&principal.id),
            sandbox: Arc::new(GatewaySandbox::Firecracker(Box::new(sandbox))),
            env,
            work_dir: PathBuf::from(GUEST_WORK_DIR),
            config: request,
            state: RemoteSandboxState::Creating,
        };
        sandboxes.insert(sandbox_id.clone(), record);
        Ok(sandbox_id)
    }

    pub(crate) async fn start_sandbox(
        &self,
        principal: &AuthenticatedPrincipal,
        sandbox_id: &str,
    ) -> anyhow::Result<()> {
        let lifecycle_lock = self.lifecycle_lock(sandbox_id);
        let _lifecycle_guard = lifecycle_lock.lock().await;
        let sandbox = self
            .get_sandbox(principal, sandbox_id)
            .ok_or_else(|| anyhow::anyhow!("sandbox not found"))?
            .sandbox;
        sandbox.start().await
    }

    fn lifecycle_lock(&self, sandbox_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.lifecycle_locks
            .lock()
            .expect("lifecycle lock registry poisoned")
            .entry(sandbox_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    pub(crate) fn get_sandbox(
        &self,
        principal: &AuthenticatedPrincipal,
        sandbox_id: &str,
    ) -> Option<SandboxLease> {
        let sandboxes = self.lock_sandboxes();
        let record = sandboxes.get(sandbox_id)?;
        if *record.owner != *principal.id {
            return None;
        }
        Some(SandboxLease {
            sandbox: Arc::clone(&record.sandbox),
            env: Arc::clone(&record.env),
            work_dir: record.work_dir.clone(),
            config: record.config.clone(),
            state: record.state,
        })
    }

    pub(crate) fn mark_running(&self, sandbox_id: &str) -> Result<(), GatewayStateError> {
        let mut sandboxes = self.lock_sandboxes();
        if !sandboxes.contains_key(sandbox_id) {
            return Err(GatewayStateError::NotFound);
        }
        set_registry_state(&self.registry, sandbox_id, RegistryState::Running)
            .map_err(GatewayStateError::Storage)?;
        if let Some(record) = sandboxes.get_mut(sandbox_id) {
            record.state = RemoteSandboxState::Running;
        }
        Ok(())
    }

    pub(crate) async fn reset_sandbox(&self, sandbox_id: &str) -> anyhow::Result<()> {
        let lifecycle_lock = self.lifecycle_lock(sandbox_id);
        let _lifecycle_guard = lifecycle_lock.lock().await;
        let sandbox = self.get_sandbox_unchecked(sandbox_id)?.sandbox;
        sandbox.reset().await
    }

    pub(crate) async fn take_sandbox_for_delete(
        &self,
        sandbox_id: &str,
    ) -> Result<GatewaySandbox, GatewayStateError> {
        let lifecycle_lock = self.lifecycle_lock(sandbox_id);
        let _lifecycle_guard = lifecycle_lock.lock().await;
        let sandbox = self.take_sandbox_for_shutdown(sandbox_id)?;
        self.lifecycle_locks
            .lock()
            .expect("lifecycle lock registry poisoned")
            .remove(sandbox_id);
        Ok(sandbox)
    }

    pub(crate) fn take_sandbox_for_shutdown(
        &self,
        sandbox_id: &str,
    ) -> Result<GatewaySandbox, GatewayStateError> {
        let mut sandboxes = self.lock_sandboxes();
        let record = sandboxes
            .get(sandbox_id)
            .ok_or(GatewayStateError::NotFound)?;
        if Arc::strong_count(&record.sandbox) != 1 {
            return Err(GatewayStateError::Busy);
        }
        set_registry_state(&self.registry, sandbox_id, RegistryState::Deleting)
            .map_err(GatewayStateError::Storage)?;
        let record = sandboxes
            .remove(sandbox_id)
            .expect("sandbox record was present");
        drop(sandboxes);
        self.cancel_sandbox_executions(sandbox_id);
        Arc::try_unwrap(record.sandbox).map_err(|_| GatewayStateError::Busy)
    }

    pub(crate) async fn shutdown_all(&self) -> anyhow::Result<()> {
        let lifecycle_locks = self
            .lifecycle_locks
            .lock()
            .expect("lifecycle lock registry poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut lifecycle_guards = Vec::with_capacity(lifecycle_locks.len());
        for lifecycle_lock in lifecycle_locks {
            lifecycle_guards.push(lifecycle_lock.lock_owned().await);
        }
        let sandboxes = self
            .lock_sandboxes()
            .drain()
            .map(
                |(id, record)| -> anyhow::Result<(String, Arc<GatewaySandbox>)> {
                    set_registry_state(&self.registry, &id, RegistryState::Deleting)?;
                    self.cancel_sandbox_executions(&id);
                    Ok((id, record.sandbox))
                },
            )
            .collect::<Result<Vec<_>, _>>()?;
        for (id, sandbox) in sandboxes {
            let sandbox = Arc::try_unwrap(sandbox)
                .map_err(|_| anyhow::anyhow!("sandbox handle is still shared"))?;
            Box::new(sandbox).shutdown().await?;
            delete_registry_record(&self.registry, &id)?;
        }
        Ok(())
    }

    pub(crate) fn finish_shutdown(&self, sandbox_id: &str) -> Result<(), GatewayStateError> {
        delete_registry_record(&self.registry, sandbox_id).map_err(GatewayStateError::Storage)
    }

    pub(crate) fn insert_execution(
        &self,
        sandbox_id: &str,
        execution_id: String,
        abort: CancellationToken,
    ) {
        self.executions
            .lock()
            .expect("execution registry poisoned")
            .insert(
                execution_id,
                ExecutionRecord {
                    sandbox_id: sandbox_id.to_owned(),
                    abort,
                },
            );
    }

    pub(crate) fn remove_execution(&self, execution_id: &str) {
        self.executions
            .lock()
            .expect("execution registry poisoned")
            .remove(execution_id);
    }

    pub(crate) fn cancel_execution(&self, sandbox_id: &str, execution_id: &str) -> bool {
        self.executions
            .lock()
            .expect("execution registry poisoned")
            .get(execution_id)
            .map(|record| {
                let allowed = record.sandbox_id == sandbox_id;
                if allowed {
                    record.abort.cancel();
                }
                allowed
            })
            .unwrap_or(false)
    }

    pub(crate) fn execution_exists(&self, sandbox_id: &str, execution_id: &str) -> bool {
        self.executions
            .lock()
            .expect("execution registry poisoned")
            .get(execution_id)
            .is_some_and(|record| record.sandbox_id == sandbox_id)
    }

    fn cancel_sandbox_executions(&self, sandbox_id: &str) {
        self.executions
            .lock()
            .expect("execution registry poisoned")
            .retain(|_, record| {
                if record.sandbox_id == sandbox_id {
                    record.abort.cancel();
                    false
                } else {
                    true
                }
            });
    }

    fn get_sandbox_unchecked(&self, sandbox_id: &str) -> anyhow::Result<SandboxLease> {
        let sandboxes = self.lock_sandboxes();
        let record = sandboxes
            .get(sandbox_id)
            .ok_or_else(|| anyhow::anyhow!("sandbox not found"))?;
        Ok(SandboxLease {
            sandbox: Arc::clone(&record.sandbox),
            env: Arc::clone(&record.env),
            work_dir: record.work_dir.clone(),
            config: record.config.clone(),
            state: record.state,
        })
    }

    fn lock_sandboxes(&self) -> std::sync::MutexGuard<'_, HashMap<String, SandboxRecord>> {
        self.sandboxes.lock().expect("sandbox registry poisoned")
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn normalize_request_config(
    principal: &AuthenticatedPrincipal,
    mut request: RemoteSandboxConfig,
) -> anyhow::Result<RemoteSandboxConfig> {
    if let Some(work_dir) = &request.work_dir
        && work_dir != GUEST_WORK_DIR
    {
        anyhow::bail!("work_dir must be omitted or /workspace");
    }
    if request.fs_allowlist.is_empty() {
        request.fs_allowlist = principal
            .fs_allowlist
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
    } else {
        validate_fs_paths(&request.fs_allowlist, "fs_allowlist")?;
        if !principal.fs_allowlist.is_empty() {
            for path in &request.fs_allowlist {
                if !principal.fs_allowlist.iter().any(|allowed| {
                    let allowed = allowed.to_string_lossy();
                    Path::new(path).starts_with(Path::new(allowed.as_ref()))
                }) {
                    anyhow::bail!(
                        "fs_allowlist entry {path:?} is not permitted for this principal"
                    );
                }
            }
        }
    }
    validate_fs_paths(&request.fs_denylist, "fs_denylist")?;
    Ok(request)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn build_sandbox_config(request: &RemoteSandboxConfig) -> anyhow::Result<SandboxConfig> {
    let resource_limits = ResourceLimits {
        max_cpus: request.resource_limits.max_cpus,
        max_processes: request.resource_limits.max_processes,
        max_memory_mb: request.resource_limits.max_memory_mb,
        max_disk_mb: request.resource_limits.max_disk_mb,
        timeout: request
            .resource_limits
            .timeout_ms
            .map(std::time::Duration::from_millis),
    };
    Ok(SandboxConfig {
        fs_allowlist: request.fs_allowlist.iter().map(PathBuf::from).collect(),
        fs_denylist: request.fs_denylist.iter().map(PathBuf::from).collect(),
        net_allowlist: request
            .net_allowlist
            .iter()
            .map(|rule| NetRule {
                host: rule.host.clone(),
                port_range: rule.port_range,
            })
            .collect(),
        resource_limits,
        work_dir: None,
        trust_profile: request.trust_profile,
        host_binds: Vec::new(),
    })
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn validate_fs_paths(paths: &[String], field: &str) -> anyhow::Result<()> {
    for path in paths {
        if path != GUEST_WORK_DIR
            && (!path.starts_with("/workspace/")
                || path.contains('\0')
                || path.contains('\\')
                || path
                    .split('/')
                    .any(|component| component == "." || component == ".."))
        {
            anyhow::bail!("{field} entries must be /workspace or a subpath");
        }
    }
    Ok(())
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn validate_firecracker_policies(
    request: &RemoteSandboxConfig,
    backend: &crate::auth::FirecrackerBackendConfig,
) -> anyhow::Result<()> {
    if !request.fs_allowlist.is_empty() {
        anyhow::bail!("Firecracker backend does not support fs_allowlist entries");
    }
    if !request.fs_denylist.is_empty() {
        anyhow::bail!("Firecracker backend does not support fs_denylist entries");
    }
    if !request.net_allowlist.is_empty() {
        anyhow::bail!("Firecracker backend does not support net_allowlist rules");
    }
    let limits = &request.resource_limits;
    if limits.max_processes.is_some() {
        anyhow::bail!("Firecracker backend does not support max_processes");
    }
    if limits.max_disk_mb.is_some() && backend.max_disk_mib.is_none() {
        anyhow::bail!(
            "Firecracker backend does not configure max_disk_mib, so max_disk_mb is unsupported"
        );
    }
    if let Some(max_disk_mb) = limits.max_disk_mb
        && let Some(max_disk_mib) = backend.max_disk_mib
        && u64::try_from(max_disk_mb).unwrap_or(u64::MAX) > u64::from(max_disk_mib)
    {
        anyhow::bail!(
            "Firecracker max_disk_mb {} exceeds backend maximum {}",
            max_disk_mb,
            max_disk_mib
        );
    }
    if let Some(max_cpus) = limits.max_cpus {
        let max_cpus = u32::try_from(max_cpus)
            .map_err(|_| anyhow::anyhow!("Firecracker max_cpus is too large"))?;
        if max_cpus > backend.max_vcpu_count {
            anyhow::bail!(
                "Firecracker max_cpus {} exceeds backend maximum {}",
                max_cpus,
                backend.max_vcpu_count
            );
        }
    }
    if let Some(max_memory_mb) = limits.max_memory_mb {
        let max_memory_mb = u32::try_from(max_memory_mb)
            .map_err(|_| anyhow::anyhow!("Firecracker max_memory_mb is too large"))?;
        if max_memory_mb > backend.max_memory_mib {
            anyhow::bail!(
                "Firecracker max_memory_mb {} exceeds backend maximum {}",
                max_memory_mb,
                backend.max_memory_mib
            );
        }
    }
    Ok(())
}

#[cfg_attr(not(target_os = "linux"), allow(unused_variables, unused_mut))]
fn restore_sandbox(
    config: &crate::auth::GatewayConfig,
    persisted: PersistedSandbox,
) -> anyhow::Result<SandboxRecord> {
    let principal = config
        .principal(&persisted.owner)
        .ok_or_else(|| anyhow::anyhow!("persisted sandbox owner is not configured"))?;
    let PersistedSandbox {
        ref sandbox_id,
        owner: _,
        config: request,
        vsock_guest_cid,
        state: _,
    } = persisted;
    #[cfg(target_os = "linux")]
    match (&config.backend(), vsock_guest_cid) {
        (crate::auth::GatewayBackend::Firecracker(backend), Some(cid)) => {
            if cid < backend.vsock_guest_cid_base {
                anyhow::bail!(
                    "persisted sandbox {sandbox_id:?} has a vsock CID below the configured base"
                );
            }
            let persisted_config = normalize_request_config(&principal, request)?;
            validate_firecracker_policies(&persisted_config, backend)?;
            let sandbox_config = build_sandbox_config(&persisted_config)?;
            let run_directory = config.work_root().join("sandboxes").join(sandbox_id);
            let sandbox = FirecrackerSandbox::new(
                sandbox_config,
                FirecrackerSandboxConfig {
                    backend: backend.clone(),
                    run_directory,
                    vsock_guest_cid: cid,
                },
            );
            let env = sandbox.env();
            return Ok(SandboxRecord {
                owner: Arc::clone(&principal.id),
                sandbox: Arc::new(GatewaySandbox::Firecracker(Box::new(sandbox))),
                env,
                work_dir: PathBuf::from(GUEST_WORK_DIR),
                config: persisted_config,
                state: RemoteSandboxState::Creating,
            });
        }
        (crate::auth::GatewayBackend::Firecracker(_), None) => {
            anyhow::bail!("persisted sandbox {sandbox_id:?} is missing a vsock CID");
        }
        (crate::auth::GatewayBackend::Bwrap, Some(_)) => {
            anyhow::bail!("persisted sandbox {sandbox_id:?} has a vsock CID on the Bwrap backend");
        }
        _ => {}
    }
    #[cfg(target_os = "linux")]
    {
        let persisted_config = normalize_request_config(&principal, request)?;
        let sandbox_config = build_sandbox_config(&persisted_config)?;
        let work_dir = config
            .work_root()
            .join("sandboxes")
            .join(&persisted.sandbox_id);
        let sandbox = GatewaySandbox::Bwrap(Box::new(BwrapSandbox::new_mapped(
            sandbox_config,
            work_dir,
            PathBuf::from(GUEST_WORK_DIR),
        )?));
        let env = sandbox.env();
        Ok(SandboxRecord {
            owner: Arc::clone(&principal.id),
            sandbox: Arc::new(sandbox),
            env: Arc::clone(&env),
            work_dir: PathBuf::from(GUEST_WORK_DIR),
            config: persisted_config,
            state: RemoteSandboxState::Running,
        })
    }
    #[cfg(target_os = "macos")]
    {
        if vsock_guest_cid.is_some() {
            anyhow::bail!(
                "persisted sandbox {sandbox_id:?} has a vsock CID on the Seatbelt backend"
            );
        }
        let persisted_config = normalize_request_config(&principal, request)?;
        let sandbox_config = build_sandbox_config(&persisted_config)?;
        let work_dir = config
            .work_root()
            .join("sandboxes")
            .join(&persisted.sandbox_id);
        let mut sandbox_config = sandbox_config;
        sandbox_config.work_dir = Some(work_dir);
        let sandbox =
            GatewaySandbox::Seatbelt(llm_harness_sandbox::SeatbeltSandbox::new(sandbox_config)?);
        let env = sandbox.env();
        Ok(SandboxRecord {
            owner: Arc::clone(&principal.id),
            sandbox: Arc::new(sandbox),
            env,
            work_dir: PathBuf::from(GUEST_WORK_DIR),
            config: persisted_config,
            state: RemoteSandboxState::Running,
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (config, vsock_guest_cid);
        anyhow::bail!("no sandbox backend is available on this platform")
    }
}

fn create_private_directory(path: &std::path::Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                anyhow::bail!("{path:?} must be a directory");
            }
            if metadata.permissions().mode() & 0o777 != 0o700 {
                anyhow::bail!("{path:?} must have 0700 permissions");
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn lock_work_root(work_root: &std::path::Path) -> anyhow::Result<File> {
    let lock_path = work_root.join(".gateway.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&lock_path)?;
    if !file.metadata()?.is_file() {
        anyhow::bail!("gateway work_root lock must be a regular file");
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            anyhow::bail!("gateway work_root {work_root:?} is already in use");
        }
        return Err(error.into());
    }
    Ok(file)
}

const REGISTRY_APPLICATION_ID: i64 = 0x4c_48_53_47;
const REGISTRY_SCHEMA_VERSION: i64 = 2;
const REGISTRY_VSOCK_CID_INDEX: &str = "idx_sandboxes_vsock_guest_cid";

fn open_registry(path: &std::path::Path) -> anyhow::Result<Connection> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        anyhow::bail!("gateway sandbox registry must be a regular file");
    }
    if file.metadata()?.permissions().mode() & 0o777 != 0o600 {
        anyhow::bail!("gateway sandbox registry must have 0600 permissions");
    }

    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    let journal_mode = connection.query_row("PRAGMA journal_mode = WAL", [], |row| {
        row.get::<_, String>(0)
    })?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        anyhow::bail!("gateway sandbox registry must use WAL mode");
    }
    connection.pragma_update(None, "synchronous", "FULL")?;

    let application_id =
        connection.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0))?;
    let schema_version =
        connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    if application_id == 0 && schema_version == 0 {
        connection.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             CREATE TABLE sandboxes (
                 sandbox_id TEXT PRIMARY KEY,
                 owner TEXT NOT NULL,
                 config_json TEXT NOT NULL,
                 state TEXT NOT NULL CHECK (state IN ('creating', 'running', 'deleting')),
                 created_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL,
                 vsock_guest_cid INTEGER
             );
             CREATE UNIQUE INDEX {REGISTRY_VSOCK_CID_INDEX}
                 ON sandboxes(vsock_guest_cid);
             PRAGMA application_id = {REGISTRY_APPLICATION_ID};
             PRAGMA user_version = {REGISTRY_SCHEMA_VERSION};
             COMMIT;",
        ))?;
    } else if application_id != REGISTRY_APPLICATION_ID {
        anyhow::bail!("unsupported gateway sandbox registry schema");
    } else if schema_version == 1 {
        connection.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             CREATE TABLE sandboxes_v2 (
                 sandbox_id TEXT PRIMARY KEY,
                 owner TEXT NOT NULL,
                 config_json TEXT NOT NULL,
                 state TEXT NOT NULL CHECK (state IN ('creating', 'running', 'deleting')),
                 created_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL,
                 vsock_guest_cid INTEGER
             );
             INSERT INTO sandboxes_v2
                 (sandbox_id, owner, config_json, state, created_at_ms, updated_at_ms, vsock_guest_cid)
             SELECT sandbox_id, owner, config_json, state, created_at_ms, updated_at_ms, NULL
                 FROM sandboxes;
             DROP TABLE sandboxes;
             ALTER TABLE sandboxes_v2 RENAME TO sandboxes;
             CREATE UNIQUE INDEX idx_sandboxes_vsock_guest_cid
                 ON sandboxes(vsock_guest_cid);
             PRAGMA application_id = {REGISTRY_APPLICATION_ID};
             PRAGMA user_version = {REGISTRY_SCHEMA_VERSION};
             COMMIT;",
        ))?;
    } else if schema_version != REGISTRY_SCHEMA_VERSION {
        anyhow::bail!("unsupported gateway sandbox registry schema");
    }
    validate_registry_schema(&connection)?;
    let check = connection.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))?;
    if check != "ok" {
        anyhow::bail!("gateway sandbox registry integrity check failed: {check}");
    }
    Ok(connection)
}

fn validate_registry_schema(connection: &Connection) -> anyhow::Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(sandboxes)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    let expected = [
        "sandbox_id",
        "owner",
        "config_json",
        "state",
        "created_at_ms",
        "updated_at_ms",
        "vsock_guest_cid",
    ];
    if columns != expected {
        anyhow::bail!("invalid gateway sandbox registry schema");
    }
    let mut statement = connection.prepare("PRAGMA index_list(sandboxes)")?;
    let indexes = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if !indexes
        .iter()
        .any(|(name, unique)| name == REGISTRY_VSOCK_CID_INDEX && *unique == 1)
    {
        anyhow::bail!("invalid gateway sandbox registry schema");
    }
    Ok(())
}

fn unix_now_ms() -> anyhow::Result<i64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| anyhow::anyhow!("system clock is before UNIX epoch: {error}"))?
        .as_millis();
    i64::try_from(millis).map_err(|_| anyhow::anyhow!("system time is too large"))
}

#[cfg(target_os = "linux")]
fn allocate_vsock_guest_cid(
    registry: &std::sync::Mutex<Connection>,
    cid_base: u32,
) -> anyhow::Result<u32> {
    let connection = lock_registry(registry);
    let mut statement = connection.prepare("SELECT vsock_guest_cid FROM sandboxes")?;
    let used = statement
        .query_map([], |row| row.get::<_, Option<u32>>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let used_values = used.into_iter().flatten().collect::<Vec<u32>>();
    if used_values.iter().any(|cid| *cid < cid_base) {
        anyhow::bail!("gateway registry contains a vsock CID below the configured base");
    }
    let mut used = used_values
        .into_iter()
        .collect::<std::collections::BTreeSet<u32>>();
    used.retain(|cid| *cid >= cid_base);
    for candidate in cid_base..=u32::MAX {
        if !used.contains(&candidate) {
            return Ok(candidate);
        }
    }
    anyhow::bail!("no vsock CID is available")
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn insert_registry_record(
    registry: &std::sync::Mutex<Connection>,
    sandbox_id: &str,
    owner: &str,
    config_json: &str,
    vsock_guest_cid: Option<u32>,
    now_ms: i64,
) -> anyhow::Result<()> {
    let connection = lock_registry(registry);
    connection.execute(
        "INSERT INTO sandboxes
             (sandbox_id, owner, config_json, state, vsock_guest_cid, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, 'creating', ?4, ?5, ?5)",
        rusqlite::params![sandbox_id, owner, config_json, vsock_guest_cid, now_ms],
    )?;
    Ok(())
}

fn set_registry_state(
    registry: &std::sync::Mutex<Connection>,
    sandbox_id: &str,
    state: RegistryState,
) -> anyhow::Result<()> {
    let connection = lock_registry(registry);
    let now_ms = unix_now_ms()?;
    let changed = connection.execute(
        "UPDATE sandboxes SET state = ?1, updated_at_ms = ?2 WHERE sandbox_id = ?3",
        rusqlite::params![state.as_str(), now_ms, sandbox_id],
    )?;
    if changed != 1 {
        anyhow::bail!("sandbox registry record {sandbox_id:?} is missing");
    }
    Ok(())
}

fn delete_registry_record(
    registry: &std::sync::Mutex<Connection>,
    sandbox_id: &str,
) -> anyhow::Result<()> {
    let connection = lock_registry(registry);
    let changed = connection.execute(
        "DELETE FROM sandboxes WHERE sandbox_id = ?1",
        rusqlite::params![sandbox_id],
    )?;
    if changed != 1 {
        anyhow::bail!("sandbox registry record {sandbox_id:?} is missing");
    }
    Ok(())
}

fn lock_registry(registry: &std::sync::Mutex<Connection>) -> std::sync::MutexGuard<'_, Connection> {
    registry.lock().expect("sandbox registry database poisoned")
}

fn reconcile_registry(
    registry: &std::sync::Mutex<Connection>,
    sandbox_root: &std::path::Path,
    config: &crate::auth::GatewayConfig,
) -> anyhow::Result<Vec<PersistedSandbox>> {
    let mut work_roots = scan_work_roots(sandbox_root)?;
    let records = load_registry_records(registry)?;
    let mut retained = Vec::with_capacity(records.len());
    for record in records {
        validate_sandbox_id(&record.sandbox_id)?;
        if record.state == RegistryState::Deleting {
            if work_roots.remove(&record.sandbox_id) {
                remove_work_root(&sandbox_root.join(&record.sandbox_id))?;
            }
            delete_registry_record(registry, &record.sandbox_id)?;
        } else {
            if !work_roots.remove(&record.sandbox_id) {
                anyhow::bail!(
                    "persisted sandbox {:?} is missing its work root",
                    record.sandbox_id
                );
            }
            if config.principal(&record.owner).is_none() {
                anyhow::bail!(
                    "persisted sandbox {:?} has an unconfigured owner",
                    record.sandbox_id
                );
            }
            retained.push(record);
        }
    }
    let orphan_count = work_roots.len();
    for sandbox_id in work_roots {
        remove_work_root(&sandbox_root.join(sandbox_id))?;
    }
    if orphan_count > 0 {
        tracing::info!(
            orphan_count,
            sandbox_root = %sandbox_root.display(),
            "removed orphan sandbox work roots"
        );
    }
    Ok(retained)
}

fn scan_work_roots(sandbox_root: &std::path::Path) -> anyhow::Result<HashSet<String>> {
    let mut work_roots = HashSet::new();
    for entry in std::fs::read_dir(sandbox_root)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid UTF-8 entry in gateway sandbox root"))?;
        if !entry.file_type()?.is_dir() || uuid::Uuid::parse_str(file_name).is_err() {
            anyhow::bail!("unexpected entry {file_name:?} in gateway sandbox root");
        }
        work_roots.insert(file_name.to_owned());
    }
    Ok(work_roots)
}

fn load_registry_records(
    registry: &std::sync::Mutex<Connection>,
) -> anyhow::Result<Vec<PersistedSandbox>> {
    let connection = lock_registry(registry);
    let mut statement = connection.prepare(
        "SELECT sandbox_id, owner, config_json, vsock_guest_cid, state
         FROM sandboxes
         ORDER BY sandbox_id",
    )?;
    let records = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<u32>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    records
        .into_iter()
        .map(
            |(
                sandbox_id,
                owner,
                config_json,
                vsock_guest_cid,
                state,
            )| -> anyhow::Result<PersistedSandbox> {
                let config = serde_json::from_str(&config_json).map_err(|error| {
                    anyhow::anyhow!("invalid persisted sandbox config: {error}")
                })?;
                Ok(PersistedSandbox {
                    sandbox_id,
                    owner,
                    config,
                    vsock_guest_cid,
                    state: RegistryState::parse(&state)?,
                })
            },
        )
        .collect()
}

fn validate_sandbox_id(sandbox_id: &str) -> anyhow::Result<()> {
    if uuid::Uuid::parse_str(sandbox_id).is_err() {
        anyhow::bail!("invalid sandbox id {sandbox_id:?} in gateway registry");
    }
    Ok(())
}

fn remove_work_root(path: &std::path::Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => std::fs::remove_dir_all(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub(crate) struct SandboxLease {
    pub(crate) sandbox: Arc<GatewaySandbox>,
    pub(crate) env: Arc<dyn ExecutionEnv>,
    pub(crate) work_dir: PathBuf,
    pub(crate) config: RemoteSandboxConfig,
    pub(crate) state: RemoteSandboxState,
}
