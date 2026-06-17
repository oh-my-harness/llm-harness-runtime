use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use llm_harness_types::*;
use tokio_util::sync::CancellationToken;

/// Concrete `ExecutionEnv` backed by the real OS filesystem and shell.
pub struct OsEnv {
    working_dir: PathBuf,
}

impl OsEnv {
    /// Create a new `OsEnv` with the given working directory.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
        }
    }
}

fn resolve_shell() -> Result<(PathBuf, &'static str), EnvError> {
    #[cfg(windows)]
    {
        if let Some(path) = std::env::var_os("CODING_AGENT_SHELL") {
            let path = PathBuf::from(path);
            if path.is_file() {
                return Ok((path, "-lc"));
            }
        }

        let git_bash = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        if git_bash.is_file() {
            return Ok((git_bash, "-lc"));
        }

        Ok((PathBuf::from("bash.exe"), "-lc"))
    }

    #[cfg(not(windows))]
    {
        if let Some(path) = std::env::var_os("SHELL") {
            let path = PathBuf::from(path);
            if path.is_file() {
                return Ok((path, "-lc"));
            }
        }

        let bash = PathBuf::from("/bin/bash");
        if bash.is_file() {
            return Ok((bash, "-lc"));
        }

        Ok((PathBuf::from("sh"), "-c"))
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
        let path = path.to_path_buf();
        Box::pin(async move {
            tokio::select! {
                result = tokio::fs::read_to_string(&path) => {
                    result.map_err(|e| match e.kind() {
                        std::io::ErrorKind::NotFound => EnvError::NotFound(path),
                        std::io::ErrorKind::PermissionDenied => EnvError::PermissionDenied(path),
                        _ => EnvError::Io(e),
                    })
                }
                _ = abort.cancelled() => Err(EnvError::Aborted),
            }
        })
    }

    fn read_text_lines<'a>(
        &'a self,
        path: &'a Path,
        max_lines: Option<usize>,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<String>, EnvError>> {
        let path = path.to_path_buf();
        Box::pin(async move {
            tokio::select! {
                result = tokio::fs::read_to_string(&path) => {
                    let text = result.map_err(|e| match e.kind() {
                        std::io::ErrorKind::NotFound => EnvError::NotFound(path),
                        _ => EnvError::Io(e),
                    })?;
                    let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
                    Ok(if let Some(n) = max_lines {
                        lines.into_iter().take(n).collect()
                    } else {
                        lines
                    })
                }
                _ = abort.cancelled() => Err(EnvError::Aborted),
            }
        })
    }

    fn read_binary_file<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<u8>, EnvError>> {
        let path = path.to_path_buf();
        Box::pin(async move {
            tokio::select! {
                result = tokio::fs::read(&path) => {
                    result.map_err(|e| match e.kind() {
                        std::io::ErrorKind::NotFound => EnvError::NotFound(path),
                        _ => EnvError::Io(e),
                    })
                }
                _ = abort.cancelled() => Err(EnvError::Aborted),
            }
        })
    }

    fn write_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let path = path.to_path_buf();
        let content = content.to_vec();
        Box::pin(async move {
            tokio::select! {
                result = tokio::fs::write(&path, &content) => {
                    result.map_err(EnvError::Io)
                }
                _ = abort.cancelled() => Err(EnvError::Aborted),
            }
        })
    }

    fn append_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        use tokio::io::AsyncWriteExt;
        let path = path.to_path_buf();
        let content = content.to_vec();
        Box::pin(async move {
            tokio::select! {
                result = async {
                    let mut file = tokio::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .await
                        .map_err(EnvError::Io)?;
                    file.write_all(&content).await.map_err(EnvError::Io)
                } => result,
                _ = abort.cancelled() => Err(EnvError::Aborted),
            }
        })
    }

    fn file_info<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<FileInfo, EnvError>> {
        let path = path.to_path_buf();
        Box::pin(async move {
            tokio::select! {
                result = tokio::fs::metadata(&path) => {
                    let meta = result.map_err(|e| match e.kind() {
                        std::io::ErrorKind::NotFound => EnvError::NotFound(path.clone()),
                        _ => EnvError::Io(e),
                    })?;
                    let modified = meta
                        .modified()
                        .map(DateTime::from)
                        .unwrap_or_else(|_| Utc::now());
                    Ok(FileInfo {
                        path,
                        is_dir: meta.is_dir(),
                        size: if meta.is_file() { meta.len() } else { 0 },
                        modified,
                    })
                }
                _ = abort.cancelled() => Err(EnvError::Aborted),
            }
        })
    }

    fn list_dir<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<FileInfo>, EnvError>> {
        let path = path.to_path_buf();
        Box::pin(async move {
            tokio::select! {
                result = async {
                    let mut read_dir = tokio::fs::read_dir(&path).await.map_err(|e| match e.kind() {
                        std::io::ErrorKind::NotFound => EnvError::NotFound(path.clone()),
                        _ => EnvError::Io(e),
                    })?;
                    let mut entries = Vec::new();
                    while let Some(entry) = read_dir.next_entry().await.map_err(EnvError::Io)? {
                        if let Ok(meta) = entry.metadata().await {
                            let modified = meta
                                .modified()
                                .map(DateTime::from)
                                .unwrap_or_else(|_| Utc::now());
                            entries.push(FileInfo {
                                path: entry.path(),
                                is_dir: meta.is_dir(),
                                size: if meta.is_file() { meta.len() } else { 0 },
                                modified,
                            });
                        }
                    }
                    Ok(entries)
                } => result,
                _ = abort.cancelled() => Err(EnvError::Aborted),
            }
        })
    }

    fn exists<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<bool, EnvError>> {
        let path = path.to_path_buf();
        Box::pin(async move {
            tokio::select! {
                result = tokio::fs::metadata(&path) => {
                    Ok(result.is_ok())
                }
                _ = abort.cancelled() => Err(EnvError::Aborted),
            }
        })
    }

    fn create_dir<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let path = path.to_path_buf();
        Box::pin(async move {
            tokio::select! {
                result = async {
                    if recursive {
                        tokio::fs::create_dir_all(&path).await
                    } else {
                        tokio::fs::create_dir(&path).await
                    }
                } => result.map_err(EnvError::Io),
                _ = abort.cancelled() => Err(EnvError::Aborted),
            }
        })
    }

    fn remove<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        _force: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let path = path.to_path_buf();
        Box::pin(async move {
            tokio::select! {
                result = async {
                    if recursive {
                        tokio::fs::remove_dir_all(&path).await
                    } else {
                        // Try file first, then directory.
                        match tokio::fs::remove_file(&path).await {
                            Ok(()) => Ok(()),
                            Err(_) => tokio::fs::remove_dir(&path).await,
                        }
                    }
                } => result.map_err(EnvError::Io),
                _ = abort.cancelled() => Err(EnvError::Aborted),
            }
        })
    }

    fn create_temp_dir<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<PathBuf, EnvError>> {
        let prefix = prefix.to_string();
        Box::pin(async move {
            let base = std::env::temp_dir();
            let unique = uuid::Uuid::now_v7().to_string().replace('-', "");
            let dir = base.join(format!("{prefix}{unique}"));
            tokio::fs::create_dir(&dir).await.map_err(EnvError::Io)?;
            Ok(dir)
        })
    }

    fn execute_shell<'a>(
        &'a self,
        cmd: &'a str,
        opts: ShellOptions<'a>,
    ) -> BoxFuture<'a, Result<ShellOutput, EnvError>> {
        let cmd = cmd.to_string();
        let cwd = opts
            .cwd
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.working_dir.clone());
        let env_vars: Vec<(String, String)> = opts
            .env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let timeout = opts.timeout;
        let abort = opts.abort;

        Box::pin(async move {
            let run = async {
                let (shell, flag) = resolve_shell()?;
                let mut command = tokio::process::Command::new(shell);
                command.arg(flag).arg(&cmd).current_dir(&cwd);
                for (k, v) in &env_vars {
                    command.env(k, v);
                }
                let output = command.output().await.map_err(|e| {
                    if cfg!(windows) && e.kind() == std::io::ErrorKind::NotFound {
                        EnvError::Other(
                            "bash shell not found; install Git for Windows or set CODING_AGENT_SHELL"
                                .into(),
                        )
                    } else {
                        EnvError::Io(e)
                    }
                })?;
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                let exit_code = output.status.code().unwrap_or(-1);
                Ok(ShellOutput {
                    stdout,
                    stderr,
                    exit_code,
                })
            };

            match timeout {
                Some(dur) => {
                    tokio::select! {
                        result = run => result,
                        _ = tokio::time::sleep(dur) => {
                            Err(EnvError::Other("command timed out".into()))
                        }
                        _ = abort.cancelled() => Err(EnvError::Aborted),
                    }
                }
                None => {
                    tokio::select! {
                        result = run => result,
                        _ = abort.cancelled() => Err(EnvError::Aborted),
                    }
                }
            }
        })
    }

    fn cleanup<'a>(&'a self) -> BoxFuture<'a, Result<(), EnvError>> {
        Box::pin(async move { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;

    #[tokio::test]
    async fn read_text_file_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let env = OsEnv::new(tmp.path());
        let content = env
            .read_text_file(&path, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn list_dir_finds_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "b").unwrap();

        let env = OsEnv::new(tmp.path());
        let entries = env
            .list_dir(tmp.path(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn exists_returns_correct_values() {
        let tmp = TempDir::new().unwrap();
        let existing = tmp.path().join("exists.txt");
        std::fs::write(&existing, "x").unwrap();
        let missing = tmp.path().join("missing.txt");

        let env = OsEnv::new(tmp.path());
        assert!(
            env.exists(&existing, CancellationToken::new())
                .await
                .unwrap()
        );
        assert!(
            !env.exists(&missing, CancellationToken::new())
                .await
                .unwrap()
        );
    }
}
