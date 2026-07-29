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
use llm_harness_runtime_sandbox_os::drain_child_output;
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
    work_dir: PathBuf,
    caps: Vec<AllowedRoot>,
    config: SandboxConfig,
}

impl BwrapEnv {
    /// Create a new `BwrapEnv` from a sandbox configuration.
    ///
    /// This is `pub(crate)` so external callers must go through
    /// [`BwrapSandbox::new`], which calls `validate_config` first.
    pub(crate) fn new(config: SandboxConfig) -> Result<Self> {
        let work_dir = config
            .work_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("/tmp/sandbox-bwrap"));
        std::fs::create_dir_all(&work_dir)?;

        let work_dir_canon = std::fs::canonicalize(&work_dir).unwrap_or_else(|_| work_dir.clone());

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

        Ok(Self {
            work_dir: work_dir_canon,
            caps,
            config,
        })
    }

    /// Working directory path (canonicalized).
    fn working_dir_path(&self) -> &Path {
        &self.work_dir
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
            self.work_dir.join(path)
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
        let work_dir = self.work_dir.to_string_lossy().to_string();
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
        let work_dir = self.work_dir.clone();
        Box::pin(async move {
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

            // Enforce config-level timeout as an upper bound. If both the
            // caller-specified timeout and the config timeout are set, use
            // the minimum. If only the config timeout is set, apply it.
            if let Some(config_timeout) = self.config.resource_limits.timeout {
                opts.timeout = Some(match opts.timeout {
                    Some(t) => t.min(config_timeout),
                    None => config_timeout,
                });
            }

            let cwd = opts.cwd.unwrap_or(&self.work_dir).to_path_buf();
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

    /// Returns true if `path` is safe to use as a disposable root for `reset()`.
    ///
    /// Checks:
    /// 1. Canonicalize the path (resolves `.`, `..`, symlinks).
    /// 2. Reject root `/` or paths with no parent.
    /// 3. Reject paths with fewer than 2 components (e.g. `/tmp`, `/home`).
    /// 4. Reject the process's current working directory.
    /// 5. Reject ancestors of the current working directory.
    fn is_safe_disposable_root(path: &Path) -> bool {
        // Canonicalize to resolve `.`, `..`, and symlinks.
        let canon = match std::fs::canonicalize(path) {
            Ok(c) => c,
            Err(_) => return false,
        };

        // Reject root.
        if canon == Path::new("/") || canon.parent().is_none() {
            return false;
        }

        // Reject shallow paths (e.g. `/tmp`, `/home`, `/var`).
        // Require at least 2 path components below root.
        let component_count = canon
            .components()
            .filter(|c| !matches!(c, std::path::Component::RootDir))
            .count();
        if component_count < 2 {
            return false;
        }

        // Reject the process's current working directory or any ancestor of it.
        if let Ok(cwd) = std::env::current_dir()
            && (canon == cwd || (cwd.starts_with(&canon) && cwd != canon))
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
            let work_dir = self.env.working_dir_path();
            if !Self::is_safe_disposable_root(work_dir) {
                anyhow::bail!(
                    "refusing to reset work_dir {:?}: not a safe disposable root \
                     (must be canonicalizable, have ≥2 path components, and not be \
                     root/cwd/ancestor-of-cwd)",
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
