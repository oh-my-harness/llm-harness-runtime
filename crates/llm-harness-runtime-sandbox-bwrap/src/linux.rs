//! Linux bwrap sandbox backend.
//!
//! [`BwrapEnv`] enforces filesystem isolation using [`cap_std::fs::Dir`]
//! capability objects. Each allowed root (working directory + `fs_allowlist`
//! entries) is opened once as an ambient authority. All subsequent file
//! operations go through the `Dir` handle, which prevents:
//!
//! - `..` traversal
//! - absolute-path escape
//! - symlink escape (symlinks pointing outside the allowed root are rejected)
//!
//! Shell commands run inside a `bwrap --unshare-all` namespace for full
//! mount/net/pid isolation.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, MetadataExt, OpenOptions};
use chrono::{DateTime, Utc};
use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_runtime::platform::sandbox::{Sandbox, SandboxConfig};
use llm_harness_runtime_sandbox_os::drain_child_output_guarded;
use llm_harness_types::{EnvError, ExecutionEnv, FileInfo, ShellOptions, ShellOutput};
use tokio_util::sync::CancellationToken;

// ── Path helpers ───────────────────────────────────────────────────────────

/// Lexically normalize a path: resolve `.` and `..` components without
/// touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// Map an `io::Error` to `EnvError`, preserving the offending path.
fn map_io_err(e: std::io::Error, path: PathBuf) -> EnvError {
    match e.kind() {
        std::io::ErrorKind::NotFound => EnvError::NotFound(path),
        std::io::ErrorKind::PermissionDenied => EnvError::PermissionDenied(path),
        std::io::ErrorKind::AlreadyExists => EnvError::AlreadyExists(path),
        _ => EnvError::Io(e),
    }
}

/// Like `map_io_err` but maps `InvalidData` to `InvalidUtf8` (for text reads).
fn map_read_err(e: std::io::Error, path: PathBuf) -> EnvError {
    match e.kind() {
        std::io::ErrorKind::NotFound => EnvError::NotFound(path),
        std::io::ErrorKind::PermissionDenied => EnvError::PermissionDenied(path),
        std::io::ErrorKind::InvalidData => EnvError::InvalidUtf8(path),
        _ => EnvError::Io(e),
    }
}

/// Convert `SystemTime` metadata to `DateTime<Utc>`.
fn modified_to_datetime(meta: &cap_std::fs::Metadata) -> DateTime<Utc> {
    let secs = meta.mtime();
    let nsecs = meta.mtime_nsec() as u32;
    DateTime::from_timestamp(secs, nsecs).unwrap_or_default()
}

// ── Lifecycle state ────────────────────────────────────────────────────────

/// Sandbox lifecycle state, tracked atomically to coordinate shutdown.
///
/// - `Running`: normal operation; new work is admitted.
/// - `Resetting`: reset is in progress; new work is rejected so reset can
///   acquire the lock without racing against a newly-admitted command.
///   After cleanup, the state transitions back to `Running`.
/// - `ShuttingDown`: shutdown has begun; new work is rejected, active
///   commands are cancelled and killed.
/// - `Stopped`: shutdown is complete; no further operations are possible.
/// - `Tainted`: reset cleanup failed or panicked mid-mutation; the sandbox
///   is unusable and must be shut down and recreated.  New work is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum LifecycleState {
    Running = 0,
    Resetting = 1,
    ShuttingDown = 2,
    Stopped = 3,
    Tainted = 4,
}

impl LifecycleState {
    fn from_atomic(v: u8) -> Self {
        match v {
            0 => LifecycleState::Running,
            1 => LifecycleState::Resetting,
            2 => LifecycleState::ShuttingDown,
            3 => LifecycleState::Stopped,
            _ => LifecycleState::Tainted,
        }
    }
}
/// Maximum time `shutdown()` waits for an active shell command to terminate
/// after sending kill before proceeding with workspace deletion.
const SHUTDOWN_KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

// ── ResetLifecycleGuard ────────────────────────────────────────────────────

/// RAII guard that ensures the lifecycle state is rolled back from
/// `Resetting` to `Running` when dropped, unless explicitly completed.
///
/// This prevents the sandbox from being permanently stuck in `Resetting` if
/// `reset()` is cancelled (the future is dropped) or errors out at any
/// await point — including token swap, lock acquisition timeout,
/// `spawn_blocking` errors, and workspace clearing I/O errors.
///
/// The guard is created by [`BwrapEnv::begin_reset`] (transitioning
/// `Running` → `Resetting`).  On `Drop`, if not completed, it calls
/// [`BwrapEnv::finish_reset`] to roll back to `Running`.  If shutdown stole
/// the state (`Resetting` → `ShuttingDown`) while reset was in progress,
/// `finish_reset` returns `false` and the guard does nothing — shutdown
/// handles cleanup.
///
/// Once mutation starts (inside the blocking cleanup task), the guard must
/// be **handed off** via [`ResetLifecycleGuard::handoff`] so that dropping
/// the caller future does not roll back to `Running` while cleanup is still
/// running.  The blocking task then owns the lifecycle transition.
struct ResetLifecycleGuard {
    env: Arc<BwrapEnv>,
    completed: bool,
}

impl ResetLifecycleGuard {
    /// Attempt to enter the `Resetting` state.
    ///
    /// Returns `None` if the sandbox is not in `Running` state (already
    /// resetting, shutting down, or stopped).
    fn begin(env: Arc<BwrapEnv>) -> Option<Self> {
        if env.begin_reset() {
            Some(Self {
                env,
                completed: false,
            })
        } else {
            None
        }
    }

    /// Transfer lifecycle ownership to the blocking cleanup task.
    /// After this call, dropping the caller future does **not** roll back
    /// to `Running` — the blocking task is responsible for the final
    /// lifecycle transition (`finish_reset` on success, `taint` on error).
    /// Consumes the guard so `Drop` is a no-op.
    fn handoff(mut self) {
        self.completed = true;
    }
}

impl Drop for ResetLifecycleGuard {
    fn drop(&mut self) {
        if !self.completed {
            // Roll back to Running.  If shutdown stole the state
            // (Resetting -> ShuttingDown), finish_reset() returns false
            // and we do nothing — shutdown handles cleanup.
            self.env.finish_reset();
        }
    }
}

// ── TaintOnDrop ────────────────────────────────────────────────────────────

/// RAII guard used inside the blocking cleanup task to ensure the sandbox
/// transitions to `Tainted` if cleanup panics or returns an error.
///
/// On success, [`BwrapEnv::finish_reset`] transitions `Resetting → Running`
/// before this guard drops, so the `taint` CAS fails (state is no longer
/// `Resetting`) and the drop is a no-op.  On error,
/// [`BwrapEnv::taint`] transitions `Resetting → Tainted` before drop, so
/// the drop CAS also fails.  Only on **panic** (where neither method was
/// called) does the drop perform the `Resetting → Tainted` transition.
struct TaintOnDrop {
    env: Arc<BwrapEnv>,
}

impl Drop for TaintOnDrop {
    fn drop(&mut self) {
        self.env.taint();
    }
}

// ── AllowedRoot ────────────────────────────────────────────────────────────

/// A capability root: a directory opened as an ambient authority, plus its
/// canonical path (for routing path lookups to the correct `Dir`).
#[derive(Debug)]
struct AllowedRoot {
    path: PathBuf,
    dir: Dir,
}

// ── BwrapEnv ───────────────────────────────────────────────────────────────

/// `ExecutionEnv` that enforces filesystem isolation via `cap-std` capability
/// directories and runs shell commands inside a `bwrap --unshare-all` namespace.
///
/// File operations are routed to the `Dir` whose root contains the requested
/// path. `cap-std` prevents `..` traversal, absolute-path escape, and symlink
/// escape at the syscall level — no lexical-only check can be bypassed.
///
/// Shell commands run inside `bwrap --unshare-all`, which creates a fresh
/// mount/net/pid namespace per invocation.
#[derive(Debug)]
pub struct BwrapEnv {
    /// The configured working directory path (returned by `working_dir()`).
    work_dir: PathBuf,
    /// The canonicalized working directory (used for cap-std and bwrap).
    work_dir_canon: PathBuf,
    caps: Vec<AllowedRoot>,
    config: SandboxConfig,
    /// Whether the sandbox created the work_dir itself (`work_dir=None`).
    /// Only owned directories can be `reset()` or deleted by `shutdown()`.
    /// Caller-provided directories are never touched.
    owned: bool,
    /// Lifecycle state: `Running` admits new work; `ShuttingDown`/`Stopped`
    /// reject new work and trigger cancellation of active commands.
    lifecycle: std::sync::atomic::AtomicU8,
    /// Internal cancellation token fired when shutdown begins.  Active
    /// shell commands observe this via `drain_child_output`, which kills
    /// and waits the child when the token fires.
    ///
    /// Wrapped in a `std::sync::Mutex` so `reset()` can atomically swap in
    /// a fresh token and cancel the old one — interrupting active commands
    /// without permanently disabling the sandbox.  The mutex is only held
    /// briefly (clone or swap, no `.await`), so `std::sync::Mutex` is safe.
    shutdown_token: std::sync::Mutex<CancellationToken>,
    /// Mutex ensuring `reset()` / `shutdown()` are mutually exclusive with
    /// file operations and shell execution, so a running agent cannot
    /// recreate content during cleanup.
    /// Wrapped in `Arc` so `reset()` can obtain an `OwnedMutexGuard` via
    /// `lock_owned()` and move it into a `spawn_blocking` task, keeping the
    /// lock held even if the caller future is dropped.
    reset_lock: Arc<tokio::sync::Mutex<()>>,
    /// Test-only synchronization hook for `clear_work_dir_contents`.
    /// When set, the cleanup task signals `started`, waits on `gate`, and
    /// may simulate failure or panic based on `mode`.
    #[cfg(test)]
    cleanup_hook: std::sync::OnceLock<std::sync::Arc<CleanupHook>>,
}

impl BwrapEnv {
    /// Create a new `BwrapEnv` from a sandbox configuration.
    ///
    /// This is `pub(crate)` so external callers must go through
    /// [`BwrapSandbox::new`], which calls `validate_config` first.
    pub(crate) fn new(config: SandboxConfig) -> Result<Self> {
        let owned = config.work_dir.is_none();
        let work_dir = match &config.work_dir {
            Some(p) => p.clone(),
            None => {
                // Generate a unique directory to prevent cross-agent sharing
                // when work_dir is not explicitly configured.
                let id = uuid::Uuid::new_v4();
                PathBuf::from(format!("/tmp/sandbox-bwrap-{id}"))
            }
        };

        if owned {
            // Create with 0700 permissions, bypassing umask, so other Unix
            // users on the same host cannot read agent files.
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

        let mut caps = Vec::new();

        // Open the working directory as a capability root.
        let work_dir_cap = Dir::open_ambient_dir(&work_dir_canon, ambient_authority())
            .map_err(|e| anyhow::anyhow!("failed to open work_dir capability: {e}"))?;
        caps.push(AllowedRoot {
            path: work_dir_canon.clone(),
            dir: work_dir_cap,
        });

        // Open each fs_allowlist entry as an additional capability root.
        for entry in &config.fs_allowlist {
            let canon = std::fs::canonicalize(entry).map_err(|e| {
                anyhow::anyhow!(
                    "fs_allowlist entry {:?} cannot be canonicalized: {e}",
                    entry
                )
            })?;
            let cap = Dir::open_ambient_dir(&canon, ambient_authority()).map_err(|e| {
                anyhow::anyhow!("failed to open fs_allowlist capability {:?}: {e}", canon)
            })?;
            caps.push(AllowedRoot {
                path: canon,
                dir: cap,
            });
        }

        // Ownership is tracked in memory, not in the filesystem.  The agent
        // cannot tamper with an in-process bool, unlike a sentinel file that
        // lives inside the agent-writable workspace.

        Ok(Self {
            work_dir,
            work_dir_canon,
            caps,
            config,
            owned,
            lifecycle: std::sync::atomic::AtomicU8::new(LifecycleState::Running as u8),
            shutdown_token: std::sync::Mutex::new(CancellationToken::new()),
            reset_lock: Arc::new(tokio::sync::Mutex::new(())),
            #[cfg(test)]
            cleanup_hook: std::sync::OnceLock::new(),
        })
    }

    /// Working directory path (canonicalized).
    fn working_dir_path(&self) -> &Path {
        &self.work_dir
    }

    /// Returns whether the sandbox owns the work_dir (i.e. it was
    /// auto-generated because `work_dir=None`). Only owned directories
    /// can be `reset()` or deleted by `shutdown()`.
    pub(crate) fn owned(&self) -> bool {
        self.owned
    }

    /// Install a test-only cleanup hook for phase-controlled cancellation
    /// tests.  Must be called before `reset()`.
    #[cfg(test)]
    fn set_cleanup_hook(&self, hook: CleanupHook) {
        let _ = self.cleanup_hook.set(std::sync::Arc::new(hook));
    }

    /// Returns the current lifecycle state.
    fn lifecycle(&self) -> LifecycleState {
        LifecycleState::from_atomic(self.lifecycle.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// Transition to `ShuttingDown`. Returns `true` if this call performed
    /// the transition.  Accepts both `Running` and `Resetting` as valid
    /// starting states so shutdown can interrupt an in-progress reset.
    fn begin_shutdown(&self) -> bool {
        // Try Running -> ShuttingDown first (common case).
        if self
            .lifecycle
            .compare_exchange(
                LifecycleState::Running as u8,
                LifecycleState::ShuttingDown as u8,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
        {
            return true;
        }
        // Try Tainted -> ShuttingDown (shutdown of a tainted sandbox).
        if self
            .lifecycle
            .compare_exchange(
                LifecycleState::Tainted as u8,
                LifecycleState::ShuttingDown as u8,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
        {
            return true;
        }
        // Try Resetting -> ShuttingDown (shutdown during reset).
        self.lifecycle
            .compare_exchange(
                LifecycleState::Resetting as u8,
                LifecycleState::ShuttingDown as u8,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Transition `Running -> Resetting`. Returns `true` if this call
    /// performed the transition.  Fails if the sandbox is already
    /// resetting, shutting down, or stopped.
    fn begin_reset(&self) -> bool {
        self.lifecycle
            .compare_exchange(
                LifecycleState::Running as u8,
                LifecycleState::Resetting as u8,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Transition `Resetting -> Running`. Returns `true` on success.
    /// Fails if shutdown stole the state (Resetting -> ShuttingDown) while
    /// reset was in progress — in that case, reset returns Ok and lets
    /// shutdown handle cleanup.
    fn finish_reset(&self) -> bool {
        self.lifecycle
            .compare_exchange(
                LifecycleState::Resetting as u8,
                LifecycleState::Running as u8,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Transition `Resetting -> Tainted`.  Called when reset cleanup fails
    /// or panics — the workspace may be partially mutated and cannot be
    /// trusted, so the sandbox becomes permanently unusable.  Returns
    /// `true` if this call performed the transition.  No-op if the state
    /// has already moved away from `Resetting` (e.g. shutdown stole it).
    fn taint(&self) -> bool {
        self.lifecycle
            .compare_exchange(
                LifecycleState::Resetting as u8,
                LifecycleState::Tainted as u8,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Transition to `Stopped`.
    fn finish_shutdown(&self) {
        self.lifecycle.store(
            LifecycleState::Stopped as u8,
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    /// Clone the current shutdown token.  Callers should clone once and use
    /// the clone for the duration of their operation so they observe
    /// cancellation even if `reset()` later swaps in a new token.
    fn shutdown_token_clone(&self) -> CancellationToken {
        self.shutdown_token
            .lock()
            .expect("shutdown_token mutex poisoned")
            .clone()
    }

    /// Cancel the current shutdown token (used by `shutdown()`).
    fn cancel_shutdown_token(&self) {
        self.shutdown_token
            .lock()
            .expect("shutdown_token mutex poisoned")
            .cancel();
    }

    /// Atomically replace the shutdown token with `new_token` and return the
    /// old one.  Used by `reset()` to interrupt active commands (by
    /// cancelling the old token) while installing a fresh token so the
    /// sandbox remains usable afterwards.
    fn swap_shutdown_token(&self, new_token: CancellationToken) -> CancellationToken {
        let mut guard = self
            .shutdown_token
            .lock()
            .expect("shutdown_token mutex poisoned");
        std::mem::replace(&mut *guard, new_token)
    }

    /// Acquire `reset_lock` with cancellation and lifecycle awareness.
    ///
    /// Returns both the lock guard and the shutdown token that was current
    /// at the time of acquisition.  The caller must use **this same token**
    /// for the duration of the operation (not re-clone from the field) so
    /// that a `reset()` token-swap racing between lock acquisition and
    /// token clone does not let the operation escape cancellation.
    ///
    /// - Rejects immediately if the sandbox is shutting down or stopped.
    /// - Races the lock acquisition against the caller's `abort` token and
    ///   the sandbox's `shutdown_token`, so a queued operation does not
    ///   block indefinitely behind a long-running command or shutdown.
    /// - Rechecks cancellation/lifecycle *after* acquiring the lock, closing
    ///   the TOCTOU window between queueing and admission.
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
        // Post-acquisition recheck: cancellation may have fired while queued.
        if abort.is_cancelled() {
            return Err(EnvError::Aborted);
        }
        if self.lifecycle() != LifecycleState::Running {
            return Err(EnvError::Other("sandbox is shutting down".into()));
        }
        Ok((guard, shutdown_token))
    }

    /// Clear all contents of the work_dir using the cap-std `Dir` handle.
    ///
    /// Unlike `remove_dir_all` + `create_dir_all`, this keeps the `Dir`
    /// capability handle valid (the directory inode is not destroyed), so
    /// subsequent file operations continue to work after `reset()`.
    ///
    /// Symlinks are removed with `remove_file` (never followed), preventing
    /// accidental deletion of targets outside the sandbox.
    fn clear_work_dir_contents(&self) -> std::io::Result<()> {
        // Test hook: signal that cleanup has started, then wait for the
        // test to allow proceed.  This lets phase-controlled tests pause
        // cleanup at entry, drop the reset future, and verify that
        // admission is NOT reopened while the blocking task is still
        // running.
        #[cfg(test)]
        if let Some(hook) = self.cleanup_hook.get() {
            let _ = hook.started.send(());
            let _ = hook.gate.lock().unwrap().recv();
            match hook.mode {
                CleanupMode::Normal => {}
                CleanupMode::Fail | CleanupMode::FailPartial => {
                    // FailPartial proceeds past this point and fails after
                    // the first deletion (handled inside the loop below).
                    if matches!(hook.mode, CleanupMode::Fail) {
                        return Err(std::io::Error::other("injected cleanup failure"));
                    }
                }
                CleanupMode::Panic => panic!("injected cleanup panic"),
            }
        }
        let work_cap = &self.caps[0].dir;
        for entry in work_cap.entries()? {
            let entry = entry?;
            let name = entry.file_name();
            let meta = work_cap.symlink_metadata(&name)?;
            if meta.is_dir() {
                work_cap.remove_dir_all(&name)?;
            } else {
                work_cap.remove_file(&name)?;
            }
            #[cfg(test)]
            if let Some(hook) = self.cleanup_hook.get()
                && matches!(hook.mode, CleanupMode::FailPartial)
            {
                return Err(std::io::Error::other("injected partial cleanup failure"));
            }
        }
        Ok(())
    }

    /// Resolve `path` to a `(Dir, relative_path, full_path)` triple.
    ///
    /// 1. Make the path absolute (join with work_dir if relative).
    /// 2. Lexically normalize (handle `.` and `..`).
    /// 3. Find which capability root contains the path.
    /// 4. Return a cloned `Dir` handle and the relative path within it.
    ///
    /// The lexical check is only for *routing* — the actual security is
    /// enforced by `cap-std` when the `Dir` opens the relative path.
    fn resolve_to_cap(&self, path: &Path) -> Result<(Dir, PathBuf, PathBuf), EnvError> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.work_dir_canon.join(path)
        };
        let normalized = lexical_normalize(&resolved);

        for root in &self.caps {
            let norm_root = lexical_normalize(&root.path);
            if normalized == norm_root {
                let dir = root.dir.try_clone().map_err(EnvError::Io)?;
                return Ok((dir, PathBuf::from("."), normalized));
            }
            if normalized.starts_with(&norm_root) {
                let rel = normalized.strip_prefix(&norm_root).unwrap();
                let dir = root.dir.try_clone().map_err(EnvError::Io)?;
                return Ok((dir, rel.to_path_buf(), normalized));
            }
        }

        Err(EnvError::PermissionDenied(normalized))
    }

    /// System directories bind-mounted read-only so that commands inside the
    /// sandbox can find executables and shared libraries.
    ///
    /// `/etc` is intentionally **not** bound: it contains host credentials
    /// (`/etc/passwd`, `/etc/shadow`, API keys in `/etc/environment`, etc.).
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

        // Read-only system directories for executables and libraries.
        for (src, dest) in BwrapEnv::system_ro_binds() {
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
        let work_dir = self.work_dir_canon.to_string_lossy().to_string();
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
        &self.work_dir
    }

    fn read_text_file<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<String, EnvError>> {
        let resolved = self.resolve_to_cap(path);
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let (dir, rel, full) = resolved?;
            let full = full.clone();
            tokio::task::spawn_blocking(move || {
                dir.read_to_string(&rel).map_err(|e| map_read_err(e, full))
            })
            .await
            .map_err(|e| EnvError::Other(format!("spawn_blocking: {e}")))?
        }
        .boxed()
    }

    fn read_text_lines<'a>(
        &'a self,
        path: &'a Path,
        max_lines: Option<usize>,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<String>, EnvError>> {
        let resolved = self.resolve_to_cap(path);
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let (dir, rel, full) = resolved?;
            let full = full.clone();
            let text = tokio::task::spawn_blocking(move || {
                dir.read_to_string(&rel).map_err(|e| map_read_err(e, full))
            })
            .await
            .map_err(|e| EnvError::Other(format!("spawn_blocking: {e}")))??;
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
        let resolved = self.resolve_to_cap(path);
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let (dir, rel, full) = resolved?;
            let full = full.clone();
            tokio::task::spawn_blocking(move || {
                use std::io::Read;
                let mut file = dir.open(&rel).map_err(|e| map_io_err(e, full.clone()))?;
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)
                    .map_err(|e| map_io_err(e, full))?;
                Ok(buf)
            })
            .await
            .map_err(|e| EnvError::Other(format!("spawn_blocking: {e}")))?
        }
        .boxed()
    }

    fn write_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let resolved = self.resolve_to_cap(path);
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let (dir, rel, full) = resolved?;
            let full = full.clone();
            let content = content.to_vec();
            tokio::task::spawn_blocking(move || {
                // Create parent directories if needed.
                if let Some(parent) = rel.parent()
                    && !parent.as_os_str().is_empty()
                    && parent != Path::new(".")
                {
                    dir.create_dir_all(parent)
                        .map_err(|e| map_io_err(e, full.clone()))?;
                }
                dir.write(&rel, &content).map_err(|e| map_io_err(e, full))
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
        let resolved = self.resolve_to_cap(path);
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let (dir, rel, full) = resolved?;
            let full = full.clone();
            let content = content.to_vec();
            tokio::task::spawn_blocking(move || {
                use std::io::Write;
                let mut file = dir
                    .open_with(&rel, OpenOptions::new().create(true).append(true))
                    .map_err(|e| map_io_err(e, full.clone()))?;
                file.write_all(&content).map_err(|e| map_io_err(e, full))
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
        let resolved = self.resolve_to_cap(path);
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let (dir, rel, full) = resolved?;
            let full = full.clone();
            tokio::task::spawn_blocking(move || {
                let meta = if rel == Path::new(".") {
                    dir.dir_metadata()
                        .map_err(|e| map_io_err(e, full.clone()))?
                } else {
                    dir.metadata(&rel)
                        .map_err(|e| map_io_err(e, full.clone()))?
                };
                Ok(FileInfo {
                    path: full,
                    is_dir: meta.is_dir(),
                    size: meta.len(),
                    modified: modified_to_datetime(&meta),
                })
            })
            .await
            .map_err(|e| EnvError::Other(format!("spawn_blocking: {e}")))?
        }
        .boxed()
    }

    fn list_dir<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<FileInfo>, EnvError>> {
        let resolved = self.resolve_to_cap(path);
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let (dir, rel, full) = resolved?;
            let full = full.clone();
            tokio::task::spawn_blocking(move || {
                let entries = if rel == Path::new(".") {
                    dir.entries().map_err(|e| map_io_err(e, full.clone()))?
                } else {
                    dir.open_dir(&rel)
                        .map_err(|e| map_io_err(e, full.clone()))?
                        .entries()
                        .map_err(|e| map_io_err(e, full.clone()))?
                };
                let mut results = Vec::new();
                for entry in entries {
                    let entry = entry.map_err(|e| map_io_err(e, full.clone()))?;
                    let entry_path = full.join(entry.file_name());
                    let meta = entry
                        .metadata()
                        .map_err(|e| map_io_err(e, entry_path.clone()))?;
                    results.push(FileInfo {
                        path: entry_path,
                        is_dir: meta.is_dir(),
                        size: meta.len(),
                        modified: modified_to_datetime(&meta),
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
        let resolved = self.resolve_to_cap(path);
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let (dir, rel, _full) = resolved?;
            tokio::task::spawn_blocking(move || Ok(dir.exists(&rel)))
                .await
                .map_err(|e| EnvError::Other(format!("spawn_blocking: {e}")))?
        }
        .boxed()
    }

    fn create_dir<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let resolved = self.resolve_to_cap(path);
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let (dir, rel, full) = resolved?;
            let full = full.clone();
            tokio::task::spawn_blocking(move || {
                if recursive {
                    dir.create_dir_all(&rel).map_err(|e| map_io_err(e, full))
                } else {
                    dir.create_dir(&rel).map_err(|e| match e.kind() {
                        std::io::ErrorKind::AlreadyExists => EnvError::AlreadyExists(full),
                        _ => map_io_err(e, full),
                    })
                }
            })
            .await
            .map_err(|e| EnvError::Other(format!("spawn_blocking: {e}")))?
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
        let resolved = self.resolve_to_cap(path);
        async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let (dir, rel, full) = resolved?;
            let full = full.clone();
            tokio::task::spawn_blocking(move || {
                let meta = match dir.symlink_metadata(&rel) {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        if force {
                            return Ok(());
                        }
                        return Err(EnvError::NotFound(full));
                    }
                    Err(e) => return Err(map_io_err(e, full)),
                };

                if meta.is_dir() {
                    if recursive {
                        dir.remove_dir_all(&rel).map_err(|e| map_io_err(e, full))?;
                    } else {
                        dir.remove_dir(&rel).map_err(|e| map_io_err(e, full))?;
                    }
                } else {
                    dir.remove_file(&rel).map_err(|e| map_io_err(e, full))?;
                }
                Ok(())
            })
            .await
            .map_err(|e| EnvError::Other(format!("spawn_blocking: {e}")))?
        }
        .boxed()
    }

    fn create_temp_dir<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<PathBuf, EnvError>> {
        let dir_result = self.caps[0].dir.try_clone();
        let work_dir = self.work_dir_canon.clone();
        Box::pin(async move {
            let abort = CancellationToken::new();
            let (_guard, _token) = self.acquire_op_lock(&abort).await?;
            let dir = dir_result.map_err(EnvError::Io)?;
            let prefix = prefix.to_string();
            tokio::task::spawn_blocking(move || {
                let name = format!("{}_{}", prefix, uuid::Uuid::new_v4());
                dir.create_dir(&name).map_err(EnvError::Io)?;
                Ok(work_dir.join(&name))
            })
            .await
            .map_err(|e| EnvError::Other(format!("spawn_blocking: {e}")))?
        })
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
            // acquire_op_lock returns the shutdown token that was current at
            // the time of lock acquisition.  We use THIS token (not a fresh
            // clone) for the entire operation, which linearizes the epoch:
            // if reset() swaps the token after we acquire the lock, our token
            // is the *old* one that reset() cancels, so we still observe the
            // cancellation.  A second clone from the field would race with
            // the swap and could pick up the new (uncancelled) token.
            let (_guard, shutdown_token) = self.acquire_op_lock(&opts.abort).await?;

            // Enforce config-level timeout as an upper bound. If both the
            // caller-specified timeout and the config timeout are set, use
            // the minimum. If only the config timeout is set, apply it.
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
            let bwrap_args = self.build_bwrap_args(&cwd);

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

            // Re-check lifecycle: shutdown may have begun between
            // acquire_op_lock and here.  If so, refuse to spawn.
            if self.lifecycle() != LifecycleState::Running || shutdown_token.is_cancelled() {
                return Err(EnvError::Other("sandbox is shutting down".into()));
            }

            // Acquire an admission permit BEFORE spawning.  This bounds the
            // total number of in-flight (unreaped) children to
            // REAPER_QUEUE_CAPACITY, preventing unbounded reaper resource
            // growth.  If the reaper is unavailable or at capacity, refuse
            // to spawn (fail-closed).
            let reaper = llm_harness_runtime_sandbox_os::ChildReaper::global()
                .ok_or_else(|| EnvError::Other("child reaper unavailable".into()))?;
            let permit = reaper
                .try_admit()
                .ok_or_else(|| EnvError::Other("sandbox at capacity or reaper unhealthy".into()))?;

            let child = match shell_cmd
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(child) => child,
                Err(e) => {
                    // permit is dropped here (released) when we return Err.
                    return Err(if e.kind() == std::io::ErrorKind::NotFound {
                        EnvError::Other("bwrap not available — install bubblewrap".into())
                    } else {
                        EnvError::Io(e)
                    });
                }
            };

            // drain_child_output_guarded handles all cancellation paths
            // (abort, timeout, shutdown) with kill+wait, and wraps the child
            // in a ReapGuard that guarantees reap even if this future is
            // dropped before completion (via a background tokio::spawn task).
            // If the token was cancelled between the check above and this
            // call, the shutdown branch fires immediately and the
            // just-spawned child is killed+reaped.  The permit moves into
            // the ReapGuard and is released when the child is confirmed
            // exited.
            drain_child_output_guarded(child, opts, shutdown_token, Some(permit)).await
        }
        .boxed()
    }

    fn cleanup<'a>(&'a self) -> BoxFuture<'a, Result<(), EnvError>> {
        async move { Ok(()) }.boxed()
    }
}

// ── BwrapSandbox ───────────────────────────────────────────────────────────

/// Sandbox backed by bubblewrap(1) for OS-level permission isolation.
///
/// `env()` returns a [`BwrapEnv`] that:
/// - Enforces file operations through `cap-std` capability directories,
///   preventing `..` traversal, absolute-path escape, and symlink escape.
/// - Runs `execute_shell` inside `bwrap --unshare-all` (fresh mount/net/pid
///   namespace per command).
/// - Clears the environment via `env -i`; only `PATH`, `HOME`, and
///   caller-specified variables are injected.
/// - Enforces `resource_limits.timeout` as an upper bound on all shell
///   command timeouts.
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
        let env = Arc::new(BwrapEnv::new(config.clone())?);
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

    /// Run a real `bwrap --unshare-all ... true` invocation using `bwrap_bin`
    /// to verify that user namespaces are available and the binary can exec a
    /// process inside the namespace.
    ///
    /// This is more thorough than `bwrap --version`: it detects missing
    /// user-namespace support, kernel restrictions, and permission issues.
    /// The environment is cleared and a minimal `PATH` is set so the probe
    /// does not inherit host credentials.
    async fn run_bwrap_probe(bwrap_bin: &str) -> Result<()> {
        let mut cmd = tokio::process::Command::new(bwrap_bin);
        cmd.env_clear();
        cmd.env("PATH", "/usr/bin:/bin");
        cmd.args(["--unshare-all", "--die-with-parent", "--new-session"]);

        // Use the same system read-only binds as `build_bwrap_args` so the
        // probe mirrors a real sandbox invocation (includes /lib64, /sbin).
        for (src, dest) in BwrapEnv::system_ro_binds() {
            cmd.args(["--ro-bind-try", src, dest]);
        }

        cmd.args(["--proc", "/proc"])
            .args(["--dev", "/dev"])
            .args(["--tmpfs", "/tmp"])
            .arg("--")
            .arg("/usr/bin/env")
            .arg("-i")
            .arg("PATH=/usr/bin:/bin")
            .arg("true")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let output = cmd.output().await;

        match output {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                Err(anyhow::anyhow!(
                    "bwrap sandbox invocation failed (exit {:?}); \
                     check kernel user-namespace support. stderr: {}",
                    o.status.code(),
                    stderr.trim()
                ))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(anyhow::anyhow!("bwrap not available — install bubblewrap"))
            }
            Err(e) => Err(anyhow::anyhow!("failed to run bwrap: {e}")),
        }
    }

    /// Check whether the default `bwrap` is installed and can invoke a sandbox.
    async fn check_bwrap_available() -> Result<()> {
        Self::run_bwrap_probe("bwrap").await
    }
}

impl Sandbox for BwrapSandbox {
    fn start(&self) -> BoxFuture<'_, Result<()>> {
        async move {
            Self::check_bwrap_available().await?;
            // Reject start if the sandbox is not in a startable state.
            // Tainted sandboxes (reset cleanup failed) cannot be restarted —
            // they must be shut down and recreated.  Stopped/ShuttingDown
            // sandboxes are likewise not restartable.
            match self.env.lifecycle() {
                LifecycleState::Running => {
                    self.running.store(true, Ordering::SeqCst);
                    Ok(())
                }
                LifecycleState::Tainted => Err(anyhow::anyhow!(
                    "cannot start: sandbox is tainted (reset cleanup failed)"
                )),
                other => Err(anyhow::anyhow!(
                    "cannot start: sandbox is in {other:?} state"
                )),
            }
        }
        .boxed()
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        self.env.clone()
    }

    fn reset(&self) -> BoxFuture<'_, Result<()>> {
        async move {
            // Atomically close admission: Running → Resetting.
            //
            // The RAII guard ensures we roll back to Running if the future
            // is cancelled (dropped) or errors out **before mutation
            // starts** — i.e. during token swap or lock acquisition.  Once
            // the blocking cleanup task is spawned, lifecycle ownership is
            // handed off to that task (see guard.handoff() below), so
            // dropping the caller future does NOT roll back to Running
            // while cleanup is still in progress.
            let guard = match ResetLifecycleGuard::begin(self.env.clone()) {
                Some(g) => g,
                None => anyhow::bail!("sandbox is not running; cannot reset"),
            };

            // Only auto-generated directories (work_dir=None) can be reset.
            // Caller-provided directories are never touched — the sandbox
            // does not own them and must not claim ownership by deleting
            // their contents.  The guard rolls back to Running on drop.
            if !self.env.owned() {
                anyhow::bail!(
                    "refusing to reset work_dir {:?}: sandbox does not own this directory (work_dir was explicitly provided; only auto-generated directories with work_dir=None can be reset)",
                    self.env.working_dir_path()
                );
            }

            // Interrupt active shell commands by swapping in a fresh token
            // and cancelling the old one.  drain_child_output_guarded
            // observes the old token's cancellation, kills+waits the child
            // via ReapGuard, and returns Err — which releases the op-lock
            // so reset can proceed.
            //
            // The lifecycle is Resetting (not Running), so new operations
            // are rejected by acquire_op_lock() during this window.
            let old_token = self.env.swap_shutdown_token(CancellationToken::new());
            old_token.cancel();

            // Acquire an owned lock guard with a timeout.  Active commands
            // should exit promptly (killed by the token cancellation
            // above), but the timeout guarantees boundedness if a file
            // operation is in progress (file ops don't observe the token
            // mid-execution).  On timeout, the guard rolls back to Running
            // via Drop — safe because no mutation has started.
            //
            // OwnedMutexGuard is static and can be moved into the
            // spawn_blocking closure, keeping the lock held for the entire
            // duration of cleanup even if the caller future is dropped.
            let owned_guard = tokio::time::timeout(
                SHUTDOWN_KILL_GRACE,
                self.env.reset_lock.clone().lock_owned(),
            )
            .await
           .map_err(|_| anyhow::anyhow!("reset timed out waiting for lock"))?;
            // (guard rolls back to Running via Drop on timeout error)

            // Transfer lifecycle ownership to the blocking task.  After
            // this call, dropping the caller future does NOT roll back to
            // Running — the blocking task is responsible for the final
            // transition (finish_reset on success, taint on error or
            // panic).  This closes the P0 race where cancellation reopens
            // admission while cleanup is still deleting files.
            guard.handoff();

            let env = self.env.clone();
            let handle = tokio::task::spawn_blocking(
                move || -> std::io::Result<()> {
                    // TaintOnDrop ensures Resetting->Tainted if cleanup
                    // panics (neither finish_reset nor taint was called).
                    // On success, finish_reset() runs first so state is
                    // Running and this drop's taint() CAS fails (no-op).
                    // On error, taint() runs first so state is Tainted and
                    // this drop's CAS also fails (no-op).
                    let _taint = TaintOnDrop { env: env.clone() };
                    // Hold the owned lock guard for the entire cleanup so
                    // no new command can race into a partially-cleared
                    // workspace.  Released when the closure returns or
                    // panics (local variable drop).
                    let _lock = owned_guard;
                    let result = env.clear_work_dir_contents();
                    match &result {
                        Ok(()) => {
                            env.finish_reset();
                        }
                        Err(_) => {
                            env.taint();
                        }
                    }
                    result
                },
            );

            // Await the blocking task.  If the caller drops this future,
            // the JoinHandle is dropped but the task continues — it still
            // holds the owned lock and the TaintOnDrop guard, so cleanup
            // completes and the lifecycle transitions correctly.
            match handle.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(anyhow::anyhow!("reset failed: {e}")),
                Err(join_err) => {
                    // Task panicked or was cancelled (runtime shutdown).
                    // TaintOnDrop may have already run (panic path) or
                    // not (cancellation path).  Ensure Tainted in either
                    // case — taint() is a CAS from Resetting, so it is a
                    // no-op if already transitioned.
                    self.env.taint();
                    Err(anyhow::anyhow!("reset task failed: {join_err}"))
                }
            }
        }
        .boxed()
    }

    fn shutdown(self: Box<Self>) -> BoxFuture<'static, Result<()>> {
        let sandbox = *self;
        async move {
            // Transition to ShuttingDown.  This causes new operations to
            // be rejected via acquire_op_lock.
            let was_running = sandbox.env.begin_shutdown();
            if !was_running {
                // Already shutting down or stopped.
                sandbox.env.finish_shutdown();
                sandbox.running.store(false, Ordering::SeqCst);
                return Ok(());
            }

            // Cancel active commands via the internal token.
            // drain_child_output observes the cancellation, kills+waits the
            // child, and returns Err — which releases the op-lock.
            sandbox.env.cancel_shutdown_token();

            // Acquire the lock with a timeout so an unbounded command
            // cannot block shutdown forever.  Active commands should exit
            // promptly (killed by the token cancellation above).
            match tokio::time::timeout(SHUTDOWN_KILL_GRACE, sandbox.env.reset_lock.lock()).await {
                Ok(guard) => {
                    // Lock acquired — safe to delete the workspace while
                    // holding the guard so no new operation can race in.
                    let _guard = guard;
                    if sandbox.env.owned() {
                        let work_dir = sandbox.env.work_dir_canon.clone();
                        if work_dir.exists() {
                            tokio::fs::remove_dir_all(&work_dir).await?;
                        }
                    }
                }
                Err(_) => {
                    // Lock acquisition timed out — an operation is still
                    // holding the lock and may be writing to the workspace.
                    // Do NOT delete the workspace; preserve it to avoid
                    // corrupting in-flight work.
                    sandbox.env.finish_shutdown();
                    sandbox.running.store(false, Ordering::SeqCst);
                    return Err(anyhow::anyhow!(
                        "shutdown timed out waiting for lock; workspace preserved"
                    ));
                }
            }

            sandbox.env.finish_shutdown();
            sandbox.running.store(false, Ordering::SeqCst);
            Ok(())
        }
        .boxed()
    }

    fn config(&self) -> &SandboxConfig {
        &self.config
    }

    fn is_running(&self) -> bool {
        // Unify the dual state sources: the sandbox is "running" only when
        // start() has been called AND the lifecycle is Running.  A Tainted
        // sandbox (or one that is shutting down / stopped) reports false.
        self.running.load(Ordering::SeqCst) && self.env.lifecycle() == LifecycleState::Running
    }
}

// ── Test hooks ─────────────────────────────────────────────────────────────

/// Synchronization hook for phase-controlled reset cancellation tests.
///
/// When installed on a `BwrapEnv`, `clear_work_dir_contents` signals
/// `started`, blocks on `gate`, then either proceeds normally, returns an
/// error, or panics, depending on `mode`.
#[cfg(test)]
#[derive(Debug)]
struct CleanupHook {
    started: std::sync::mpsc::Sender<()>,
    gate: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    mode: CleanupMode,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
enum CleanupMode {
    Normal,
    Fail,
    Panic,
    /// Fail after the first successful deletion, simulating a partial
    /// cleanup error (some files deleted, some remaining).
    FailPartial,
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
