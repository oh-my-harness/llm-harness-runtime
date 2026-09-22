use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use llm_harness_platform::sandbox::{NetRule, ResourceLimits, Sandbox, SandboxConfig};
use llm_harness_sandbox::BwrapSandbox;
use llm_harness_sandbox_remote::RemoteSandboxConfig;
use llm_harness_sandbox_remote::protocol::RemoteSandboxState;
use llm_harness_types::ExecutionEnv;
use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use crate::auth::AuthenticatedPrincipal;

pub(crate) const GUEST_WORK_DIR: &str = "/workspace";

pub(crate) type SharedState = Arc<GatewayState>;

pub(crate) struct GatewayState {
    config: crate::auth::GatewayConfig,
    work_root: PathBuf,
    _work_root_lock: File,
    registry: std::sync::Mutex<Connection>,
    sandboxes: std::sync::Mutex<HashMap<String, SandboxRecord>>,
    executions: std::sync::Mutex<HashMap<String, ExecutionRecord>>,
}

#[derive(Debug)]
pub(crate) enum GatewayStateError {
    InvalidConfig(anyhow::Error),
    Storage(anyhow::Error),
    Capacity,
    Busy,
    NotFound,
}

impl GatewayStateError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::InvalidConfig(error) => error.to_string(),
            Self::Storage(_) => "sandbox registry operation failed".to_owned(),
            Self::Capacity => "gateway sandbox limit reached".to_owned(),
            Self::Busy => "sandbox is busy; retry after active requests complete".to_owned(),
            Self::NotFound => "sandbox not found".to_owned(),
        }
    }

    pub(crate) fn detail(&self) -> String {
        match self {
            Self::Storage(error) => error.to_string(),
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

pub(crate) enum GatewaySandbox {
    Bwrap(BwrapSandbox),
}

impl Sandbox for GatewaySandbox {
    fn start(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        match self {
            Self::Bwrap(sandbox) => sandbox.start(),
        }
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        match self {
            Self::Bwrap(sandbox) => sandbox.env(),
        }
    }

    fn reset(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        match self {
            Self::Bwrap(sandbox) => sandbox.reset(),
        }
    }

    fn shutdown(self: Box<Self>) -> BoxFuture<'static, anyhow::Result<()>> {
        match *self {
            Self::Bwrap(sandbox) => Box::new(sandbox).shutdown(),
        }
    }

    fn config(&self) -> &SandboxConfig {
        match self {
            Self::Bwrap(sandbox) => sandbox.config(),
        }
    }

    fn is_running(&self) -> bool {
        match self {
            Self::Bwrap(sandbox) => sandbox.is_running(),
        }
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
    state: RegistryState,
}

impl GatewayState {
    pub(crate) fn new(mut config: crate::auth::GatewayConfig) -> anyhow::Result<Self> {
        config.validate()?;
        let work_root = config.prepare_work_root()?;
        let sandbox_root = work_root.join("sandboxes");
        create_private_directory(&sandbox_root)?;
        let work_root_lock = lock_work_root(&work_root)?;
        let registry = std::sync::Mutex::new(open_registry(&work_root.join("registry.sqlite"))?);
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
            set_registry_state(&registry, &sandbox_id, RegistryState::Running)?;
            sandboxes.insert(sandbox_id, record);
        }
        Ok(Self {
            config,
            work_root,
            _work_root_lock: work_root_lock,
            registry,
            sandboxes: std::sync::Mutex::new(sandboxes),
            executions: std::sync::Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn config(&self) -> &crate::auth::GatewayConfig {
        &self.config
    }

    pub(crate) fn create_sandbox(
        &self,
        principal: &AuthenticatedPrincipal,
        request: RemoteSandboxConfig,
    ) -> Result<String, GatewayStateError> {
        if matches!(
            self.config.backend(),
            crate::auth::GatewayBackend::Firecracker(_)
        ) {
            return Err(GatewayStateError::InvalidConfig(anyhow::anyhow!(
                "Firecracker backend execution is not integrated yet"
            )));
        }
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
                Ok(sandbox) => GatewaySandbox::Bwrap(sandbox),
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

    pub(crate) async fn start_sandbox(
        &self,
        principal: &AuthenticatedPrincipal,
        sandbox_id: &str,
    ) -> anyhow::Result<()> {
        let sandbox = self
            .get_sandbox(principal, sandbox_id)
            .ok_or_else(|| anyhow::anyhow!("sandbox not found"))?
            .sandbox;
        sandbox.start().await
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
        let sandbox = self.get_sandbox_unchecked(sandbox_id)?.sandbox;
        sandbox.reset().await
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
    })
}

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

fn restore_sandbox(
    config: &crate::auth::GatewayConfig,
    persisted: PersistedSandbox,
) -> anyhow::Result<SandboxRecord> {
    let principal = config
        .principal(&persisted.owner)
        .ok_or_else(|| anyhow::anyhow!("persisted sandbox owner is not configured"))?;
    let persisted_config = normalize_request_config(&principal, persisted.config)?;
    let sandbox_config = build_sandbox_config(&persisted_config)?;
    let work_dir = config
        .work_root()
        .join("sandboxes")
        .join(&persisted.sandbox_id);
    let sandbox = GatewaySandbox::Bwrap(BwrapSandbox::new_mapped(
        sandbox_config,
        work_dir,
        PathBuf::from(GUEST_WORK_DIR),
    )?);
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
const REGISTRY_SCHEMA_VERSION: i64 = 1;

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
                 updated_at_ms INTEGER NOT NULL
             );
             PRAGMA application_id = {REGISTRY_APPLICATION_ID};
             PRAGMA user_version = {REGISTRY_SCHEMA_VERSION};
             COMMIT;",
        ))?;
    } else if application_id != REGISTRY_APPLICATION_ID || schema_version != REGISTRY_SCHEMA_VERSION
    {
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
    ];
    if columns != expected {
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

fn insert_registry_record(
    registry: &std::sync::Mutex<Connection>,
    sandbox_id: &str,
    owner: &str,
    config_json: &str,
    now_ms: i64,
) -> anyhow::Result<()> {
    let connection = lock_registry(registry);
    connection.execute(
        "INSERT INTO sandboxes
             (sandbox_id, owner, config_json, state, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, 'creating', ?4, ?4)",
        rusqlite::params![sandbox_id, owner, config_json, now_ms],
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
        "SELECT sandbox_id, owner, config_json, state
         FROM sandboxes
         ORDER BY sandbox_id",
    )?;
    let records = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    records
        .into_iter()
        .map(
            |(sandbox_id, owner, config_json, state)| -> anyhow::Result<PersistedSandbox> {
                let config = serde_json::from_str(&config_json).map_err(|error| {
                    anyhow::anyhow!("invalid persisted sandbox config: {error}")
                })?;
                Ok(PersistedSandbox {
                    sandbox_id,
                    owner,
                    config,
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
