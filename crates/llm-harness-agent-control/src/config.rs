use serde::Deserialize;

use crate::auth::AuthenticatedPrincipal;

#[derive(Debug, Clone, Deserialize)]
pub struct ControlServiceConfig {
    pub bind: Option<String>,
    pub port: Option<u16>,
    pub default_model: String,
    pub gateway: GatewayConfig,
    pub llm: LlmConfig,
    pub principals: Vec<PrincipalConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    pub base_url: String,
    pub token_env: String,
    #[serde(default)]
    pub allow_insecure_http: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LlmConfig {
    pub api_key_env: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrincipalConfig {
    pub principal: String,
    pub token_env: String,
    #[serde(default)]
    pub max_trust_profile: llm_harness_platform::trust::TrustProfile,
    #[serde(default)]
    pub max_sessions: Option<usize>,
}

fn default_provider() -> String {
    "anthropic".to_owned()
}

impl ControlServiceConfig {
    pub fn from_slice(value: &[u8]) -> anyhow::Result<Self> {
        serde_json::from_slice(value).map_err(|error| anyhow::anyhow!("invalid config: {error}"))
    }

    pub fn bind_addr(&self) -> String {
        self.bind.clone().unwrap_or_else(|| "127.0.0.1".to_owned())
    }

    pub fn port(&self) -> u16 {
        self.port.unwrap_or(7090)
    }

    pub fn llm_api_key(&self) -> anyhow::Result<String> {
        std::env::var(&self.llm.api_key_env).map_err(|error| {
            anyhow::anyhow!(
                "failed to read LLM API key from {}: {error}",
                self.llm.api_key_env
            )
        })
    }

    pub fn gateway_token(&self) -> anyhow::Result<String> {
        std::env::var(&self.gateway.token_env).map_err(|error| {
            anyhow::anyhow!(
                "failed to read gateway token from {}: {error}",
                self.gateway.token_env
            )
        })
    }

    pub(crate) fn authenticate_token(&self, token: &str) -> Option<AuthenticatedPrincipal> {
        let digest = sha2::Sha256::digest(token.as_bytes());
        self.principals.iter().find_map(|principal| {
            let expected = std::env::var(&principal.token_env).ok()?;
            let expected_digest = sha2::Sha256::digest(expected.as_bytes());
            (digest == expected_digest).then_some(AuthenticatedPrincipal {
                id: principal.principal.clone(),
                max_trust_profile: principal.max_trust_profile,
                max_sessions: principal.max_sessions.unwrap_or(16),
            })
        })
    }
}

use sha2::Digest;
