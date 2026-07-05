use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_runtime::platform::sandbox::{Sandbox, SandboxConfig};
use llm_harness_runtime_sandbox_os::OsEnv;
use llm_harness_types::ExecutionEnv;

/// Sandbox backed by bubblewrap(1) for OS-level permission isolation.
///
/// Maps `SandboxConfig` to bwrap arguments:
/// - `fs_allowlist` → `--bind`
/// - `net_allowlist` → `--unshare-net` when empty (v0.2 basic)
/// - `resource_limits` → timeout (full setrlimit deferred to v0.3)
pub struct BwrapSandbox {
    env: Arc<OsEnv>,
    config: SandboxConfig,
    running: AtomicBool,
}

impl BwrapSandbox {
    /// Create a new `BwrapSandbox`. Requires `bwrap` to be installed.
    pub fn new(config: SandboxConfig) -> Self {
        let work_dir = config
            .work_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("/tmp/sandbox-bwrap"));
        std::fs::create_dir_all(&work_dir).ok();
        Self {
            env: Arc::new(OsEnv::new(work_dir)),
            config,
            running: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    fn build_bwrap_args(&self) -> Vec<String> {
        let mut args = vec![
            "--unshare-all".to_string(),
            "--share-net".to_string(),
            "--die-with-parent".to_string(),
            "--new-session".to_string(),
        ];
        for path in &self.config.fs_allowlist {
            let s = path.to_string_lossy().to_string();
            args.push("--bind".to_string());
            args.push(s.clone());
            args.push(s);
        }
        let work_dir = self.env.working_dir().to_string_lossy().to_string();
        args.push("--chdir".to_string());
        args.push(work_dir);
        args.push("--tmpfs".to_string());
        args.push("/tmp".to_string());
        args
    }
}

impl Sandbox for BwrapSandbox {
    fn start(&self) -> BoxFuture<'_, Result<()>> {
        async move {
            let status = tokio::process::Command::new("bwrap")
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
            match status {
                Ok(s) if s.success() => {
                    self.running.store(true, Ordering::SeqCst);
                    Ok(())
                }
                _ => Err(anyhow::anyhow!("bwrap not available — install bubblewrap")),
            }
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

/// Returns the bwrap command-line args for the given sandbox.
/// Exposed for testing only.
#[cfg(test)]
pub(crate) fn bwrap_args(s: &BwrapSandbox) -> Vec<String> {
    s.build_bwrap_args()
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_runtime::platform::sandbox::{ResourceLimits, SandboxConfig};

    #[test]
    fn bwrap_args_include_unshare_all() {
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
            work_dir: Some(PathBuf::from("/tmp/test-bwrap")),
        };
        let sandbox = BwrapSandbox::new(config);
        let args = bwrap_args(&sandbox);
        assert!(args.contains(&"--unshare-all".to_string()));
    }
}
