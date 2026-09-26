//! Mobile agent core: assembles the existing `AgentHarness` +
//! `RemoteSandboxClient` behind a small surface for a mobile UI.
//!
//! The core owns no retry/persistence policy of its own — errors propagate to
//! the caller (the UI) verbatim, per the MVP design spec.

use std::sync::Arc;

use anyhow::Context as _;
use tokio::sync::broadcast;

use llm_harness_agent::harness::{AgentHarness, AgentHarnessOptions};
use llm_harness_sandbox_remote::{RemoteSandbox, RemoteSandboxClient, RemoteSandboxClientConfig};
use llm_harness_tools::{BashTool, EditTool, GlobTool, GrepTool, ReadTool, WriteTool};

mod events;

pub use events::{MobileEvent, map_agent_event, map_event};

/// Connection configuration for a mobile session.
#[derive(Debug, Clone)]
pub struct MobileConfig {
    /// Gateway base URL, e.g. `http://127.0.0.1:7080`.
    pub gateway_url: String,
    /// Gateway bearer token (32+ bytes).
    pub gateway_token: String,
    /// LLM API key.
    pub api_key: String,
    /// LLM model id, e.g. `claude-sonnet-4-6`.
    pub model: String,
    /// LLM base URL override; empty uses the provider default.
    pub llm_base_url: String,
}

/// One mobile session: a remote sandbox plus an agent loop driving it.
pub struct MobileSession {
    harness: Arc<AgentHarness>,
    sandbox: Arc<RemoteSandbox>,
    event_tx: broadcast::Sender<MobileEvent>,
}

impl MobileSession {
    /// Connect to the gateway, create a sandbox, and assemble the harness.
    pub async fn connect(config: MobileConfig) -> anyhow::Result<Arc<Self>> {
        let client_config = RemoteSandboxClientConfig::new(
            config.gateway_url.clone(),
            config.gateway_token.clone(),
        )
        .context("invalid gateway client config")?;
        let client = Arc::new(RemoteSandboxClient::new(client_config)?);
        let sandbox = Arc::new(
            RemoteSandbox::connect(sandbox_config(), client)
                .await
                .context("connect to gateway sandbox")?,
        );
        let llm_client: Arc<dyn llm_adapter::Provider> = {
            let mut builder = llm_adapter::anthropic::AnthropicProvider::builder(config.api_key);
            if !config.llm_base_url.is_empty() {
                builder = builder.base_url(config.llm_base_url.clone());
            }
            Arc::new(builder.build())
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
             (a Linux workspace mounted at /workspace). Use the provided \
             tools to inspect files and run commands. Be concise."
                .to_owned(),
        );

        let harness = Arc::new(AgentHarness::new_in_memory(llm_client, env, opts).await);

        let (event_tx, _) = broadcast::channel(256);
        let session = Arc::new(Self {
            harness,
            sandbox,
            event_tx,
        });

        // Event pump: forward harness events to mobile subscribers.
        let mut rx = session.harness.subscribe();
        let tx = session.event_tx.clone();
        tokio::spawn(async move {
            loop {
                let event = match rx.recv().await {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let _ = tx.send(MobileEvent::Failed {
                            message: format!("event stream lagged by {n} events"),
                        });
                        continue;
                    }
                };
                if let Some(mobile) = map_event(&event) {
                    let _ = tx.send(mobile);
                }
            }
        });

        Ok(session)
    }

    /// Send a user message; returns immediately, results flow as events.
    pub fn send(self: &Arc<Self>, text: String) {
        let harness = Arc::clone(&self.harness);
        let session = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = harness.prompt(text).await {
                let _ = session.event_tx.send(MobileEvent::Failed {
                    message: error.to_string(),
                });
            }
        });
    }

    /// Subscribe to mobile events.
    pub fn events(&self) -> broadcast::Receiver<MobileEvent> {
        self.event_tx.subscribe()
    }

    /// Shut down the sandbox and release remote resources.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        llm_harness_platform::sandbox::Sandbox::shutdown(Box::new(<RemoteSandbox as Clone>::clone(
            &self.sandbox,
        )))
        .await
    }
}

/// MVP sandbox policy: workspace-only file access, no network allowlist, no
/// resource limits.
fn sandbox_config() -> llm_harness_platform::sandbox::SandboxConfig {
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
    }
}
