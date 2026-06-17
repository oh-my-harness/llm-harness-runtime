use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::EnvError;

/// Shell 命令执行输出。
pub struct ShellOutput {
    /// 标准输出内容。
    pub stdout: String,
    /// 标准错误内容。
    pub stderr: String,
    /// 进程退出码。
    pub exit_code: i32,
}

/// 文件或目录的元数据。
pub struct FileInfo {
    /// 条目的绝对路径。
    pub path: PathBuf,
    /// 是否为目录。
    pub is_dir: bool,
    /// 文件大小（字节）；目录为 0。
    pub size: u64,
    /// 最后修改时间。
    pub modified: DateTime<Utc>,
}

/// Shell 命令执行选项。
#[allow(clippy::type_complexity)]
pub struct ShellOptions<'a> {
    /// 覆盖工作目录；`None` 表示使用 env 默认工作目录。
    pub cwd: Option<&'a Path>,
    /// 额外注入的环境变量。
    pub env: Vec<(&'a str, &'a str)>,
    /// 超时时长；`None` 表示无超时。
    pub timeout: Option<Duration>,
    /// 取消信号。
    pub abort: CancellationToken,
    /// 流式 stdout 回调；`None` 时仅在最终 Output 中返回完整内容。
    pub on_stdout: Option<Box<dyn FnMut(&str) + Send + 'a>>,
    /// 流式 stderr 回调；`None` 时仅在最终 Output 中返回完整内容。
    pub on_stderr: Option<Box<dyn FnMut(&str) + Send + 'a>>,
}

/// 执行环境抽象——将文件系统和 shell 操作与具体平台解耦。
///
/// 实现方可以是本地 OS、Docker 容器、WASM 沙箱或测试 mock。
pub trait ExecutionEnv: Send + Sync {
    /// 返回 env 的默认工作目录。
    fn working_dir(&self) -> &Path;

    /// 读取文本文件；非 UTF-8 内容返回 `EnvError::InvalidUtf8`。
    fn read_text_file<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<String, EnvError>>;

    /// 读取文本文件的行；`max_lines` 为 `None` 时读取全部行。
    fn read_text_lines<'a>(
        &'a self,
        path: &'a Path,
        max_lines: Option<usize>,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<String>, EnvError>>;

    /// 读取二进制文件的原始字节。
    fn read_binary_file<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<u8>, EnvError>>;

    /// 写入文件（覆盖）。
    fn write_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>>;

    /// 追加内容到文件末尾（JSONL 存储的核心操作）。
    fn append_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>>;

    /// 获取文件或目录的元数据。
    fn file_info<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<FileInfo, EnvError>>;

    /// 列出目录内容。
    fn list_dir<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<FileInfo>, EnvError>>;

    /// 检查路径是否存在。
    fn exists<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<bool, EnvError>>;

    /// 创建目录；`recursive` 对应 `mkdir -p`。
    fn create_dir<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>>;

    /// 删除文件或目录；`recursive` 对应 `rm -r`，`force` 对应 `rm -f`。
    fn remove<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        force: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>>;

    /// 创建临时目录；返回其绝对路径。
    fn create_temp_dir<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<PathBuf, EnvError>>;

    /// 执行 shell 命令。
    fn execute_shell<'a>(
        &'a self,
        cmd: &'a str,
        opts: ShellOptions<'a>,
    ) -> BoxFuture<'a, Result<ShellOutput, EnvError>>;

    /// 释放 env 持有的临时资源（best-effort）。
    fn cleanup<'a>(&'a self) -> BoxFuture<'a, Result<(), EnvError>>;
}

/// Execution environment placeholder for agents that do not run tools.
///
/// All file-system and shell operations return `EnvError::Other`. Runtime crates
/// should inject their own [`ExecutionEnv`] when tools need workspace access.
#[derive(Debug, Clone)]
pub struct UnsupportedEnv {
    working_dir: PathBuf,
}

impl UnsupportedEnv {
    /// Create an unsupported environment rooted at the current relative path.
    pub fn new() -> Self {
        Self {
            working_dir: PathBuf::from("."),
        }
    }

    /// Create an unsupported environment with a custom working directory label.
    pub fn with_working_dir(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
        }
    }

    fn unsupported<'a, T: Send + 'a>() -> BoxFuture<'a, Result<T, EnvError>> {
        Box::pin(async {
            Err(EnvError::Other(
                "execution environment is not available".into(),
            ))
        })
    }
}

impl Default for UnsupportedEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecutionEnv for UnsupportedEnv {
    fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    fn read_text_file<'a>(
        &'a self,
        _path: &'a Path,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<String, EnvError>> {
        Self::unsupported()
    }

    fn read_text_lines<'a>(
        &'a self,
        _path: &'a Path,
        _max_lines: Option<usize>,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<String>, EnvError>> {
        Self::unsupported()
    }

    fn read_binary_file<'a>(
        &'a self,
        _path: &'a Path,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<u8>, EnvError>> {
        Self::unsupported()
    }

    fn write_file<'a>(
        &'a self,
        _path: &'a Path,
        _content: &'a [u8],
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        Self::unsupported()
    }

    fn append_file<'a>(
        &'a self,
        _path: &'a Path,
        _content: &'a [u8],
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        Self::unsupported()
    }

    fn file_info<'a>(
        &'a self,
        _path: &'a Path,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<FileInfo, EnvError>> {
        Self::unsupported()
    }

    fn list_dir<'a>(
        &'a self,
        _path: &'a Path,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<FileInfo>, EnvError>> {
        Self::unsupported()
    }

    fn exists<'a>(
        &'a self,
        _path: &'a Path,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<bool, EnvError>> {
        Self::unsupported()
    }

    fn create_dir<'a>(
        &'a self,
        _path: &'a Path,
        _recursive: bool,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        Self::unsupported()
    }

    fn remove<'a>(
        &'a self,
        _path: &'a Path,
        _recursive: bool,
        _force: bool,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        Self::unsupported()
    }

    fn create_temp_dir<'a>(&'a self, _prefix: &'a str) -> BoxFuture<'a, Result<PathBuf, EnvError>> {
        Self::unsupported()
    }

    fn execute_shell<'a>(
        &'a self,
        _cmd: &'a str,
        _opts: ShellOptions<'a>,
    ) -> BoxFuture<'a, Result<ShellOutput, EnvError>> {
        Self::unsupported()
    }

    fn cleanup<'a>(&'a self) -> BoxFuture<'a, Result<(), EnvError>> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_output_fields() {
        let o = ShellOutput {
            stdout: "hello".into(),
            stderr: "".into(),
            exit_code: 0,
        };
        assert_eq!(o.exit_code, 0);
    }

    #[test]
    fn file_info_is_dir() {
        let fi = FileInfo {
            path: PathBuf::from("/tmp"),
            is_dir: true,
            size: 0,
            modified: chrono::Utc::now(),
        };
        assert!(fi.is_dir);
    }

    #[test]
    fn execution_env_is_object_safe() {
        fn _accepts(_: &dyn ExecutionEnv) {}
    }

    #[test]
    fn unsupported_env_reports_unavailable_operations() {
        let env = UnsupportedEnv::new();
        let err = futures::executor::block_on(
            env.read_text_file(Path::new("README.md"), CancellationToken::new()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not available"));
    }
}
