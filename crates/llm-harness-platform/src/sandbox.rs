use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use llm_harness_types::ExecutionEnv;

use anyhow::Result;

/// Network access rule for sandbox configuration.
#[derive(Debug, Clone)]
pub struct NetRule {
    /// Target host.
    pub host: String,
    /// Optional port range (inclusive). `None` means all ports.
    pub port_range: Option<(u16, u16)>,
}

/// Resource limits for sandbox execution.
#[derive(Debug, Clone, Default)]
pub struct ResourceLimits {
    /// Maximum CPU count. `None` means no limit.
    pub max_cpus: Option<usize>,
    /// Maximum number of concurrent processes in the sandbox. `None` means
    /// no limit.
    pub max_processes: Option<usize>,
    /// Maximum memory in MB. `None` means no limit.
    pub max_memory_mb: Option<usize>,
    /// Maximum disk usage in MB. `None` means no limit.
    pub max_disk_mb: Option<usize>,
    /// Execution wall-clock timeout. `None` means no limit.
    pub timeout: Option<Duration>,
}

/// A host directory bound into the sandbox at a guest path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostBind {
    /// Absolute host-side directory.
    pub host: PathBuf,
    /// Absolute guest-side path it appears at (e.g. `/workspace/myproj`).
    pub guest: PathBuf,
}

/// Sandbox permission boundary declaration (v0.2 minimal set).
#[derive(Debug, Clone, Default)]
pub struct SandboxConfig {
    /// File system path allowlist (read/write).
    pub fs_allowlist: Vec<PathBuf>,
    /// File system path denylist (exceptions to the above allowlist).
    pub fs_denylist: Vec<PathBuf>,
    /// Network access rules.
    pub net_allowlist: Vec<NetRule>,
    /// Resource limits.
    pub resource_limits: ResourceLimits,
    /// Sandbox working directory.
    ///
    /// After `start()` succeeds, `env().working_dir()` MUST return this path
    /// (if set), otherwise the sandbox backend's default working directory.
    pub work_dir: Option<PathBuf>,
    /// Trust level granted to this sandbox. Backends parameterize their
    /// isolation policy by it; `restricted` is the conservative default.
    pub trust_profile: crate::trust::TrustProfile,
    /// Host directories bound into the sandbox (trusted profiles only;
    /// ignored on `restricted`).
    pub host_binds: Vec<HostBind>,
}

/// Sandbox abstraction — wraps `ExecutionEnv`, adding lifecycle management.
///
/// ## `work_dir` semantic contract
/// After `start()` succeeds, `env().working_dir()` returns:
/// - If `SandboxConfig.work_dir` is set → MUST return that path.
/// - If not set → returns the backend's default working directory.
///
/// Backends MUST NOT silently modify `working_dir()` after `start()`.
pub trait Sandbox: Send + Sync {
    /// Start or verify the sandbox is ready.
    fn start(&self) -> BoxFuture<'_, Result<()>>;

    /// Return the execution environment inside the sandbox.
    fn env(&self) -> Arc<dyn ExecutionEnv>;

    /// Reset to initial state.
    fn reset(&self) -> BoxFuture<'_, Result<()>>;

    /// Shut down the sandbox, releasing resources.
    fn shutdown(self: Box<Self>) -> BoxFuture<'static, Result<()>>;

    /// Return the current sandbox configuration.
    fn config(&self) -> &SandboxConfig;

    /// Whether the sandbox is currently running.
    fn is_running(&self) -> bool;
}
