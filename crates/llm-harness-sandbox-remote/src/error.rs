use std::path::PathBuf;

use llm_harness_types::EnvError;

/// Errors returned by the remote sandbox client and protocol.
#[derive(Debug, thiserror::Error)]
pub enum RemoteSandboxError {
    #[error("invalid remote sandbox configuration: {0}")]
    InvalidConfig(String),
    #[error("gateway request failed with status {status}: {message}")]
    Gateway {
        status: u16,
        code: String,
        message: String,
        path: Option<PathBuf>,
    },
    #[error("remote sandbox transport error: {0}")]
    Transport(String),
    #[error("remote sandbox protocol error: {0}")]
    Protocol(String),
    #[error("remote sandbox request timed out")]
    Timeout,
    #[error("remote sandbox request aborted")]
    Aborted,
    #[error("remote shell command aborted")]
    ShellAborted {
        stdout: String,
        stderr: String,
        stdout_truncated_bytes: u64,
        stderr_truncated_bytes: u64,
    },
    #[error("remote shell command timed out")]
    ShellTimeout {
        stdout: String,
        stderr: String,
        stdout_truncated_bytes: u64,
        stderr_truncated_bytes: u64,
    },
}

impl RemoteSandboxError {
    pub(crate) fn invalid_config(message: impl Into<String>) -> Self {
        Self::InvalidConfig(message.into())
    }

    pub(crate) fn transport(message: impl Into<String>) -> Self {
        Self::Transport(message.into())
    }

    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    pub(crate) fn gateway(
        status: u16,
        code: impl Into<String>,
        message: impl Into<String>,
        path: Option<PathBuf>,
    ) -> Self {
        Self::Gateway {
            status,
            code: code.into(),
            message: message.into(),
            path,
        }
    }
}

impl From<RemoteSandboxError> for EnvError {
    fn from(value: RemoteSandboxError) -> Self {
        match value {
            RemoteSandboxError::Gateway {
                status,
                code,
                message,
                path,
            } => match (status, code.as_str(), path) {
                (404, _, Some(path)) => EnvError::NotFound(path),
                (403, _, Some(path)) => EnvError::PermissionDenied(path),
                (401, _, _) => EnvError::Other(format!("remote sandbox unauthorized: {message}")),
                (408, _, _) | (504, _, _) => {
                    EnvError::Other(format!("remote sandbox timeout: {message}"))
                }
                (_, _, path) => EnvError::Other(format!(
                    "remote sandbox gateway error (status {status}, code {code}, path {}): {message}",
                    path.map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_default()
                )),
            },
            RemoteSandboxError::Timeout => {
                EnvError::Other("remote sandbox request timed out".to_string())
            }
            RemoteSandboxError::Aborted => EnvError::Aborted,
            RemoteSandboxError::ShellAborted {
                stdout,
                stderr,
                stdout_truncated_bytes,
                stderr_truncated_bytes,
            } => EnvError::ShellAborted {
                stdout,
                stderr,
                stdout_truncated_bytes,
                stderr_truncated_bytes,
            },
            RemoteSandboxError::ShellTimeout {
                stdout,
                stderr,
                stdout_truncated_bytes,
                stderr_truncated_bytes,
            } => EnvError::ShellTimeout {
                stdout,
                stderr,
                stdout_truncated_bytes,
                stderr_truncated_bytes,
            },
            RemoteSandboxError::InvalidConfig(message) => EnvError::Other(message),
            RemoteSandboxError::Transport(message) => EnvError::Other(message),
            RemoteSandboxError::Protocol(message) => EnvError::Other(message),
        }
    }
}
