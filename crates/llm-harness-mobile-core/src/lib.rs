//! Mobile agent core: assembles the existing `AgentHarness` +
//! `RemoteSandboxClient` behind a small surface for a mobile UI.
//!
//! The core owns no retry/persistence policy of its own — errors propagate to
//! the caller (the UI) verbatim, per the MVP design spec.

use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use tokio::sync::broadcast;

use llm_harness_agent::harness::{AgentHarness, AgentHarnessOptions};
use llm_harness_sandbox_remote::{RemoteSandbox, RemoteSandboxClient, RemoteSandboxClientConfig};
use llm_harness_tools::{BashTool, EditTool, GlobTool, GrepTool, ReadTool, WriteTool};

mod events;

pub use events::{MobileEvent, map_agent_event, map_event};
/// Public wrapper for integration tests; FFI surface uses it via connect.
pub fn parse_trust_profile_pub(value: &str) -> anyhow::Result<llm_harness_platform::trust::TrustProfile> {
    parse_trust_profile(value)
}

uniffi::setup_scaffolding!();

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
    queue: Mutex<Vec<MobileEvent>>,
}

impl MobileSession {
    pub(crate) async fn new(config: MobileConfig) -> anyhow::Result<Arc<Self>> {
        ensure_runtime();
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

        let harness = Arc::new(AgentHarness::new_in_memory(llm_client, env, opts).await);

        let session = Arc::new(Self {
            harness,
            sandbox,
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
        match remote {
            Some(llm_harness_sandbox_remote::RemoteSandboxError::InvalidConfig(_)) => {
                MobileError::InvalidConfig {
                    detail: error.to_string(),
                }
            }
            Some(_) => MobileError::Connect {
                detail: error.to_string(),
            },
            None => MobileError::Internal {
                detail: error.to_string(),
            },
        }
    }
}
#[uniffi::export]
pub async fn connect(config: MobileConfig) -> Result<Arc<MobileSession>, MobileError> {
    let session = ensure_runtime().block_on(MobileSession::new(config))?;
    Ok(session)
}

/// Install a global multi-thread tokio runtime exactly once.
///
/// uniffi drives its async entry points on the foreign side, so the Rust
/// futures need a live runtime context for `tokio::spawn` and IO resources.
/// The handle leaks deliberately: the runtime must live for the whole process.
fn ensure_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime")
    })
}
