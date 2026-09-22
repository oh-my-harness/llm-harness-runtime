//! Runtime execution environment backed by a VM guest agent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use llm_harness_types::{EnvError, ExecutionEnv, FileInfo, ShellOptions, ShellOutput};
use llm_harness_vm_agent_protocol::{AgentMessage, FileContent, FileEntry, ProcessResult};
use tokio_util::sync::CancellationToken;

use llm_harness_vm_agent_service::{GuestAgentClient, GuestAgentClientError};

const WORK_DIR: &str = "/workspace";

/// An [`ExecutionEnv`] that executes files and shell commands in one VM guest.
pub struct GuestAgentEnv {
    client: Arc<GuestAgentClient>,
    work_dir: PathBuf,
}

impl GuestAgentEnv {
    /// Create an environment over a connected guest-agent client.
    pub fn new(client: GuestAgentClient) -> Self {
        Self {
            client: Arc::new(client),
            work_dir: PathBuf::from(WORK_DIR),
        }
    }

    /// Borrow the shared guest-agent client.
    pub fn client(&self) -> &GuestAgentClient {
        &self.client
    }

    async fn request(
        &self,
        message: AgentMessage,
        abort: CancellationToken,
    ) -> Result<AgentMessage, EnvError> {
        if abort.is_cancelled() {
            return Err(EnvError::Aborted);
        }
        tokio::select! {
            result = self.client.request(message) => result.map_err(env_error),
            _ = abort.cancelled() => Err(EnvError::Aborted),
        }
    }

    async fn read_content(
        &self,
        path: &Path,
        binary: bool,
        abort: CancellationToken,
    ) -> Result<FileContent, EnvError> {
        let path = workspace_path(path)?;
        let response = self
            .request(AgentMessage::ReadFile { path, binary }, abort)
            .await?;
        match response {
            AgentMessage::ReadFileResult { content } => Ok(content),
            AgentMessage::Error { message, .. } => Err(EnvError::Other(message)),
            other => Err(unexpected_response("read file", other)),
        }
    }
}

impl ExecutionEnv for GuestAgentEnv {
    fn working_dir(&self) -> &Path {
        &self.work_dir
    }

    fn read_text_file<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<String, EnvError>> {
        Box::pin(async move {
            match self.read_content(path, false, abort).await? {
                FileContent::Text { value } => Ok(value),
                FileContent::Binary { .. } => Err(EnvError::Other(
                    "guest agent returned binary content for a text read".to_owned(),
                )),
            }
        })
    }

    fn read_text_lines<'a>(
        &'a self,
        path: &'a Path,
        max_lines: Option<usize>,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<String>, EnvError>> {
        Box::pin(async move {
            let content = self.read_text_file(path, abort).await?;
            let lines = content.lines();
            Ok(match max_lines {
                Some(max_lines) => lines.take(max_lines).map(str::to_owned).collect(),
                None => lines.map(str::to_owned).collect(),
            })
        })
    }

    fn read_binary_file<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<u8>, EnvError>> {
        Box::pin(async move {
            match self.read_content(path, true, abort).await? {
                FileContent::Binary { base64 } => base64::engine::general_purpose::STANDARD
                    .decode(base64)
                    .map_err(|error| EnvError::Other(format!("invalid guest base64: {error}"))),
                FileContent::Text { .. } => Err(EnvError::Other(
                    "guest agent returned text content for a binary read".to_owned(),
                )),
            }
        })
    }

    fn write_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        self.write_file_internal(path, content, false, abort)
    }

    fn append_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        self.write_file_internal(path, content, true, abort)
    }

    fn file_info<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<FileInfo, EnvError>> {
        Box::pin(async move {
            let path = workspace_path(path)?;
            let response = self.request(AgentMessage::FileInfo { path }, abort).await?;
            match response {
                AgentMessage::FileInfoResult { entry } => convert_file_info(entry),
                AgentMessage::Error { message, .. } => Err(EnvError::Other(message)),
                other => Err(unexpected_response("file info", other)),
            }
        })
    }

    fn list_dir<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<FileInfo>, EnvError>> {
        Box::pin(async move {
            let path = workspace_path(path)?;
            let response = self.request(AgentMessage::ListDir { path }, abort).await?;
            match response {
                AgentMessage::ListDirResult { entries } => {
                    entries.into_iter().map(convert_file_info).collect()
                }
                AgentMessage::Error { message, .. } => Err(EnvError::Other(message)),
                other => Err(unexpected_response("list directory", other)),
            }
        })
    }

    fn exists<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<bool, EnvError>> {
        Box::pin(async move {
            let path = workspace_path(path)?;
            let response = self.request(AgentMessage::Exists { path }, abort).await?;
            match response {
                AgentMessage::ExistsResult { exists } => Ok(exists),
                AgentMessage::Error { message, .. } => Err(EnvError::Other(message)),
                other => Err(unexpected_response("exists", other)),
            }
        })
    }

    fn create_dir<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        Box::pin(async move {
            let path = workspace_path(path)?;
            let response = self
                .request(AgentMessage::CreateDir { path, recursive }, abort)
                .await?;
            expect_empty_response(response, "create directory")
        })
    }

    fn remove<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        force: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        Box::pin(async move {
            let path = workspace_path(path)?;
            let response = self
                .request(
                    AgentMessage::Remove {
                        path,
                        recursive,
                        force,
                    },
                    abort,
                )
                .await?;
            expect_empty_response(response, "remove")
        })
    }

    fn create_temp_dir<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<PathBuf, EnvError>> {
        let client = Arc::clone(&self.client);
        let prefix = prefix.to_owned();
        Box::pin(async move {
            let response = client
                .request(AgentMessage::CreateTempDir { prefix })
                .await
                .map_err(env_error)?;
            match response {
                AgentMessage::CreateTempDirResult { path } => Ok(PathBuf::from(path)),
                AgentMessage::Error { message, .. } => Err(EnvError::Other(message)),
                other => Err(unexpected_response("create temporary directory", other)),
            }
        })
    }

    fn execute_shell<'a>(
        &'a self,
        cmd: &'a str,
        opts: ShellOptions<'a>,
    ) -> BoxFuture<'a, Result<ShellOutput, EnvError>> {
        Box::pin(async move {
            if opts.abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let cwd = match opts.cwd {
                Some(path) => Some(workspace_path(path)?),
                None => None,
            };
            let env = opts
                .env
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect::<BTreeMap<_, _>>();
            let timeout_ms = opts
                .timeout
                .map(|timeout| {
                    u64::try_from(timeout.as_millis())
                        .map_err(|_| EnvError::Other("shell timeout is too large".to_owned()))
                })
                .transpose()?;
            let max_output_bytes = u64::try_from(
                opts.max_output_bytes
                    .unwrap_or(ShellOptions::DEFAULT_MAX_OUTPUT_BYTES),
            )
            .map_err(|_| EnvError::Other("shell output limit is too large".to_owned()))?;
            let command = cmd.to_owned();
            let start_client = Arc::clone(&self.client);
            let mut start_task = tokio::spawn(async move {
                start_client
                    .start_process(command, cwd, env, timeout_ms, max_output_bytes)
                    .await
            });
            let process = tokio::select! {
                result = &mut start_task => {
                    result
                        .map_err(|error| EnvError::Other(format!("guest process task failed: {error}")))?
                        .map_err(env_error)?
                }
                _ = opts.abort.cancelled() => {
                    let cleanup_client = Arc::clone(&self.client);
                    tokio::spawn(async move {
                        if let Ok(mut process) = start_task
                            .await
                            .map_err(|error| EnvError::Other(format!("guest process task failed: {error}")))
                            .and_then(|result| result.map_err(env_error))
                        {
                            let _ = cleanup_client
                                .cancel_process(&process.execution_id, true)
                                .await;
                            while process.events.recv().await.is_some() {}
                        }
                    });
                    return Err(EnvError::ShellAborted {
                        stdout: String::new(),
                        stderr: String::new(),
                        stdout_truncated_bytes: 0,
                        stderr_truncated_bytes: 0,
                    });
                }
            };

            let mut events = process.events;
            let mut stdout = String::new();
            let mut stderr = String::new();
            let mut on_stdout = opts.on_stdout;
            let mut on_stderr = opts.on_stderr;
            let mut cancel_sent = false;
            let mut aborted = false;

            loop {
                tokio::select! {
                    event = events.recv() => {
                        let Some(event) = event else {
                            return Err(EnvError::Other(
                                "guest agent process stream closed before exit".to_owned(),
                            ));
                        };
                        match event {
                            AgentMessage::ProcessStdout { chunk, .. } => {
                                if let Some(callback) = on_stdout.as_mut() {
                                    callback(&chunk);
                                }
                                stdout.push_str(&chunk);
                            }
                            AgentMessage::ProcessStderr { chunk, .. } => {
                                if let Some(callback) = on_stderr.as_mut() {
                                    callback(&chunk);
                                }
                                stderr.push_str(&chunk);
                            }
                            AgentMessage::ProcessExit { result, .. } => {
                                return terminal_result(result, stdout, stderr, aborted);
                            }
                            AgentMessage::ProcessTimeout { result, .. } => {
                                return Err(EnvError::ShellTimeout {
                                    stdout,
                                    stderr,
                                    stdout_truncated_bytes: result.stdout_truncated_bytes,
                                    stderr_truncated_bytes: result.stderr_truncated_bytes,
                                });
                            }
                            AgentMessage::ProcessError { message, .. } => {
                                return Err(EnvError::Other(message));
                            }
                            other => return Err(unexpected_response("process event", other)),
                        }
                    }
                    _ = opts.abort.cancelled(), if !cancel_sent => {
                        cancel_sent = true;
                        aborted = true;
                        if let Err(error) = self
                            .client
                            .cancel_process(&process.execution_id, true)
                            .await
                        {
                            return Err(EnvError::Other(format!(
                                "failed to cancel aborted guest process: {error}"
                            )));
                        }
                    }
                }
            }
        })
    }

    fn cleanup(&self) -> BoxFuture<'_, Result<(), EnvError>> {
        Box::pin(async move { self.client.cleanup().await.map_err(env_error) })
    }
}

impl GuestAgentEnv {
    fn write_file_internal<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        append: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        Box::pin(async move {
            let path = workspace_path(path)?;
            let response = self
                .request(
                    AgentMessage::WriteFile {
                        path,
                        content: FileContent::Binary {
                            base64: base64::engine::general_purpose::STANDARD.encode(content),
                        },
                        append,
                    },
                    abort,
                )
                .await?;
            expect_empty_response(response, "write file")
        })
    }
}

fn workspace_path(path: &Path) -> Result<String, EnvError> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        EnvError::Other(format!(
            "guest agent path must be UTF-8: {}",
            path.display()
        ))
    })
}

fn convert_file_info(entry: FileEntry) -> Result<FileInfo, EnvError> {
    let modified_ms = i64::try_from(entry.modified_ms)
        .map_err(|_| EnvError::Other("guest file modification time is invalid".to_owned()))?;
    let modified = DateTime::<Utc>::from_timestamp_millis(modified_ms)
        .ok_or_else(|| EnvError::Other("guest file modification time is invalid".to_owned()))?;
    Ok(FileInfo {
        path: PathBuf::from(entry.path),
        is_dir: entry.is_dir,
        size: entry.size,
        modified,
    })
}

fn terminal_result(
    result: ProcessResult,
    stdout: String,
    stderr: String,
    aborted: bool,
) -> Result<ShellOutput, EnvError> {
    if aborted {
        return Err(EnvError::ShellAborted {
            stdout,
            stderr,
            stdout_truncated_bytes: result.stdout_truncated_bytes,
            stderr_truncated_bytes: result.stderr_truncated_bytes,
        });
    }
    if result.exit_code != 0 {
        return Err(EnvError::ShellFailed {
            exit_code: result.exit_code,
            stdout,
            stderr,
            stdout_truncated_bytes: result.stdout_truncated_bytes,
            stderr_truncated_bytes: result.stderr_truncated_bytes,
        });
    }
    Ok(ShellOutput {
        stdout,
        stderr,
        exit_code: result.exit_code,
        stdout_truncated_bytes: result.stdout_truncated_bytes,
        stderr_truncated_bytes: result.stderr_truncated_bytes,
    })
}

fn expect_empty_response(response: AgentMessage, operation: &'static str) -> Result<(), EnvError> {
    match response {
        AgentMessage::WriteFileResult
        | AgentMessage::CreateDirResult
        | AgentMessage::RemoveResult => Ok(()),
        AgentMessage::Error { message, .. } => Err(EnvError::Other(message)),
        other => Err(unexpected_response(operation, other)),
    }
}

fn unexpected_response(operation: &'static str, message: AgentMessage) -> EnvError {
    EnvError::Other(format!(
        "unexpected guest agent response for {operation}: {message:?}"
    ))
}

fn env_error(error: GuestAgentClientError) -> EnvError {
    EnvError::Other(error.to_string())
}

#[cfg(target_os = "linux")]
#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_vm_agent_protocol::{Frame, ProtocolPeer};
    use llm_harness_vm_agent_service::{GuestAgentService, WorkspaceAgentHandler};
    use llm_harness_vm_agent_transport::FrameStream;
    use tempfile::TempDir;
    use tokio::io::duplex;
    use tokio::task::JoinHandle;

    fn test_environment() -> (
        TempDir,
        GuestAgentEnv,
        JoinHandle<Result<(), llm_harness_vm_agent_service::ServiceError>>,
    ) {
        let root = tempfile::tempdir().expect("workspace root");
        let (host_io, guest_io) = duplex(1024 * 1024);
        let client = GuestAgentClient::new(host_io).expect("guest client");
        let env = GuestAgentEnv::new(client);
        let guest_stream =
            FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest).expect("guest stream");
        let service = GuestAgentService::new(
            WorkspaceAgentHandler::new(root.path()).expect("workspace agent handler"),
        );
        let task = tokio::spawn(async move { service.serve(guest_stream).await });
        (root, env, task)
    }

    #[tokio::test]
    async fn files_and_shell_round_trip_through_the_guest_agent() {
        let (root, env, service) = test_environment();
        let abort = CancellationToken::new();
        let path = Path::new("/workspace/file.txt");

        env.write_file(path, b"first\n", abort.clone())
            .await
            .expect("write file");
        env.append_file(path, b"second\n", abort.clone())
            .await
            .expect("append file");
        assert_eq!(
            env.read_text_file(path, abort.clone())
                .await
                .expect("read text"),
            "first\nsecond\n"
        );
        assert_eq!(
            env.read_binary_file(path, abort.clone())
                .await
                .expect("read binary"),
            b"first\nsecond\n"
        );
        let info = env.file_info(path, abort.clone()).await.expect("file info");
        assert_eq!(info.path, path);
        assert_eq!(info.size, 13);
        assert_eq!(
            env.list_dir(Path::new("/workspace"), abort.clone())
                .await
                .expect("list directory")
                .len(),
            1
        );

        let mut stdout_chunks = Vec::new();
        let output = env
            .execute_shell(
                "printf guest-output",
                ShellOptions {
                    cwd: None,
                    env: Vec::new(),
                    timeout: Some(std::time::Duration::from_secs(5)),
                    abort: CancellationToken::new(),
                    on_stdout: Some(Box::new(|chunk| stdout_chunks.push(chunk.to_owned()))),
                    on_stderr: None,
                    max_output_bytes: None,
                },
            )
            .await
            .expect("execute shell");
        assert_eq!(output.stdout, "guest-output");
        assert_eq!(stdout_chunks, vec!["guest-output".to_owned()]);
        assert!(root.path().join("file.txt").is_file());
        drop(env);
        service.await.expect("service task").expect("service ok");
    }

    #[tokio::test]
    async fn shell_nonzero_exit_maps_to_shell_failed() {
        let (_root, env, service) = test_environment();
        let result = env
            .execute_shell(
                "exit 7",
                ShellOptions {
                    cwd: None,
                    env: Vec::new(),
                    timeout: Some(std::time::Duration::from_secs(5)),
                    abort: CancellationToken::new(),
                    on_stdout: None,
                    on_stderr: None,
                    max_output_bytes: None,
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(EnvError::ShellFailed { exit_code: 7, .. })
        ));
        drop(env);
        service.await.expect("service task").expect("service ok");
    }

    #[tokio::test]
    async fn shell_abort_cancels_the_guest_process() {
        let (_root, env, service) = test_environment();
        let abort = CancellationToken::new();
        let cancelled = abort.clone();
        let shell_task = tokio::spawn(async move {
            env.execute_shell(
                "sleep 30",
                ShellOptions {
                    cwd: None,
                    env: Vec::new(),
                    timeout: Some(std::time::Duration::from_secs(30)),
                    abort,
                    on_stdout: None,
                    on_stderr: None,
                    max_output_bytes: None,
                },
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancelled.cancel();
        let result = shell_task.await.expect("shell task");
        assert!(matches!(result, Err(EnvError::ShellAborted { .. })));
        service.await.expect("service task").expect("service ok");
    }

    #[tokio::test]
    async fn shell_abort_during_startup_cancels_after_process_start() {
        let (host_io, guest_io) = duplex(64 * 1024);
        let client = GuestAgentClient::new(host_io).expect("guest client");
        let env = GuestAgentEnv::new(client);
        let abort = CancellationToken::new();
        let cancelled = abort.clone();
        let shell_task = tokio::spawn(async move {
            env.execute_shell(
                "sleep 30",
                ShellOptions {
                    cwd: None,
                    env: Vec::new(),
                    timeout: Some(std::time::Duration::from_secs(30)),
                    abort,
                    on_stdout: None,
                    on_stderr: None,
                    max_output_bytes: None,
                },
            )
            .await
        });
        let mut guest =
            FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest).expect("guest stream");
        let start_request = guest.recv().await.expect("start request");
        let start_request_id = start_request.request_id();
        assert!(matches!(
            start_request.into_message(),
            AgentMessage::StartProcess { .. }
        ));

        cancelled.cancel();
        let result = shell_task.await.expect("shell task");
        assert!(matches!(result, Err(EnvError::ShellAborted { .. })));

        guest
            .send(
                Frame::new(
                    ProtocolPeer::Guest,
                    start_request_id,
                    AgentMessage::ProcessStarted {
                        execution_id: "execution-startup-abort".to_owned(),
                        pid: 42,
                    },
                )
                .expect("process started frame"),
            )
            .await
            .expect("send process started");

        let cancel_request = guest.recv().await.expect("cancel request");
        let cancel_request_id = cancel_request.request_id();
        assert!(matches!(
            cancel_request.into_message(),
            AgentMessage::CancelProcess { force: true, .. }
        ));
        guest
            .send(
                Frame::new(
                    ProtocolPeer::Guest,
                    cancel_request_id,
                    AgentMessage::CancelProcessResult { cancelled: true },
                )
                .expect("cancel result frame"),
            )
            .await
            .expect("send cancel result");
        guest
            .send(
                Frame::new(
                    ProtocolPeer::Guest,
                    start_request_id,
                    AgentMessage::ProcessExit {
                        execution_id: "execution-startup-abort".to_owned(),
                        result: Default::default(),
                    },
                )
                .expect("process exit frame"),
            )
            .await
            .expect("send process exit");
        guest.close().await.expect("close guest stream");
    }

    #[tokio::test]
    async fn file_request_abort_still_consumes_the_late_response() {
        let (host_io, guest_io) = duplex(64 * 1024);
        let client = GuestAgentClient::new(host_io).expect("guest client");
        let env = GuestAgentEnv::new(client);
        let abort = CancellationToken::new();
        let cancelled = abort.clone();
        let read_task = tokio::spawn(async move {
            let result = env
                .read_text_file(Path::new("/workspace/late.txt"), abort)
                .await;
            (env, result)
        });
        let mut guest =
            FrameStream::with_default_limit(guest_io, ProtocolPeer::Guest).expect("guest stream");
        let read_request = guest.recv().await.expect("read request");
        let read_request_id = read_request.request_id();
        assert!(matches!(
            read_request.into_message(),
            AgentMessage::ReadFile { .. }
        ));

        cancelled.cancel();
        guest
            .send(
                Frame::new(
                    ProtocolPeer::Guest,
                    read_request_id,
                    AgentMessage::ReadFileResult {
                        content: FileContent::Text {
                            value: "late".to_owned(),
                        },
                    },
                )
                .expect("read result frame"),
            )
            .await
            .expect("send late read result");
        let (env, result) = read_task.await.expect("read task");
        assert!(matches!(result, Err(EnvError::Aborted)));

        let ping_task = tokio::spawn(async move { env.client().ping().await });
        let ping_request = guest.recv().await.expect("ping request");
        let ping_request_id = ping_request.request_id();
        assert_eq!(ping_request.into_message(), AgentMessage::Ping);
        guest
            .send(
                Frame::new(ProtocolPeer::Guest, ping_request_id, AgentMessage::Pong)
                    .expect("pong frame"),
            )
            .await
            .expect("send pong");
        ping_task
            .await
            .expect("ping task")
            .expect("ping after late response");
        guest.close().await.expect("close guest stream");
    }
}
