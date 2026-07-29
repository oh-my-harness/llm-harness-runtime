use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_runtime::platform::sandbox::{Sandbox, SandboxConfig};
use llm_harness_runtime_sandbox_os::{OsEnv, utf8_safe_boundary};
use llm_harness_types::{
    EnvError, ExecutionEnv, FileInfo, ShellOptions, ShellOutput,
};
use tokio_util::sync::CancellationToken;

// ── BwrapEnv ───────────────────────────────────────────────────────────────

/// `ExecutionEnv` that delegates file operations to an inner `OsEnv` but
/// runs shell commands inside a bubblewrap namespace, enforcing filesystem,
/// network, and environment isolation.
///
/// File operations (read/write/list/etc.) run on the host `OsEnv` restricted
/// to the working directory. Shell commands run inside `bwrap --unshare-all`,
/// which creates a fresh mount/net/pid namespace per invocation. The working
/// directory and `fs_allowlist` entries are bind-mounted into the namespace so
/// that shell commands and file operations see the same workspace.
pub struct BwrapEnv {
    inner: OsEnv,
    config: SandboxConfig,
}

impl BwrapEnv {
    /// Create a new `BwrapEnv` from a sandbox configuration.
    pub fn new(config: SandboxConfig) -> Self {
        let work_dir = config
            .work_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("/tmp/sandbox-bwrap"));
        std::fs::create_dir_all(&work_dir).ok();
        Self {
            inner: OsEnv::new(work_dir),
            config,
        }
    }

    /// System directories bind-mounted read-only so that commands inside the
    /// sandbox can find executables and shared libraries. Uses `--ro-bind-try`
    /// so non-existent paths (e.g. `/lib64` on some distros) are silently
    /// skipped instead of causing a failure.
    fn system_ro_binds() -> &'static [(&'static str, &'static str)] {
        &[
            ("/usr", "/usr"),
            ("/etc", "/etc"),
            ("/bin", "/bin"),
            ("/lib", "/lib"),
            ("/lib64", "/lib64"),
            ("/sbin", "/sbin"),
        ]
    }

    /// Build the bwrap argument vector for a shell command invocation.
    fn build_bwrap_args(&self, cwd: &Path) -> Vec<String> {
        let mut args = vec![
            "--unshare-all".to_string(),
            "--die-with-parent".to_string(),
            "--new-session".to_string(),
        ];

        // Share network only when the allowlist is non-empty.
        if !self.config.net_allowlist.is_empty() {
            args.push("--share-net".to_string());
        }

        // Read-only system directories for executables and libraries.
        for (src, dest) in Self::system_ro_binds() {
            args.push("--ro-bind-try".to_string());
            args.push(src.to_string());
            args.push(dest.to_string());
        }

        // /proc and /dev are needed by many commands.
        args.push("--proc".to_string());
        args.push("/proc".to_string());
        args.push("--dev".to_string());
        args.push("/dev".to_string());

        // Fresh /tmp tmpfs; the work dir is bind-mounted *after* this so it
        // is visible at its real path inside the namespace.
        args.push("--tmpfs".to_string());
        args.push("/tmp".to_string());

        // Bind the working directory (read-write).
        let work_dir = self.inner.working_dir().to_string_lossy().to_string();
        args.push("--bind".to_string());
        args.push(work_dir.clone());
        args.push(work_dir.clone());

        // Bind additional fs_allowlist entries (read-write).
        for path in &self.config.fs_allowlist {
            let s = path.to_string_lossy().to_string();
            args.push("--bind".to_string());
            args.push(s.clone());
            args.push(s);
        }

        // Change directory to the requested cwd (or work dir).
        let chdir = cwd.to_string_lossy().to_string();
        args.push("--chdir".to_string());
        args.push(chdir);

        args
    }
}

impl ExecutionEnv for BwrapEnv {
    fn working_dir(&self) -> &Path {
        self.inner.working_dir()
    }

    fn read_text_file<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<String, EnvError>> {
        self.inner.read_text_file(path, abort)
    }

    fn read_text_lines<'a>(
        &'a self,
        path: &'a Path,
        max_lines: Option<usize>,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<String>, EnvError>> {
        self.inner.read_text_lines(path, max_lines, abort)
    }

    fn read_binary_file<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<u8>, EnvError>> {
        self.inner.read_binary_file(path, abort)
    }

    fn write_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        self.inner.write_file(path, content, abort)
    }

    fn append_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        self.inner.append_file(path, content, abort)
    }

    fn file_info<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<FileInfo, EnvError>> {
        self.inner.file_info(path, abort)
    }

    fn list_dir<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<FileInfo>, EnvError>> {
        self.inner.list_dir(path, abort)
    }

    fn exists<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<bool, EnvError>> {
        self.inner.exists(path, abort)
    }

    fn create_dir<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        self.inner.create_dir(path, recursive, abort)
    }

    fn remove<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        force: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        self.inner.remove(path, recursive, force, abort)
    }

    fn create_temp_dir<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<PathBuf, EnvError>> {
        self.inner.create_temp_dir(prefix)
    }

    fn execute_shell<'a>(
        &'a self,
        cmd: &'a str,
        mut opts: ShellOptions<'a>,
    ) -> BoxFuture<'a, Result<ShellOutput, EnvError>> {
        async move {
            if opts.abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }

            let cwd = opts.cwd.unwrap_or(self.inner.working_dir());
            let bwrap_args = self.build_bwrap_args(cwd);

            // Build: bwrap <args> -- /usr/bin/env -i PATH=... HOME=... <injected> sh -c "<cmd>"
            //
            // `env -i` starts with a completely empty environment. We inject
            // only PATH, HOME, and the caller-specified variables, so host
            // credentials (API keys, tokens) are never inherited.
            let mut shell_cmd = tokio::process::Command::new("bwrap");
            shell_cmd.args(&bwrap_args);
            shell_cmd.arg("--");
            shell_cmd.arg("/usr/bin/env");
            shell_cmd.arg("-i");
            shell_cmd.arg("PATH=/usr/bin:/bin");
            shell_cmd.arg(format!("HOME={}", cwd.to_string_lossy()));
            for (key, val) in &opts.env {
                shell_cmd.arg(format!("{key}={val}"));
            }
            shell_cmd.args(["sh", "-c", cmd]);

            let mut child = shell_cmd
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        EnvError::Other("bwrap not available — install bubblewrap".into())
                    } else {
                        EnvError::Io(e)
                    }
                })?;

            let mut stdout_handle = child.stdout.take();
            let mut stderr_handle = child.stderr.take();

            macro_rules! drain_pipe {
                ($handle:expr, $cb:expr) => {
                    async {
                        use tokio::io::AsyncReadExt;
                        let mut buf = Vec::with_capacity(4096);
                        let mut carry = Vec::new();
                        if let Some(h) = $handle.as_mut() {
                            let mut chunk = [0u8; 4096];
                            loop {
                                let n = h.read(&mut chunk).await.map_err(EnvError::Io)?;
                                if n == 0 {
                                    break;
                                }
                                buf.extend_from_slice(&chunk[..n]);
                                if let Some(cb) = $cb.as_deref_mut() {
                                    carry.extend_from_slice(&chunk[..n]);
                                    let split = utf8_safe_boundary(&carry);
                                    let s = String::from_utf8_lossy(&carry[..split]);
                                    cb(&s);
                                    carry = carry[split..].to_vec();
                                }
                            }
                        }
                        if let Some(cb) = $cb.as_deref_mut() {
                            if !carry.is_empty() {
                                let s = String::from_utf8_lossy(&carry);
                                cb(&s);
                            }
                        }
                        Ok::<String, EnvError>(String::from_utf8_lossy(&buf).into_owned())
                    }
                };
            }

            let drain = async {
                let stdout_task = drain_pipe!(stdout_handle, opts.on_stdout);
                let stderr_task = drain_pipe!(stderr_handle, opts.on_stderr);
                let (stdout_res, stderr_res) = tokio::join!(stdout_task, stderr_task);
                let stdout = stdout_res?;
                let stderr = stderr_res?;
                let status = child.wait().await.map_err(EnvError::Io)?;
                Ok::<_, EnvError>((stdout, stderr, status))
            };

            let (stdout, stderr, status) = if let Some(timeout) = opts.timeout {
                tokio::select! {
                    biased;
                    _ = opts.abort.cancelled() => {
                        let _ = child.start_kill();
                        return Err(EnvError::Aborted);
                    }
                    res = tokio::time::timeout(timeout, drain) => match res {
                        Ok(Ok(v)) => v,
                        Ok(Err(e)) => {
                            let _ = child.start_kill();
                            return Err(e);
                        }
                        Err(_) => {
                            let _ = child.start_kill();
                            return Err(EnvError::ShellFailed {
                                exit_code: -1,
                                stdout: String::new(),
                                stderr: "timeout".into(),
                            });
                        }
                    },
                }
            } else {
                tokio::select! {
                    biased;
                    _ = opts.abort.cancelled() => {
                        let _ = child.start_kill();
                        return Err(EnvError::Aborted);
                    }
                    res = drain => match res {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = child.start_kill();
                            return Err(e);
                        }
                    },
                }
            };

            if !status.success() {
                Err(EnvError::ShellFailed {
                    exit_code: status.code().unwrap_or(-1),
                    stdout,
                    stderr,
                })
            } else {
                Ok(ShellOutput {
                    stdout,
                    stderr,
                    exit_code: status.code().unwrap_or(0),
                })
            }
        }
        .boxed()
    }

    fn cleanup<'a>(&'a self) -> BoxFuture<'a, Result<(), EnvError>> {
        self.inner.cleanup()
    }
}

// ── BwrapSandbox ───────────────────────────────────────────────────────────

/// Sandbox backed by bubblewrap(1) for OS-level permission isolation.
///
/// `env()` returns a `BwrapEnv` whose `execute_shell` runs each command inside
/// a fresh `bwrap --unshare-all` namespace. File operations are delegated to
/// the host `OsEnv` restricted to the working directory.
///
/// Maps `SandboxConfig` to bwrap arguments:
/// - `fs_allowlist` → `--bind` (read-write, in addition to the work dir)
/// - `net_allowlist` → `--share-net` when non-empty; `--unshare-all` blocks
///   network when empty
/// - `resource_limits.timeout` → enforced via `tokio::time::timeout`
/// - Environment is cleared via `env -i`; only `PATH`, `HOME`, and
///   caller-specified variables are injected
pub struct BwrapSandbox {
    env: Arc<BwrapEnv>,
    config: SandboxConfig,
    running: AtomicBool,
}

impl BwrapSandbox {
    /// Create a new `BwrapSandbox`. Requires `bwrap` to be installed.
    pub fn new(config: SandboxConfig) -> Self {
        let env = Arc::new(BwrapEnv::new(config.clone()));
        Self {
            env,
            config,
            running: AtomicBool::new(false),
        }
    }

    /// Check whether `bwrap` is installed and functional.
    async fn check_bwrap_available() -> Result<()> {
        let status = tokio::process::Command::new("bwrap")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        match status {
            Ok(s) if s.success() => Ok(()),
            Ok(_) => Err(anyhow::anyhow!("bwrap --version returned non-zero exit")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(anyhow::anyhow!("bwrap not available — install bubblewrap"))
            }
            Err(e) => Err(anyhow::anyhow!("failed to run bwrap: {e}")),
        }
    }
}

impl Sandbox for BwrapSandbox {
    fn start(&self) -> BoxFuture<'_, Result<()>> {
        async move {
            Self::check_bwrap_available().await?;
            self.running.store(true, Ordering::SeqCst);
            Ok(())
        }
        .boxed()
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        self.env.clone()
    }

    fn reset(&self) -> BoxFuture<'_, Result<()>> {
        async move {
            let work_dir = self.env.working_dir();
            if work_dir.exists() {
                tokio::fs::remove_dir_all(work_dir).await?;
                tokio::fs::create_dir_all(work_dir).await?;
            }
            Ok(())
        }
        .boxed()
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
#[path = "tests.rs"] mod tests;
