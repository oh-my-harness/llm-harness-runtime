//! Mobile agent core: assembles the existing `AgentHarness` +
//! `RemoteSandboxClient` behind a small surface for a mobile UI.
//!
//! The core owns no retry/persistence policy of its own — errors propagate to
//! the caller (the UI) verbatim, per the MVP design spec.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::Context as _;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use llm_harness_agent::harness::{AgentHarness, AgentHarnessOptions};
use llm_harness_platform::sandbox::Sandbox;
use llm_harness_sandbox_remote::{RemoteSandbox, RemoteSandboxClient, RemoteSandboxClientConfig};
use llm_harness_tools::{BashTool, EditTool, GlobTool, GrepTool, ReadTool, WriteTool};
use llm_harness_types::{AgentMessage, ContentBlock, UserMessage};

mod events;
pub mod manager;

pub use events::{MobileEvent, map_agent_event, map_event};
pub use manager::{MobileHistoryMessage, MobileSessionManager, SessionSummary};
/// Public wrapper for integration tests; FFI surface uses it via connect.
pub fn parse_trust_profile_pub(
    value: &str,
) -> anyhow::Result<llm_harness_platform::trust::TrustProfile> {
    parse_trust_profile(value)
}

/// Connect a fresh remote sandbox for `config` (shared by connect + resume).
pub(crate) async fn connect_sandbox(config: &MobileConfig) -> anyhow::Result<Arc<RemoteSandbox>> {
    let client_config = if config.allow_insecure_http {
        RemoteSandboxClientConfig::new_insecure(
            config.gateway_url.clone(),
            config.gateway_token.clone(),
        )
    } else {
        RemoteSandboxClientConfig::new(config.gateway_url.clone(), config.gateway_token.clone())
    }
    .context("invalid gateway client config")?;
    let client = Arc::new(RemoteSandboxClient::new(client_config)?);
    let sandbox = Arc::new(
        RemoteSandbox::connect(
            sandbox_config(parse_trust_profile(&config.trust_profile)?),
            client,
        )
        .await
        .context("connect to gateway sandbox")?,
    );
    Ok(sandbox)
}

uniffi::setup_scaffolding!();

/// File metadata exposed to the mobile UI.
#[derive(Debug, Clone, uniffi::Record)]
pub struct MobileFileInfo {
    /// Absolute guest-side path.
    pub path: String,
    /// True when the entry is a directory.
    pub is_dir: bool,
    /// File size in bytes; directories report zero.
    pub size: u64,
    /// Last modification time in Unix milliseconds.
    pub modified_at_ms: u64,
}

/// Connection configuration for a mobile session.
#[derive(Debug, Clone, uniffi::Record)]
pub struct MobileConfig {
    /// Gateway base URL, e.g. `http://10.0.2.2:7080`.
    pub gateway_url: String,
    /// Gateway bearer token (32+ bytes).
    pub gateway_token: String,
    /// LLM API key.
    pub api_key: String,
    /// LLM model id, e.g. `claude-sonnet-4-6`.
    pub model: String,
    /// LLM base URL override; empty uses the provider default.
    pub llm_base_url: String,
    /// LLM provider protocol: `anthropic` (default) or `openai`.
    pub provider: String,
    /// Permit plain HTTP to non-loopback gateways (trusted LAN only).
    pub allow_insecure_http: bool,
    /// Trust profile requested for the sandbox: `restricted` | `trusted-lan`
    /// | `trusted-full`. Invalid values fail at connect.
    pub trust_profile: String,
}

/// One mobile session: a remote sandbox plus an agent loop driving it.
///
/// Events are drained via [`MobileSession::poll_event`]: an internal FFI-side
/// queue buffers every mapped event; the UI polls (e.g. on a 100ms timer) and
/// renders. This sidesteps cross-FFI callbacks for the MVP.
#[derive(uniffi::Object)]
pub struct MobileSession {
    harness: Arc<AgentHarness>,
    sandbox: Arc<RemoteSandbox>,
    session_id: Option<String>,
    sandbox_recreated: bool,
    queue: Mutex<Vec<MobileEvent>>,
}

impl MobileSession {
    pub(crate) async fn new(config: MobileConfig) -> anyhow::Result<Arc<Self>> {
        let sandbox = crate::connect_sandbox(&config).await?;
        Self::assemble(config, sandbox, None, false).await
    }

    /// Assemble a session around an already-connected sandbox. `session_id`
    /// attaches the session to a persisted `Session` (resume); `None` creates
    /// an in-memory session (MVP path).
    pub(crate) async fn assemble(
        config: MobileConfig,
        sandbox: Arc<RemoteSandbox>,
        session_id: Option<String>,
        sandbox_recreated: bool,
    ) -> anyhow::Result<Arc<Self>> {
        // Runtime context comes from the caller's block_on; no bare enter.
        let llm_client: Arc<dyn llm_adapter::Provider> = match config.provider.as_str() {
            "openai" => {
                let mut builder = llm_adapter::openai::OpenAIProvider::builder(config.api_key);
                if !config.llm_base_url.is_empty() {
                    builder = builder.base_url(config.llm_base_url.clone());
                }
                Arc::new(builder.build())
            }
            _ => {
                let mut builder =
                    llm_adapter::anthropic::AnthropicProvider::builder(config.api_key);
                if !config.llm_base_url.is_empty() {
                    builder = builder.base_url(config.llm_base_url.clone());
                }
                Arc::new(builder.build())
            }
        };

        let env = llm_harness_platform::sandbox::Sandbox::env(&*sandbox);

        let mut opts = AgentHarnessOptions::new(config.model.clone());
        opts.tools = vec![
            Arc::new(BashTool::new()) as Arc<dyn llm_harness_types::Tool>,
            Arc::new(ReadTool::new(None)),
            Arc::new(WriteTool::new(None)),
            Arc::new(EditTool::new(None)),
            Arc::new(GlobTool::new()),
            Arc::new(GrepTool::new()),
        ];
        opts.system_prompt = Some(
            "You are a helpful agent working in the user's remote sandbox \
             (an isolated workspace). Use the provided tools to inspect files \
             and run commands; paths are relative to the sandbox work \
             directory. Be concise."
                .to_owned(),
        );

        crate::probe(&format!("assemble: enter (id={session_id:?})"));
        let harness = Arc::new(match session_id.as_deref() {
            Some(id) => {
                let storage = crate::manager::open_storage(id).await?;
                AgentHarness::with_session(
                    llm_client,
                    env,
                    llm_harness_agent::session::Session::new(storage),
                    opts,
                )
            }
            None => AgentHarness::new_in_memory(llm_client, env, opts).await,
        });
        crate::probe("assemble: harness built");

        let session = Arc::new(Self {
            harness,
            sandbox,
            session_id,
            sandbox_recreated,
            queue: Mutex::new(Vec::new()),
        });

        // Event pump: forward harness events into the FFI-side queue.
        let mut rx = session.harness.subscribe();
        let session_weak = Arc::downgrade(&session);
        tokio::spawn(async move {
            loop {
                let event = match rx.recv().await {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        if let Some(session) = session_weak.upgrade() {
                            session.push(MobileEvent::Failed {
                                message: format!("event stream lagged by {n} events"),
                            });
                        } else {
                            break;
                        }
                        continue;
                    }
                };
                if let Some(session) = session_weak.upgrade() {
                    if let Some(mobile) = map_event(&event) {
                        session.push(mobile);
                    }
                } else {
                    break;
                }
            }
        });

        Ok(session)
    }

    fn push(&self, event: MobileEvent) {
        if matches!(event, MobileEvent::Failed { .. }) {
            let msg = match &event {
                MobileEvent::Failed { message } => message.clone(),
                _ => String::new(),
            };
            crate::probe(&format!("event pump: Failed: {msg}"));
        }
        if let Ok(mut queue) = self.queue.lock() {
            // Bound the buffer: drop oldest text deltas under backpressure.
            if queue.len() > 4096 {
                queue.remove(0);
            }
            queue.push(event);
        }
    }
}

#[uniffi::export]
impl MobileSession {
    /// Persisted session id; `None` for ad-hoc (non-resumed) sessions.
    #[uniffi::method(name = "sessionId")]
    pub fn session_id(&self) -> Option<String> {
        self.session_id.clone()
    }

    #[uniffi::method(name = "sandboxId")]
    pub fn sandbox_id(&self) -> String {
        self.sandbox.sandbox_id().to_owned()
    }

    /// True when resume had to create a fresh sandbox (old one unavailable).
    #[uniffi::method(name = "sandboxRecreated")]
    pub fn sandbox_recreated(&self) -> bool {
        self.sandbox_recreated
    }

    /// Replay persisted history (read-only bubbles; new events continue from
    /// here). Empty for ad-hoc sessions. Sync on purpose — see the manager
    /// methods: foreign-driven futures needing the reactor deadlock.
    pub fn history(&self) -> Result<Vec<crate::manager::MobileHistoryMessage>, MobileError> {
        let Some(id) = self.session_id.as_deref() else {
            return Ok(Vec::new());
        };
        let storage =
            run_on_runtime(crate::manager::open_storage(id)).map_err(MobileError::from)?;
        let session = llm_harness_agent::session::Session::new(storage);
        let context =
            run_on_runtime(session.build_context()).map_err(|e| MobileError::Internal {
                detail: format!("read session history: {e}"),
            })?;
        Ok(crate::manager::history_to_mobile(&context.messages))
    }
}

#[uniffi::export]
impl MobileSession {
    /// Send a user message; returns immediately, results flow as events.
    /// Takes the session by value (an `Arc`) so the spawned prompt task can
    /// route failures into this session's event queue.
    pub fn send(self: Arc<Self>, text: String) {
        let runtime = ensure_runtime();
        runtime.spawn(async move {
            let harness = Arc::clone(&self.harness);
            if let Err(error) = harness.prompt(text).await {
                self.push(MobileEvent::Failed {
                    message: error.to_string(),
                });
            }
        });
    }

    /// Send guidance to the running agent without starting a new turn.
    #[uniffi::method(name = "steer")]
    pub fn steer(self: Arc<Self>, text: String) -> Result<(), MobileError> {
        self.harness
            .steer_message(AgentMessage::User(UserMessage {
                content: vec![ContentBlock::Text { text }],
                timestamp: chrono::Utc::now(),
            }))
            .map_err(|error| MobileError::Internal {
                detail: error.to_string(),
            })
    }

    /// Cancel the current run. Completion is observed through `RunAborted`.
    #[uniffi::method(name = "abort")]
    pub fn abort(self: Arc<Self>) {
        self.harness.abort();
    }

    /// Upload bytes into the sandbox, replacing the destination file.
    ///
    /// `guest_path` may be absolute or relative to the sandbox working
    /// directory. The gateway still enforces the sandbox path boundary.
    #[uniffi::method(name = "uploadFile")]
    pub fn upload_file(
        self: Arc<Self>,
        guest_path: String,
        content: Vec<u8>,
    ) -> Result<u64, MobileError> {
        let path = resolve_sandbox_path(&self.sandbox.env(), &guest_path)?;
        let size = u64::try_from(content.len()).map_err(|_| MobileError::Internal {
            detail: "uploaded file is too large".to_owned(),
        })?;
        run_on_runtime(
            self.sandbox
                .env()
                .write_file(&path, &content, CancellationToken::new()),
        )
        .map_err(|error| MobileError::Internal {
            detail: format!("upload file {path:?}: {error:#}"),
        })?;
        Ok(size)
    }

    /// Read bytes from the sandbox. Text requests reject invalid UTF-8.
    #[uniffi::method(name = "readFile")]
    pub fn read_file(
        self: Arc<Self>,
        guest_path: String,
        binary: bool,
    ) -> Result<Vec<u8>, MobileError> {
        let path = resolve_sandbox_path(&self.sandbox.env(), &guest_path)?;
        let bytes = run_on_runtime(async {
            if binary {
                self.sandbox
                    .env()
                    .read_binary_file(&path, CancellationToken::new())
                    .await
            } else {
                self.sandbox
                    .env()
                    .read_text_file(&path, CancellationToken::new())
                    .await
                    .map(String::into_bytes)
            }
        })
        .map_err(|error| MobileError::Internal {
            detail: format!("read file {path:?}: {error:#}"),
        })?;
        Ok(bytes)
    }

    /// Return metadata for one file or directory.
    #[uniffi::method(name = "fileInfo")]
    pub fn file_info(self: Arc<Self>, guest_path: String) -> Result<MobileFileInfo, MobileError> {
        let path = resolve_sandbox_path(&self.sandbox.env(), &guest_path)?;
        let info = run_on_runtime(
            self.sandbox
                .env()
                .file_info(&path, CancellationToken::new()),
        )
        .map_err(|error| MobileError::Internal {
            detail: format!("file info {path:?}: {error:#}"),
        })?;
        Ok(mobile_file_info(info))
    }

    /// List files in a sandbox directory. An empty path lists the workspace.
    #[uniffi::method(name = "listFiles")]
    pub fn list_files(
        self: Arc<Self>,
        guest_path: String,
    ) -> Result<Vec<MobileFileInfo>, MobileError> {
        let path = resolve_sandbox_directory(&self.sandbox.env(), &guest_path)?;
        let entries = run_on_runtime(self.sandbox.env().list_dir(&path, CancellationToken::new()))
            .map_err(|error| MobileError::Internal {
                detail: format!("list directory {path:?}: {error:#}"),
            })?;
        Ok(entries.into_iter().map(mobile_file_info).collect())
    }

    pub async fn shutdown(&self) -> Result<(), MobileError> {
        let sandbox = <RemoteSandbox as Clone>::clone(&self.sandbox);
        Ok(ensure_runtime()
            .spawn(async move {
                llm_harness_platform::sandbox::Sandbox::shutdown(Box::new(sandbox)).await
            })
            .await
            .map_err(|e| MobileError::Internal {
                detail: e.to_string(),
            })??)
    }

    /// Poll one buffered UI event, oldest first. `None` when idle.
    #[uniffi::method(name = "pollEvent")]
    pub fn poll_event(&self) -> Option<MobileEvent> {
        let mut queue = self.queue.lock().ok()?;
        if queue.is_empty() {
            None
        } else {
            Some(queue.remove(0))
        }
    }
}

/// MVP sandbox policy: workspace-only file access, no network allowlist, no
/// resource limits. Trust profile comes from `MobileConfig` at connect time.
fn sandbox_config(
    trust_profile: llm_harness_platform::trust::TrustProfile,
) -> llm_harness_platform::sandbox::SandboxConfig {
    llm_harness_platform::sandbox::SandboxConfig {
        fs_allowlist: Vec::new(),
        fs_denylist: Vec::new(),
        net_allowlist: Vec::new(),
        resource_limits: llm_harness_platform::sandbox::ResourceLimits {
            max_cpus: None,
            max_processes: None,
            max_memory_mb: None,
            max_disk_mb: None,
            timeout: None,
        },
        work_dir: None,
        trust_profile,
        host_binds: Vec::new(),
    }
}

fn resolve_sandbox_path(
    env: &Arc<dyn llm_harness_types::ExecutionEnv>,
    guest_path: &str,
) -> Result<PathBuf, MobileError> {
    let value = guest_path.trim();
    if value.is_empty() {
        return Err(MobileError::Internal {
            detail: "file path is empty".to_owned(),
        });
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(env.working_dir().join(path))
}

fn resolve_sandbox_directory(
    env: &Arc<dyn llm_harness_types::ExecutionEnv>,
    guest_path: &str,
) -> Result<PathBuf, MobileError> {
    if guest_path.trim().is_empty() {
        return Ok(env.working_dir().to_path_buf());
    }
    resolve_sandbox_path(env, guest_path)
}

fn mobile_file_info(info: llm_harness_types::FileInfo) -> MobileFileInfo {
    MobileFileInfo {
        path: info.path.to_string_lossy().into_owned(),
        is_dir: info.is_dir,
        size: info.size,
        modified_at_ms: u64::try_from(info.modified.timestamp_millis()).unwrap_or(0),
    }
}

/// Parse the FFI trust-profile string into the wire enum.
pub(crate) fn parse_trust_profile(
    value: &str,
) -> anyhow::Result<llm_harness_platform::trust::TrustProfile> {
    match value.trim() {
        "" | "restricted" => Ok(llm_harness_platform::trust::TrustProfile::Restricted),
        "trusted-lan" => Ok(llm_harness_platform::trust::TrustProfile::TrustedLan),
        "trusted-full" => Ok(llm_harness_platform::trust::TrustProfile::TrustedFull),
        other => Err(anyhow::anyhow!(
            "invalid trust profile {other:?}: expected restricted, trusted-lan or trusted-full"
        )),
    }
}

/// FFI-facing error surface. Anything else collapses into `Internal`.
///
/// Variant field is `detail` (not `message`): uniffi-generated Kotlin uses
/// the field name verbatim and `message` collides with `Throwable.message`.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum MobileError {
    #[error("gateway rejected the configuration: {detail}")]
    InvalidConfig { detail: String },
    #[error("gateway connection failed: {detail}")]
    Connect { detail: String },
    #[error("{detail}")]
    Internal { detail: String },
}

impl From<anyhow::Error> for MobileError {
    fn from(error: anyhow::Error) -> Self {
        let remote = error.downcast_ref::<llm_harness_sandbox_remote::RemoteSandboxError>();
        let detail = format!("{error:#}");
        match remote {
            Some(llm_harness_sandbox_remote::RemoteSandboxError::InvalidConfig(_)) => {
                MobileError::InvalidConfig { detail }
            }
            Some(_) => MobileError::Connect { detail },
            None => MobileError::Internal { detail },
        }
    }
}
#[uniffi::export]
pub fn connect(config: MobileConfig) -> Result<Arc<MobileSession>, MobileError> {
    let session = run_on_runtime(MobileSession::new(config))?;
    Ok(session)
}

/// Install a global multi-thread tokio runtime exactly once.
///
/// FFI entry points that need async work call `runtime.block_on(...)`, which
/// enters the runtime context for the duration — that is what gives
/// `tokio::fs`/`tokio::spawn`/reqwest their reactor. Do NOT `enter()` here:
/// foreign threads call these repeatedly and nested EnterGuards must be
/// dropped in reverse order (tokio panics otherwise).
/// Run `future` to completion on the process-wide runtime.
///
/// This is the ONLY place allowed to call `block_on`. Calls that originate
/// from inside a runtime-driven task (event pump, spawned prompt) must not
/// nest `block_on` — tokio panics on that. Foreign FFI threads have no
/// ambient context and get `block_on`; runtime threads get the future
/// spawned onto the runtime instead.
pub(crate) fn run_on_runtime<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    let runtime = ensure_runtime();
    match tokio::runtime::Handle::try_current() {
        // Already on a runtime thread: cannot block that thread (it may be
        // driving other tasks). LocalSet-style await is unavailable in a sync
        // fn, so detach a task instead — Send bound applies to spawn only.
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        Err(_) => runtime.block_on(future),
    }
}

/// Debug probe: append a timestamped line to the session probe log.
pub(crate) fn probe(msg: &str) {
    use std::io::Write;
    let root = if let Some(dir) = std::env::var_os("OHM_MOBILE_DATA_DIR") {
        std::path::PathBuf::from(dir)
    } else if let Some(dir) = crate::manager::session_root() {
        dir
    } else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("probe.log"))
    {
        let _ = writeln!(
            f,
            "{} {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            msg
        );
    }
}

fn ensure_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_harness_types::UnsupportedEnv;
    use std::sync::Arc;

    #[test]
    fn resolves_relative_paths_against_sandbox_working_dir() {
        let env: Arc<dyn llm_harness_types::ExecutionEnv> =
            Arc::new(UnsupportedEnv::with_working_dir("/workspace"));
        assert_eq!(
            resolve_sandbox_path(&env, "result.txt").unwrap(),
            PathBuf::from("/workspace/result.txt")
        );
    }

    #[test]
    fn keeps_absolute_guest_paths_unchanged() {
        let env: Arc<dyn llm_harness_types::ExecutionEnv> =
            Arc::new(UnsupportedEnv::with_working_dir("/workspace"));
        assert_eq!(
            resolve_sandbox_path(&env, "/workspace/result.txt").unwrap(),
            PathBuf::from("/workspace/result.txt")
        );
    }

    #[test]
    fn rejects_empty_file_paths() {
        let env: Arc<dyn llm_harness_types::ExecutionEnv> =
            Arc::new(UnsupportedEnv::with_working_dir("/workspace"));
        assert!(resolve_sandbox_path(&env, "   ").is_err());
    }

    #[test]
    fn resolves_empty_directory_to_working_dir() {
        let env: Arc<dyn llm_harness_types::ExecutionEnv> =
            Arc::new(UnsupportedEnv::with_working_dir("/workspace"));
        assert_eq!(
            resolve_sandbox_directory(&env, "  ").unwrap(),
            PathBuf::from("/workspace")
        );
    }

    #[test]
    fn converts_file_info_for_ffi() {
        let info = llm_harness_types::FileInfo {
            path: PathBuf::from("/workspace/result.txt"),
            is_dir: false,
            size: 12,
            modified: chrono::DateTime::from_timestamp_millis(1_700_000_000_000)
                .expect("valid timestamp"),
        };
        let mobile = mobile_file_info(info);
        assert_eq!(mobile.path, "/workspace/result.txt");
        assert!(!mobile.is_dir);
        assert_eq!(mobile.size, 12);
        assert_eq!(mobile.modified_at_ms, 1_700_000_000_000);
    }
}
