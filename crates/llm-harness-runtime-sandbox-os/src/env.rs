use std::path::{Path, PathBuf};

use chrono::DateTime;
use futures::future::BoxFuture;
use futures::FutureExt;
use llm_harness_types::{EnvError, ExecutionEnv, FileInfo, ShellOptions, ShellOutput};
use tokio_util::sync::CancellationToken;

/// Concrete `ExecutionEnv` backed by the real OS filesystem and shell.
///
/// Reimplemented from `llm-harness-core` to bypass the `#[cfg(test)]` gate.
pub struct OsEnv {
    working_dir: PathBuf,
}

impl OsEnv {
    /// Create a new `OsEnv` rooted at `working_dir`.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
        }
    }

    fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.working_dir.join(path)
        }
    }
}

impl ExecutionEnv for OsEnv {
    fn working_dir(&self) -> &Path {
        &self.working_dir
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
            let full = self.resolve(path);
            tokio::fs::read_to_string(&full)
                .await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => EnvError::NotFound(full),
                    std::io::ErrorKind::PermissionDenied => EnvError::PermissionDenied(full),
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
            let text = self.read_text_file(path, abort.clone()).await?;
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
            let full = self.resolve(path);
            tokio::fs::read(&full)
                .await
                .map_err(|e| match e.kind() {
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
            let full = self.resolve(path);
            if let Some(parent) = full.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(EnvError::Io)?;
            }
            tokio::fs::write(&full, content)
                .await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::PermissionDenied => EnvError::PermissionDenied(full),
                    _ => EnvError::Io(e),
                })
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
            let full = self.resolve(path);
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&full)
                .await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::PermissionDenied => EnvError::PermissionDenied(full),
                    _ => EnvError::Io(e),
                })?;
            file.write_all(content).await.map_err(EnvError::Io)?;
            Ok(())
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
            let full = self.resolve(path);
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
                modified: DateTime::from_timestamp(modified, 0).unwrap_or_default(),
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
            let full = self.resolve(path);
            let mut entries = tokio::fs::read_dir(&full)
                .await
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => EnvError::NotFound(full),
                    _ => EnvError::Io(e),
                })?;
            let mut results = Vec::new();
            while let Ok(Some(entry)) = entries.next_entry().await {
                let entry_path = entry.path();
                let meta = entry.metadata().await.map_err(EnvError::Io)?;
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
                    modified: DateTime::from_timestamp(modified, 0).unwrap_or_default(),
                });
            }
            Ok(results)
        }
        .boxed()
    }

    fn exists<'a>(
        &'a self,
        path: &'a Path,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<bool, EnvError>> {
        async move {
            let full = self.resolve(path);
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
            let full = self.resolve(path);
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
            let full = self.resolve(path);
            match tokio::fs::metadata(&full).await {
                Ok(m) if m.is_dir() => {
                    if recursive {
                        tokio::fs::remove_dir_all(&full).await.map_err(|e| {
                            if force && e.kind() == std::io::ErrorKind::NotFound {
                                return EnvError::Aborted; // use as no-op signal
                            }
                            EnvError::Io(e)
                        })?;
                    } else {
                        tokio::fs::remove_dir(&full).await.map_err(EnvError::Io)?;
                    }
                }
                _ => {
                    tokio::fs::remove_file(&full).await.map_err(|e| {
                        if force && e.kind() == std::io::ErrorKind::NotFound {
                            return EnvError::Aborted; // use as no-op signal
                        }
                        EnvError::Io(e)
                    })?;
                }
            }
            Ok(())
        }
        .boxed()
    }

    fn create_temp_dir<'a>(
        &'a self,
        prefix: &'a str,
    ) -> BoxFuture<'a, Result<PathBuf, EnvError>> {
        async move {
            let dir = std::env::temp_dir().join(format!("{prefix}_{}", uuid::Uuid::new_v4()));
            tokio::fs::create_dir_all(&dir).await.map_err(EnvError::Io)?;
            Ok(dir)
        }
        .boxed()
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
            let child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(opts.cwd.unwrap_or(&self.working_dir))
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(EnvError::Io)?;

            let output = if let Some(timeout) = opts.timeout {
                tokio::time::timeout(timeout, child.wait_with_output())
                    .await
                    .map_err(|_| EnvError::ShellFailed {
                        exit_code: -1,
                        stderr: "timeout".into(),
                    })?
                    .map_err(EnvError::Io)?
            } else {
                child.wait_with_output().await.map_err(EnvError::Io)?
            };

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            if !output.status.success() {
                Err(EnvError::ShellFailed {
                    exit_code: output.status.code().unwrap_or(-1),
                    stderr,
                })
            } else {
                Ok(ShellOutput {
                    stdout,
                    stderr,
                    exit_code: output.status.code().unwrap_or(0),
                })
            }
        }
        .boxed()
    }

    fn cleanup<'a>(&'a self) -> BoxFuture<'a, Result<(), EnvError>> {
        async move { Ok(()) }.boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn write_and_read_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let env = OsEnv::new(tmp.path());
        let abort = CancellationToken::new();

        env.write_file(Path::new("test.txt"), b"hello", abort.clone())
            .await
            .unwrap();

        let content = env
            .read_text_file(Path::new("test.txt"), abort)
            .await
            .unwrap();
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn read_nonexistent_file_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let env = OsEnv::new(tmp.path());
        let abort = CancellationToken::new();

        let err = env
            .read_text_file(Path::new("nonexistent.txt"), abort)
            .await
            .unwrap_err();
        assert!(matches!(err, EnvError::NotFound(_)));
    }
}
