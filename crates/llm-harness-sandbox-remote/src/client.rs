use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use futures::StreamExt;
use llm_harness_platform::sandbox::SandboxConfig;
use llm_harness_types::ShellOutput;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::{RemoteSandboxClientConfig, RemoteSandboxConfig};
use crate::error::RemoteSandboxError;
use crate::protocol::{
    CreateSandboxRequest, CreateSandboxResponse, FileContent, FileInfoRequest, FileInfoResponse,
    FileInfoResponseValue, GatewayErrorResponse, ListDirRequest, ListDirResponse, ReadFileRequest,
    ReadFileResponse, RemoteExecEvent, RemoteSandboxStatus, ShellExecRequest, ShellExecResponse,
    WriteFileRequest,
};
use crate::sse::{SseDecoder, SseEvent};

type ShellCallback<'a> = Box<dyn FnMut(&str) + Send + 'a>;

/// HTTP client for the remote sandbox gateway.
#[derive(Debug, Clone)]
pub struct RemoteSandboxClient {
    config: RemoteSandboxClientConfig,
    http: Client,
}

impl RemoteSandboxClient {
    /// Create a client from a validated configuration.
    pub fn new(config: RemoteSandboxClientConfig) -> Result<Self, RemoteSandboxError> {
        config.validate()?;
        let http = Client::builder()
            .connect_timeout(config.connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .user_agent(concat!(
                "llm-harness-sandbox-remote/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|error| {
                RemoteSandboxError::transport(format!("build HTTP client: {error}"))
            })?;
        Ok(Self { config, http })
    }

    /// Create a sandbox on the gateway.
    pub async fn create_sandbox(
        &self,
        config: SandboxConfig,
    ) -> Result<CreateSandboxResponse, RemoteSandboxError> {
        let config = RemoteSandboxConfig::try_from(config)?;
        self.request_json(
            Method::POST,
            "v1/sandboxes",
            Some(CreateSandboxRequest { config }),
            None,
            StatusCode::CREATED,
        )
        .await
    }

    /// Get sandbox status from the gateway.
    pub async fn get_sandbox(
        &self,
        sandbox_id: &str,
    ) -> Result<RemoteSandboxStatus, RemoteSandboxError> {
        let sandbox_id = path_segment(sandbox_id, "sandbox id")?;
        self.request_json(
            Method::GET,
            &format!("v1/sandboxes/{sandbox_id}"),
            None::<&()>,
            None,
            StatusCode::OK,
        )
        .await
    }

    /// Reset a sandbox to its initial state.
    pub async fn reset_sandbox(&self, sandbox_id: &str) -> Result<(), RemoteSandboxError> {
        let sandbox_id = path_segment(sandbox_id, "sandbox id")?;
        self.request_empty(
            Method::POST,
            &format!("v1/sandboxes/{sandbox_id}/reset"),
            Some(serde_json::json!({})),
            None,
        )
        .await?;
        Ok(())
    }

    /// Delete a sandbox from the gateway.
    pub async fn delete_sandbox(&self, sandbox_id: &str) -> Result<(), RemoteSandboxError> {
        let sandbox_id = path_segment(sandbox_id, "sandbox id")?;
        self.request_empty(
            Method::DELETE,
            &format!("v1/sandboxes/{sandbox_id}"),
            None::<&()>,
            None,
        )
        .await?;
        Ok(())
    }

    /// Clean temporary resources without resetting the sandbox.
    pub async fn cleanup_sandbox(&self, sandbox_id: &str) -> Result<(), RemoteSandboxError> {
        let sandbox_id = path_segment(sandbox_id, "sandbox id")?;
        self.request_empty(
            Method::POST,
            &format!("v1/sandboxes/{sandbox_id}/cleanup"),
            Some(serde_json::json!({})),
            None,
        )
        .await?;
        Ok(())
    }

    /// Read a text or binary file from the sandbox.
    pub async fn read_file(
        &self,
        sandbox_id: &str,
        path: &Path,
        binary: bool,
    ) -> Result<Vec<u8>, RemoteSandboxError> {
        let sandbox_id = path_segment(sandbox_id, "sandbox id")?;
        let path = path_string(path)?;
        let error_path = path.clone();
        let response: ReadFileResponse = self
            .request_json(
                Method::POST,
                &format!("v1/sandboxes/{sandbox_id}/fs/read"),
                Some(ReadFileRequest { path, binary }),
                Some(error_path),
                StatusCode::OK,
            )
            .await?;
        match response.content {
            FileContent::Text { value } if !binary => Ok(value.into_bytes()),
            FileContent::Binary { base64 } if binary => base64::engine::general_purpose::STANDARD
                .decode(&base64)
                .map_err(|error| RemoteSandboxError::protocol(format!("invalid base64: {error}"))),
            _ => Err(RemoteSandboxError::protocol(
                "gateway returned mismatched file content kind",
            )),
        }
    }

    /// Write or append a file in the sandbox.
    pub async fn write_file(
        &self,
        sandbox_id: &str,
        path: &Path,
        content: &[u8],
        append: bool,
    ) -> Result<(), RemoteSandboxError> {
        let sandbox_id = path_segment(sandbox_id, "sandbox id")?;
        let path = path_string(path)?;
        let error_path = path.clone();
        let content = if let Ok(text) = std::str::from_utf8(content) {
            FileContent::Text {
                value: text.to_owned(),
            }
        } else {
            FileContent::Binary {
                base64: base64::engine::general_purpose::STANDARD.encode(content),
            }
        };
        self.request_empty(
            Method::POST,
            &format!("v1/sandboxes/{sandbox_id}/fs/write"),
            Some(WriteFileRequest {
                path,
                content,
                append,
            }),
            Some(error_path),
        )
        .await?;
        Ok(())
    }

    /// Get file metadata.
    pub async fn file_info(
        &self,
        sandbox_id: &str,
        path: &Path,
    ) -> Result<FileInfoResponse, RemoteSandboxError> {
        let sandbox_id = path_segment(sandbox_id, "sandbox id")?;
        let path = path_string(path)?;
        let error_path = path.clone();
        let response: FileInfoResponseValue = self
            .request_json(
                Method::POST,
                &format!("v1/sandboxes/{sandbox_id}/fs/info"),
                Some(FileInfoRequest { path }),
                Some(error_path),
                StatusCode::OK,
            )
            .await?;
        Ok(response.info)
    }

    /// List a directory.
    pub async fn list_dir(
        &self,
        sandbox_id: &str,
        path: &Path,
    ) -> Result<ListDirResponse, RemoteSandboxError> {
        let sandbox_id = path_segment(sandbox_id, "sandbox id")?;
        let path = path_string(path)?;
        let error_path = path.clone();
        self.request_json(
            Method::POST,
            &format!("v1/sandboxes/{sandbox_id}/fs/list"),
            Some(ListDirRequest { path }),
            Some(error_path),
            StatusCode::OK,
        )
        .await
    }

    /// Check whether a path exists.
    pub async fn exists(&self, sandbox_id: &str, path: &Path) -> Result<bool, RemoteSandboxError> {
        let sandbox_id = path_segment(sandbox_id, "sandbox id")?;
        let path = path_string(path)?;
        let error_path = path.clone();
        let response: crate::protocol::ExistsResponse = self
            .request_json(
                Method::POST,
                &format!("v1/sandboxes/{sandbox_id}/fs/exists"),
                Some(crate::protocol::ExistsRequest { path }),
                Some(error_path),
                StatusCode::OK,
            )
            .await?;
        Ok(response.exists)
    }

    /// Create a directory.
    pub async fn create_dir(
        &self,
        sandbox_id: &str,
        path: &Path,
        recursive: bool,
    ) -> Result<(), RemoteSandboxError> {
        let sandbox_id = path_segment(sandbox_id, "sandbox id")?;
        let path = path_string(path)?;
        let error_path = path.clone();
        self.request_empty(
            Method::POST,
            &format!("v1/sandboxes/{sandbox_id}/fs/create-dir"),
            Some(crate::protocol::CreateDirRequest { path, recursive }),
            Some(error_path),
        )
        .await?;
        Ok(())
    }

    /// Remove a file or directory.
    pub async fn remove(
        &self,
        sandbox_id: &str,
        path: &Path,
        recursive: bool,
        force: bool,
    ) -> Result<(), RemoteSandboxError> {
        let sandbox_id = path_segment(sandbox_id, "sandbox id")?;
        let path = path_string(path)?;
        let error_path = path.clone();
        self.request_empty(
            Method::POST,
            &format!("v1/sandboxes/{sandbox_id}/fs/remove"),
            Some(crate::protocol::RemoveRequest {
                path,
                recursive,
                force,
            }),
            Some(error_path),
        )
        .await?;
        Ok(())
    }

    /// Create a temporary directory in the sandbox.
    pub async fn create_temp_dir(
        &self,
        sandbox_id: &str,
        prefix: &str,
    ) -> Result<String, RemoteSandboxError> {
        let sandbox_id = path_segment(sandbox_id, "sandbox id")?;
        validate_temp_dir_prefix(prefix)?;
        let response: crate::protocol::CreateTempDirResponse = self
            .request_json(
                Method::POST,
                &format!("v1/sandboxes/{sandbox_id}/fs/create-temp-dir"),
                Some(crate::protocol::CreateTempDirRequest {
                    prefix: prefix.to_owned(),
                }),
                None,
                StatusCode::OK,
            )
            .await?;
        Ok(response.path)
    }

    /// Execute a shell command and consume its SSE event stream.
    pub async fn execute_shell(
        &self,
        sandbox_id: &str,
        mut request: ShellExecRequest,
        mut on_stdout: Option<ShellCallback<'_>>,
        mut on_stderr: Option<ShellCallback<'_>>,
        abort: CancellationToken,
    ) -> Result<ShellOutput, RemoteSandboxError> {
        if abort.is_cancelled() {
            return Err(RemoteSandboxError::Aborted);
        }
        let sandbox_id = path_segment(sandbox_id, "sandbox id")?;
        if request.max_output_bytes == 0 {
            return Err(RemoteSandboxError::protocol(
                "max output bytes must be positive",
            ));
        }
        validate_shell_request(&request)?;
        let max_exec_timeout_ms = self.max_exec_timeout_ms()?;
        request.timeout_ms = Some(
            request
                .timeout_ms
                .unwrap_or(max_exec_timeout_ms)
                .min(max_exec_timeout_ms),
        );
        let max_output_bytes = request.max_output_bytes;

        let timeout = Duration::from_millis(request.timeout_ms.unwrap()) + Duration::from_secs(5);
        let max_sse_event_bytes = max_sse_event_bytes(request.max_output_bytes);
        let url = self
            .config
            .endpoint
            .join(&format!("v1/sandboxes/{sandbox_id}/exec"))
            .map_err(|error| RemoteSandboxError::transport(format!("build exec URL: {error}")))?;
        let response = self
            .http
            .post(url)
            .header(
                AUTHORIZATION,
                format!("Bearer {}", self.config.bearer_token),
            )
            .header(ACCEPT, "text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .header("x-request-id", Uuid::new_v4().to_string())
            .timeout(timeout)
            .json(&request)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let status = response.status();
        if !status.is_success() {
            return Err(self.map_gateway_error(response, None).await);
        }
        if status != StatusCode::OK {
            return Err(RemoteSandboxError::protocol(format!(
                "expected 200 OK from gateway, got {status}"
            )));
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if !media_type(&content_type).eq_ignore_ascii_case("text/event-stream") {
            return Err(RemoteSandboxError::protocol(format!(
                "unexpected exec content type: {content_type}"
            )));
        }

        let mut execution_id = response
            .headers()
            .get("x-execution-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if let Some(execution_id) = execution_id.as_deref() {
            path_segment(execution_id, "execution id")?;
        }
        let mut decoder = SseDecoder::default();
        let mut stream = response.bytes_stream();
        let mut kill_requested = false;
        let mut kill_sent = false;
        let mut aborted = false;

        loop {
            tokio::select! {
                biased;
                _ = abort.cancelled(), if !kill_requested => {
                    kill_requested = true;
                    aborted = true;
                    if let Some(execution_id) = execution_id.as_deref() {
                        kill_sent = true;
                        self.abort_execution(sandbox_id, execution_id).await?;
                    }
                }
                chunk = stream.next() => {
                    let chunk = chunk
                        .transpose()
                        .map_err(Self::map_reqwest_error)?;
                    let Some(chunk) = chunk else {
                        let final_event = decoder.finish().map_err(RemoteSandboxError::protocol)?;
                        if let Some(event) = final_event
                            && let Some(result) = self.parse_exec_event(
                                event,
                                &mut execution_id,
                                &mut on_stdout,
                                &mut on_stderr,
                                aborted,
                                max_output_bytes,
                            )?
                        {
                            return Ok(result);
                        }
                        return Err(RemoteSandboxError::protocol(
                            "shell execution stream ended without an exit event",
                        ));
                    };

                    let events = decoder
                        .push(&chunk, max_sse_event_bytes)
                        .map_err(RemoteSandboxError::protocol)?;
                    for event in events {
                        if let Some(result) = self.parse_exec_event(
                            event,
                            &mut execution_id,
                            &mut on_stdout,
                            &mut on_stderr,
                            aborted,
                            max_output_bytes,
                        )? {
                            return Ok(result);
                        }
                        if kill_requested
                            && !kill_sent
                            && let Some(execution_id) = execution_id.as_deref()
                        {
                            kill_sent = true;
                            self.abort_execution(sandbox_id, execution_id).await?;
                        }
                    }
                }
            }
        }
    }

    /// Kill a running shell execution.
    pub async fn kill_execution(
        &self,
        sandbox_id: &str,
        execution_id: &str,
    ) -> Result<(), RemoteSandboxError> {
        let sandbox_id = path_segment(sandbox_id, "sandbox id")?;
        let execution_id = path_segment(execution_id, "execution id")?;
        self.request_empty(
            Method::POST,
            &format!("v1/sandboxes/{sandbox_id}/exec/{execution_id}/kill"),
            Some(serde_json::json!({})),
            None,
        )
        .await?;
        Ok(())
    }

    async fn abort_execution(
        &self,
        sandbox_id: &str,
        execution_id: &str,
    ) -> Result<(), RemoteSandboxError> {
        match self.kill_execution(sandbox_id, execution_id).await {
            Err(error) if is_execution_not_found(&error) => Ok(()),
            result => result,
        }
    }

    fn max_exec_timeout_ms(&self) -> Result<u64, RemoteSandboxError> {
        u64::try_from(self.config.max_exec_timeout.as_millis()).map_err(|_| {
            RemoteSandboxError::invalid_config("remote sandbox max execution timeout is too large")
        })
    }

    fn parse_exec_event(
        &self,
        event: SseEvent,
        execution_id: &mut Option<String>,
        on_stdout: &mut Option<ShellCallback<'_>>,
        on_stderr: &mut Option<ShellCallback<'_>>,
        aborted: bool,
        max_output_bytes: usize,
    ) -> Result<Option<ShellOutput>, RemoteSandboxError> {
        let value: RemoteExecEvent = serde_json::from_str(&event.data)
            .map_err(|error| RemoteSandboxError::protocol(format!("decode exec event: {error}")))?;
        let expected_event_name = remote_exec_event_name(&value);
        if event.event.as_deref() != Some(expected_event_name) {
            return Err(RemoteSandboxError::protocol(
                "SSE event name does not match the execution event type",
            ));
        }
        match value {
            RemoteExecEvent::Started { execution_id: id } => {
                path_segment(&id, "execution id")?;
                if let Some(existing) = execution_id.as_deref()
                    && existing != id
                {
                    return Err(RemoteSandboxError::protocol(
                        "gateway returned inconsistent execution ids",
                    ));
                }
                *execution_id = Some(id);
                Ok(None)
            }
            RemoteExecEvent::Stdout { chunk } => {
                if execution_id.is_none() {
                    return Err(RemoteSandboxError::protocol(
                        "stdout event received before execution id",
                    ));
                }
                if let Some(callback) = on_stdout.as_deref_mut() {
                    callback(&chunk);
                }
                Ok(None)
            }
            RemoteExecEvent::Stderr { chunk } => {
                if execution_id.is_none() {
                    return Err(RemoteSandboxError::protocol(
                        "stderr event received before execution id",
                    ));
                }
                if let Some(callback) = on_stderr.as_deref_mut() {
                    callback(&chunk);
                }
                Ok(None)
            }
            RemoteExecEvent::Exit { result } => {
                if execution_id.is_none() {
                    return Err(RemoteSandboxError::protocol(
                        "exit event received before execution id",
                    ));
                }
                validate_shell_output(&result, max_output_bytes)?;
                let output = ShellOutput {
                    stdout: result.stdout,
                    stderr: result.stderr,
                    exit_code: result.exit_code,
                    stdout_truncated_bytes: result.stdout_truncated_bytes,
                    stderr_truncated_bytes: result.stderr_truncated_bytes,
                };
                if aborted {
                    Err(RemoteSandboxError::ShellAborted {
                        stdout: output.stdout,
                        stderr: output.stderr,
                        stdout_truncated_bytes: output.stdout_truncated_bytes,
                        stderr_truncated_bytes: output.stderr_truncated_bytes,
                    })
                } else {
                    Ok(Some(output))
                }
            }
            RemoteExecEvent::Timeout { result } => {
                if execution_id.is_none() {
                    return Err(RemoteSandboxError::protocol(
                        "timeout event received before execution id",
                    ));
                }
                validate_shell_output(&result, max_output_bytes)?;
                Err(RemoteSandboxError::ShellTimeout {
                    stdout: result.stdout,
                    stderr: result.stderr,
                    stdout_truncated_bytes: result.stdout_truncated_bytes,
                    stderr_truncated_bytes: result.stderr_truncated_bytes,
                })
            }
            RemoteExecEvent::Error { code, message } => {
                if execution_id.is_none() {
                    return Err(RemoteSandboxError::protocol(
                        "error event received before execution id",
                    ));
                }
                Err(RemoteSandboxError::gateway(500, code, message, None))
            }
        }
    }

    async fn request_json<Req, Res>(
        &self,
        method: Method,
        path: &str,
        body: Option<Req>,
        path_for_error: Option<String>,
        expected_status: StatusCode,
    ) -> Result<Res, RemoteSandboxError>
    where
        Req: Serialize,
        Res: DeserializeOwned,
    {
        let url = self.config.endpoint.join(path).map_err(|error| {
            RemoteSandboxError::transport(format!("build request URL: {error}"))
        })?;
        let mut request = self
            .http
            .request(method, url)
            .header(
                AUTHORIZATION,
                format!("Bearer {}", self.config.bearer_token),
            )
            .header(ACCEPT, "application/json")
            .header("x-request-id", Uuid::new_v4().to_string())
            .timeout(self.config.request_timeout);
        if let Some(body) = body {
            request = request.header(CONTENT_TYPE, "application/json").json(&body);
        }

        let response = request.send().await.map_err(Self::map_reqwest_error)?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.map_gateway_error(response, path_for_error).await);
        }
        if status != expected_status {
            return Err(RemoteSandboxError::protocol(format!(
                "expected {expected_status} from gateway, got {status}"
            )));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if !media_type(&content_type).eq_ignore_ascii_case("application/json") {
            return Err(RemoteSandboxError::protocol(format!(
                "unexpected gateway content type: {content_type}"
            )));
        }

        let body = read_response_body(response, self.config.max_response_bytes).await?;
        serde_json::from_slice::<Res>(&body).map_err(|error| {
            RemoteSandboxError::protocol(format!("decode gateway response: {error}"))
        })
    }

    async fn request_empty<Req>(
        &self,
        method: Method,
        path: &str,
        body: Option<Req>,
        path_for_error: Option<String>,
    ) -> Result<(), RemoteSandboxError>
    where
        Req: Serialize,
    {
        let url = self.config.endpoint.join(path).map_err(|error| {
            RemoteSandboxError::transport(format!("build request URL: {error}"))
        })?;
        let mut request = self
            .http
            .request(method, url)
            .header(
                AUTHORIZATION,
                format!("Bearer {}", self.config.bearer_token),
            )
            .header(ACCEPT, "application/json")
            .header("x-request-id", Uuid::new_v4().to_string())
            .timeout(self.config.request_timeout);
        if let Some(body) = body {
            request = request.header(CONTENT_TYPE, "application/json").json(&body);
        }

        let response = request.send().await.map_err(Self::map_reqwest_error)?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.map_gateway_error(response, path_for_error).await);
        }
        if status != reqwest::StatusCode::NO_CONTENT {
            return Err(RemoteSandboxError::protocol(format!(
                "expected 204 No Content from gateway, got {status}"
            )));
        }
        let body = read_response_body(response, 0).await?;
        if !body.is_empty() {
            return Err(RemoteSandboxError::protocol(
                "gateway returned a body with 204 No Content",
            ));
        }

        Ok(())
    }

    async fn map_gateway_error(
        &self,
        response: reqwest::Response,
        path_for_error: Option<String>,
    ) -> RemoteSandboxError {
        let status = response.status().as_u16();
        let body = match read_response_body(response, 64 * 1024).await {
            Ok(body) => String::from_utf8_lossy(&body).into_owned(),
            Err(error) => format!("unreadable gateway error body: {error}"),
        };
        match serde_json::from_slice::<GatewayErrorResponse>(body.as_bytes()) {
            Ok(error) => RemoteSandboxError::gateway(
                status,
                error.code,
                error.message,
                error
                    .path
                    .map(PathBuf::from)
                    .or_else(|| path_for_error.map(PathBuf::from)),
            ),
            Err(_) => RemoteSandboxError::gateway(status, "invalid_gateway_error", body, None),
        }
    }

    fn map_reqwest_error(error: reqwest::Error) -> RemoteSandboxError {
        if error.is_timeout() {
            RemoteSandboxError::Timeout
        } else {
            RemoteSandboxError::transport(error.to_string())
        }
    }
}

pub(crate) fn path_string(path: &Path) -> Result<String, RemoteSandboxError> {
    let raw_path = path.to_str().ok_or_else(|| {
        RemoteSandboxError::invalid_config(format!(
            "remote sandbox path must be valid UTF-8: {}",
            path.display()
        ))
    })?;
    let path = raw_path.replace('\\', "/");
    validate_path_string(&path)?;
    Ok(path)
}

pub(crate) fn validate_path_string(path: &str) -> Result<(), RemoteSandboxError> {
    if !path.starts_with('/') {
        return Err(RemoteSandboxError::invalid_config(format!(
            "remote sandbox path must be absolute: {path}"
        )));
    }
    if path
        .split('/')
        .any(|component| component == "." || component == "..")
    {
        return Err(RemoteSandboxError::invalid_config(format!(
            "remote sandbox path must not contain `.` or `..` components: {path}"
        )));
    }
    if path.contains('\0') {
        return Err(RemoteSandboxError::invalid_config(
            "remote sandbox path must not contain NUL".to_owned(),
        ));
    }
    if path.contains('\\') {
        return Err(RemoteSandboxError::invalid_config(
            "remote sandbox path must not contain backslashes".to_owned(),
        ));
    }
    Ok(())
}

fn validate_shell_request(request: &ShellExecRequest) -> Result<(), RemoteSandboxError> {
    if request.timeout_ms == Some(0) {
        return Err(RemoteSandboxError::protocol(
            "shell timeout must be at least one millisecond",
        ));
    }
    if let Some(cwd) = request.cwd.as_deref() {
        validate_path_string(cwd)
            .map_err(|_| RemoteSandboxError::protocol("invalid shell working directory"))?;
    }
    for (key, value) in &request.env {
        validate_environment_pair(key, value)?;
    }
    Ok(())
}

fn validate_environment_pair(key: &str, value: &str) -> Result<(), RemoteSandboxError> {
    if key.is_empty() || key.contains('=') || key.contains('\0') {
        return Err(RemoteSandboxError::protocol(
            "shell environment keys must be non-empty and must not contain `=` or NUL",
        ));
    }
    if value.contains('\0') {
        return Err(RemoteSandboxError::protocol(
            "shell environment values must not contain NUL",
        ));
    }
    Ok(())
}

fn validate_temp_dir_prefix(prefix: &str) -> Result<(), RemoteSandboxError> {
    if prefix.is_empty()
        || prefix.contains('/')
        || prefix.contains('\\')
        || prefix.contains('\0')
        || prefix == "."
        || prefix == ".."
    {
        return Err(RemoteSandboxError::protocol(
            "temporary directory prefix must be a non-empty path component",
        ));
    }
    Ok(())
}

pub(crate) fn path_segment<'a>(value: &'a str, field: &str) -> Result<&'a str, RemoteSandboxError> {
    if value.is_empty()
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
    {
        return Err(RemoteSandboxError::protocol(format!(
            "invalid gateway {field}: identifiers must be non-empty alphanumeric, dash, or underscore"
        )));
    }
    Ok(value)
}

async fn read_response_body(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, RemoteSandboxError> {
    if let Some(length) = response.content_length()
        && length > max_bytes as u64
    {
        return Err(RemoteSandboxError::protocol(format!(
            "gateway response body exceeds {max_bytes} bytes"
        )));
    }

    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(RemoteSandboxClient::map_reqwest_error)?;
        if chunk.len() > max_bytes - body.len() {
            return Err(RemoteSandboxError::protocol(format!(
                "gateway response body exceeds {max_bytes} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn max_sse_event_bytes(max_output_bytes: usize) -> usize {
    max_output_bytes.saturating_mul(6).saturating_add(1024)
}

fn media_type(content_type: &str) -> &str {
    content_type.split(';').next().unwrap_or_default().trim()
}

fn validate_shell_output(
    result: &ShellExecResponse,
    max_output_bytes: usize,
) -> Result<(), RemoteSandboxError> {
    if result.stdout.len() > max_output_bytes || result.stderr.len() > max_output_bytes {
        return Err(RemoteSandboxError::protocol(
            "gateway returned shell output exceeding the requested limit",
        ));
    }
    Ok(())
}

fn is_execution_not_found(error: &RemoteSandboxError) -> bool {
    matches!(
        error,
        RemoteSandboxError::Gateway {
            status: 404,
            code,
            ..
        } if code == "execution_not_found"
    )
}

fn remote_exec_event_name(event: &RemoteExecEvent) -> &'static str {
    match event {
        RemoteExecEvent::Started { .. } => "started",
        RemoteExecEvent::Stdout { .. } => "stdout",
        RemoteExecEvent::Stderr { .. } => "stderr",
        RemoteExecEvent::Exit { .. } => "exit",
        RemoteExecEvent::Timeout { .. } => "timeout",
        RemoteExecEvent::Error { .. } => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_invalid_shell_cwd_before_sending_request() {
        let config =
            RemoteSandboxClientConfig::new("http://127.0.0.1:8080", "token").expect("valid config");
        let client = RemoteSandboxClient::new(config).expect("valid client");
        let request = ShellExecRequest {
            command: "true".to_owned(),
            cwd: Some("relative".to_owned()),
            env: std::collections::BTreeMap::new(),
            timeout_ms: None,
            max_output_bytes: 1024,
        };
        let result = client
            .execute_shell("sandbox", request, None, None, CancellationToken::new())
            .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("relative cwd should be rejected"),
        };
        assert!(matches!(error, RemoteSandboxError::Protocol(_)));
    }

    #[tokio::test]
    async fn rejects_temp_dir_prefix_that_is_not_a_path_component() {
        let config =
            RemoteSandboxClientConfig::new("http://127.0.0.1:8080", "token").expect("valid config");
        let client = RemoteSandboxClient::new(config).expect("valid client");
        let result = client.create_temp_dir("agent/escape", "sandbox").await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("path-like prefix should be rejected"),
        };
        assert!(matches!(error, RemoteSandboxError::Protocol(_)));
    }

    #[tokio::test]
    async fn rejects_sse_event_name_that_does_not_match_payload_type() {
        let config =
            RemoteSandboxClientConfig::new("http://127.0.0.1:8080", "token").expect("valid config");
        let client = RemoteSandboxClient::new(config).expect("valid client");
        let event = SseEvent {
            event: Some("stdout".to_owned()),
            data: r#"{"type":"started","execution_id":"execution"}"#.to_owned(),
        };
        let result = client.parse_exec_event(event, &mut None, &mut None, &mut None, false, 1024);
        assert!(matches!(result, Err(RemoteSandboxError::Protocol(_))));
    }

    #[tokio::test]
    async fn rejects_sub_millisecond_shell_timeout_before_sending_request() {
        let config =
            RemoteSandboxClientConfig::new("http://127.0.0.1:8080", "token").expect("valid config");
        let client = RemoteSandboxClient::new(config).expect("build client");
        let request = ShellExecRequest {
            command: "true".to_owned(),
            cwd: None,
            env: std::collections::BTreeMap::new(),
            timeout_ms: Some(0),
            max_output_bytes: 1024,
        };
        let result = client
            .execute_shell("sandbox", request, None, None, CancellationToken::new())
            .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("sub-millisecond timeout should be rejected"),
        };
        assert!(matches!(error, RemoteSandboxError::Protocol(_)));
    }

    #[test]
    fn rejects_shell_results_exceeding_output_limit() {
        let result = ShellExecResponse {
            exit_code: 0,
            stdout: "0123456789".to_owned(),
            stderr: String::new(),
            stdout_truncated_bytes: 0,
            stderr_truncated_bytes: 0,
        };
        let error = validate_shell_output(&result, 6).expect_err("output should be rejected");
        assert!(matches!(error, RemoteSandboxError::Protocol(_)));
    }
}
