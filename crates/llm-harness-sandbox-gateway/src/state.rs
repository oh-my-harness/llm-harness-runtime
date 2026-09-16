use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use llm_harness_platform::sandbox::{NetRule, ResourceLimits, Sandbox, SandboxConfig};
use llm_harness_sandbox::BwrapSandbox;
use llm_harness_sandbox_remote::RemoteSandboxConfig;
use llm_harness_sandbox_remote::protocol::RemoteSandboxState;
use llm_harness_types::ExecutionEnv;
use tokio_util::sync::CancellationToken;

use crate::auth::AuthenticatedPrincipal;

pub(crate) type SharedState = Arc<GatewayState>;

pub(crate) struct GatewayState {
    config: crate::auth::GatewayConfig,
    sandboxes: std::sync::Mutex<HashMap<String, SandboxRecord>>,
    executions: std::sync::Mutex<HashMap<String, ExecutionRecord>>,
}

#[derive(Debug)]
pub(crate) enum GatewayStateError {
    InvalidConfig(anyhow::Error),
    Capacity,
    Busy,
    NotFound,
}

impl GatewayStateError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::InvalidConfig(error) => error.to_string(),
            Self::Capacity => "gateway sandbox limit reached".to_owned(),
            Self::Busy => "sandbox is busy; retry after active requests complete".to_owned(),
            Self::NotFound => "sandbox not found".to_owned(),
        }
    }
}

pub(crate) struct SandboxRecord {
    pub(crate) owner: Arc<str>,
    pub(crate) sandbox: Arc<BwrapSandbox>,
    pub(crate) env: Arc<dyn ExecutionEnv>,
    pub(crate) work_dir: PathBuf,
    pub(crate) config: RemoteSandboxConfig,
    pub(crate) state: RemoteSandboxState,
}

pub(crate) struct ExecutionRecord {
    pub(crate) sandbox_id: String,
    pub(crate) abort: CancellationToken,
}

impl GatewayState {
    pub(crate) fn new(config: crate::auth::GatewayConfig) -> anyhow::Result<Self> {
        Ok(Self {
            config,
            sandboxes: std::sync::Mutex::new(HashMap::new()),
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
        let mut sandboxes = self.lock_sandboxes();
        if sandboxes.len() >= self.config.max_sandboxes {
            return Err(GatewayStateError::Capacity);
        }
        if request.work_dir.is_some() {
            return Err(GatewayStateError::InvalidConfig(anyhow::anyhow!(
                "work_dir must be omitted; this gateway creates an isolated per-sandbox root"
            )));
        }
        let fs_allowlist = self
            .validate_fs_allowlist(principal, &request)
            .map_err(GatewayStateError::InvalidConfig)?;
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
        let sandbox_config = SandboxConfig {
            fs_allowlist,
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
        };

        let sandbox =
            BwrapSandbox::new(sandbox_config).map_err(GatewayStateError::InvalidConfig)?;
        let sandbox_id = uuid::Uuid::new_v4().to_string();
        let env = sandbox.env();
        let work_dir = env.working_dir().to_path_buf();
        let record = SandboxRecord {
            owner: Arc::clone(&principal.id),
            sandbox: Arc::new(sandbox),
            env,
            work_dir,
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

    pub(crate) fn mark_running(&self, sandbox_id: &str) {
        if let Some(record) = self.lock_sandboxes().get_mut(sandbox_id) {
            record.state = RemoteSandboxState::Running;
        }
    }

    pub(crate) async fn reset_sandbox(&self, sandbox_id: &str) -> anyhow::Result<()> {
        let sandbox = self.get_sandbox_unchecked(sandbox_id)?.sandbox;
        sandbox.reset().await
    }

    pub(crate) fn take_sandbox_for_shutdown(
        &self,
        sandbox_id: &str,
    ) -> Result<BwrapSandbox, GatewayStateError> {
        let mut sandboxes = self.lock_sandboxes();
        let record = sandboxes
            .get(sandbox_id)
            .ok_or(GatewayStateError::NotFound)?;
        if Arc::strong_count(&record.sandbox) != 1 {
            return Err(GatewayStateError::Busy);
        }
        let record = sandboxes
            .remove(sandbox_id)
            .expect("sandbox record was present");
        drop(sandboxes);
        self.cancel_sandbox_executions(sandbox_id);
        Ok(Arc::try_unwrap(record.sandbox).expect("sandbox handle was uniquely owned"))
    }

    pub(crate) async fn shutdown_all(&self) -> anyhow::Result<()> {
        let sandboxes = self
            .lock_sandboxes()
            .drain()
            .map(|(id, record)| {
                self.cancel_sandbox_executions(&id);
                (id, record.sandbox)
            })
            .collect::<Vec<_>>();
        for (_, sandbox) in sandboxes {
            let sandbox = Arc::try_unwrap(sandbox)
                .map_err(|_| anyhow::anyhow!("sandbox handle is still shared"))?;
            Box::new(sandbox).shutdown().await?;
        }
        Ok(())
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

    fn validate_fs_allowlist(
        &self,
        principal: &AuthenticatedPrincipal,
        request: &RemoteSandboxConfig,
    ) -> anyhow::Result<Vec<PathBuf>> {
        let mut paths = Vec::with_capacity(request.fs_allowlist.len());
        for path in &request.fs_allowlist {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                anyhow::bail!("fs_allowlist entries must be absolute canonical paths");
            }
            let canonical = std::fs::canonicalize(&path)
                .map_err(|error| anyhow::anyhow!("canonicalize {path:?}: {error}"))?;
            if !principal
                .fs_allowlist
                .iter()
                .any(|allowed| canonical == *allowed || canonical.starts_with(allowed))
            {
                anyhow::bail!(
                    "fs_allowlist entry {canonical:?} is not permitted for this principal"
                );
            }
            paths.push(canonical);
        }
        Ok(paths)
    }

    fn lock_sandboxes(&self) -> std::sync::MutexGuard<'_, HashMap<String, SandboxRecord>> {
        self.sandboxes.lock().expect("sandbox registry poisoned")
    }
}

pub(crate) struct SandboxLease {
    pub(crate) sandbox: Arc<BwrapSandbox>,
    pub(crate) env: Arc<dyn ExecutionEnv>,
    pub(crate) work_dir: PathBuf,
    pub(crate) config: RemoteSandboxConfig,
    pub(crate) state: RemoteSandboxState,
}
