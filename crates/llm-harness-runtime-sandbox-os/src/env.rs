use std::path::{Path, PathBuf};

use chrono::DateTime;
use futures::FutureExt;
use futures::future::BoxFuture;
use llm_harness_types::{EnvError, ExecutionEnv, FileInfo, ShellOptions, ShellOutput};
use tokio_util::sync::CancellationToken;

/// 找到 `bytes` 中最后一个完整 UTF-8 字符的边界索引。
///
/// 返回值 `split` 满足：`bytes[..split]` 是合法 UTF-8（可整块解码），
/// `bytes[split..]` 是可能不完整的多字节序列尾部（留到下一轮处理）。
/// 如果 `bytes` 全部是合法 UTF-8（或空），返回 `bytes.len()`。
fn utf8_safe_boundary(bytes: &[u8]) -> usize {
    let len = bytes.len();
    if len == 0 {
        return 0;
    }
    // 从末尾往回找，最多回退 4 字节（UTF-8 最长 4 字节，需覆盖 4 字节字符的起始字节）
    let start = len.saturating_sub(4);
    for i in (start..len).rev() {
        let b = bytes[i];
        // 找到一个 UTF-8 起始字节（非 continuation byte: 10xxxxxx）
        if b & 0xC0 != 0x80 {
            // 计算这个字符的预期长度
            let expected = if b & 0x80 == 0 {
                1
            } else if b & 0xE0 == 0xC0 {
                2
            } else if b & 0xF0 == 0xE0 {
                3
            } else if b & 0xF8 == 0xF0 {
                4
            } else {
                // 无效的起始字节，直接跳过（lossy 会处理）
                continue;
            };
            let end = i + expected;
            if end <= len {
                // 完整字符在范围内
                return end;
            } else {
                // 不完整——从这个字符开始都是残留
                return i;
            }
        }
    }
    // 全是 continuation bytes（不应该发生，但 fallback：全部留到下一轮）
    0
}

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
                    // `read_to_string` returns `InvalidData` for non-UTF-8
                    // bytes; surface it as `InvalidUtf8` so callers (e.g.
                    // `ReadTool`) can fall back to binary handling instead of
                    // treating it as a generic I/O failure.
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

    fn create_temp_dir<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<PathBuf, EnvError>> {
        async move {
            let dir = std::env::temp_dir().join(format!("{prefix}_{}", uuid::Uuid::new_v4()));
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
            // Windows 没有 `sh`；用 `cmd.exe /D /Q /C`。`/D` 禁用 AutoRun，
            // `/Q` 关闭命令回显，避免循环等复合命令污染 stdout。Unix 保持 `sh -c`。
            //
            // 关键：命令字符串必须用 `raw_arg` 传递，绕过 Rust `Command` 在 Windows
            // 上的 C runtime 参数转义。若用 `arg`/`args`，含引号的命令（如
            // `python -c "..."`）会被转义成 `\"`，而 cmd.exe 不识别 `\"`、按自身
            // `/C` 规则剥引号后把参数切碎，导致下游程序收到截断的参数（issue #81）。
            // `raw_arg` 把字符串原样拼入 CreateProcessW 命令行，由 cmd.exe 按原生
            // 引号规则解析，与用户在 cmd.exe 中手敲命令的行为一致。
            #[cfg(target_os = "windows")]
            let mut shell_cmd = {
                use std::os::windows::process::CommandExt;
                let mut c = std::process::Command::new("cmd.exe");
                c.raw_arg("/D");
                c.raw_arg("/Q");
                c.raw_arg("/C");
                c.raw_arg(cmd);
                tokio::process::Command::from(c)
            };
            #[cfg(not(target_os = "windows"))]
            let mut shell_cmd = {
                let mut c = tokio::process::Command::new("sh");
                c.args(["-c", cmd]);
                c
            };
            // 注入调用方指定的环境变量（如 PYTHONUTF8=1），否则 opts.env 会被静默丢弃。
            for (key, val) in &opts.env {
                shell_cmd.env(key, val);
            }
            let mut child = shell_cmd
                .current_dir(opts.cwd.unwrap_or(&self.working_dir))
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(EnvError::Io)?;

            // 取走 piped handle；若 on_stdout/on_stderr 为 None，仍 drain 到缓冲以构造 ShellOutput。
            let mut stdout_handle = child.stdout.take();
            let mut stderr_handle = child.stderr.take();

            // 读取一个 piped handle 直到 EOF，每读到一块数据：
            // - 若有回调：用 carry-over buffer 处理跨块的 UTF-8 边界，把完整部分喂给回调
            // - 累积全部原始字节到 buf，最终 lossy 转成 String 返回
            macro_rules! drain_pipe {
                ($handle:expr, $cb:expr) => {
                    async {
                        use tokio::io::AsyncReadExt;
                        let mut buf = Vec::with_capacity(4096);
                        let mut carry = Vec::new(); // 上一块尾部的不完整 UTF-8 字节
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
                                    // 找到最后一个完整 UTF-8 字符的边界
                                    let split = utf8_safe_boundary(&carry);
                                    let s = String::from_utf8_lossy(&carry[..split]);
                                    cb(&s);
                                    carry = carry[split..].to_vec();
                                }
                            }
                        }
                        // EOF 时把残留字节 lossy 喂给回调
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
                // 等待进程退出，拿 exit code
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

            // ExecutionEnv exposes text rather than platform-native byte streams.
            // Normalize Windows newlines so callers and tool output see the same
            // representation on every platform.
            #[cfg(target_os = "windows")]
            let (stdout, stderr) = (stdout.replace("\r\n", "\n"), stderr.replace("\r\n", "\n"));

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
    async fn execute_shell_echo() {
        let tmp = tempfile::tempdir().unwrap();
        let env = OsEnv::new(tmp.path());
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
        };

        #[cfg(not(target_os = "windows"))]
        let cmd = "echo hello";
        #[cfg(target_os = "windows")]
        let cmd = "echo hello";

        let out = env.execute_shell(cmd, opts).await.unwrap();
        assert_eq!(out.stdout.trim(), "hello");
        assert_eq!(out.exit_code, 0);
    }

    #[tokio::test]
    async fn execute_shell_preserves_quoted_command() {
        // Regression test for issue #81: Windows 上 `cmd.exe /C` 与 Rust `Command`
        // 的 C runtime `\"` 转义冲突，导致含引号的命令被静默截断。
        //
        // 用 `echo` 验证命令字符串中的引号被原样保留（不被转义成 `\"`、也不被
        // 剥掉），不依赖任何外部程序，确定性可复现：
        //   - Windows (cmd.exe): echo 原样打印参数，引号保留 -> `"hello world"`
        //   - Unix (sh):         echo 经 shell 分词剥掉引号   -> `hello world`
        // 旧实现（`args`）下 Windows 会输出 `\"hello world\"`（C runtime 转义后
        // cmd.exe 剥引号的残留），本测试失败；改用 `raw_arg` 后输出干净引号。
        let tmp = tempfile::tempdir().unwrap();
        let env = OsEnv::new(tmp.path());
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
        };

        let cmd = "echo \"hello world\"";

        let out = env.execute_shell(cmd, opts).await.unwrap();
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);

        #[cfg(not(target_os = "windows"))]
        assert_eq!(out.stdout.trim(), "hello world");
        #[cfg(target_os = "windows")]
        assert_eq!(out.stdout.trim(), "\"hello world\"");
    }

    #[tokio::test]
    async fn execute_shell_nonzero_exit_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let env = OsEnv::new(tmp.path());
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
        };

        #[cfg(not(target_os = "windows"))]
        let cmd = "exit 1";
        #[cfg(target_os = "windows")]
        let cmd = "exit /b 1";

        let result = env.execute_shell(cmd, opts).await;
        assert!(matches!(
            result,
            Err(EnvError::ShellFailed { exit_code: 1, .. })
        ));
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

    #[tokio::test]
    async fn read_text_file_binary_returns_invalid_utf8() {
        // Non-UTF-8 bytes must surface as `InvalidUtf8` (not `Io`) so that
        // `ReadTool` can fall back to binary-file handling.
        let tmp = tempfile::tempdir().unwrap();
        let env = OsEnv::new(tmp.path());
        let abort = CancellationToken::new();

        env.write_file(
            Path::new("bin.dat"),
            &[0xFF, 0xFE, 0x00, 0x01],
            abort.clone(),
        )
        .await
        .unwrap();

        let err = env
            .read_text_file(Path::new("bin.dat"), abort)
            .await
            .unwrap_err();
        assert!(matches!(err, EnvError::InvalidUtf8(_)), "got {err:?}");
    }
    #[tokio::test]
    async fn execute_shell_streams_stdout_callback() {
        use std::sync::{Arc, Mutex};

        let tmp = tempfile::tempdir().unwrap();
        let env = OsEnv::new(tmp.path());

        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let collected_clone = collected.clone();
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: Some(Box::new(move |chunk: &str| {
                collected_clone.lock().unwrap().push(chunk.to_string());
            })),
            on_stderr: None,
        };

        // 分多行输出，制造多个 chunk 的机会
        #[cfg(not(target_os = "windows"))]
        let cmd = "for i in 1 2 3; do echo line$i; done";
        #[cfg(target_os = "windows")]
        let cmd = "for /L %i in (1,1,3) do echo line%i";

        let out = env.execute_shell(cmd, opts).await.unwrap();
        assert_eq!(out.exit_code, 0);

        // 回调必须被调用至少一次；累积内容应覆盖全部输出
        let chunks = collected.lock().unwrap();
        assert!(!chunks.is_empty(), "on_stdout callback was never invoked");
        let joined = chunks.join("");
        assert!(
            joined.contains("line1") && joined.contains("line2") && joined.contains("line3"),
            "streamed chunks did not cover full stdout: {joined:?}"
        );
        // ShellOutput 仍含完整内容
        assert_eq!(out.stdout.trim_end(), "line1\nline2\nline3");
    }
    #[tokio::test]
    async fn execute_shell_streams_stderr_callback() {
        use std::sync::{Arc, Mutex};

        let tmp = tempfile::tempdir().unwrap();
        let env = OsEnv::new(tmp.path());

        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let collected_clone = collected.clone();
        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: Some(Box::new(move |chunk: &str| {
                collected_clone.lock().unwrap().push(chunk.to_string());
            })),
        };

        // 写到 stderr
        #[cfg(not(target_os = "windows"))]
        let cmd = "echo err1 >&2; echo err2 >&2";
        #[cfg(target_os = "windows")]
        let cmd = "echo err1>&2&&echo err2>&2";

        let out = env.execute_shell(cmd, opts).await.unwrap();
        assert_eq!(out.exit_code, 0);

        let chunks = collected.lock().unwrap();
        assert!(!chunks.is_empty(), "on_stderr callback was never invoked");
        let joined = chunks.join("");
        assert!(
            joined.contains("err1") && joined.contains("err2"),
            "streamed stderr chunks did not cover full stderr: {joined:?}"
        );
        assert_eq!(out.stderr.trim_end(), "err1\nerr2");
        // stdout 应为空
        assert_eq!(out.stdout.trim(), "");
    }
    #[tokio::test]
    async fn execute_shell_abort_cancels_long_running() {
        let tmp = tempfile::tempdir().unwrap();
        let env = OsEnv::new(tmp.path());
        let abort = CancellationToken::new();
        let abort_clone = abort.clone();

        #[cfg(not(target_os = "windows"))]
        let cmd = "sleep 30";
        #[cfg(target_os = "windows")]
        let cmd = "timeout /t 30 /nobreak >nul";

        // 50ms 后触发取消
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            abort_clone.cancel();
        });

        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort,
            on_stdout: None,
            on_stderr: None,
        };

        let start = std::time::Instant::now();
        let result = env.execute_shell(cmd, opts).await;
        let elapsed = start.elapsed();

        assert!(matches!(result, Err(EnvError::Aborted)), "expected Aborted");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "abort took too long: {elapsed:?}"
        );
    }
    #[tokio::test]
    async fn execute_shell_timeout_returns_shell_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let env = OsEnv::new(tmp.path());

        #[cfg(not(target_os = "windows"))]
        let cmd = "sleep 30";
        #[cfg(target_os = "windows")]
        let cmd = "timeout /t 30 /nobreak >nul";

        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: Some(std::time::Duration::from_millis(50)),
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
        };

        let start = std::time::Instant::now();
        let result = env.execute_shell(cmd, opts).await;
        let elapsed = start.elapsed();

        assert!(
            matches!(result, Err(EnvError::ShellFailed { stderr, .. }) if stderr == "timeout"),
            "expected ShellFailed with stderr \"timeout\""
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "timeout took too long: {elapsed:?}"
        );
    }
    #[tokio::test]
    async fn execute_shell_none_callbacks_returns_full_output() {
        // None-callback 路径：不触发回调，仅返回 ShellOutput
        let tmp = tempfile::tempdir().unwrap();
        let env = OsEnv::new(tmp.path());

        #[cfg(not(target_os = "windows"))]
        let cmd = "echo noproblem; echo toerr >&2";
        #[cfg(target_os = "windows")]
        let cmd = "echo noproblem & echo toerr 1>&2";

        let opts = ShellOptions {
            cwd: None,
            env: vec![],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
        };

        let out = env.execute_shell(cmd, opts).await.unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout.trim(), "noproblem");
        assert_eq!(out.stderr.trim(), "toerr");
    }
    #[tokio::test]
    async fn execute_shell_streams_multibyte_utf8() {
        // 验证 carry-over buffer：多字节 UTF-8 字符跨块时回调不应产生替换字符。
        // 这里用大量 ASCII 填充 + 末尾中文，确保中文不会被拆到两个 chunk 里产生 U+FFFD。
        // 注意：read chunk 大小是 4096，我们输出 >4096 字节来触发跨块。
        let tmp = tempfile::tempdir().unwrap();
        let env = OsEnv::new(tmp.path());

        let collected: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let collected_clone = collected.clone();
        // Windows 上 Python stdout 默认可能是系统代码页（如 cp936/GBK），导致非 ASCII
        // 字节按非 UTF-8 编码，from_utf8_lossy 产生 U+FFFD。设 PYTHONUTF8=1 强制
        // Python 以 UTF-8 输出，使本测试聚焦于 carry-over buffer 逻辑而非编码差异。
        #[cfg(target_os = "windows")]
        let env_vars: Vec<(&str, &str)> = vec![("PYTHONUTF8", "1")];
        #[cfg(not(target_os = "windows"))]
        let env_vars: Vec<(&str, &str)> = vec![];

        let opts = ShellOptions {
            cwd: None,
            env: env_vars,
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: Some(Box::new(move |chunk: &str| {
                collected_clone.lock().unwrap().push(chunk.to_string());
            })),
            on_stderr: None,
        };

        // 输出 5000 字节 ASCII（超出 4096 chunk 边界），末尾跟一个 3 字节 UTF-8 中文字符
        #[cfg(not(target_os = "windows"))]
        let cmd = "python3 -c 'import sys; sys.stdout.write(\"A\"*5000 + \"中\")'";
        #[cfg(target_os = "windows")]
        let cmd = "python -c \"import sys; sys.stdout.write('A'*5000 + '\u{4e2d}')\"";

        let out = env.execute_shell(cmd, opts).await;
        // python3 可能不存在，跳过测试而非失败
        let out = match out {
            Ok(o) => o,
            Err(_) => return, // 环境无 python3，跳过
        };
        assert_eq!(out.exit_code, 0);

        let chunks = collected.lock().unwrap();
        let joined = chunks.join("");
        // 回调拼接后应包含完整的 "中" 字符，不应用 U+FFFD
        assert!(
            joined.contains('中'),
            "streamed callback output missing multibyte char, got replacement instead: {joined:?}"
        );
        assert!(
            !joined.contains('\u{FFFD}'),
            "streamed callback produced U+FFFD replacement char: {joined:?}"
        );
        // ShellOutput 也应完整
        assert!(out.stdout.ends_with('中'));
    }

    #[tokio::test]
    async fn execute_shell_applies_env_vars() {
        // Regression: opts.env 以前被静默丢弃，子进程收不到注入的环境变量。
        // 通过 shell 读取变量值验证注入生效。
        let tmp = tempfile::tempdir().unwrap();
        let env = OsEnv::new(tmp.path());
        let opts = ShellOptions {
            cwd: None,
            env: vec![("LLM_HARNESS_TEST_VAR", "injected_value_42")],
            timeout: None,
            abort: CancellationToken::new(),
            on_stdout: None,
            on_stderr: None,
        };

        #[cfg(not(target_os = "windows"))]
        let cmd = "echo $LLM_HARNESS_TEST_VAR";
        #[cfg(target_os = "windows")]
        let cmd = "echo %LLM_HARNESS_TEST_VAR%";

        let out = env.execute_shell(cmd, opts).await.unwrap();
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
        assert_eq!(out.stdout.trim(), "injected_value_42");
    }

    #[test]
    fn utf8_safe_boundary_handles_4_byte_char() {
        // Regression for issue #85 问题 2：完整 4 字节 UTF-8 字符（如 emoji）在
        // carry 末尾时，旧的 saturating_sub(3) 跳过起始字节，返回 0（整块不输出）。
        // saturating_sub(4) 修复后应正确返回完整长度。
        let mut bytes = vec![b'A'; 5000];
        // U+1F600 (😀): F0 9F 98 80
        bytes.extend_from_slice(&[0xF0, 0x9F, 0x98, 0x80]);
        let split = utf8_safe_boundary(&bytes);
        assert_eq!(split, bytes.len(), "完整 4 字节字符在末尾应返回完整长度");

        // 不完整 4 字节字符（3/4）：起始字节在范围内，应保留到下一轮
        let mut bytes2 = vec![b'A'; 5000];
        bytes2.extend_from_slice(&[0xF0, 0x9F, 0x98]); // 缺最后 1 字节
        let split2 = utf8_safe_boundary(&bytes2);
        assert_eq!(split2, 5000, "不完整 4 字节字符应保留到下一轮");

        // 4 字节字符跨块：起始字节在第 4093 位置，第 4096 字节是 continuation
        let mut bytes3 = vec![b'A'; 4093];
        bytes3.extend_from_slice(&[0xF0, 0x9F, 0x98]); // 3/4，恰好 4096 字节
        let split3 = utf8_safe_boundary(&bytes3);
        assert_eq!(split3, 4093, "跨块的不完整 4 字节字符应从起始字节处截断");
    }
}
