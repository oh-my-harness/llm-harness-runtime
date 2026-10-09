//! `MobileSession`: one remote sandbox plus an agent loop driving it.
//!
//! Events are drained via `poll_event` (an FFI-side queue the UI polls);
//! this sidesteps cross-FFI callbacks.

use std::{
    path::PathBuf,
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

/// Build an LLM provider from a (provider, api key, base URL) triple.
///
/// `provider` selects the wire protocol; anything other than `"openai"` falls
/// back to the Anthropic protocol, matching [`MobileConfig::provider`]'s
/// documented default. An empty `base_url` keeps the provider's own default
/// endpoint. Construction performs no I/O, so a bad URL/key only surfaces when
/// a turn is actually sent.
pub(crate) fn build_provider(
    provider: &str,
    api_key: &str,
    base_url: &str,
) -> Arc<dyn llm_adapter::Provider> {
    match provider {
        "openai" => {
            let mut builder = llm_adapter::openai::OpenAIProvider::builder(api_key.to_owned());
            if !base_url.is_empty() {
                builder = builder.base_url(base_url.to_owned());
            }
            Arc::new(builder.build())
        }
        _ => {
            let mut builder =
                llm_adapter::anthropic::AnthropicProvider::builder(api_key.to_owned());
            if !base_url.is_empty() {
                builder = builder.base_url(base_url.to_owned());
            }
            Arc::new(builder.build())
        }
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
/// Execution backend for a session: a remote gateway sandbox or the phone
/// itself (no tools, pure chat).
#[derive(Clone)]
pub(crate) enum SandboxHandle {
    Remote(Arc<RemoteSandbox>),
    /// Sandbox-free session: no tools, no gateway. LLM access only.
    Local,
}

impl SandboxHandle {
    fn env(&self) -> Arc<dyn llm_harness_types::ExecutionEnv> {
        match self {
            SandboxHandle::Remote(sandbox) => Sandbox::env(&**sandbox),
            SandboxHandle::Local => Arc::new(llm_harness_sandbox::OsEnv::new(std::env::temp_dir())),
        }
    }

    /// Host project binds (remote only; local sessions have none).
    fn host_binds(&self) -> Vec<(PathBuf, PathBuf)> {
        match self {
            SandboxHandle::Remote(sandbox) => sandbox.host_binds().to_vec(),
            SandboxHandle::Local => Vec::new(),
        }
    }

    /// Gateway sandbox id (remote only; local sessions report empty).
    fn sandbox_id(&self) -> &str {
        match self {
            SandboxHandle::Remote(sandbox) => sandbox.sandbox_id(),
            SandboxHandle::Local => "",
        }
    }
}

#[derive(uniffi::Object)]
pub struct MobileSession {
    harness: Arc<AgentHarness>,
    sandbox: SandboxHandle,
    session_id: Option<String>,
    sandbox_recreated: bool,
    queue: Mutex<Vec<MobileEvent>>,
}

impl MobileSession {
    pub(crate) async fn new(config: MobileConfig) -> anyhow::Result<Arc<Self>> {
        if config.gateway_url.is_empty() {
            return Self::assemble(config, SandboxHandle::Local, None, false).await;
        }
        let sandbox = connect_sandbox(&config).await?;
        Self::assemble(config, SandboxHandle::Remote(sandbox), None, false).await
    }

    /// Assemble a session around an existing backend. `session_id`
    /// attaches the session to a persisted `Session` (resume); `None` creates
    /// an in-memory session (MVP path).
    pub(crate) async fn assemble(
        config: MobileConfig,
        sandbox: SandboxHandle,
        session_id: Option<String>,
        sandbox_recreated: bool,
    ) -> anyhow::Result<Arc<Self>> {
        // Runtime context comes from the caller's block_on; no bare enter.
        let llm_client = build_provider(&config.provider, &config.api_key, &config.llm_base_url);

        let env = sandbox.env();

        let mut opts = AgentHarnessOptions::new(config.model.clone());
        match &sandbox {
            SandboxHandle::Remote(_) => {
                opts.tools = vec![
                    Arc::new(BashTool::new()) as Arc<dyn llm_harness_types::Tool>,
                    Arc::new(ReadTool::new(None)),
                    Arc::new(WriteTool::new(None)),
                    Arc::new(EditTool::new(None)),
                    Arc::new(GlobTool::new()),
                    Arc::new(GrepTool::new()),
                ];
            }
            // Sandbox-free sessions get no tools: pure LLM conversation.
            SandboxHandle::Local => {}
        }
        // Trusted profiles bind host project directories read-write at their
        // real host paths. Tell the agent where they are so it works in the
        // real repo instead of concluding the sandbox is empty (its cwd is a
        // fresh work dir). `git` cannot read /etc/gitconfig under the
        // seatbelt policy; GIT_CONFIG_NOSYSTEM=1 avoids the fatal error.
        let host_binds: Vec<PathBuf> = sandbox
            .host_binds()
            .iter()
            .map(|(host, _guest)| host.clone())
            .collect();

        // ── System prompt: baseline only; project knowledge comes from the
        // repo's AGENTS.md and the skills system, not from hardcoded prompt
        // text (omp model: context files + skills metadata + skill:// reads).
        let conclusion_first = "When running tests or builds, state the \
            conclusion first (e.g. \"12 passed, 3 failed\") before any details \
            or logs.";
        let mut prompt = if sandbox.host_binds().is_empty() {
            "You are a helpful assistant. Be concise.".to_owned()
        } else {
            "You are a helpful agent working on the user's machine with \
             direct shell access. Be concise."
                .to_owned()
        };
        prompt.push_str(conclusion_first);

        if !host_binds.is_empty() {
            // Machine facts the agent cannot discover: bind locations and
            // the seatbelt gitconfig quirk. Kept minimal; project knowledge
            // belongs in the repo's AGENTS.md, loaded below.
            let mut extra = String::from(
                "\n\n## Environment\n- Host directories bound read-write to \
                 this session; always use these absolute paths for cd/ls/git \
                 (there is no /home, and /workspace/<name> does not exist in \
                 shells):\n",
            );
            for host in &host_binds {
                extra.push_str(&format!("  - {}\n", host.display()));
            }
            extra.push_str(
                "- Your shell starts directly inside the first directory \
                 above; relative paths work from there.\n",
            );
            prompt.push_str(&extra);
            // Project context: the repo's own AGENTS.md (project knowledge
            // belongs to the repo, not to hardcoded prompt text). Truncate
            // very long files — context files are advisory.
            let agents_root = host_binds.first().unwrap();
            let agents_md = agents_root.join("AGENTS.md");
            if let Ok(content) = tokio::fs::read_to_string(&agents_md).await {
                // Truncate very long files: context files are advisory.
                let content = if content.chars().count() > 12_000 {
                    let head: String = content.chars().take(12_000).collect();
                    format!("{head}\n…(truncated)")
                } else {
                    content
                };
                prompt.push_str("\n\n## Project instructions (AGENTS.md)\n\n");
                prompt.push_str(&content);
            }
        }
        opts.system_prompt = Some(prompt);

        // ── Skills: load from the machine's skills root and expose the
        // skill_read tool (omp-style: prompt carries name+description only;
        // the model reads skill://<name> on demand).
        if !host_binds.is_empty() {
            let skills_root = host_binds[0].join(".harness").join("skills");
            let (skills, _diags) =
                llm_harness_agent::load_skills(env.as_ref(), &[skills_root]).await;
            if !skills.is_empty() {
                let readable = Arc::new(std::sync::RwLock::new(skills.clone()));
                opts.skills = skills;
                opts.readable_skills = Some(readable);
                opts.tools
                    .push(Arc::new(llm_harness_agent::SkillReadTool::new(
                        opts.readable_skills.clone().unwrap(),
                        env.clone(),
                    )));
            }
        }
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

    /// Switch the LLM behind this session: model id plus the provider
    /// configuration (provider protocol, base URL, API key) it runs on.
    ///
    /// This is what the "switch LLM profile" UI calls: the app never has to
    /// re-enter a key to change models. A new provider client is built and
    /// installed, so cross-provider switches work mid-conversation instead of
    /// requiring a new session.
    ///
    /// Takes effect at the start of the next turn: Idle applies immediately, a
    /// running turn keeps the client it started with and the change is queued
    /// (the harness records a `ModelChange` entry naming the new provider).
    /// `contextWindow` / `maxTokens` are optional compaction-estimation hints.
    #[uniffi::method(name = "setLlm")]
    pub fn set_llm(
        &self,
        model: String,
        provider: String,
        base_url: String,
        api_key: String,
        context_window: Option<u32>,
        max_tokens: Option<u32>,
    ) -> Result<(), MobileError> {
        let info = match (context_window, max_tokens) {
            (Some(window), Some(tokens)) => Some(llm_harness_agent::agent::ModelInfo {
                context_window: window,
                max_tokens: tokens,
            }),
            _ => None,
        };
        let client = build_provider(&provider, &api_key, &base_url);
        let harness = Arc::clone(&self.harness);
        run_on_runtime(async move {
            harness
                .switch_llm(client, model, info, Some(provider))
                .await
                .map_err(|e| MobileError::Internal {
                    detail: format!("switch llm: {e}"),
                })
        })
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
        let sandbox = self.sandbox.clone();
        Ok(ensure_runtime()
            .spawn(async move {
                match sandbox {
                    SandboxHandle::Remote(sandbox) => {
                        llm_harness_platform::sandbox::Sandbox::shutdown(Box::new(
                            <RemoteSandbox as Clone>::clone(&sandbox),
                        ))
                        .await
                    }
                    // Local sessions hold no host resources to release.
                    SandboxHandle::Local => Ok(()),
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn chat_only_config(provider: &str, model: &str, base_url: &str) -> MobileConfig {
        MobileConfig {
            gateway_url: String::new(),
            gateway_token: String::new(),
            api_key: "test-key".into(),
            model: model.into(),
            llm_base_url: base_url.into(),
            provider: provider.into(),
            allow_insecure_http: false,
            trust_profile: "restricted".into(),
        }
    }

    /// Wire-protocol selection: `openai` builds the OpenAI-compatible provider,
    /// every other name falls back to the Anthropic protocol. The two protocols
    /// advertise distinct capabilities, so this is observable without I/O.
    #[test]
    fn build_provider_selects_protocol_and_defaults_to_anthropic() {
        let openai = build_provider("openai", "k", "http://127.0.0.1:9/v1");
        assert!(
            openai.capabilities().supports_json_schema(),
            "openai protocol expected for provider=\"openai\""
        );

        let anthropic = build_provider("anthropic", "k", "");
        assert!(
            !anthropic.capabilities().supports_json_schema(),
            "anthropic protocol expected for provider=\"anthropic\""
        );

        let fallback = build_provider("some-unknown-gateway", "k", "");
        assert_eq!(
            fallback.capabilities(),
            anthropic.capabilities(),
            "unknown provider names must fall back to the anthropic protocol"
        );
    }

    /// Switching LLM configuration must not need a gateway, a network round
    /// trip, or a new session: the client is rebuilt from the profile and the
    /// harness picks it up for the next turn.
    #[tokio::test(flavor = "multi_thread")]
    async fn set_llm_switches_provider_mid_session() {
        let session = MobileSession::new(chat_only_config(
            "openai",
            "deepseek-v4-flash",
            "https://api.deepseek.com",
        ))
        .await
        .expect("chat-only session assembly must not touch the network");
        assert_eq!(session.harness.state().model, "deepseek-v4-flash");

        // Cross-provider switch: openai/deepseek -> anthropic/claude.
        session
            .set_llm(
                "claude-sonnet-4-6".into(),
                "anthropic".into(),
                "https://api.anthropic.com".into(),
                "other-key".into(),
                None,
                None,
            )
            .expect("switch must succeed while idle");

        assert_eq!(session.harness.state().model, "claude-sonnet-4-6");
    }
}
