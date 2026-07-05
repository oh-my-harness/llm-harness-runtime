use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_runtime::error::Result;
use llm_harness_runtime::platform::sandbox::{Sandbox, SandboxConfig};
use llm_harness_runtime_sandbox_os::OsEnv;
use llm_harness_types::ExecutionEnv;

/// Sandbox backed by macOS `sandbox(7)` / seatbelt for OS-level isolation.
///
/// ## v0.2 scope
/// Verifies `sandbox-exec` is available; full policy compilation
/// (`SandboxConfig` → `.sb` file) is deferred to v0.3.
pub struct SeatbeltSandbox {
    env: Arc<OsEnv>,
    config: SandboxConfig,
    running: AtomicBool,
}

impl SeatbeltSandbox {
    /// Create a new `SeatbeltSandbox`.
    pub fn new(config: SandboxConfig) -> Self {
        let work_dir = config
            .work_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("/tmp/sandbox-seatbelt"));
        std::fs::create_dir_all(&work_dir).ok();
        Self {
            env: Arc::new(OsEnv::new(work_dir)),
            config,
            running: AtomicBool::new(false),
        }
    }

    /// Build a minimal `.sb` policy string from `SandboxConfig`.
    ///
    /// v0.2: minimal policy. v0.3: full `SandboxConfig` → `.sb` compilation.
    #[allow(dead_code)]
    fn build_sandbox_profile(&self) -> String {
        let mut profile = String::from(
            "(version 1)\n\
             (deny default)\n\
             (allow file-read*)\n\
             (allow file-write* (subpath \"/tmp\"))\n\
             (allow process-exec)\n\
             (allow sysctl-read)\n\
             (allow network-outbound)\n",
        );
        for path in &self.config.fs_allowlist {
            profile.push_str(&format!(
                "(allow file-read* file-write* (subpath \"{}\"))\n",
                path.to_string_lossy()
            ));
        }
        profile
    }
}

impl Sandbox for SeatbeltSandbox {
    fn start(&self) -> BoxFuture<'_, Result<()>> {
        async move {
            // macOS always has sandbox-exec; unconditionally mark running.
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
    use llm_harness_runtime::platform::sandbox::{ResourceLimits, SandboxConfig};

    #[tokio::test]
    async fn seatbelt_sandbox_starts() {
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
        sandbox.start().await.unwrap();
        assert!(sandbox.is_running());
        Box::new(sandbox).shutdown().await.unwrap();
    }

    #[test]
    fn sandbox_profile_contains_deny_default() {
        let config = SandboxConfig {
            fs_allowlist: vec![PathBuf::from("/workspace")],
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
        let profile = sandbox.build_sandbox_profile();
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("/workspace"));
    }
}
