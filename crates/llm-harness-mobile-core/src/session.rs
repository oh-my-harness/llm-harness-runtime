//! `MobileSession`: one remote sandbox plus an agent loop driving it.
//!
//! Events are drained via `poll_event` (an FFI-side queue the UI polls);
//! this sidesteps cross-FFI callbacks.

use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use llm_harness_agent::harness::{AgentHarness, AgentHarnessOptions};
use llm_harness_platform::sandbox::Sandbox;
use llm_harness_sandbox_remote::{RemoteSandbox, RemoteSandboxClient, RemoteSandboxClientConfig};
use llm_harness_tools::{BashTool, EditTool, GlobTool, GrepTool, ReadTool, WriteTool};
use llm_harness_types::{AgentMessage, ContentBlock, UserMessage};

use crate::{
    MobileConfig, MobileError, MobileEvent,
    events::map_event,
    files::{MobileFileInfo, mobile_file_info, resolve_sandbox_directory, resolve_sandbox_path},
    runtime::{ensure_runtime, run_on_runtime},
};

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

/// One mobile session: a remote sandbox plus an agent loop driving it.
///
/// Events are drained via `poll_event`: an internal FFI-side queue buffers
/// every mapped event; the UI polls (e.g. on a 100ms timer) and renders.
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
        let sandbox = connect_sandbox(&config).await?;
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
