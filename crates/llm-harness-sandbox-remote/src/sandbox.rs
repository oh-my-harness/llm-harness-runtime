use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use futures::future::BoxFuture;
use llm_harness_platform::sandbox::{Sandbox, SandboxConfig};
use llm_harness_types::{EnvError, ExecutionEnv, FileInfo, ShellOptions, ShellOutput};
use tokio_util::sync::CancellationToken;

use crate::client::RemoteSandboxClient;
use crate::error::RemoteSandboxError;
use crate::protocol::{
    CreateSandboxResponse, FileInfoResponse, ListDirResponse, RemoteSandboxState, ShellExecRequest,
};

struct RemoteSandboxInner {
    client: Arc<RemoteSandboxClient>,
    sandbox_id: String,
    work_dir: PathBuf,
    backend: String,
    config: SandboxConfig,
    running: AtomicBool,
    remote_active: AtomicBool,
}

/// Runtime-side handle to a sandbox managed by a remote gateway.
///
/// Cloning shares the same underlying sandbox; remote resources are cleaned
/// up when the last handle drops (or `shutdown` is called once).
#[derive(Clone)]
pub struct RemoteSandbox {
    inner: Arc<RemoteSandboxInner>,
}

impl RemoteSandbox {
    /// Create a remote sandbox and return a runtime handle.
    pub async fn connect(
        config: SandboxConfig,
        client: Arc<RemoteSandboxClient>,
    ) -> Result<Self, RemoteSandboxError> {
        let response = client.create_sandbox(config.clone()).await?;
        let work_dir = match validate_create_response(&response, &config) {
            Ok(work_dir) => work_dir,
            Err(error) => {
                match crate::client::path_segment(&response.sandbox_id, "sandbox id") {
                    Ok(sandbox_id) => {
                        if let Err(cleanup_error) = client.delete_sandbox(sandbox_id).await {
                            tracing::warn!(
                                sandbox_id = %response.sandbox_id,
                                error = %cleanup_error,
                                "failed to delete remote sandbox after an invalid create response"
                            );
                        }
                    }
                    Err(_) => {
                        tracing::warn!(
                            sandbox_id = %response.sandbox_id,
                            "cannot delete remote sandbox after an invalid create response id"
                        );
                    }
                }
                return Err(error);
            }
        };
        let running = response.state == RemoteSandboxState::Running;
        let inner = Arc::new(RemoteSandboxInner {
            client,
            sandbox_id: response.sandbox_id,
            work_dir,
            backend: response.backend,
            config,
            running: AtomicBool::new(running),
            remote_active: AtomicBool::new(true),
        });
        Ok(Self { inner })
    }

    /// Return the gateway-reported backend identifier.
    pub fn backend(&self) -> &str {
        &self.inner.backend
    }

    /// Return the gateway-assigned sandbox id.
    pub fn sandbox_id(&self) -> &str {
        &self.inner.sandbox_id
    }

    async fn wait_until_running(&self) -> Result<(), RemoteSandboxError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let status = self
                .inner
                .client
                .get_sandbox(&self.inner.sandbox_id)
                .await?;
            self.validate_status_identity(&status)?;
            match status.state {
                RemoteSandboxState::Running => {
                    self.inner.running.store(true, Ordering::SeqCst);
                    return Ok(());
                }
                RemoteSandboxState::Creating if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                RemoteSandboxState::Creating => {
                    return Err(RemoteSandboxError::protocol(
                        "remote sandbox did not become ready within 30 seconds",
                    ));
                }
                state => {
                    self.inner.running.store(false, Ordering::SeqCst);
                    return Err(RemoteSandboxError::gateway(
                        409,
                        "sandbox_not_running",
                        format!("remote sandbox state is {state:?}"),
                        None,
                    ));
                }
            }
        }
    }

    fn validate_status_identity(
        &self,
        status: &crate::protocol::RemoteSandboxStatus,
    ) -> Result<(), RemoteSandboxError> {
        if status.sandbox_id != self.inner.sandbox_id {
            return Err(RemoteSandboxError::protocol(
                "gateway status returned a different sandbox id",
            ));
        }
        if status.backend != self.inner.backend {
            return Err(RemoteSandboxError::protocol(
                "gateway status returned a different backend identifier",
            ));
        }
        let work_dir = PathBuf::from(&status.work_dir);
        crate::client::path_string(&work_dir).map_err(|_| {
            RemoteSandboxError::protocol("gateway status returned an invalid work directory")
        })?;
        if work_dir != self.inner.work_dir {
            return Err(RemoteSandboxError::protocol(
                "gateway status returned a different work directory",
            ));
        }
        Ok(())
    }
}

fn validate_create_response(
    response: &CreateSandboxResponse,
    config: &SandboxConfig,
) -> Result<PathBuf, RemoteSandboxError> {
    crate::client::path_segment(&response.sandbox_id, "sandbox id")?;
    if response.backend.trim().is_empty() {
        return Err(RemoteSandboxError::protocol(
            "gateway returned an empty backend identifier",
        ));
    }

    let work_dir = PathBuf::from(&response.work_dir);
    crate::client::path_string(&work_dir)
        .map_err(|_| RemoteSandboxError::protocol("gateway returned an invalid work directory"))?;
    if let Some(expected) = config.work_dir.as_ref()
        && expected != &work_dir
    {
        return Err(RemoteSandboxError::protocol(
            "gateway returned a work directory different from the requested directory",
        ));
    }
    if config
        .fs_denylist
        .iter()
        .any(|deny| work_dir == *deny || work_dir.starts_with(deny))
    {
        return Err(RemoteSandboxError::protocol(
            "gateway returned a work directory inside the filesystem denylist",
        ));
    }
    Ok(work_dir)
}

impl Sandbox for RemoteSandbox {
    fn start(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.wait_until_running()
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))
        })
    }

    fn env(&self) -> Arc<dyn ExecutionEnv> {
        Arc::new(RemoteEnv {
            inner: Arc::clone(&self.inner),
        })
    }

    fn reset(&self) -> BoxFuture<'_, Result<()>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            inner
                .client
                .reset_sandbox(&inner.sandbox_id)
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))
        })
    }

    fn shutdown(self: Box<Self>) -> BoxFuture<'static, Result<()>> {
        let sandbox = *self;
        Box::pin(async move {
            match sandbox
                .inner
                .client
                .delete_sandbox(&sandbox.inner.sandbox_id)
                .await
            {
                Ok(()) => {
                    sandbox.inner.running.store(false, Ordering::SeqCst);
                    sandbox.inner.remote_active.store(false, Ordering::SeqCst);
                    Ok(())
                }
                Err(error) => Err(anyhow::anyhow!(error.to_string())),
            }
        })
    }

    fn config(&self) -> &SandboxConfig {
        &self.inner.config
    }

    fn is_running(&self) -> bool {
        self.inner.running.load(Ordering::SeqCst)
    }
}

impl Drop for RemoteSandboxInner {
    fn drop(&mut self) {
        if !self.remote_active.load(Ordering::SeqCst) {
            return;
        }

        let client = Arc::clone(&self.client);
        let sandbox_id = self.sandbox_id.clone();
        spawn_remote_cleanup(client, sandbox_id);
    }
}

fn spawn_remote_cleanup(client: Arc<RemoteSandboxClient>, sandbox_id: String) {
    let sandbox_id_for_error = sandbox_id.clone();
    let cleanup = std::thread::Builder::new()
        .name("llm-harness-remote-sandbox-cleanup".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::warn!(
                        sandbox_id = %sandbox_id,
                        error = %error,
                        "failed to build remote sandbox cleanup runtime"
                    );
                    return;
                }
            };
            if let Err(error) = runtime.block_on(client.delete_sandbox(&sandbox_id)) {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    error = %error,
                    "failed to delete remote sandbox on drop"
                );
            }
        });
    if let Err(error) = cleanup {
        tracing::warn!(
            sandbox_id = %sandbox_id_for_error,
            error = %error,
            "failed to spawn remote sandbox cleanup thread"
        );
    }
}

/// Remote implementation of [`ExecutionEnv`].
pub struct RemoteEnv {
    inner: Arc<RemoteSandboxInner>,
}

async fn abortable_operation<T, Operation>(
    abort: CancellationToken,
    operation: Operation,
) -> Result<T, EnvError>
where
    Operation: Future<Output = Result<T, RemoteSandboxError>>,
{
    if abort.is_cancelled() {
        return Err(EnvError::Aborted);
    }
    tokio::select! {
        _ = abort.cancelled() => Err(EnvError::Aborted),
        result = operation => result.map_err(EnvError::from),
    }
}

fn validated_guest_path(value: &str) -> Result<PathBuf, EnvError> {
    let path = PathBuf::from(value);
    crate::client::path_string(&path)
        .map_err(|_| EnvError::Other("gateway returned an invalid guest path".to_owned()))?;
    Ok(path)
}

fn validated_file_info(
    response: FileInfoResponse,
    requested_path: &Path,
) -> Result<FileInfo, EnvError> {
    let path = validated_guest_path(&response.path)?;
    if path != requested_path {
        return Err(EnvError::Other(
            "gateway returned file metadata for a different path".to_owned(),
        ));
    }
    Ok(FileInfo {
        path,
        is_dir: response.is_dir,
        size: response.size,
        modified: response.modified,
    })
}

fn validated_temp_dir_path(value: String, work_dir: &Path) -> Result<PathBuf, EnvError> {
    let path = validated_guest_path(&value)?;
    if path == work_dir || !path.starts_with(work_dir) {
        return Err(EnvError::Other(
            "gateway returned a temporary directory outside the sandbox work directory".to_owned(),
        ));
    }
    Ok(path)
}

fn validated_list_dir(
    response: ListDirResponse,
    requested_path: &Path,
) -> Result<Vec<FileInfo>, EnvError> {
    let requested_path = validated_guest_path(&requested_path.to_string_lossy())?;
    response
        .entries
        .into_iter()
        .map(|entry| {
            let path = validated_guest_path(&entry.path)?;
            if path.parent() != Some(&requested_path) {
                return Err(EnvError::Other(
                    "gateway returned a directory entry outside the requested directory".to_owned(),
                ));
            }
            Ok(FileInfo {
                path,
                is_dir: entry.is_dir,
                size: entry.size,
                modified: entry.modified,
            })
        })
        .collect()
}

impl ExecutionEnv for RemoteEnv {
    fn working_dir(&self) -> &Path {
        &self.inner.work_dir
    }

    fn read_text_file<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<String, EnvError>> {
        Box::pin(async move {
            let bytes = abortable_operation(
                abort,
                self.inner
                    .client
                    .read_file(&self.inner.sandbox_id, path, false),
            )
            .await?;
            String::from_utf8(bytes).map_err(|_| EnvError::InvalidUtf8(path.to_path_buf()))
        })
    }

    fn read_text_lines<'a>(
        &'a self,
        path: &'a Path,
        max_lines: Option<usize>,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<String>, EnvError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                return Err(EnvError::Aborted);
            }
            let bytes = abortable_operation(
                abort,
                self.inner
                    .client
                    .read_file(&self.inner.sandbox_id, path, false),
            )
            .await?;
            let text =
                String::from_utf8(bytes).map_err(|_| EnvError::InvalidUtf8(path.to_path_buf()))?;
            let lines = text
                .lines()
                .take(max_lines.unwrap_or(usize::MAX))
                .map(str::to_owned)
                .collect();
            Ok(lines)
        })
    }

    fn read_binary_file<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<u8>, EnvError>> {
        let inner = Arc::clone(&self.inner);
        let path = path.to_path_buf();
        Box::pin(async move {
            abortable_operation(
                abort,
                inner.client.read_file(&inner.sandbox_id, &path, true),
            )
            .await
        })
    }

    fn write_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let inner = Arc::clone(&self.inner);
        let path = path.to_path_buf();
        let content = content.to_vec();
        Box::pin(async move {
            abortable_operation(
                abort,
                inner
                    .client
                    .write_file(&inner.sandbox_id, &path, &content, false),
            )
            .await
        })
    }

    fn append_file<'a>(
        &'a self,
        path: &'a Path,
        content: &'a [u8],
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let inner = Arc::clone(&self.inner);
        let path = path.to_path_buf();
        let content = content.to_vec();
        Box::pin(async move {
            abortable_operation(
                abort,
                inner
                    .client
                    .write_file(&inner.sandbox_id, &path, &content, true),
            )
            .await
        })
    }

    fn file_info<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<FileInfo, EnvError>> {
        let inner = Arc::clone(&self.inner);
        let path = path.to_path_buf();
        Box::pin(async move {
            let info = abortable_operation(abort, inner.client.file_info(&inner.sandbox_id, &path))
                .await?;
            validated_file_info(info, &path)
        })
    }

    fn list_dir<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<FileInfo>, EnvError>> {
        let inner = Arc::clone(&self.inner);
        let path = path.to_path_buf();
        Box::pin(async move {
            let response =
                abortable_operation(abort, inner.client.list_dir(&inner.sandbox_id, &path)).await?;
            validated_list_dir(response, &path)
        })
    }

    fn exists<'a>(
        &'a self,
        path: &'a Path,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<bool, EnvError>> {
        let inner = Arc::clone(&self.inner);
        let path = path.to_path_buf();
        Box::pin(async move {
            abortable_operation(abort, inner.client.exists(&inner.sandbox_id, &path)).await
        })
    }

    fn create_dir<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let inner = Arc::clone(&self.inner);
        let path = path.to_path_buf();
        Box::pin(async move {
            abortable_operation(
                abort,
                inner.client.create_dir(&inner.sandbox_id, &path, recursive),
            )
            .await
        })
    }

    fn remove<'a>(
        &'a self,
        path: &'a Path,
        recursive: bool,
        force: bool,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), EnvError>> {
        let inner = Arc::clone(&self.inner);
        let path = path.to_path_buf();
        Box::pin(async move {
            abortable_operation(
                abort,
                inner
                    .client
                    .remove(&inner.sandbox_id, &path, recursive, force),
            )
            .await
        })
    }

    fn create_temp_dir<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<PathBuf, EnvError>> {
        let inner = Arc::clone(&self.inner);
        let prefix = prefix.to_owned();
        Box::pin(async move {
            let path = inner
                .client
                .create_temp_dir(&inner.sandbox_id, &prefix)
                .await?;
            validated_temp_dir_path(path, &inner.work_dir)
        })
    }

    fn execute_shell<'a>(
        &'a self,
        cmd: &'a str,
        opts: ShellOptions<'a>,
    ) -> BoxFuture<'a, Result<ShellOutput, EnvError>> {
        let inner = Arc::clone(&self.inner);
        let command = cmd.to_owned();
        let cwd = opts.cwd.map(|path| path.to_path_buf());
        let env = opts
            .env
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>();
        let timeout_ms = opts.timeout.map(|timeout| timeout.as_millis());
        let max_output_bytes = opts
            .max_output_bytes
            .unwrap_or(ShellOptions::DEFAULT_MAX_OUTPUT_BYTES);
        let abort = opts.abort.clone();
        let on_stdout = opts.on_stdout;
        let on_stderr = opts.on_stderr;

        Box::pin(async move {
            let cwd = match cwd {
                Some(path) => Some(crate::client::path_string(&path).map_err(EnvError::from)?),
                None => None,
            };
            let timeout_ms = timeout_ms
                .map(|milliseconds| {
                    u64::try_from(milliseconds)
                        .map_err(|_| EnvError::Other("shell timeout is too large".to_owned()))
                })
                .transpose()?;
            let request = ShellExecRequest {
                command,
                cwd,
                env,
                timeout_ms,
                max_output_bytes,
            };
            inner
                .client
                .execute_shell(&inner.sandbox_id, request, on_stdout, on_stderr, abort)
                .await
                .map_err(EnvError::from)
        })
    }

    fn cleanup(&self) -> BoxFuture<'_, Result<(), EnvError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            inner
                .client
                .cleanup_sandbox(&inner.sandbox_id)
                .await
                .map_err(EnvError::from)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_platform::sandbox::{NetRule, ResourceLimits};

    fn config() -> SandboxConfig {
        SandboxConfig {
            fs_allowlist: vec![PathBuf::from("/workspace")],
            fs_denylist: Vec::new(),
            net_allowlist: vec![NetRule {
                host: "example.internal".to_owned(),
                port_range: None,
            }],
            resource_limits: ResourceLimits {
                max_cpus: None,
                max_processes: None,
                max_memory_mb: None,
                max_disk_mb: None,
                timeout: None,
            },
            work_dir: Some(PathBuf::from("/workspace")),
            trust_profile: Default::default(),
        }
    }

    fn response(work_dir: &str) -> CreateSandboxResponse {
        CreateSandboxResponse {
            sandbox_id: "sandbox-id".to_owned(),
            work_dir: work_dir.to_owned(),
            backend: "test".to_owned(),
            state: crate::protocol::RemoteSandboxState::Running,
            trust_profile: Default::default(),
        }
    }

    #[test]
    fn accepts_gateway_work_dir_matching_request() {
        let work_dir =
            validate_create_response(&response("/workspace"), &config()).expect("valid response");
        assert_eq!(work_dir, PathBuf::from("/workspace"));
    }

    #[test]
    fn rejects_gateway_work_dir_different_from_request() {
        let error = validate_create_response(&response("/workspace/other"), &config())
            .expect_err("mismatched work dir should be rejected");
        assert!(matches!(error, RemoteSandboxError::Protocol(_)));
    }

    #[test]
    fn rejects_temporary_directory_outside_work_dir() {
        let error = validated_temp_dir_path("/outside/agent".to_owned(), Path::new("/workspace"))
            .expect_err("temporary directory outside work_dir should be rejected");
        assert!(matches!(error, EnvError::Other(_)));
    }

    #[test]
    fn rejects_file_metadata_for_a_different_path() {
        let response = FileInfoResponse {
            path: "/workspace/other.txt".to_owned(),
            is_dir: false,
            size: 1,
            modified: chrono::Utc::now(),
        };
        let result = validated_file_info(response, Path::new("/workspace/input.txt"));
        assert!(matches!(result, Err(EnvError::Other(_))));
    }

    #[test]
    fn rejects_list_entry_with_invalid_guest_path() {
        let response = ListDirResponse {
            entries: vec![FileInfoResponse {
                path: "../escape".to_owned(),
                is_dir: false,
                size: 1,
                modified: chrono::Utc::now(),
            }],
        };
        let result = validated_list_dir(response, Path::new("/workspace"));
        assert!(matches!(result, Err(EnvError::Other(_))));
    }

    #[test]
    fn rejects_list_entry_outside_requested_directory() {
        let response = ListDirResponse {
            entries: vec![FileInfoResponse {
                path: "/outside/file.txt".to_owned(),
                is_dir: false,
                size: 1,
                modified: chrono::Utc::now(),
            }],
        };
        let result = validated_list_dir(response, Path::new("/workspace"));
        assert!(matches!(result, Err(EnvError::Other(_))));
    }
}
