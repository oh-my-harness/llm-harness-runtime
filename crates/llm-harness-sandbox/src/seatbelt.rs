//! macOS seatbelt sandbox backend.
//!
//! [`SeatbeltEnv`] enforces filesystem isolation via lexical path checks
//! combined with symlink resolution. Shell commands run inside
//! `sandbox-exec -f <policy.sb>` for kernel-level syscall filtering.
//!
//! ## Threat model
//!
//! Two escape paths: structured file API (checked in Rust) and shell
//! commands (filtered by sandbox-exec policy). The file API canonicalizes
//! the resolved path and re-checks containment to prevent symlink escape,
//! but a TOCTOU window remains between canonicalization and the actual
//! open. Shell commands rely on a comprehensive denylist seatbelt policy
//! that denies reads to /etc, /var, /root, /Users, /Library, /System,
//! /private/etc, /private/var, /opt, /srv, /home and denies writes to
//! /private/tmp (except work_dir) and /.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_platform::sandbox::{Sandbox, SandboxConfig};
use llm_harness_types::{EnvError, ExecutionEnv, FileInfo, ShellOptions, ShellOutput};
use tokio_util::sync::CancellationToken;

// ── Policy generation ─────────────────────────────────────────────────────

/// Generate a seatbelt `.sb` policy string from the sandbox configuration.
///
/// The policy uses `(allow default)` with a **comprehensive denylist** of
/// sensitive host paths. This is the practical macOS seatbelt pattern:
/// `(deny default)` is too strict for general-purpose shell commands (the
/// dynamic linker, locale, and other system services need broad implicit
/// access), so instead we allow default and deny every sensitive path
/// explicitly: /etc, /var, /root, /Users, /Library, /System, /private/etc,
/// /private/var, /private/tmp (except work_dir), /opt, /srv, /home.
///
/// Seatbelt uses **last-match-wins** rule precedence: when multiple rules
/// match, the last one in the profile determines the outcome. So the
/// work_dir allow (which appears after the /private/tmp deny) correctly
/// overrides the deny for work_dir paths.
///
/// This is a pure function — it does not touch the filesystem. The caller
/// is responsible for canonicalizing `work_dir` and `allowlist` paths
/// before passing them.
pub(crate) fn generate_policy(
    _config: &SandboxConfig,
    work_dir_canon: &Path,
    allowlist_canon: &[PathBuf],
) -> String {
    let work_dir_str = work_dir_canon.to_string_lossy();
    let mut s = String::with_capacity(1024);

    s.push_str("(version 1)\n");
    s.push_str(";; Allow default, then deny sensitive host paths\n");
    s.push_str("(allow default)\n");
    s.push('\n');

    // Deny network entirely.
    s.push_str(";; Deny network\n");
    s.push_str("(deny network*)\n");
    s.push('\n');

    // Deny reads to sensitive host paths. Both the symlink path (/etc)
    // and the real path (/private/etc) are denied to prevent bypass.
    s.push_str(";; Deny reads to sensitive host paths\n");
    for path in [
        "/etc",
        "/var",
        "/root",
        "/Users",
        "/Library",
        "/System",
        "/private/etc",
        "/private/var",
        "/private/root",
        "/opt",
        "/srv",
        "/home",
    ] {
        s.push_str(&format!(r#"(deny file-read* (subpath "{path}"))"#));
        s.push('\n');
    }
    s.push('\n');

    // Deny writes to /private/tmp (seatbelt last-match-wins: the work_dir
    // allow below overrides this for work_dir paths).
    // Also deny writes to all of / to prevent writing anywhere except
    // explicitly allowed paths.
    s.push_str(";; Deny writes to /private/tmp, allow work_dir (last-match-wins)\n");
    s.push_str(r#"(deny file-write* (subpath "/private/tmp"))"#);
    s.push('\n');
    s.push_str(r#"(deny file-write* (subpath "/"))"#);
    s.push('\n');

    // /dev/null for shell output redirection.
    s.push_str(r#"(allow file-write* (literal "/dev/null"))"#);
    s.push('\n');

    // Allow /private/var/select (locale/sh setup, harmless system dir).
    s.push_str(r#"(allow file-read* (subpath "/private/var/select"))"#);
    s.push('\n');

    // Work_dir: read-write (overrides the /private/tmp deny via last-match-wins).
    s.push_str(&format!(r#"(allow file-read* (subpath "{work_dir_str}"))"#));
    s.push('\n');
    s.push_str(&format!(
        r#"(allow file-write* (subpath "{work_dir_str}"))"#
    ));
    s.push('\n');
    s.push('\n');

    // fs_allowlist entries: read-write.
    if !allowlist_canon.is_empty() {
        s.push_str(";; fs_allowlist entries read-write\n");
        for entry in allowlist_canon {
            let entry_str = entry.to_string_lossy();
            s.push_str(&format!(r#"(allow file-read* (subpath "{entry_str}"))"#));
            s.push('\n');
            s.push_str(&format!(r#"(allow file-write* (subpath "{entry_str}"))"#));
            s.push('\n');
        }
    }

    s
}

// LifecycleState, SHUTDOWN_KILL_GRACE, and lexical_normalize are shared
// with bwrap.rs via the `lifecycle` module.
use crate::lifecycle::{LifecycleAtomic, LifecycleState, SHUTDOWN_KILL_GRACE, lexical_normalize};

// ── ResetLifecycleGuard ────────────────────────────────────────────────────

/// RAII guard that ensures the lifecycle state is rolled back from
/// `Resetting` to `Running` when dropped, unless explicitly completed.
struct ResetLifecycleGuard {
    env: Arc<SeatbeltEnv>,
    completed: bool,
}

impl ResetLifecycleGuard {
    fn begin(env: Arc<SeatbeltEnv>) -> Option<Self> {
        if env.lifecycle.begin_reset() {
            Some(Self {
                env,
                completed: false,
            })
        } else {
            None
        }
    }

    fn handoff(mut self) {
        self.completed = true;
    }
}

impl Drop for ResetLifecycleGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.env.lifecycle.finish_reset();
        }
    }
}

// ── TaintOnDrop ────────────────────────────────────────────────────────────

struct TaintOnDrop {
    env: Arc<SeatbeltEnv>,
}

impl Drop for TaintOnDrop {
    fn drop(&mut self) {
        self.env.lifecycle.taint();
    }
}

// ── SeatbeltEnv ────────────────────────────────────────────────────────────

/// `ExecutionEnv` that enforces filesystem isolation via lexical path
/// checks and runs shell commands inside `sandbox-exec`.
#[derive(Debug)]
pub struct SeatbeltEnv {
    work_dir: PathBuf,
    work_dir_canon: PathBuf,
    allowlist_canon: Vec<PathBuf>,
    config: SandboxConfig,
    owned: bool,
    policy_path: PathBuf,
    lifecycle: LifecycleAtomic,
    shutdown_token: std::sync::Mutex<CancellationToken>,
    reset_lock: Arc<tokio::sync::Mutex<()>>,
}

impl SeatbeltEnv {
    pub(crate) fn new(
        work_dir: PathBuf,
        work_dir_canon: PathBuf,
        allowlist_canon: Vec<PathBuf>,
        config: SandboxConfig,
        owned: bool,
        policy_path: PathBuf,
    ) -> Self {
        Self {
            work_dir,
            work_dir_canon,
            allowlist_canon,
            config,
            owned,
            policy_path,
            lifecycle: LifecycleAtomic::new(),
            shutdown_token: std::sync::Mutex::new(CancellationToken::new()),
            reset_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn lifecycle(&self) -> LifecycleState {
        self.lifecycle.load()
    }

    fn shutdown_token_clone(&self) -> CancellationToken {
        self.shutdown_token
            .lock()
            .expect("shutdown_token mutex poisoned")
            .clone()
    }

    fn cancel_shutdown_token(&self) {
        self.shutdown_token
            .lock()
            .expect("shutdown_token mutex poisoned")
            .cancel();
    }

    fn swap_shutdown_token(&self, new_token: CancellationToken) -> CancellationToken {
        let mut guard = self
            .shutdown_token
            .lock()
            .expect("shutdown_token mutex poisoned");
        std::mem::replace(&mut *guard, new_token)
    }

    async fn acquire_op_lock(
        &self,
        abort: &CancellationToken,
    ) -> Result<(tokio::sync::MutexGuard<'_, ()>, CancellationToken), EnvError> {
        if self.lifecycle() != LifecycleState::Running {
            return Err(EnvError::Other("sandbox is shutting down".into()));
        }
        let shutdown_token = self.shutdown_token_clone();
        let guard = tokio::select! {
            biased;
            _ = abort.cancelled() => return Err(EnvError::Aborted),
            _ = shutdown_token.cancelled() => {
                return Err(EnvError::Other("sandbox is shutting down".into()));
            }
            guard = self.reset_lock.lock() => guard,
        };
        if abort.is_cancelled() {
            return Err(EnvError::Aborted);
        }
        if self.lifecycle() != LifecycleState::Running {
            return Err(EnvError::Other("sandbox is shutting down".into()));
        }
        Ok((guard, shutdown_token))
    }

    /// Clear all contents of the work_dir using std::fs.
    ///
    /// Unlike `remove_dir_all` + `create_dir_all`, this keeps the directory
    /// inode alive, so subsequent file operations continue to work after
    /// `reset()`. Symlinks are removed with `remove_file` (never followed).
    fn clear_work_dir_contents(&self) -> std::io::Result<()> {
        for entry in std::fs::read_dir(&self.work_dir_canon)? {
            let entry = entry?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
        }
        Ok(())
    }

    fn working_dir_path(&self) -> &Path {
        &self.work_dir
    }

    fn owned(&self) -> bool {
        self.owned
    }

    /// Resolve `path` to a canonical path within an allowed root.
    ///
    /// 1. Make the path absolute (join with work_dir if relative).
    /// 2. Lexically normalize (handle `.` and `..`).
    /// 3. Canonicalize the path (resolve symlinks) via the filesystem.
    /// 4. Re-check containment in work_dir or fs_allowlist.
    ///
    /// Step 3-4 prevents symlink escape: a symlink inside work_dir
    /// pointing to `/etc/passwd` is resolved to its real target, which
    /// fails the containment check. A TOCTOU window remains between
    /// canonicalization and the actual open, but this is far narrower
    /// than unconditional symlink-following.
    fn resolve_path(&self, path: &Path) -> Result<PathBuf, EnvError> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.work_dir_canon.join(path)
        };
        let normalized = lexical_normalize(&resolved);

        // Lexical containment check — fast path to reject obvious escapes
        // before touching the filesystem.
        let lex_ok = normalized == self.work_dir_canon
            || normalized.starts_with(&self.work_dir_canon)
            || self
                .allowlist_canon
                .iter()
                .any(|root| normalized == *root || normalized.starts_with(root));
        if !lex_ok {
            return Err(EnvError::PermissionDenied(normalized));
        }

        // Canonicalize to resolve symlinks, then re-check containment.
        // If the path doesn't exist yet (e.g. write_file creating a new
        // file), canonicalize the parent and append the file name.
        let canonical = match std::fs::canonicalize(&normalized) {
            Ok(c) => c,
            Err(_) => {
                // Path doesn't exist — canonicalize the parent and join
                // the final component. This handles write_file/append_file
                // creating new files.
                let parent = normalized.parent();
                let file_name = normalized.file_name();
                match (parent, file_name) {
                    (Some(p), Some(name)) if !p.as_os_str().is_empty() => {
                        let parent_canon = std::fs::canonicalize(p).map_err(EnvError::Io)?;
                        parent_canon.join(name)
                    }
                    _ => normalized.clone(),
                }
            }
        };

        if canonical == self.work_dir_canon || canonical.starts_with(&self.work_dir_canon) {
            return Ok(canonical);
        }
        for root in &self.allowlist_canon {
            if canonical == *root || canonical.starts_with(root) {
                return Ok(canonical);
            }
        }
        Err(EnvError::PermissionDenied(canonical))
    }
}

impl ExecutionEnv for SeatbeltEnv {
    fn working_dir(&self) -> &Path {
        &self.work_dir
    }

    fn read_text_file<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<String, EnvError>> {
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let full = self.resolve_path(path)?;
            tokio::fs::read_to_string(&full)
                .await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => EnvError::NotFound(full),
                    std::io::ErrorKind::PermissionDenied => EnvError::PermissionDenied(full),
                    std::io::ErrorKind::InvalidData => EnvError::InvalidUtf8(full),
                    _ => EnvError::Io(e),
                })
        }
        .boxed()
    }

    fn read_text_lines<'a>(
        &'a self,
        path: &'a Path,
        max_lines: Option<usize>,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<String>, EnvError>> {
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let full = self.resolve_path(path)?;
            let text = tokio::fs::read_to_string(&full)
                .await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => EnvError::NotFound(full),
                    std::io::ErrorKind::PermissionDenied => EnvError::PermissionDenied(full),
                    std::io::ErrorKind::InvalidData => EnvError::InvalidUtf8(full),
                    _ => EnvError::Io(e),
                })?;
            let lines: Vec<String> = match max_lines {
                Some(max) => text.lines().take(max).map(|s| s.to_owned()).collect(),
                None => text.lines().map(|s| s.to_owned()).collect(),
            };
            Ok(lines)
        }
        .boxed()
    }

    fn read_binary_file<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<u8>, EnvError>> {
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let full = self.resolve_path(path)?;
            tokio::fs::read(&full).await.map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => EnvError::NotFound(full),
                std::io::ErrorKind::PermissionDenied => EnvError::PermissionDenied(full),
                _ => EnvError::Io(e),
            })
        }
        .boxed()
    }

    fn write_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let full = self.resolve_path(path)?;
            let content = content.to_vec();
            tokio::task::spawn_blocking(move || -> std::result::Result<(), EnvError> {
                if let Some(parent) = full.parent()
                    && !parent.as_os_str().is_empty()
                    && parent != Path::new("/")
                {
                    std::fs::create_dir_all(parent).map_err(EnvError::Io)?;
                }
                std::fs::write(&full, &content).map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => EnvError::NotFound(full),
                    std::io::ErrorKind::PermissionDenied => EnvError::PermissionDenied(full),
                    _ => EnvError::Io(e),
                })
            })
            .await
            .map_err(|e| EnvError::Other(format!("spawn_blocking: {e}")))?
        }
        .boxed()
    }

    fn append_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let full = self.resolve_path(path)?;
            let content = content.to_vec();
            tokio::task::spawn_blocking(move || -> std::result::Result<(), EnvError> {
                if let Some(parent) = full.parent()
                    && !parent.as_os_str().is_empty()
                    && parent != Path::new("/")
                {
                    std::fs::create_dir_all(parent).map_err(EnvError::Io)?;
                }
                use std::io::Write;
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&full)
                    .map_err(|e| match e.kind() {
                        std::io::ErrorKind::NotFound => EnvError::NotFound(full),
                        std::io::ErrorKind::PermissionDenied => EnvError::PermissionDenied(full),
                        _ => EnvError::Io(e),
                    })?;
                file.write_all(&content).map_err(EnvError::Io)
            })
            .await
            .map_err(|e| EnvError::Other(format!("spawn_blocking: {e}")))?
        }
        .boxed()
    }

    fn file_info<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<FileInfo, EnvError>> {
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let full = self.resolve_path(path)?;
            let meta = tokio::fs::metadata(&full)
                .await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => EnvError::NotFound(full.clone()),
                    std::io::ErrorKind::PermissionDenied => {
                        EnvError::PermissionDenied(full.clone())
                    }
                    _ => EnvError::Io(e),
                })?;
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Ok(FileInfo {
                path: full,
                is_dir: meta.is_dir(),
                size: meta.len(),
                modified: chrono::DateTime::from_timestamp(modified, 0).unwrap_or_default(),
            })
        }
        .boxed()
    }

    fn list_dir<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<FileInfo>, EnvError>> {
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let full = self.resolve_path(path)?;
            tokio::task::spawn_blocking(move || -> std::result::Result<Vec<FileInfo>, EnvError> {
                let entries = std::fs::read_dir(&full).map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => EnvError::NotFound(full),
                    std::io::ErrorKind::PermissionDenied => EnvError::PermissionDenied(full),
                    _ => EnvError::Io(e),
                })?;
                let mut results = Vec::new();
                for entry in entries {
                    let entry = entry.map_err(EnvError::Io)?;
                    let entry_path = entry.path();
                    let meta = entry.metadata().map_err(EnvError::Io)?;
                    let modified = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    results.push(FileInfo {
                        path: entry_path,
                        is_dir: meta.is_dir(),
                        size: meta.len(),
                        modified: chrono::DateTime::from_timestamp(modified, 0).unwrap_or_default(),
                    });
                }
                Ok(results)
            })
            .await
            .map_err(|e| EnvError::Other(format!("spawn_blocking: {e}")))?
        }
        .boxed()
    }

    fn exists<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<bool, EnvError>> {
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let full = self.resolve_path(path)?;
            Ok(tokio::fs::try_exists(&full).await.unwrap_or(false))
        }
        .boxed()
    }

    fn create_dir<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let full = self.resolve_path(path)?;
            if recursive {
                tokio::fs::create_dir_all(&full)
                    .await
                    .map_err(EnvError::Io)?;
            } else {
                tokio::fs::create_dir(&full)
                    .await
                    .map_err(|e| match e.kind() {
                        std::io::ErrorKind::AlreadyExists => EnvError::AlreadyExists(full),
                        _ => EnvError::Io(e),
                    })?;
            }
            Ok(())
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
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let full = self.resolve_path(path)?;
            tokio::task::spawn_blocking(move || -> std::result::Result<(), EnvError> {
                let meta = match std::fs::symlink_metadata(&full) {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        if force {
                            return Ok(());
                        }
                        return Err(EnvError::NotFound(full));
                    }
                    Err(e) => return Err(EnvError::Io(e)),
                };
                if meta.is_dir() {
                    if recursive {
                        std::fs::remove_dir_all(&full).map_err(EnvError::Io)?;
                    } else {
                        std::fs::remove_dir(&full).map_err(EnvError::Io)?;
                    }
                } else {
                    std::fs::remove_file(&full).map_err(EnvError::Io)?;
                }
                Ok(())
            })
            .await
            .map_err(|e| EnvError::Other(format!("spawn_blocking: {e}")))?
        }
        .boxed()
    }

    fn create_temp_dir<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<PathBuf, EnvError>> {
        async move {
            let (_guard, _token) = self.acquire_op_lock(&CancellationToken::new()).await?;
            let id = uuid::Uuid::new_v4();
            let dir = self.work_dir_canon.join(format!("{prefix}-{id}"));
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(EnvError::Io)?;
            Ok(dir)
        }
        .boxed()
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
            let (_guard, shutdown_token) = self.acquire_op_lock(&opts.abort).await?;

            // Enforce config-level timeout as an upper bound (same as bwrap).
            if let Some(config_timeout) = self.config.resource_limits.timeout {
                opts.timeout = Some(match opts.timeout {
                    Some(t) => t.min(config_timeout),
                    None => config_timeout,
                });
            }

            let cwd = opts
                .cwd
                .map(|c| c.to_path_buf())
                .unwrap_or_else(|| self.work_dir_canon.clone());

            let mut shell_cmd = tokio::process::Command::new("sandbox-exec");
            shell_cmd.arg("-f").arg(&self.policy_path);
            shell_cmd.arg("--");
            shell_cmd.arg("/usr/bin/env");
            shell_cmd.arg("-i");
            shell_cmd.arg("PATH=/usr/bin:/bin");
            shell_cmd.arg(format!("HOME={}", cwd.to_string_lossy()));
            for (key, val) in &opts.env {
                shell_cmd.arg(format!("{key}={val}"));
            }
            shell_cmd.args(["sh", "-c", cmd]);
            shell_cmd.current_dir(&cwd);

            // Re-check lifecycle.
            if self.lifecycle() != LifecycleState::Running || shutdown_token.is_cancelled() {
                return Err(EnvError::Other("sandbox is shutting down".into()));
            }

            // Acquire a pre-spawn admission (same as bwrap).
            let reaper = crate::shell_runner::ChildReaper::global()
                .ok_or_else(|| EnvError::Other("child reaper unavailable".into()))?;
            let admission = reaper
                .try_admit()
                .ok_or_else(|| EnvError::Other("sandbox at capacity or reaper unhealthy".into()))?;

            let child = match shell_cmd
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(child) => child,
                Err(e) => {
                    return Err(if e.kind() == std::io::ErrorKind::NotFound {
                        EnvError::Other("sandbox-exec not available".into())
                    } else {
                        EnvError::Io(e)
                    });
                }
            };

            let lease = admission.into_lease(child);
            crate::shell_runner::drain_child_output_guarded(lease, opts, shutdown_token).await
        }
        .boxed()
    }

    fn cleanup<'a>(&'a self) -> BoxFuture<'a, Result<(), EnvError>> {
        async move { Ok(()) }.boxed()
    }
}

// ── SeatbeltSandbox ────────────────────────────────────────────────────────

/// Sandbox backed by macOS `sandbox-exec` for OS-level isolation.
///
/// File operations are checked at the Rust level via lexical path
/// normalization and root containment. Shell commands run inside
/// `sandbox-exec -f <policy.sb>` for kernel-level syscall filtering.
///
/// **Unimplemented policies** (fail-closed in `new()`):
/// - `fs_denylist` — not enforced; rejected at construction.
/// - `net_allowlist` rules — seatbelt network policy semantics complex;
///   rejected at construction.
/// - `max_cpus`, `max_memory_mb`, `max_disk_mb` — not enforced; rejected.
pub struct SeatbeltSandbox {
    env: Arc<SeatbeltEnv>,
    config: SandboxConfig,
    running: AtomicBool,
}

impl SeatbeltSandbox {
    /// Create a new `SeatbeltSandbox`.
    ///
    /// Returns an error if the configuration contains policies that this
    /// backend cannot enforce (fail-closed), same as bwrap.
    pub fn new(config: SandboxConfig) -> Result<Self> {
        Self::validate_config(&config)?;

        let owned = config.work_dir.is_none();
        let work_dir = match &config.work_dir {
            Some(p) => p.clone(),
            None => {
                let id = uuid::Uuid::new_v4();
                PathBuf::from(format!("/tmp/seatbelt-{id}"))
            }
        };

        if owned {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&work_dir)
                .or_else(|e| {
                    if e.kind() == std::io::ErrorKind::AlreadyExists {
                        Ok(())
                    } else {
                        Err(e)
                    }
                })?;
        } else {
            std::fs::create_dir_all(&work_dir)?;
        }

        let work_dir_canon = std::fs::canonicalize(&work_dir)
            .map_err(|e| anyhow::anyhow!("failed to canonicalize work_dir: {e}"))?;

        let allowlist_canon = config
            .fs_allowlist
            .iter()
            .map(|p| {
                std::fs::canonicalize(p)
                    .map_err(|e| anyhow::anyhow!("fs_allowlist entry {:?}: {e}", p))
            })
            .collect::<Result<Vec<_>>>()?;

        let policy_path =
            std::env::temp_dir().join(format!("seatbelt-{}.sb", uuid::Uuid::new_v4()));

        let env = Arc::new(SeatbeltEnv::new(
            work_dir,
            work_dir_canon,
            allowlist_canon,
            config.clone(),
            owned,
            policy_path,
        ));

        Ok(Self {
            env,
            config,
            running: AtomicBool::new(false),
        })
    }

    /// Reject configurations with unimplemented policies (same as bwrap).
    fn validate_config(config: &SandboxConfig) -> Result<()> {
        if !config.fs_denylist.is_empty() {
            anyhow::bail!(
                "fs_denylist is not supported by SeatbeltSandbox; \
                 remove denylist entries or use a backend that enforces them"
            );
        }
        if !config.net_allowlist.is_empty() {
            anyhow::bail!(
                "net_allowlist rules are not supported by SeatbeltSandbox; \
                 seatbelt network policy semantics are complex. \
                 Use an empty allowlist for full network isolation."
            );
        }
        let limits = &config.resource_limits;
        if limits.max_cpus.is_some() {
            anyhow::bail!("max_cpus is not supported by SeatbeltSandbox");
        }
        if limits.max_memory_mb.is_some() {
            anyhow::bail!("max_memory_mb is not supported by SeatbeltSandbox");
        }
        if limits.max_disk_mb.is_some() {
            anyhow::bail!("max_disk_mb is not supported by SeatbeltSandbox");
        }
        Ok(())
    }
}

impl Sandbox for SeatbeltSandbox {
    fn start(&self) -> BoxFuture<'_, Result<()>> {
        async move {
            // Check lifecycle state — same as bwrap. A Tainted sandbox
            // (reset cleanup failed) cannot be restarted; it must be shut
            // down and recreated. Stopped/ShuttingDown are likewise not
            // restartable.
            match self.env.lifecycle() {
                LifecycleState::Running => {}
                LifecycleState::Tainted => {
                    return Err(anyhow::anyhow!(
                        "cannot start: sandbox is tainted (reset cleanup failed)"
                    ));
                }
                other => {
                    return Err(anyhow::anyhow!(
                        "cannot start: sandbox is in {other:?} state"
                    ));
                }
            }

            // If already started, short-circuit to Ok (idempotent).
            if self.running.load(Ordering::SeqCst) {
                return Ok(());
            }

            // Generate and write the policy file.
            let policy = generate_policy(
                &self.config,
                &self.env.work_dir_canon,
                &self.env.allowlist_canon,
            );
            std::fs::write(&self.env.policy_path, policy)
                .map_err(|e| anyhow::anyhow!("failed to write policy file: {e}"))?;

            // Probe sandbox-exec availability.
            let result = tokio::process::Command::new("sandbox-exec")
                .arg("-f")
                .arg(&self.env.policy_path)
                .arg("--")
                .arg("/usr/bin/true")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .await;

            match result {
                Ok(o) if o.status.success() => {
                    self.running.store(true, Ordering::SeqCst);
                    Ok(())
                }
                Ok(o) => {
                    let _ = std::fs::remove_file(&self.env.policy_path);
                    Err(anyhow::anyhow!(
                        "sandbox-exec probe failed (exit {:?}): {}",
                        o.status.code(),
                        String::from_utf8_lossy(&o.stderr).trim()
                    ))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let _ = std::fs::remove_file(&self.env.policy_path);
                    Err(anyhow::anyhow!("sandbox-exec not available on this system"))
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&self.env.policy_path);
                    Err(anyhow::anyhow!("failed to run sandbox-exec: {e}"))
                }
            }
        }
        .boxed()
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        self.env.clone()
    }

    fn reset(&self) -> BoxFuture<'_, Result<()>> {
        async move {
            let guard = match ResetLifecycleGuard::begin(self.env.clone()) {
                Some(g) => g,
                None => anyhow::bail!("sandbox is not running; cannot reset"),
            };

            if !self.env.owned() {
                anyhow::bail!(
                    "refusing to reset work_dir {:?}: sandbox does not own this directory \
                     (work_dir was explicitly provided; only auto-generated directories \
                     with work_dir=None can be reset)",
                    self.env.working_dir_path()
                );
            }

            let old_token = self.env.swap_shutdown_token(CancellationToken::new());
            old_token.cancel();

            let owned_guard = tokio::time::timeout(
                SHUTDOWN_KILL_GRACE,
                self.env.reset_lock.clone().lock_owned(),
            )
            .await
            .map_err(|_| anyhow::anyhow!("reset timed out waiting for lock"))?;

            guard.handoff();

            let env = self.env.clone();
            let handle = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                let _taint = TaintOnDrop { env: env.clone() };
                let _lock = owned_guard;
                let result = env.clear_work_dir_contents();
                match &result {
                    Ok(()) => {
                        env.lifecycle.finish_reset();
                    }
                    Err(_) => {
                        env.lifecycle.taint();
                    }
                }
                result
            });

            match handle.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(anyhow::anyhow!("reset failed: {e}")),
                Err(join_err) => {
                    self.env.lifecycle.taint();
                    Err(anyhow::anyhow!("reset task failed: {join_err}"))
                }
            }
        }
        .boxed()
    }

    fn shutdown(self: Box<Self>) -> BoxFuture<'static, Result<()>> {
        let sandbox = *self;
        async move {
            let was_running = sandbox.env.lifecycle.begin_shutdown();
            if !was_running {
                sandbox.env.lifecycle.finish_shutdown();
                sandbox.running.store(false, Ordering::SeqCst);
                return Ok(());
            }

            sandbox.env.cancel_shutdown_token();

            match tokio::time::timeout(SHUTDOWN_KILL_GRACE, sandbox.env.reset_lock.lock()).await {
                Ok(guard) => {
                    let _guard = guard;
                    // Delete the policy file.
                    let _ = tokio::fs::remove_file(&sandbox.env.policy_path).await;
                    // Delete work_dir if owned.
                    if sandbox.env.owned() {
                        let work_dir = sandbox.env.work_dir_canon.clone();
                        if work_dir.exists() {
                            tokio::fs::remove_dir_all(&work_dir).await?;
                        }
                    }
                }
                Err(_) => {
                    // Lock timed out — preserve workspace, still clean up policy file.
                    let _ = tokio::fs::remove_file(&sandbox.env.policy_path).await;
                    sandbox.env.lifecycle.finish_shutdown();
                    sandbox.running.store(false, Ordering::SeqCst);
                    return Err(anyhow::anyhow!(
                        "shutdown timed out waiting for lock; workspace preserved"
                    ));
                }
            }

            sandbox.env.lifecycle.finish_shutdown();
            sandbox.running.store(false, Ordering::SeqCst);
            Ok(())
        }
        .boxed()
    }

    fn config(&self) -> &SandboxConfig {
        &self.config
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst) && self.env.lifecycle() == LifecycleState::Running
    }
}

#[cfg(test)]
#[path = "seatbelt_tests.rs"]
mod tests;
