//! Mobile agent core: assembles the existing `AgentHarness` +
//! `RemoteSandboxClient` behind a small surface for a mobile UI.
//!
//! The core owns no retry/persistence policy of its own — errors propagate to
//! the caller (the UI) verbatim, per the MVP design spec.

mod events;
mod files;
pub mod manager;
mod runtime;
pub mod session;

pub use events::{MobileEvent, map_agent_event, map_event};
pub use files::MobileFileInfo;
pub use manager::{MobileHistoryMessage, MobileSessionManager, SessionSummary};
pub(crate) use runtime::run_on_runtime;
pub use session::MobileSession;

/// Public wrapper for integration tests; FFI surface uses it via connect.
pub fn parse_trust_profile_pub(
    value: &str,
) -> anyhow::Result<llm_harness_platform::trust::TrustProfile> {
    session::parse_trust_profile(value)
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
pub fn connect(
    config: MobileConfig,
) -> Result<std::sync::Arc<session::MobileSession>, MobileError> {
    let session = run_on_runtime(session::MobileSession::new(config))?;
    Ok(session)
}
