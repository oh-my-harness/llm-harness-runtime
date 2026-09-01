#![cfg(test)]

//! In-memory `ExecutionEnv` implementation for unit-testing the bash/read/
//! write/edit tools without touching the real filesystem.
//!
//! `MockExecutionEnv` holds a fake filesystem (`HashMap<PathBuf, Vec<u8>>`
//! for files, `Vec<PathBuf>` for directories) protected by a
//! `parking_lot::Mutex`. All trait methods normalize the path against the
//! env working dir (`/tmp/mock`) and perform the operation synchronously
//! inside the pinned future — no lock guard is ever held across an `.await`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use futures::future::BoxFuture;
use llm_harness_types::{EnvError, ExecutionEnv, FileInfo, ShellOptions, ShellOutput};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Inner mutable state of [`MockExecutionEnv`], guarded by its `Mutex`.
struct Inner {
    /// File contents keyed by normalized absolute path.
    files: HashMap<PathBuf, Vec<u8>>,
    /// Directories keyed by normalized absolute path.
    dirs: Vec<PathBuf>,
    /// Last `ShellOptions` seen by `execute_shell` (for test assertions).
    last_shell_opts: Option<RecordedShellOpts>,
    /// When set, `create_temp_dir` returns `EnvError::Other` (degradation test).
    fail_temp_dir: bool,
    /// When set, `write_file` returns `EnvError::Other` (degradation test).
    fail_write: bool,
}

/// A snapshot of the `ShellOptions` passed to `execute_shell`, for tests that
/// verify cwd/env/timeout propagation without running a real shell.
#[derive(Debug, Clone, Default)]
pub struct RecordedShellOpts {
    /// Working directory the command was asked to run in (resolved absolute).
    pub cwd: Option<PathBuf>,
    /// Extra environment variables supplied to the command.
    pub env: Vec<(String, String)>,
    /// Timeout in seconds, if any.
    pub timeout_secs: Option<u64>,
}

/// In-memory `ExecutionEnv` for unit tests.
///
/// Provides a fake filesystem (files + directories) and a fake shell, so the
/// bash/read/write/edit tools can be exercised without real I/O. The working
/// directory is fixed to `/tmp/mock`; relative paths are resolved against it.
pub struct MockExecutionEnv {
    working_dir: PathBuf,
    inner: Mutex<Inner>,
}

impl MockExecutionEnv {
    /// Create an empty mock env with working dir `/tmp/mock`.
    pub fn new() -> Self {
        Self {
            working_dir: PathBuf::from("/tmp/mock"),
            inner: Mutex::new(Inner {
                files: HashMap::new(),
                dirs: vec![PathBuf::from("/tmp/mock")],
                last_shell_opts: None,
                fail_temp_dir: false,
                fail_write: false,
            }),
        }
    }

    /// Inject a failure into `create_temp_dir` (returns `EnvError::Other`).
    pub fn set_fail_temp_dir(&self, fail: bool) {
        self.inner.lock().fail_temp_dir = fail;
    }

    /// Inject a failure into `write_file` (returns `EnvError::Other`).
    pub fn set_fail_write(&self, fail: bool) {
        self.inner.lock().fail_write = fail;
    }

    /// Seed a file synchronously (test-setup helper).
    pub fn write_file_sync(&self, path: &Path, content: &str) {
        let normalized = self.normalize(path);
        self.inner
            .lock()
            .files
            .insert(normalized, content.as_bytes().to_vec());
    }

    /// Seed raw binary bytes synchronously (test-setup helper).
    pub fn write_binary_sync(&self, path: &Path, content: &[u8]) {
        let normalized = self.normalize(path);
        self.inner.lock().files.insert(normalized, content.to_vec());
    }

    /// Seed a directory synchronously (test-setup helper).
    pub fn mkdir_sync(&self, path: &Path) {
        let normalized = self.normalize(path);
        let mut guard = self.inner.lock();
        if !guard.dirs.contains(&normalized) {
            guard.dirs.push(normalized);
        }
    }

    /// Return a snapshot of the last `ShellOptions` passed to `execute_shell`,
    /// or `None` if `execute_shell` was never called. Useful for asserting
    /// that `BashTool` propagates `cwd` / `env` / `timeout` correctly.
    pub fn last_shell_opts(&self) -> Option<RecordedShellOpts> {
        self.inner.lock().last_shell_opts.clone()
    }

    /// Resolve a relative path against the working dir; absolute paths pass through.
    /// Normalizes `.` components so that `working_dir/.` matches `working_dir`.
    fn normalize(&self, path: &Path) -> PathBuf {
        let raw = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.working_dir.join(path)
        };
        // Strip "." components — the mock filesystem stores canonical paths.
        let mut cleaned = PathBuf::new();
        for component in raw.components() {
            if component == std::path::Component::CurDir {
                continue;
            }
            cleaned.push(component);
        }
        if cleaned.as_os_str().is_empty() {
            self.working_dir.clone()
        } else {
            cleaned
        }
    }
}

impl Default for MockExecutionEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecutionEnv for MockExecutionEnv {
    fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    fn read_text_file<'a>(
        &'a self,
        path: &'a Path,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<String, EnvError>> {
        let normalized = self.normalize(path);
        Box::pin(async move {
            let guard = self.inner.lock();
            match guard.files.get(&normalized) {
                Some(bytes) => match String::from_utf8(bytes.clone()) {
                    Ok(s) => Ok(s),
                    Err(_) => Err(EnvError::InvalidUtf8(normalized)),
                },
                None => {
                    if guard.dirs.contains(&normalized) {
                        Err(EnvError::IsADirectory(normalized))
                    } else {
                        Err(EnvError::NotFound(normalized))
                    }
                }
            }
        })
    }

    fn read_text_lines<'a>(
        &'a self,
        path: &'a Path,
        max_lines: Option<usize>,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<String>, EnvError>> {
        let normalized = self.normalize(path);
        Box::pin(async move {
            let guard = self.inner.lock();
            let bytes = match guard.files.get(&normalized) {
                Some(b) => b.clone(),
                None => {
                    if guard.dirs.contains(&normalized) {
                        return Err(EnvError::IsADirectory(normalized));
                    }
                    return Err(EnvError::NotFound(normalized));
                }
            };
            drop(guard);
            let text = match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => return Err(EnvError::InvalidUtf8(normalized)),
            };
            let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
            if let Some(max) = max_lines {
                lines.truncate(max);
            }
            Ok(lines)
        })
    }

    fn read_binary_file<'a>(
        &'a self,
        path: &'a Path,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<u8>, EnvError>> {
        let normalized = self.normalize(path);
        Box::pin(async move {
            let guard = self.inner.lock();
            match guard.files.get(&normalized) {
                Some(bytes) => Ok(bytes.clone()),
                None => {
                    if guard.dirs.contains(&normalized) {
                        Err(EnvError::IsADirectory(normalized))
                    } else {
                        Err(EnvError::NotFound(normalized))
                    }
                }
            }
        })
    }

    fn write_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let normalized = self.normalize(path);
        let content = content.to_vec();
        Box::pin(async move {
            let mut guard = self.inner.lock();
            if guard.fail_write {
                return Err(EnvError::Other("injected failure".into()));
            }
            guard.files.insert(normalized, content);
            Ok(())
        })
    }

    fn append_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let normalized = self.normalize(path);
        let content = content.to_vec();
        Box::pin(async move {
            let mut guard = self.inner.lock();
            let entry = guard.files.entry(normalized).or_default();
            entry.extend_from_slice(&content);
            Ok(())
        })
    }

    fn file_info<'a>(
        &'a self,
        path: &'a Path,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<FileInfo, EnvError>> {
        let normalized = self.normalize(path);
        Box::pin(async move {
            let guard = self.inner.lock();
            if guard.dirs.contains(&normalized) {
                return Ok(FileInfo {
                    path: normalized,
                    is_dir: true,
                    size: 0,
                    modified: Utc::now(),
                });
            }
            if let Some(bytes) = guard.files.get(&normalized) {
                return Ok(FileInfo {
                    path: normalized,
                    is_dir: false,
                    size: bytes.len() as u64,
                    modified: Utc::now(),
                });
            }
            Err(EnvError::NotFound(normalized))
        })
    }

    fn list_dir<'a>(
        &'a self,
        path: &'a Path,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<FileInfo>, EnvError>> {
        let normalized = self.normalize(path);
        Box::pin(async move {
            let guard = self.inner.lock();
            if !guard.dirs.contains(&normalized) {
                return Err(EnvError::NotADirectory(normalized));
            }
            let mut entries: Vec<FileInfo> = Vec::new();
            // Immediate-child directories.
            for dir in &guard.dirs {
                if immediate_child(&normalized, dir).is_some() {
                    entries.push(FileInfo {
                        path: dir.clone(),
                        is_dir: true,
                        size: 0,
                        modified: Utc::now(),
                    });
                }
            }
            // Immediate-child files.
            for (file_path, bytes) in &guard.files {
                if immediate_child(&normalized, file_path).is_some() {
                    entries.push(FileInfo {
                        path: file_path.clone(),
                        is_dir: false,
                        size: bytes.len() as u64,
                        modified: Utc::now(),
                    });
                }
            }
            Ok(entries)
        })
    }

    fn exists<'a>(
        &'a self,
        path: &'a Path,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<bool, EnvError>> {
        let normalized = self.normalize(path);
        Box::pin(async move {
            let guard = self.inner.lock();
            Ok(guard.files.contains_key(&normalized) || guard.dirs.contains(&normalized))
        })
    }

    fn create_dir<'a>(
        &'a self,
        path: &'a Path,
        _recursive: bool,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let normalized = self.normalize(path);
        Box::pin(async move {
            let mut guard = self.inner.lock();
            if !guard.dirs.contains(&normalized) {
                guard.dirs.push(normalized);
            }
            Ok(())
        })
    }

    fn remove<'a>(
        &'a self,
        path: &'a Path,
        _recursive: bool,
        _force: bool,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let normalized = self.normalize(path);
        Box::pin(async move {
            let mut guard = self.inner.lock();
            guard.files.remove(&normalized);
            guard.dirs.retain(|d| *d != normalized);
            Ok(())
        })
    }

    fn create_temp_dir<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<PathBuf, EnvError>> {
        let path = self
            .working_dir
            .join(format!("{}-{}", prefix, Uuid::new_v4()));
        Box::pin(async move {
            let mut guard = self.inner.lock();
            if guard.fail_temp_dir {
                return Err(EnvError::Other("injected failure".into()));
            }
            if !guard.dirs.contains(&path) {
                guard.dirs.push(path.clone());
            }
            Ok(path)
        })
    }

    fn execute_shell<'a>(
        &'a self,
        cmd: &'a str,
        opts: ShellOptions<'a>,
    ) -> BoxFuture<'a, Result<ShellOutput, EnvError>> {
        let cmd = cmd.to_string();
        let recorded = RecordedShellOpts {
            cwd: opts.cwd.map(|p| p.to_path_buf()),
            env: opts
                .env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            timeout_secs: opts.timeout.map(|d| d.as_secs()),
        };
        Box::pin(async move {
            {
                let mut guard = self.inner.lock();
                guard.last_shell_opts = Some(recorded);
            }
            // Fake shell: `echo <text>` echoes, `false` fails, `__seq:N`
            // generates N numbered lines (exit 0), `__seq_fail:N` generates
            // N numbered lines with exit_code=1, else succeeds.
            let trimmed = cmd.trim();
            if let Some(rest) = trimmed.strip_prefix("__seq_fail:") {
                let n: usize = rest.parse().unwrap_or(0);
                let stdout = (1..=n)
                    .map(|i| format!("line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ShellOutput {
                    stdout,
                    stderr: String::new(),
                    exit_code: 1,
                    stdout_truncated_bytes: 0,
                    stderr_truncated_bytes: 0,
                })
            } else if let Some(rest) = trimmed.strip_prefix("__seq:") {
                let n: usize = rest.parse().unwrap_or(0);
                let stdout = (1..=n)
                    .map(|i| format!("line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ShellOutput {
                    stdout,
                    stderr: String::new(),
                    exit_code: 0,
                    stdout_truncated_bytes: 0,
                    stderr_truncated_bytes: 0,
                })
            } else if let Some(rest) = trimmed.strip_prefix("echo ") {
                Ok(ShellOutput {
                    stdout: rest.to_string(),
                    stderr: String::new(),
                    exit_code: 0,
                    stdout_truncated_bytes: 0,
                    stderr_truncated_bytes: 0,
                })
            } else if trimmed == "echo" {
                Ok(ShellOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                    stdout_truncated_bytes: 0,
                    stderr_truncated_bytes: 0,
                })
            } else if trimmed == "false" {
                Err(EnvError::ShellFailed {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: String::new(),
                    stdout_truncated_bytes: 0,
                    stderr_truncated_bytes: 0,
                })
            } else if trimmed == "__timeout__" {
                // Simulate the drain-layer timeout contract: return the
                // partial output captured before the child was killed.
                Err(EnvError::ShellTimeout {
                    stdout: "partial output".to_string(),
                    stderr: "timeout".to_string(),
                    stdout_truncated_bytes: 0,
                    stderr_truncated_bytes: 0,
                })
            } else {
                Ok(ShellOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                    stdout_truncated_bytes: 0,
                    stderr_truncated_bytes: 0,
                })
            }
        })
    }

    fn cleanup<'a>(&'a self) -> BoxFuture<'a, Result<(), EnvError>> {
        Box::pin(async move {
            let mut guard = self.inner.lock();
            guard.files.clear();
            guard.dirs.clear();
            Ok(())
        })
    }
}

/// If `child` is an immediate child of `parent`, returns its file name; else `None`.
fn immediate_child(parent: &Path, child: &Path) -> Option<String> {
    child
        .parent()
        .filter(|p| *p == parent)
        .and_then(|_| child.file_name())
        .map(|n| n.to_string_lossy().into_owned())
}
