//! Remote sandbox backend for `ExecutionEnv` and `Sandbox`.
//!
//! This crate defines a transport-oriented gateway protocol and implements the
//! runtime-side client. A concrete gateway may use Firecracker, Kata, gVisor,
//! or another isolation backend as long as it enforces the declared sandbox
//! boundary.

pub mod client;
pub mod config;
pub mod error;
pub mod protocol;
pub mod sandbox;
pub mod sse;

pub use client::RemoteSandboxClient;
pub use config::{NetRuleWire, RemoteSandboxClientConfig, RemoteSandboxConfig, ResourceLimitsWire};
pub use error::RemoteSandboxError;
pub use protocol::{
    CreateSandboxRequest, CreateSandboxResponse, FileContent, FileInfoResponse, ReadFileRequest,
    ReadFileResponse, RemoteExecEvent, RemoteSandboxStatus, ShellExecRequest, ShellExecResponse,
    UploadFile, UploadFilesRequest, UploadFilesResponse, UploadedFile, WriteFileRequest,
};
pub use sandbox::{RemoteEnv, RemoteSandbox};
