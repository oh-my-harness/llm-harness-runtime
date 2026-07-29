use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_runtime::platform::sandbox::{Sandbox, SandboxConfig};
use llm_harness_types::{EnvError, ExecutionEnv, UnsupportedEnv};

/// Sandbox backed by macOS `sandbox(7)` / seatbelt for OS-level isolation.
///
/// ## Current status: fail-closed (not yet implemented)
///
/// Full `sandbox-exec` policy compilation (`SandboxConfig` → `.sb` file →
/// `sandbox-exec` mediation) is not yet implemented. Until it is, this
/// backend **fails closed**: `start()` returns an error and `env()` returns
/// an `UnsupportedEnv` that rejects all operations.
///
/// This prevents callers from accidentally using an unrestricted `OsEnv`
/// when they believe they have isolation. See issue #97.
pub struct SeatbeltSandbox {
    config: SandboxConfig,
    running: AtomicBool,
}

impl SeatbeltSandbox {
    /// Create a new `SeatbeltSandbox`.
    pub fn new(config: SandboxConfig) -> Self {
        Self {
            config,
            running: AtomicBool::new(false),
        }
    }
}

impl Sandbox for SeatbeltSandbox {
    fn start(&self) -> BoxFuture<'_, Result<()>> {
        async move {
            Err(anyhow::anyhow!(
                "SeatbeltSandbox is not yet implemented; \
                 sandbox-exec policy compilation is required for isolation. \
                 Use OsEnvSandbox for development/testing or implement \
                 sandbox-exec mediation before production use."
            ))
        }
        .boxed()
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        // Fail-closed: return UnsupportedEnv instead of an unrestricted OsEnv.
        // This ensures no caller can accidentally perform host operations
        // believing they are sandboxed.
        let work_dir = self
            .config
            .work_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));
        Arc::new(UnsupportedEnv::with_working_dir(work_dir))
    }

    fn reset(&self) -> BoxFuture<'_, Result<()>> {
        async move { Ok(()) }.boxed()
    }

    fn shutdown(self: Box<Self>) -> BoxFuture<'static, Result<()>> {
        async move { Ok(()) }.boxed()
    }

    fn config(&self) -> &SandboxConfig {
        &self.config
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_runtime::platform::sandbox::ResourceLimits;

    #[tokio::test]
    async fn seatbelt_start_fails_closed() {
        let config = SandboxConfig {
            fs_allowlist: vec![],
            fs_denylist: vec![],
            net_allowlist: vec![],
            resource_limits: ResourceLimits {
                max_cpus: None,
                max_memory_mb: None,
                max_disk_mb: None,
                timeout: None,
            },
            work_dir: None,
        };
        let sandbox = SeatbeltSandbox::new(config);
        let result = sandbox.start().await;
        assert!(
            result.is_err(),
            "start() must fail-closed until sandbox-exec is implemented"
        );
    }

    #[tokio::test]
    async fn seatbelt_env_returns_unsupported() {
        let config = SandboxConfig {
            fs_allowlist: vec![],
            fs_denylist: vec![],
            net_allowlist: vec![],
            resource_limits: ResourceLimits {
                max_cpus: None,
                max_memory_mb: None,
                max_disk_mb: None,
                timeout: None,
            },
            work_dir: None,
        };
        let sandbox = SeatbeltSandbox::new(config);
        let env = sandbox.env();
        let result = env
            .read_text_file(
                std::path::Path::new("/etc/passwd"),
                tokio_util::sync::CancellationToken::new(),
            )
            .await;
        assert!(result.is_err(), "env() must not allow any operations");
    }
}
