use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_runtime::platform::sandbox::{Sandbox, SandboxConfig};
use llm_harness_runtime_sandbox_os::{OsEnv, drain_child_output};
use llm_harness_types::{
    EnvError, ExecutionEnv, FileInfo, ShellOptions, ShellOutput,
};
use tokio_util::sync::CancellationToken;

// ── Path sandboxing ────────────────────────────────────────────────────────

/// Lexically normalize a path: resolve `.` and `..` components without
/// touching the filesystem (so it works for non-existent paths too).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {} // skip "."
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// Check whether `path` falls within any of the allowed roots.
/// Both the path and the roots are lexically normalized first; absolute paths
/// outside the roots are rejected.
fn is_path_allowed(path: &Path, allowed_roots: &[PathBuf]) -> bool {
    let normalized = lexical_normalize(path);
    for root in allowed_roots {
        let normalized_root = lexical_normalize(root);
        if normalized == normalized_root {
            return true;
        }
        if normalized.starts_with(&normalized_root) {
            return true;
        }
    }
    false
}

// ── BwrapEnv ───────────────────────────────────────────────────────────────

/// `ExecutionEnv` that enforces filesystem path restrictions on all file
/// operations and runs shell commands inside a bubblewrap namespace.
///
/// File operations (read/write/list/remove/etc.) are rejected if the resolved
/// path falls outside the working directory or `fs_allowlist` entries. This
/// prevents absolute-path, `..` traversal, and cross-directory escapes.
///
/// Shell commands run inside `bwrap --unshare-all`, which creates a fresh
/// mount/net/pid namespace per invocation.
#[derive(Debug)]
pub struct BwrapEnv {
    inner: OsEnv,
    config: SandboxConfig,
    /// Allowed path roots: working directory + fs_allowlist entries.
    /// Computed once at construction time.
    allowed_roots: Vec<PathBuf>,
}

impl BwrapEnv {
    /// Create a new `BwrapEnv` from a sandbox configuration.
    pub fn new(config: SandboxConfig) -> Self {
        let work_dir = config
            .work_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("/tmp/sandbox-bwrap"));
        std::fs::create_dir_all(&work_dir).ok();

        let mut allowed_roots = vec![work_dir.clone()];
        allowed_roots.extend(config.fs_allowlist.iter().cloned());

        Self {
            inner: OsEnv::new(work_dir),
            config,
            allowed_roots,
        }
    }

    /// Resolve a path relative to the working directory, then check it is
    /// within the allowed roots. Returns the resolved path on success.
    fn check_path(&self, path: &Path) -> Result<PathBuf, EnvError> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.inner.working_dir().join(path)
        };
        if !is_path_allowed(&resolved, &self.allowed_roots) {
            return Err(EnvError::PermissionDenied(resolved));
        }
        Ok(resolved)
    }

    /// System directories bind-mounted read-only so that commands inside the
    /// sandbox can find executables and shared libraries.
    ///
    /// `/etc` is intentionally **not** bound: it contains host credentials
    /// (`/etc/passwd`, `/etc/shadow`, API keys in `/etc/environment`, etc.).
    /// Commands work fine without it because `env -i` provides a clean
    /// environment and the dynamic linker does not require `/etc/ld.so.cache`.
    fn system_ro_binds() -> &'static [(&'static str, &'static str)] {
        &[
            ("/usr", "/usr"),
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
        // Note: --share-net opens the entire host network. Granular per-host
        // filtering is not yet implemented; see validate_config().
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
        let checked = self.check_path(path);
        async move {
            let resolved = checked?;
            self.inner.read_text_file(&resolved, abort).await
        }
        .boxed()
    }

    fn read_text_lines<'a>(
        &'a self,
        path: &'a Path,
        max_lines: Option<usize>,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<String>, EnvError>> {
        let checked = self.check_path(path);
        async move {
            let resolved = checked?;
            self.inner.read_text_lines(&resolved, max_lines, abort).await
        }
        .boxed()
    }

    fn read_binary_file<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<u8>, EnvError>> {
        let checked = self.check_path(path);
        async move {
            let resolved = checked?;
            self.inner.read_binary_file(&resolved, abort).await
        }
        .boxed()
    }

    fn write_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let checked = self.check_path(path);
        async move {
            let resolved = checked?;
            self.inner.write_file(&resolved, content, abort).await
        }
        .boxed()
    }

    fn append_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let checked = self.check_path(path);
        async move {
            let resolved = checked?;
            self.inner.append_file(&resolved, content, abort).await
        }
        .boxed()
    }

    fn file_info<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<FileInfo, EnvError>> {
        let checked = self.check_path(path);
        async move {
            let resolved = checked?;
            self.inner.file_info(&resolved, abort).await
        }
        .boxed()
    }

    fn list_dir<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<FileInfo>, EnvError>> {
        let checked = self.check_path(path);
        async move {
            let resolved = checked?;
            self.inner.list_dir(&resolved, abort).await
        }
        .boxed()
    }

    fn exists<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<bool, EnvError>> {
        let checked = self.check_path(path);
        async move {
            let resolved = checked?;
            self.inner.exists(&resolved, abort).await
        }
        .boxed()
    }

    fn create_dir<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let checked = self.check_path(path);
        async move {
            let resolved = checked?;
            self.inner.create_dir(&resolved, recursive, abort).await
        }
        .boxed()
    }

    fn remove<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        force: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let checked = self.check_path(path);
        async move {
            let resolved = checked?;
            self.inner.remove(&resolved, recursive, force, abort).await
        }
        .boxed()
    }

    fn create_temp_dir<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<PathBuf, EnvError>> {
        self.inner.create_temp_dir(prefix)
    }

    fn execute_shell<'a>(
        &'a self,
        cmd: &'a str,
        opts: ShellOptions<'a>,
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

            let child = shell_cmd
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

            drain_child_output(child, opts).await
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
/// `env()` returns a `BwrapEnv` that:
/// - Rejects file operations on paths outside the working directory or
///   `fs_allowlist`.
/// - Runs `execute_shell` inside `bwrap --unshare-all` (fresh mount/net/pid
///   namespace per command).
/// - Clears the environment via `env -i`; only `PATH`, `HOME`, and
///   caller-specified variables are injected.
///
/// **Unimplemented policies** (fail-closed in `new()`):
/// - `fs_denylist` — not enforced by bwrap; rejected at construction.
/// - `net_allowlist` rules — bwrap only supports all-or-nothing network;
///   non-empty allowlist opens the entire host network, so it is rejected.
/// - `max_cpus`, `max_memory_mb`, `max_disk_mb` — not enforced; rejected.
#[derive(Debug)]
pub struct BwrapSandbox {
    env: Arc<BwrapEnv>,
    config: SandboxConfig,
    running: AtomicBool,
}

impl BwrapSandbox {
    /// Create a new `BwrapSandbox`.
    ///
    /// Returns an error if the configuration contains policies that this
    /// backend cannot enforce (fail-closed). This prevents silent
    /// downgrading of security guarantees.
    pub fn new(config: SandboxConfig) -> Result<Self> {
        Self::validate_config(&config)?;
        let env = Arc::new(BwrapEnv::new(config.clone()));
        Ok(Self {
            env,
            config,
            running: AtomicBool::new(false),
        })
    }

    /// Reject configurations with unimplemented policies instead of silently
    /// downgrading them.
    fn validate_config(config: &SandboxConfig) -> Result<()> {
        if !config.fs_denylist.is_empty() {
            anyhow::bail!(
                "fs_denylist is not supported by BwrapSandbox; \
                 remove denylist entries or use a backend that enforces them"
            );
        }
        if !config.net_allowlist.is_empty() {
            anyhow::bail!(
                "net_allowlist rules are not supported by BwrapSandbox; \
                 bwrap only supports all-or-nothing network isolation. \
                 Use an empty allowlist for full network isolation."
            );
        }
        let limits = &config.resource_limits;
        if limits.max_cpus.is_some() {
            anyhow::bail!("max_cpus is not supported by BwrapSandbox");
        }
        if limits.max_memory_mb.is_some() {
            anyhow::bail!("max_memory_mb is not supported by BwrapSandbox");
        }
        if limits.max_disk_mb.is_some() {
            anyhow::bail!("max_disk_mb is not supported by BwrapSandbox");
        }
        Ok(())
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

    /// Returns true if the work_dir is safe to reset (i.e. it is not a root
    /// or ancestor path, and it is not the current working directory).
    fn is_safe_disposable_root(path: &Path) -> bool {
        // Reject root.
        if path == Path::new("/") || path.parent().is_none() {
            return false;
        }
        // Reject the process's current working directory or any ancestor of it
        // (so reset() can never delete a directory the process relies on).
        if let Ok(cwd) = std::env::current_dir()
            && (path == cwd || (cwd.starts_with(path) && cwd != path))
        {
            return false;
        }
        true
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
            if !Self::is_safe_disposable_root(work_dir) {
                anyhow::bail!(
                    "refusing to reset work_dir {:?}: not a safe disposable root",
                    work_dir
                );
            }
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
#[path = "tests.rs"]
mod tests;
