use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode, header::CONTENT_TYPE, header::RETRY_AFTER};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use base64::Engine;
use llm_harness_platform::sandbox::Sandbox;
use llm_harness_sandbox_remote::protocol::{
    CreateDirRequest, CreateSandboxRequest, CreateSandboxResponse, CreateTempDirRequest,
    CreateTempDirResponse, FileContent, FileInfoRequest, FileInfoResponse, FileInfoResponseValue,
    GatewayErrorResponse, ListDirRequest, ListDirResponse, ReadFileRequest, ReadFileResponse,
    RemoteExecEvent, RemoteSandboxState, RemoteSandboxStatus, RemoveRequest, ShellExecRequest,
    ShellExecResponse, UploadFilesRequest, UploadFilesResponse, UploadedFile, WriteFileRequest,
};
use llm_harness_types::{EnvError, FileInfo, ShellOptions};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::auth::{AuthenticatedPrincipal, bearer_token, unauthorized};
use crate::observability::PrincipalId;
use crate::state::{GUEST_WORK_DIR, GatewayStateError, SharedState};

pub async fn serve(
    gateway: crate::SandboxGateway,
    listener: tokio::net::TcpListener,
) -> std::io::Result<()> {
    axum::serve(listener, gateway.router()).await
}

pub(crate) fn build_router(state: SharedState) -> Router {
    let protected_routes = Router::new()
        .route("/v1/sandboxes", post(create_sandbox))
        .route(
            "/v1/sandboxes/{id}",
            get(get_sandbox).delete(delete_sandbox),
        )
        .route("/v1/sandboxes/{id}/start", post(start_sandbox))
        .route("/v1/sandboxes/{id}/reset", post(reset_sandbox))
        .route("/v1/sandboxes/{id}/cleanup", post(cleanup_sandbox))
        .route("/v1/sandboxes/{id}/exec", post(exec_shell))
        .route(
            "/v1/sandboxes/{id}/exec/{execution_id}/kill",
            post(kill_execution),
        )
        .route("/v1/sandboxes/{id}/fs/read", post(read_file))
        .route("/v1/sandboxes/{id}/fs/write", post(write_file))
        .route("/v1/sandboxes/{id}/fs/upload", post(upload_files))
        .route("/v1/sandboxes/{id}/fs/info", post(file_info))
        .route("/v1/sandboxes/{id}/fs/list", post(list_dir))
        .route("/v1/sandboxes/{id}/fs/exists", post(exists))
        .route("/v1/sandboxes/{id}/fs/create-dir", post(create_dir))
        .route("/v1/sandboxes/{id}/fs/remove", post(remove))
        .route(
            "/v1/sandboxes/{id}/fs/create-temp-dir",
            post(create_temp_dir),
        )
        .route("/metrics", get(metrics))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authenticate,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            log_request,
        ))
        .layer(axum::extract::DefaultBodyLimit::max(
            state.config().max_request_bytes,
        ))
        .with_state(state.clone());

    let health_routes = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(readiness))
        .with_state(state);

    protected_routes.merge(health_routes)
}

async fn authenticate(
    State(state): State<SharedState>,
    mut request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    let principal = bearer_token(&request).and_then(|token| state.config().authenticate(token));
    let Some(principal) = principal else {
        return unauthorized();
    };
    if let Some(retry_after) = state.check_rate_limit(&principal.id, principal.requests_per_minute)
    {
        return rate_limited(principal.id.to_string(), retry_after);
    }
    let principal_id = PrincipalId(principal.id.clone());
    request.extensions_mut().insert(principal);
    let mut response = next.run(request).await;
    response.extensions_mut().insert(principal_id);
    response
}

async fn metrics(State(state): State<SharedState>) -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        state.render_metrics(),
    )
}

#[derive(serde::Serialize)]
struct HealthResponse {
    status: &'static str,
    backend: &'static str,
}

async fn health(State(state): State<SharedState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        backend: state.backend_label(),
    })
}

async fn readiness(State(state): State<SharedState>) -> Json<HealthResponse> {
    health(State(state)).await
}

async fn log_request(
    State(state): State<SharedState>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap_or_else(Uuid::new_v4);
    let mut request = request;
    request.extensions_mut().insert(request_id.to_string());
    let response = next.run(request).await;
    let principal = response
        .extensions()
        .get::<PrincipalId>()
        .map(|principal| principal.0.to_string());
    state.record_request(
        method.as_str(),
        &path,
        principal.as_deref(),
        response.status().as_u16(),
        started.elapsed(),
    );
    tracing::info!(
        request_id = %request_id,
        method = %method,
        path = %path,
        principal = principal.as_deref().unwrap_or("anonymous"),
        route = %crate::observability::metric_route(&path),
        status = response.status().as_u16(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "sandbox gateway request"
    );
    response
}

async fn create_sandbox(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    Json(request): Json<CreateSandboxRequest>,
) -> Response {
    let sandbox_id = match state.create_sandbox(&principal, request.config).await {
        Ok(id) => id,
        Err(error) => return state_error(error),
    };
    if let Err(error) = state.start_sandbox(&principal, &sandbox_id).await {
        cleanup_failed_sandbox(&state, &sandbox_id, "sandbox startup failure").await;
        tracing::warn!(sandbox_id = %sandbox_id, error = %error, "sandbox start failed");
        return gateway_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "sandbox_backend_unavailable",
            error.to_string(),
            None,
        );
    }
    if let Err(error) = state.mark_running(&sandbox_id) {
        cleanup_failed_sandbox(&state, &sandbox_id, "sandbox state persistence failure").await;
        return state_error(error);
    }
    let work_dir = state
        .get_sandbox(&principal, &sandbox_id)
        .map(|sandbox| sandbox.work_dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    tracing::info!(sandbox_id = %sandbox_id, principal = %principal.id, "sandbox created");
    let trust_profile = state.requested_profile(&sandbox_id);
    (
        StatusCode::CREATED,
        Json(CreateSandboxResponse {
            sandbox_id,
            work_dir,
            backend: state.backend_label().to_owned(),
            state: RemoteSandboxState::Running,
            trust_profile,
        }),
    )
        .into_response()
}

async fn cleanup_failed_sandbox(state: &SharedState, sandbox_id: &str, stage: &str) {
    if let Ok(sandbox) = state.take_sandbox_for_delete(sandbox_id).await
        && Box::new(sandbox).shutdown().await.is_ok()
        && let Err(registry_error) = state.finish_shutdown(sandbox_id)
    {
        tracing::warn!(
            sandbox_id = %sandbox_id,
            stage,
            error = %registry_error.detail(),
            "failed to remove sandbox registry record after sandbox failure"
        );
    }
}

async fn start_sandbox(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if state.get_sandbox(&principal, &id).is_none() {
        return not_found(&id);
    }
    if let Err(error) = state.start_sandbox(&principal, &id).await {
        tracing::warn!(
            sandbox_id = %id,
            principal = %principal.id,
            error = %error,
            "sandbox start failed"
        );
        return gateway_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "sandbox_start_failed",
            error.to_string(),
            None,
        );
    }
    if let Err(error) = state.mark_running(&id) {
        cleanup_failed_sandbox(&state, &id, "sandbox state persistence failure").await;
        return state_error(error);
    }
    tracing::info!(sandbox_id = %id, principal = %principal.id, "sandbox started");
    StatusCode::NO_CONTENT.into_response()
}

async fn get_sandbox(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Some(sandbox) = state.get_sandbox(&principal, &id) else {
        return not_found(&id);
    };
    Json(RemoteSandboxStatus {
        sandbox_id: id,
        work_dir: sandbox.work_dir.to_string_lossy().into_owned(),
        backend: state.backend_label().to_owned(),
        state: sandbox.state,
        config: sandbox.config,
    })
    .into_response()
}

async fn delete_sandbox(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if state.get_sandbox(&principal, &id).is_none() {
        return not_found(&id);
    }
    let sandbox = match state.take_sandbox_for_delete(&id).await {
        Ok(sandbox) => sandbox,
        Err(error) => return state_error(error),
    };
    if let Err(error) = Box::new(sandbox).shutdown().await {
        return gateway_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "shutdown_failed",
            error.to_string(),
            None,
        );
    }
    if let Err(error) = state.finish_shutdown(&id) {
        return state_error(error);
    }
    tracing::info!(sandbox_id = %id, principal = %principal.id, "sandbox deleted");
    StatusCode::NO_CONTENT.into_response()
}

async fn reset_sandbox(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if state.get_sandbox(&principal, &id).is_none() {
        return not_found(&id);
    }
    match state.reset_sandbox(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => gateway_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "reset_failed",
            error.to_string(),
            None,
        ),
    }
}

async fn cleanup_sandbox(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Some(sandbox) = state.get_sandbox(&principal, &id) else {
        return not_found(&id);
    };
    let temp_root_string = format!("{GUEST_WORK_DIR}/.tmp");
    let temp_root = match validate_guest_fs_path(&sandbox.config, &temp_root_string) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    match sandbox
        .env
        .remove(&temp_root, true, true, CancellationToken::new())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => fs_error(error, &temp_root_string),
    }
}

async fn read_file(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<ReadFileRequest>,
) -> Response {
    let Some(sandbox) = state.get_sandbox(&principal, &id) else {
        return not_found(&id);
    };
    let path = match validate_guest_fs_path(&sandbox.config, &request.path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let abort = CancellationToken::new();
    let content = if request.binary {
        sandbox
            .env
            .read_binary_file(&path, abort)
            .await
            .map(|bytes| FileContent::Binary {
                base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            })
    } else {
        sandbox
            .env
            .read_text_file(&path, abort)
            .await
            .map(|value| FileContent::Text { value })
    };
    match content {
        Ok(content) => (StatusCode::OK, Json(ReadFileResponse { content })).into_response(),
        Err(error) => fs_error(error, &request.path),
    }
}

async fn write_file(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<WriteFileRequest>,
) -> Response {
    let Some(sandbox) = state.get_sandbox(&principal, &id) else {
        return not_found(&id);
    };
    let bytes = match decode_content(&request.content) {
        Ok(bytes) => bytes,
        Err(error) => {
            return gateway_error(
                StatusCode::BAD_REQUEST,
                "invalid_base64",
                error,
                Some(request.path),
            );
        }
    };
    let path = match validate_guest_fs_path(&sandbox.config, &request.path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let abort = CancellationToken::new();
    let result = if request.append {
        sandbox.env.append_file(&path, &bytes, abort).await
    } else {
        sandbox.env.write_file(&path, &bytes, abort).await
    };
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => fs_error(error, &request.path),
    }
}

async fn upload_files(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<UploadFilesRequest>,
) -> Response {
    let Some(sandbox) = state.get_sandbox(&principal, &id) else {
        return not_found(&id);
    };
    if request.files.len() > MAX_UPLOAD_FILES {
        return gateway_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_many_files",
            format!("batch upload accepts at most {MAX_UPLOAD_FILES} files"),
            None,
        );
    }

    let mut seen_paths = std::collections::HashSet::with_capacity(request.files.len());
    let mut uploads = Vec::with_capacity(request.files.len());
    for file in &request.files {
        if !seen_paths.insert(file.path.as_str()) {
            return gateway_error(
                StatusCode::BAD_REQUEST,
                "duplicate_path",
                "batch upload paths must be unique".to_owned(),
                Some(file.path.clone()),
            );
        }
        let bytes = match decode_content(&file.content) {
            Ok(bytes) => bytes,
            Err(error) => {
                return gateway_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_base64",
                    error,
                    Some(file.path.clone()),
                );
            }
        };
        let path = match validate_guest_fs_path(&sandbox.config, &file.path) {
            Ok(path) => path,
            Err(response) => return *response,
        };
        uploads.push((file.path.clone(), path, bytes));
    }

    let mut uploaded = Vec::with_capacity(uploads.len());
    for (requested_path, path, bytes) in uploads {
        let byte_count = bytes.len() as u64;
        match sandbox
            .env
            .write_file(&path, &bytes, CancellationToken::new())
            .await
        {
            Ok(()) => {
                uploaded.push(UploadedFile {
                    path: requested_path,
                    bytes: byte_count,
                });
            }
            Err(error) => return fs_error(error, &requested_path),
        }
    }
    (
        StatusCode::OK,
        Json(UploadFilesResponse { files: uploaded }),
    )
        .into_response()
}

async fn file_info(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<FileInfoRequest>,
) -> Response {
    let Some(sandbox) = state.get_sandbox(&principal, &id) else {
        return not_found(&id);
    };
    let path = match validate_guest_fs_path(&sandbox.config, &request.path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    match sandbox.env.file_info(&path, CancellationToken::new()).await {
        Ok(info) => Json(FileInfoResponseValue {
            info: wire_file_info(info),
        })
        .into_response(),
        Err(error) => fs_error(error, &request.path),
    }
}

async fn list_dir(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<ListDirRequest>,
) -> Response {
    let Some(sandbox) = state.get_sandbox(&principal, &id) else {
        return not_found(&id);
    };
    let path = match validate_guest_fs_path(&sandbox.config, &request.path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    match sandbox.env.list_dir(&path, CancellationToken::new()).await {
        Ok(entries) => Json(ListDirResponse {
            entries: entries.into_iter().map(wire_file_info).collect(),
        })
        .into_response(),
        Err(error) => fs_error(error, &request.path),
    }
}

async fn exists(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<llm_harness_sandbox_remote::protocol::ExistsRequest>,
) -> Response {
    let Some(sandbox) = state.get_sandbox(&principal, &id) else {
        return not_found(&id);
    };
    let path = match validate_guest_fs_path(&sandbox.config, &request.path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    match sandbox.env.exists(&path, CancellationToken::new()).await {
        Ok(exists) => {
            Json(llm_harness_sandbox_remote::protocol::ExistsResponse { exists }).into_response()
        }
        Err(error) => fs_error(error, &request.path),
    }
}

async fn create_dir(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<CreateDirRequest>,
) -> Response {
    let Some(sandbox) = state.get_sandbox(&principal, &id) else {
        return not_found(&id);
    };
    let path = match validate_guest_fs_path(&sandbox.config, &request.path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    match sandbox
        .env
        .create_dir(&path, request.recursive, CancellationToken::new())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => fs_error(error, &request.path),
    }
}

async fn remove(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<RemoveRequest>,
) -> Response {
    let Some(sandbox) = state.get_sandbox(&principal, &id) else {
        return not_found(&id);
    };
    let path = match validate_guest_fs_path(&sandbox.config, &request.path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    match sandbox
        .env
        .remove(
            &path,
            request.recursive,
            request.force,
            CancellationToken::new(),
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => fs_error(error, &request.path),
    }
}

async fn create_temp_dir(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<CreateTempDirRequest>,
) -> Response {
    let Some(sandbox) = state.get_sandbox(&principal, &id) else {
        return not_found(&id);
    };
    if !is_valid_temp_dir_prefix(&request.prefix) {
        return gateway_error(
            StatusCode::BAD_REQUEST,
            "invalid_temp_prefix",
            "temporary directory prefix must be a non-empty path component",
            Some(request.prefix),
        );
    }
    let path = sandbox
        .work_dir
        .join(".tmp")
        .join(format!("{}-{}", request.prefix, Uuid::new_v4()));
    if let Err(response) = validate_guest_fs_path(&sandbox.config, &path.to_string_lossy()) {
        return *response;
    }
    match sandbox
        .env
        .create_dir(&path, true, CancellationToken::new())
        .await
    {
        Ok(()) => Json(CreateTempDirResponse {
            path: path.to_string_lossy().into_owned(),
        })
        .into_response(),
        Err(error) => fs_error(error, &request.prefix),
    }
}

async fn exec_shell(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<ShellExecRequest>,
) -> Response {
    let Some(sandbox) = state.get_sandbox(&principal, &id) else {
        return not_found(&id);
    };
    if let Some(response) = validate_shell_request(&state, &request) {
        return response;
    }
    if let Some(cwd) = &request.cwd
        && let Err(response) = validate_guest_fs_path(&sandbox.config, cwd)
    {
        return *response;
    }

    let execution_id = Uuid::new_v4().to_string();
    let abort = CancellationToken::new();
    state.insert_execution(&id, execution_id.clone(), abort.clone());
    let execution_guard = ExecutionGuard {
        state: state.clone(),
        execution_id: execution_id.clone(),
    };
    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    let _ = sender.try_send(sse_event(
        "started",
        &RemoteExecEvent::Started {
            execution_id: execution_id.clone(),
        },
    ));

    let stdout_sender = sender.clone();
    let stderr_sender = sender.clone();
    let env = Arc::clone(&sandbox.env);
    let command = request.command;
    let cwd = request.cwd.map(PathBuf::from);
    let env_pairs = request.env;
    let timeout_ms = request
        .timeout_ms
        .unwrap_or(state.config().default_exec_timeout_ms);
    let max_output_bytes = request.max_output_bytes;
    tokio::spawn(async move {
        let shell_env = env_pairs
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        let result = {
            let options = ShellOptions {
                cwd: cwd.as_deref(),
                env: shell_env,
                timeout: Some(Duration::from_millis(timeout_ms)),
                abort: abort.clone(),
                on_stdout: Some(Box::new(move |chunk: &str| {
                    let _ = stdout_sender.try_send(sse_event(
                        "stdout",
                        &RemoteExecEvent::Stdout {
                            chunk: chunk.to_owned(),
                        },
                    ));
                })),
                on_stderr: Some(Box::new(move |chunk: &str| {
                    let _ = stderr_sender.try_send(sse_event(
                        "stderr",
                        &RemoteExecEvent::Stderr {
                            chunk: chunk.to_owned(),
                        },
                    ));
                })),
                max_output_bytes: Some(max_output_bytes),
            };
            env.execute_shell(&command, options).await
        };
        drop(execution_guard);

        match result {
            Ok(output) => {
                let _ = sender.try_send(sse_event(
                    "exit",
                    &RemoteExecEvent::Exit {
                        result: ShellExecResponse {
                            exit_code: output.exit_code,
                            stdout: output.stdout,
                            stderr: output.stderr,
                            stdout_truncated_bytes: output.stdout_truncated_bytes,
                            stderr_truncated_bytes: output.stderr_truncated_bytes,
                        },
                    },
                ));
            }
            Err(EnvError::ShellTimeout {
                stdout,
                stderr,
                stdout_truncated_bytes,
                stderr_truncated_bytes,
            }) => {
                let _ = sender.try_send(sse_event(
                    "timeout",
                    &RemoteExecEvent::Timeout {
                        result: ShellExecResponse {
                            exit_code: -1,
                            stdout,
                            stderr,
                            stdout_truncated_bytes,
                            stderr_truncated_bytes,
                        },
                    },
                ));
            }
            Err(
                EnvError::Aborted
                | EnvError::ShellAborted {
                    stdout: _,
                    stderr: _,
                    stdout_truncated_bytes: _,
                    stderr_truncated_bytes: _,
                },
            ) => {
                let _ = sender.try_send(sse_event(
                    "exit",
                    &RemoteExecEvent::Exit {
                        result: ShellExecResponse::default(),
                    },
                ));
            }
            Err(error) => {
                let _ = sender.try_send(sse_event(
                    "error",
                    &RemoteExecEvent::Error {
                        code: "shell_failed".to_owned(),
                        message: error.to_string(),
                    },
                ));
            }
        }
        drop(sender);
    });

    let mut headers = HeaderMap::new();
    if let Ok(value) = execution_id.parse() {
        headers.insert("x-execution-id", value);
    }
    (
        StatusCode::OK,
        headers,
        axum::response::Sse::new(ReceiverStream::new(receiver)),
    )
        .into_response()
}

fn is_valid_temp_dir_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && !prefix.contains('/')
        && !prefix.contains('\\')
        && !prefix.contains('\0')
        && prefix != "."
        && prefix != ".."
}

async fn kill_execution(
    State(state): State<SharedState>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath((id, execution_id)): AxumPath<(String, String)>,
) -> Response {
    if state.get_sandbox(&principal, &id).is_none() {
        return not_found(&id);
    }
    if !state.execution_exists(&id, &execution_id) {
        return gateway_error(
            StatusCode::NOT_FOUND,
            "execution_not_found",
            "execution is not running".to_owned(),
            None,
        );
    }
    state.cancel_execution(&id, &execution_id);
    StatusCode::NO_CONTENT.into_response()
}

struct ExecutionGuard {
    state: SharedState,
    execution_id: String,
}

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.state.remove_execution(&self.execution_id);
    }
}

fn validate_shell_request(state: &SharedState, request: &ShellExecRequest) -> Option<Response> {
    let config = state.config();
    if request.command.trim().is_empty() {
        return Some(gateway_error(
            StatusCode::BAD_REQUEST,
            "invalid_command",
            "shell command must not be empty".to_owned(),
            None,
        ));
    }
    if request.max_output_bytes == 0 || request.max_output_bytes > config.max_output_bytes {
        return Some(gateway_error(
            StatusCode::BAD_REQUEST,
            "invalid_output_limit",
            format!(
                "max_output_bytes must be between 1 and {}",
                config.max_output_bytes
            ),
            None,
        ));
    }
    let timeout_ms = request.timeout_ms.unwrap_or(config.default_exec_timeout_ms);
    if timeout_ms == 0 || timeout_ms > config.max_exec_timeout_ms {
        return Some(gateway_error(
            StatusCode::BAD_REQUEST,
            "invalid_timeout",
            format!(
                "timeout_ms must be between 1 and {}",
                config.max_exec_timeout_ms
            ),
            None,
        ));
    }
    if let Some(cwd) = &request.cwd
        && !Path::new(cwd).is_absolute()
    {
        return Some(gateway_error(
            StatusCode::BAD_REQUEST,
            "invalid_cwd",
            "shell working directory must be absolute".to_owned(),
            Some(cwd.clone()),
        ));
    }
    for (key, value) in &request.env {
        if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
            return Some(gateway_error(
                StatusCode::BAD_REQUEST,
                "invalid_environment",
                "invalid shell environment pair".to_owned(),
                None,
            ));
        }
    }
    None
}

const MAX_UPLOAD_FILES: usize = 32;

fn validate_guest_fs_path(
    config: &llm_harness_sandbox_remote::RemoteSandboxConfig,
    value: &str,
) -> Result<PathBuf, Box<Response>> {
    let valid_guest_path = value.starts_with('/')
        && !value.contains('\0')
        && !value.contains('\\')
        && !value
            .split('/')
            .any(|component| component == "." || component == "..");
    if !valid_guest_path {
        return Err(Box::new(gateway_error(
            StatusCode::BAD_REQUEST,
            "invalid_path",
            "path must be an absolute guest path without '.', '..', backslashes, or NUL".to_owned(),
            Some(value.to_owned()),
        )));
    }
    if value != GUEST_WORK_DIR && !value.starts_with("/workspace/") {
        return Err(Box::new(gateway_error(
            StatusCode::FORBIDDEN,
            "path_outside_workspace",
            "path must be /workspace or a subpath".to_owned(),
            Some(value.to_owned()),
        )));
    }
    let allowed = config.fs_allowlist.is_empty()
        || value == GUEST_WORK_DIR
        || config
            .fs_allowlist
            .iter()
            .any(|allowed| Path::new(value).starts_with(Path::new(allowed.as_str())));
    let denied = config
        .fs_denylist
        .iter()
        .any(|denied| Path::new(value).starts_with(Path::new(denied.as_str())));
    if !allowed || denied {
        return Err(Box::new(gateway_error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "path is not permitted by the sandbox filesystem policy".to_owned(),
            Some(value.to_owned()),
        )));
    }
    Ok(PathBuf::from(value))
}

fn decode_content(content: &FileContent) -> Result<Vec<u8>, String> {
    match content {
        FileContent::Text { value } => Ok(value.clone().into_bytes()),
        FileContent::Binary { base64 } => base64::engine::general_purpose::STANDARD
            .decode(base64)
            .map_err(|error| error.to_string()),
    }
}

fn wire_file_info(info: FileInfo) -> FileInfoResponse {
    FileInfoResponse {
        path: info.path.to_string_lossy().into_owned(),
        is_dir: info.is_dir,
        size: info.size,
        modified: info.modified,
    }
}

fn fs_error(error: EnvError, path: &str) -> Response {
    let (status, code) = match &error {
        EnvError::PermissionDenied(_) => (StatusCode::FORBIDDEN, "forbidden"),
        EnvError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
        EnvError::AlreadyExists(_) => (StatusCode::CONFLICT, "already_exists"),
        EnvError::InvalidUtf8(_) => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_utf8"),
        EnvError::Aborted => (StatusCode::CONFLICT, "aborted"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "fs_error"),
    };
    gateway_error(status, code, error.to_string(), Some(path.to_owned()))
}

fn not_found(id: &str) -> Response {
    gateway_error(
        StatusCode::NOT_FOUND,
        "sandbox_not_found",
        format!("sandbox {id} does not exist"),
        None,
    )
}

fn gateway_error(
    status: StatusCode,
    code: impl Into<String>,
    message: impl Into<String>,
    path: Option<String>,
) -> Response {
    (
        status,
        Json(GatewayErrorResponse {
            code: code.into(),
            message: message.into(),
            path,
        }),
    )
        .into_response()
}

fn rate_limited(principal: String, retry_after: std::time::Duration) -> Response {
    let mut response = gateway_error(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limit_exceeded",
        format!("principal {principal} request rate limit exceeded"),
        None,
    );
    response.headers_mut().insert(
        RETRY_AFTER,
        retry_after
            .as_secs()
            .max(1)
            .to_string()
            .parse()
            .expect("retry-after header"),
    );
    response
}

fn state_error(error: GatewayStateError) -> Response {
    let (status, code) = match error {
        GatewayStateError::InvalidConfig(_) => (StatusCode::BAD_REQUEST, "invalid_config"),
        GatewayStateError::Storage(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "sandbox_registry_error")
        }
        GatewayStateError::Capacity => (StatusCode::SERVICE_UNAVAILABLE, "sandbox_limit_reached"),
        GatewayStateError::Busy => (StatusCode::CONFLICT, "sandbox_busy"),
        GatewayStateError::NotFound => (StatusCode::NOT_FOUND, "sandbox_not_found"),
    };
    let message = error.message();
    gateway_error(status, code, message, None)
}

fn sse_event<T: serde::Serialize>(
    event_name: &str,
    value: &T,
) -> Result<axum::response::sse::Event, Infallible> {
    let data = serde_json::to_string(value).expect("serialize SSE event");
    Ok(axum::response::sse::Event::default()
        .event(event_name)
        .data(data))
}
