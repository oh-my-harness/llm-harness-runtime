use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{
    IntoResponse, Response,
    sse::{Event, Sse},
};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use llm_harness_platform::sandbox::{NetRule, ResourceLimits, SandboxConfig};
use llm_harness_sandbox_remote::RemoteSandboxConfig;
use llm_harness_sandbox_remote::protocol::{
    CreateDirRequest, CreateTempDirRequest, CreateTempDirResponse, ExistsRequest, ExistsResponse,
    RemoveRequest,
};
use llm_harness_sandbox_remote::protocol::{
    CreateSandboxRequest, CreateSandboxResponse, FileContent, FileInfoRequest, FileInfoResponse,
    FileInfoResponseValue, GatewayErrorResponse, ListDirRequest, ListDirResponse, ReadFileRequest,
    ReadFileResponse, RemoteExecEvent, RemoteSandboxState, RemoteSandboxStatus, ShellExecRequest,
    ShellExecResponse, WriteFileRequest,
};
use tokio::net::TcpListener;
use tokio_stream::wrappers::UnboundedReceiverStream;

pub const GATEWAY_TOKEN: &str = "test-gateway-token";

pub const STREAM_OUTPUT_COMMAND: &str = r#"{
  "stdout": "out",
  "stderr": "err",
  "require_env": {"REMOTE_TEST": "1"}
}"#;
pub const WAITING_COMMAND: &str = r#"{"wait": true}"#;
pub const BOUNDED_OUTPUT_COMMAND: &str = r#"{"stdout": "0123456789"}"#;

type GatewayPathResult = Result<PathBuf, Box<Response>>;

pub fn test_sandbox_config() -> SandboxConfig {
    SandboxConfig {
        fs_allowlist: vec![PathBuf::from("/workspace")],
        fs_denylist: Vec::new(),
        net_allowlist: vec![NetRule {
            host: "example.internal".to_owned(),
            port_range: Some((1, 65535)),
        }],
        resource_limits: ResourceLimits {
            max_cpus: Some(1),
            max_processes: Some(16),
            max_memory_mb: Some(128),
            max_disk_mb: Some(64),
            timeout: Some(Duration::from_secs(5)),
        },
        work_dir: Some(PathBuf::from("/workspace")),
        trust_profile: Default::default(),
        host_binds: Vec::new(),
    }
}

struct GatewayState {
    token: String,
    root: tempfile::TempDir,
    sandboxes: Mutex<HashMap<String, TestSandbox>>,
    executions: Mutex<HashMap<String, Arc<TestExecution>>>,
    create_status: StatusCode,
    read_delay: Option<Duration>,
    create_work_dir: Option<String>,
    delete_count: Arc<Mutex<u64>>,
}

struct TestSandbox {
    root: PathBuf,
    work_dir: String,
    backend: String,
    state: RemoteSandboxState,
    config: RemoteSandboxConfig,
}

struct TestExecution {
    cancelled: AtomicBool,
}

impl TestExecution {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TestCommand {
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    #[serde(default)]
    exit_code: i32,
    #[serde(default)]
    wait: bool,
    #[serde(default)]
    require_env: BTreeMap<String, String>,
}

#[derive(Clone)]
struct SharedState(Arc<GatewayState>);

pub struct TestGateway {
    pub base_url: String,
    _state: SharedState,
}

impl TestGateway {
    pub async fn start() -> Self {
        Self::start_with_options(StatusCode::CREATED, None, None).await
    }

    pub async fn start_with_create_status(create_status: StatusCode) -> Self {
        Self::start_with_options(create_status, None, None).await
    }

    pub async fn start_with_read_delay(read_delay: Duration) -> Self {
        Self::start_with_options(StatusCode::CREATED, Some(read_delay), None).await
    }

    pub async fn start_with_create_work_dir(work_dir: String) -> Self {
        Self::start_with_options(StatusCode::CREATED, None, Some(work_dir)).await
    }

    pub fn delete_count(&self) -> u64 {
        *self._state.0.delete_count.lock().unwrap()
    }

    async fn start_with_options(
        create_status: StatusCode,
        read_delay: Option<Duration>,
        create_work_dir: Option<String>,
    ) -> Self {
        let root = tempfile::tempdir().expect("create gateway tempdir");
        let state = SharedState(Arc::new(GatewayState {
            token: GATEWAY_TOKEN.to_owned(),
            root,
            sandboxes: Mutex::new(HashMap::new()),
            executions: Mutex::new(HashMap::new()),
            create_status,
            read_delay,
            create_work_dir,
            delete_count: Arc::new(Mutex::new(0)),
        }));
        let app = Router::new()
            .route("/v1/sandboxes", post(create_sandbox))
            .route(
                "/v1/sandboxes/{id}",
                get(get_sandbox).delete(delete_sandbox),
            )
            .route("/v1/sandboxes/{id}/reset", post(reset_sandbox))
            .route("/v1/sandboxes/{id}/cleanup", post(cleanup_sandbox))
            .route("/v1/sandboxes/{id}/exec", post(exec_shell))
            .route(
                "/v1/sandboxes/{id}/exec/{execution_id}/kill",
                post(kill_execution),
            )
            .route("/v1/sandboxes/{id}/fs/read", post(read_file))
            .route("/v1/sandboxes/{id}/fs/write", post(write_file))
            .route("/v1/sandboxes/{id}/fs/info", post(file_info))
            .route("/v1/sandboxes/{id}/fs/list", post(list_dir))
            .route("/v1/sandboxes/{id}/fs/exists", post(exists))
            .route("/v1/sandboxes/{id}/fs/create-dir", post(create_dir))
            .route("/v1/sandboxes/{id}/fs/remove", post(remove))
            .route(
                "/v1/sandboxes/{id}/fs/create-temp-dir",
                post(create_temp_dir),
            )
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                authenticate,
            ))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind gateway");
        let address = listener.local_addr().expect("gateway address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("gateway server");
        });

        Self {
            base_url: format!("http://{address}"),
            _state: state,
        }
    }
}

async fn authenticate(State(state): State<SharedState>, request: Request, next: Next) -> Response {
    let expected = format!("Bearer {}", state.0.token);
    let actual = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    if actual != expected {
        return gateway_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid bearer token",
            None,
        );
    }
    next.run(request).await
}

async fn create_sandbox(
    State(state): State<SharedState>,
    Json(request): Json<CreateSandboxRequest>,
) -> Response {
    let sandbox_id = uuid::Uuid::new_v4().to_string();
    let root = state.0.root.path().join(&sandbox_id);
    std::fs::create_dir_all(&root).expect("create sandbox root");
    let work_dir = request
        .config
        .work_dir
        .clone()
        .unwrap_or_else(|| "/workspace".to_owned());
    let work_dir = state.0.create_work_dir.clone().unwrap_or(work_dir);
    let host_work_dir = guest_to_host(&root, &work_dir).expect("resolve work dir");
    std::fs::create_dir_all(&host_work_dir).expect("create work dir");

    let response = CreateSandboxResponse {
        sandbox_id: sandbox_id.clone(),
        work_dir,
        backend: "test-local-process".to_owned(),
        state: RemoteSandboxState::Running,
        trust_profile: request.config.trust_profile,
        host_binds: Vec::new(),
    };
    state.0.sandboxes.lock().unwrap().insert(
        sandbox_id,
        TestSandbox {
            root,
            work_dir: response.work_dir.clone(),
            backend: response.backend.clone(),
            state: RemoteSandboxState::Running,
            config: request.config,
        },
    );
    (state.0.create_status, Json(response)).into_response()
}

async fn get_sandbox(State(state): State<SharedState>, AxumPath(id): AxumPath<String>) -> Response {
    let sandboxes = state.0.sandboxes.lock().unwrap();
    let Some(sandbox) = sandboxes.get(&id) else {
        return not_found(&id);
    };
    let response = RemoteSandboxStatus {
        sandbox_id: id.clone(),
        work_dir: sandbox.work_dir.clone(),
        backend: sandbox.backend.clone(),
        state: sandbox.state,
        config: sandbox.config.clone(),
    };
    Json(response).into_response()
}

async fn delete_sandbox(
    State(state): State<SharedState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let removed = state.0.sandboxes.lock().unwrap().remove(&id);
    if removed.is_none() {
        return not_found(&id);
    }
    *state.0.delete_count.lock().unwrap() += 1;
    StatusCode::NO_CONTENT.into_response()
}

async fn reset_sandbox(
    State(state): State<SharedState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let sandboxes = state.0.sandboxes.lock().unwrap();
    let Some(sandbox) = sandboxes.get(&id) else {
        return not_found(&id);
    };
    if let Err(error) = std::fs::remove_dir_all(&sandbox.root) {
        return gateway_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "reset_failed",
            error.to_string(),
            None,
        );
    }
    if let Err(error) = std::fs::create_dir_all(&sandbox.root) {
        return gateway_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "reset_failed",
            error.to_string(),
            None,
        );
    }
    let host_work_dir = guest_to_host(&sandbox.root, &sandbox.work_dir).expect("resolve work dir");
    if let Err(error) = std::fs::create_dir_all(&host_work_dir) {
        return gateway_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "reset_failed",
            error.to_string(),
            None,
        );
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn cleanup_sandbox(
    State(state): State<SharedState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let sandboxes = state.0.sandboxes.lock().unwrap();
    let Some(sandbox) = sandboxes.get(&id) else {
        return not_found(&id);
    };
    let temp_root = guest_to_host(&sandbox.root, "/workspace/.tmp").expect("resolve temp root");
    if temp_root.exists() {
        let _ = std::fs::remove_dir_all(&temp_root);
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn read_file(
    State(state): State<SharedState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<ReadFileRequest>,
) -> Response {
    if let Some(read_delay) = state.0.read_delay {
        tokio::time::sleep(read_delay).await;
    }
    let sandboxes = state.0.sandboxes.lock().unwrap();
    let Some(sandbox) = sandboxes.get(&id) else {
        return not_found(&id);
    };
    let host_path = match resolve_guest_path(sandbox, &request.path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let bytes = match std::fs::read(&host_path) {
        Ok(bytes) => bytes,
        Err(error) => return fs_error(error, &request.path),
    };
    let content = if request.binary {
        FileContent::Binary {
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    } else {
        match String::from_utf8(bytes) {
            Ok(value) => FileContent::Text { value },
            Err(_) => {
                return gateway_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_utf8",
                    "file is not valid UTF-8",
                    Some(request.path.clone()),
                );
            }
        }
    };
    Json(ReadFileResponse { content }).into_response()
}

async fn write_file(
    State(state): State<SharedState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<WriteFileRequest>,
) -> Response {
    let sandboxes = state.0.sandboxes.lock().unwrap();
    let Some(sandbox) = sandboxes.get(&id) else {
        return not_found(&id);
    };
    let host_path = match resolve_guest_path(sandbox, &request.path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let bytes = match &request.content {
        FileContent::Text { value } => value.clone().into_bytes(),
        FileContent::Binary { base64 } => {
            match base64::engine::general_purpose::STANDARD.decode(base64) {
                Ok(bytes) => bytes,
                Err(error) => {
                    return gateway_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_base64",
                        error.to_string(),
                        Some(request.path.clone()),
                    );
                }
            }
        }
    };
    if let Some(parent) = host_path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return fs_error(error, &request.path);
    }
    let result = if request.append {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&host_path)
            .and_then(|mut file| file.write_all(&bytes))
    } else {
        std::fs::write(&host_path, bytes)
    };
    if let Err(error) = result {
        return fs_error(error, &request.path);
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn file_info(
    State(state): State<SharedState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<FileInfoRequest>,
) -> Response {
    let sandboxes = state.0.sandboxes.lock().unwrap();
    let Some(sandbox) = sandboxes.get(&id) else {
        return not_found(&id);
    };
    let host_path = match resolve_guest_path(sandbox, &request.path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let metadata = match std::fs::metadata(&host_path) {
        Ok(metadata) => metadata,
        Err(error) => return fs_error(error, &request.path),
    };
    let modified = metadata.modified().expect("file modified time").into();
    let response = FileInfoResponseValue {
        info: FileInfoResponse {
            path: request.path.clone(),
            is_dir: metadata.is_dir(),
            size: metadata.len(),
            modified,
        },
    };
    Json(response).into_response()
}

async fn list_dir(
    State(state): State<SharedState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<ListDirRequest>,
) -> Response {
    let sandboxes = state.0.sandboxes.lock().unwrap();
    let Some(sandbox) = sandboxes.get(&id) else {
        return not_found(&id);
    };
    let host_path = match resolve_guest_path(sandbox, &request.path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let entries = match std::fs::read_dir(&host_path) {
        Ok(entries) => entries,
        Err(error) => return fs_error(error, &request.path),
    };
    let mut response = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return fs_error(error, &request.path),
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => return fs_error(error, &request.path),
        };
        let modified = metadata.modified().expect("entry modified time").into();
        let path = format!(
            "{}/{}",
            request.path.trim_end_matches('/'),
            entry.file_name().to_string_lossy()
        );
        response.push(FileInfoResponse {
            path,
            is_dir: metadata.is_dir(),
            size: metadata.len(),
            modified,
        });
    }
    Json(ListDirResponse { entries: response }).into_response()
}

async fn exists(
    State(state): State<SharedState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<ExistsRequest>,
) -> Response {
    let sandboxes = state.0.sandboxes.lock().unwrap();
    let Some(sandbox) = sandboxes.get(&id) else {
        return not_found(&id);
    };
    let host_path = match resolve_guest_path(sandbox, &request.path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    Json(ExistsResponse {
        exists: host_path.exists(),
    })
    .into_response()
}

async fn create_dir(
    State(state): State<SharedState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<CreateDirRequest>,
) -> Response {
    let sandboxes = state.0.sandboxes.lock().unwrap();
    let Some(sandbox) = sandboxes.get(&id) else {
        return not_found(&id);
    };
    let host_path = match resolve_guest_path(sandbox, &request.path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let result = if request.recursive {
        std::fs::create_dir_all(&host_path)
    } else {
        std::fs::create_dir(&host_path)
    };
    if let Err(error) = result {
        return fs_error(error, &request.path);
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn remove(
    State(state): State<SharedState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<RemoveRequest>,
) -> Response {
    let sandboxes = state.0.sandboxes.lock().unwrap();
    let Some(sandbox) = sandboxes.get(&id) else {
        return not_found(&id);
    };
    let host_path = match resolve_guest_path(sandbox, &request.path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    if !host_path.exists() && request.force {
        return StatusCode::NO_CONTENT.into_response();
    }
    let result = if host_path.is_dir() {
        if request.recursive {
            std::fs::remove_dir_all(&host_path)
        } else {
            std::fs::remove_dir(&host_path)
        }
    } else {
        std::fs::remove_file(&host_path)
    };
    if let Err(error) = result {
        return fs_error(error, &request.path);
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn create_temp_dir(
    State(state): State<SharedState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<CreateTempDirRequest>,
) -> Response {
    let sandboxes = state.0.sandboxes.lock().unwrap();
    let Some(sandbox) = sandboxes.get(&id) else {
        return not_found(&id);
    };
    let guest_path = format!(
        "{}/.tmp/{}-{}",
        sandbox.work_dir.trim_end_matches('/'),
        request.prefix,
        uuid::Uuid::new_v4()
    );
    let host_path = match resolve_guest_path(sandbox, &guest_path) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    if let Err(error) = std::fs::create_dir_all(&host_path) {
        return fs_error(error, &guest_path);
    }
    Json(CreateTempDirResponse { path: guest_path }).into_response()
}

async fn exec_shell(
    State(state): State<SharedState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<ShellExecRequest>,
) -> Response {
    let timeout_ms = {
        let sandboxes = state.0.sandboxes.lock().unwrap();
        let Some(sandbox) = sandboxes.get(&id) else {
            return not_found(&id);
        };
        sandbox.config.resource_limits.timeout_ms
    };
    let command = match serde_json::from_str::<TestCommand>(&request.command) {
        Ok(command) => command,
        Err(error) => {
            return gateway_error(
                StatusCode::BAD_REQUEST,
                "invalid_test_command",
                error.to_string(),
                None,
            );
        }
    };

    let execution_id = uuid::Uuid::new_v4().to_string();
    let execution = Arc::new(TestExecution::new());
    state
        .0
        .executions
        .lock()
        .unwrap()
        .insert(execution_id.clone(), Arc::clone(&execution));

    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let started = RemoteExecEvent::Started {
        execution_id: execution_id.clone(),
    };
    let _ = sender.send(sse_event("started", &started));

    let stdout_collector = Arc::new(Mutex::new(BoundedOutput::new(request.max_output_bytes)));
    let stderr_collector = Arc::new(Mutex::new(BoundedOutput::new(request.max_output_bytes)));
    let timeout_ms = request.timeout_ms.or(timeout_ms).unwrap_or(5_000);
    let wait_sender = sender.clone();
    let state_for_wait = state.0.clone();
    let execution_id_for_wait = execution_id.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let env_matches = command
            .require_env
            .iter()
            .all(|(key, expected)| request.env.get(key) == Some(expected));
        let mut exit_code = if env_matches { command.exit_code } else { 1 };
        if env_matches {
            emit_exec_output(
                wait_sender.clone(),
                &stdout_collector,
                &command.stdout,
                false,
            );
            emit_exec_output(
                wait_sender.clone(),
                &stderr_collector,
                &command.stderr,
                true,
            );
        }

        let mut timed_out = false;
        if command.wait {
            loop {
                if execution.cancelled.load(Ordering::SeqCst) {
                    exit_code = -1;
                    break;
                }
                if Instant::now() >= deadline {
                    timed_out = true;
                    exit_code = -1;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        let stdout = stdout_collector.lock().unwrap().snapshot();
        let stderr = stderr_collector.lock().unwrap().snapshot();
        let result = ShellExecResponse {
            exit_code,
            stdout: stdout.0,
            stderr: stderr.0,
            stdout_truncated_bytes: stdout.1,
            stderr_truncated_bytes: stderr.1,
        };
        if timed_out {
            let _ = wait_sender.send(sse_event("timeout", &RemoteExecEvent::Timeout { result }));
        } else {
            let _ = wait_sender.send(sse_event("exit", &RemoteExecEvent::Exit { result }));
        }
        state_for_wait
            .executions
            .lock()
            .unwrap()
            .remove(&execution_id_for_wait);
    });

    let stream = UnboundedReceiverStream::new(receiver);
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-execution-id",
        execution_id.parse().expect("valid execution id header"),
    );
    (StatusCode::OK, headers, Sse::new(stream)).into_response()
}

async fn kill_execution(
    State(state): State<SharedState>,
    AxumPath((id, execution_id)): AxumPath<(String, String)>,
) -> Response {
    if !state.0.sandboxes.lock().unwrap().contains_key(&id) {
        return not_found(&id);
    }
    let executions = state.0.executions.lock().unwrap();
    let Some(execution) = executions.get(&execution_id) else {
        return gateway_error(
            StatusCode::NOT_FOUND,
            "execution_not_found",
            "execution is not running",
            None,
        );
    };
    execution.cancel();
    StatusCode::NO_CONTENT.into_response()
}

fn emit_exec_output(
    sender: tokio::sync::mpsc::UnboundedSender<Result<Event, std::convert::Infallible>>,
    collector: &Arc<Mutex<BoundedOutput>>,
    chunk: &str,
    is_stderr: bool,
) {
    if chunk.is_empty() {
        return;
    }
    collector.lock().unwrap().push(chunk.as_bytes());
    let event = if is_stderr {
        RemoteExecEvent::Stderr {
            chunk: chunk.to_owned(),
        }
    } else {
        RemoteExecEvent::Stdout {
            chunk: chunk.to_owned(),
        }
    };
    let _ = sender.send(sse_event(
        if is_stderr { "stderr" } else { "stdout" },
        &event,
    ));
}

fn sse_event<T: serde::Serialize>(
    event_name: &str,
    value: &T,
) -> Result<Event, std::convert::Infallible> {
    let data = serde_json::to_string(value).expect("serialize SSE event");
    Ok(Event::default().event(event_name).data(data))
}

fn guest_to_host(root: &Path, guest_path: &str) -> GatewayPathResult {
    if !guest_path.starts_with('/') {
        return Err(Box::new(gateway_error(
            StatusCode::BAD_REQUEST,
            "invalid_path",
            "path must be absolute",
            Some(guest_path.to_owned()),
        )));
    }
    if guest_path
        .split('/')
        .any(|component| component == "." || component == "..")
    {
        return Err(Box::new(gateway_error(
            StatusCode::BAD_REQUEST,
            "invalid_path",
            "path must not contain `.` or `..`",
            Some(guest_path.to_owned()),
        )));
    }
    let relative = guest_path.strip_prefix('/').expect("guest path is rooted");
    Ok(root.join(relative))
}

fn resolve_guest_path(sandbox: &TestSandbox, guest_path: &str) -> GatewayPathResult {
    let denied = sandbox.config.fs_denylist.iter().any(|deny| {
        guest_path == deny || guest_path.starts_with(&format!("{}/", deny.trim_end_matches('/')))
    });
    if denied {
        return Err(Box::new(gateway_error(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "path is inside the sandbox denylist",
            Some(guest_path.to_owned()),
        )));
    }

    if !sandbox.config.fs_allowlist.is_empty() {
        let allowed = sandbox.config.fs_allowlist.iter().any(|allow| {
            guest_path == allow
                || guest_path.starts_with(&format!("{}/", allow.trim_end_matches('/')))
        });
        if !allowed {
            return Err(Box::new(gateway_error(
                StatusCode::FORBIDDEN,
                "permission_denied",
                "path is outside the sandbox allowlist",
                Some(guest_path.to_owned()),
            )));
        }
    }
    guest_to_host(&sandbox.root, guest_path)
}

fn fs_error(error: std::io::Error, path: &str) -> Response {
    let status = if error.kind() == std::io::ErrorKind::NotFound {
        StatusCode::NOT_FOUND
    } else if error.kind() == std::io::ErrorKind::PermissionDenied {
        StatusCode::FORBIDDEN
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    gateway_error(status, "fs_error", error.to_string(), Some(path.to_owned()))
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

struct BoundedOutput {
    head: Vec<u8>,
    tail: Vec<u8>,
    truncated_bytes: u64,
    limit: usize,
}

impl BoundedOutput {
    fn new(limit: usize) -> Self {
        let head_capacity = limit / 2;
        let tail_capacity = limit - head_capacity;
        Self {
            head: Vec::with_capacity(head_capacity),
            tail: Vec::with_capacity(tail_capacity),
            truncated_bytes: 0,
            limit,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.limit == 0 {
            self.truncated_bytes = self.truncated_bytes.saturating_add(bytes.len() as u64);
            return;
        }

        let head_capacity = self.limit / 2;
        let head_room = head_capacity.saturating_sub(self.head.len());
        let head_len = head_room.min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_len]);
        let bytes = &bytes[head_len..];
        if bytes.is_empty() {
            return;
        }

        let tail_capacity = self.limit - head_capacity;
        self.tail.extend_from_slice(bytes);
        let overflow = self.tail.len().saturating_sub(tail_capacity);
        if overflow > 0 {
            self.truncated_bytes = self.truncated_bytes.saturating_add(overflow as u64);
            self.tail.drain(..overflow);
        }
    }

    fn snapshot(&self) -> (String, u64) {
        let mut bytes = self.head.clone();
        bytes.extend_from_slice(&self.tail);
        (
            String::from_utf8_lossy(&bytes).into_owned(),
            self.truncated_bytes,
        )
    }
}
