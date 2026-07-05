use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_runtime::error::Result;
use llm_harness_runtime::platform::sandbox::{Sandbox, SandboxConfig};
use llm_harness_types::ExecutionEnv;

use crate::env::OsEnv;

/// Pure `OsEnv` wrapper without security isolation.
///
/// Suitable for development and testing. For production isolation,
/// use `sandbox-bwrap` (Linux) or `sandbox-seatbelt` (macOS).
pub struct OsEnvSandbox {
    env: Arc<OsEnv>,
    config: SandboxConfig,
    running: AtomicBool,
}

impl OsEnvSandbox {
    /// Create a new `OsEnvSandbox`.
    ///
    /// Uses `config.work_dir` if set; otherwise falls back to the current
    /// working directory.
    pub fn new(config: SandboxConfig) -> Self {
        let work_dir = config
            .work_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/tmp".into()));
        Self {
            env: Arc::new(OsEnv::new(work_dir)),
            config,
            running: AtomicBool::new(false),
        }
    }
}

impl Sandbox for OsEnvSandbox {
    fn start(&self) -> BoxFuture<'_, Result<()>> {
        async move {
            self.running.store(true, Ordering::SeqCst);
            Ok(())
        }
        .boxed()
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        self.env.clone()
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
    async fn os_env_sandbox_starts() {
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
        let sandbox = OsEnvSandbox::new(config);
        sandbox.start().await.unwrap();
        assert!(sandbox.is_running());
        assert!(sandbox.env().working_dir().exists());
        Box::new(sandbox).shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn sandbox_env_can_read_write() {
        use std::path::Path;
        use tokio_util::sync::CancellationToken;

        let tmp = tempfile::tempdir().unwrap();
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
            work_dir: Some(tmp.path().to_path_buf()),
        };
        let sandbox = OsEnvSandbox::new(config);
        sandbox.start().await.unwrap();

        let env = sandbox.env();
        let abort = CancellationToken::new();
        env.write_file(Path::new("hello.txt"), b"hello world", abort.clone())
            .await
            .unwrap();
        let content = env
            .read_text_file(Path::new("hello.txt"), abort)
            .await
            .unwrap();
        assert_eq!(content, "hello world");

        Box::new(sandbox).shutdown().await.unwrap();
    }
}
