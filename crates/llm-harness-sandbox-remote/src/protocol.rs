use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::RemoteSandboxConfig;

/// Sandbox lifecycle state reported by the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSandboxState {
    Creating,
    Running,
    Suspended,
    Terminated,
    Failed,
}

/// Request body for `POST /v1/sandboxes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSandboxRequest {
    pub config: RemoteSandboxConfig,
}

/// Response body for sandbox creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSandboxResponse {
    pub sandbox_id: String,
    pub work_dir: String,
    pub backend: String,
    pub state: RemoteSandboxState,
    /// Trust level actually granted (echoed for audit; UIs should display it).
    #[serde(default)]
    pub trust_profile: llm_harness_platform::trust::TrustProfile,
}

/// Detailed sandbox status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSandboxStatus {
    pub sandbox_id: String,
    pub work_dir: String,
    pub backend: String,
    pub state: RemoteSandboxState,
    pub config: RemoteSandboxConfig,
}

/// File content returned by the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileContent {
    Text { value: String },
    Binary { base64: String },
}

/// Request body for reading a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileRequest {
    pub path: String,
    pub binary: bool,
}

/// Response body for reading a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileResponse {
    pub content: FileContent,
}

/// Request body for writing or appending a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileRequest {
    pub path: String,
    pub content: FileContent,
    pub append: bool,
}

/// File or directory metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfoResponse {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: DateTime<Utc>,
}

/// Request body for file metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfoRequest {
    pub path: String,
}

/// Response body for file metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfoResponseValue {
    pub info: FileInfoResponse,
}

/// Request body for listing a directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListDirRequest {
    pub path: String,
}

/// Response body for listing a directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListDirResponse {
    pub entries: Vec<FileInfoResponse>,
}

/// Request body for path existence checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExistsRequest {
    pub path: String,
}

/// Response body for path existence checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExistsResponse {
    pub exists: bool,
}

/// Request body for directory creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateDirRequest {
    pub path: String,
    pub recursive: bool,
}

/// Request body for path removal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveRequest {
    pub path: String,
    pub recursive: bool,
    pub force: bool,
}

/// Request body for temporary-directory creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTempDirRequest {
    pub prefix: String,
}

/// Response body for temporary-directory creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTempDirResponse {
    pub path: String,
}

/// Request body for environment cleanup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CleanupRequest {}

/// Request body for shell execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellExecRequest {
    pub command: String,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub timeout_ms: Option<u64>,
    pub max_output_bytes: usize,
}

/// Final shell execution result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShellExecResponse {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated_bytes: u64,
    pub stderr_truncated_bytes: u64,
}

/// Events emitted by the shell execution SSE stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteExecEvent {
    Started {
        execution_id: String,
    },
    Stdout {
        chunk: String,
    },
    Stderr {
        chunk: String,
    },
    Exit {
        #[serde(flatten)]
        result: ShellExecResponse,
    },
    Timeout {
        #[serde(flatten)]
        result: ShellExecResponse,
    },
    Error {
        code: String,
        message: String,
    },
}

/// Gateway error response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayErrorResponse {
    pub code: String,
    pub message: String,
    pub path: Option<String>,
}

impl From<FileInfoResponse> for llm_harness_types::FileInfo {
    fn from(value: FileInfoResponse) -> Self {
        Self {
            path: PathBuf::from(value.path),
            is_dir: value.is_dir,
            size: value.size,
            modified: value.modified,
        }
    }
}
