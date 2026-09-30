use std::path::PathBuf;

use serde::Deserialize;

use crate::auth::{FirecrackerBackendConfig, GatewayBackend, GatewayPrincipalConfig};

#[derive(Debug, Deserialize)]
pub struct GatewayServiceConfig {
    pub bind: Option<String>,
    pub port: Option<u16>,
    pub work_root: PathBuf,
    pub backend: BackendConfig,
    pub principals: Vec<PrincipalConfig>,
    pub max_sandboxes: Option<usize>,
    pub default_exec_timeout_ms: Option<u64>,
    pub max_exec_timeout_ms: Option<u64>,
    pub max_output_bytes: Option<usize>,
    pub max_request_bytes: Option<usize>,
    pub audit: Option<AuditConfig>,
}

#[derive(Debug, Deserialize)]
pub struct AuditConfig {
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub failure_mode: Option<crate::auth::AuditFailureMode>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BackendConfig {
    Bwrap,
    #[cfg(target_os = "macos")]
    Seatbelt,
    Firecracker(FirecrackerConfig),
}

#[derive(Debug, Deserialize)]
pub struct FirecrackerConfig {
    pub binary_path: PathBuf,
    pub kernel_image_path: PathBuf,
    pub base_image_path: PathBuf,
    pub max_vcpu_count: u32,
    pub max_memory_mib: u32,
    pub max_disk_mib: Option<u32>,
    pub vsock_guest_cid_base: u32,
    pub guest_agent_port: u32,
    pub readiness_timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct PrincipalConfig {
    pub principal: String,
    pub token_env: String,
    #[serde(default)]
    pub fs_allowlist: Vec<PathBuf>,
    #[serde(default)]
    pub host_project_dirs: Vec<PathBuf>,
    #[serde(default)]
    pub max_trust_profile: llm_harness_platform::trust::TrustProfile,
    #[serde(default)]
    pub requests_per_minute: Option<usize>,
}

impl GatewayServiceConfig {
    pub fn from_slice(value: &[u8]) -> anyhow::Result<Self> {
        serde_json::from_slice(value).map_err(|error| anyhow::anyhow!("invalid config: {error}"))
    }

    pub fn into_gateway_config(self) -> anyhow::Result<crate::GatewayConfig> {
        let principals = self
            .principals
            .into_iter()
            .map(|principal| {
                let bearer_token = std::env::var(&principal.token_env).map_err(|error| {
                    anyhow::anyhow!(
                        "failed to read bearer token from {}: {error}",
                        principal.token_env
                    )
                })?;
                Ok(GatewayPrincipalConfig {
                    principal: principal.principal,
                    bearer_token,
                    fs_allowlist: principal.fs_allowlist,
                    host_project_dirs: principal.host_project_dirs,
                    max_trust_profile: principal.max_trust_profile,
                    requests_per_minute: principal.requests_per_minute,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let mut config = crate::GatewayConfig::new(principals)?;
        config = config.with_work_root(self.work_root);
        if let Some(value) = self.max_sandboxes {
            config = config.with_max_sandboxes(value);
        }
        if let (Some(default_ms), Some(max_ms)) =
            (self.default_exec_timeout_ms, self.max_exec_timeout_ms)
        {
            config = config.with_exec_timeout_ms(default_ms, max_ms);
        }
        if let Some(value) = self.max_output_bytes {
            config = config.with_max_output_bytes(value);
        }
        if let Some(value) = self.max_request_bytes {
            config = config.with_max_request_bytes(value);
        }
        if let Some(audit) = self.audit {
            if let Some(path) = audit.path {
                config = config.with_audit_path(path);
            }
            if let Some(failure_mode) = audit.failure_mode {
                config = config.with_audit_failure_mode(failure_mode);
            }
        }
        config = config.with_backend(self.backend.into_gateway_backend()?);
        Ok(config)
    }

    pub fn bind_addr(&self) -> String {
        self.bind.clone().unwrap_or_else(|| "127.0.0.1".to_owned())
    }

    pub fn port(&self) -> u16 {
        self.port.unwrap_or(7080)
    }
}

impl BackendConfig {
    fn into_gateway_backend(self) -> anyhow::Result<GatewayBackend> {
        match self {
            Self::Bwrap => Ok(GatewayBackend::Bwrap),
            #[cfg(target_os = "macos")]
            Self::Seatbelt => Ok(GatewayBackend::Seatbelt),
            Self::Firecracker(config) => {
                Ok(GatewayBackend::Firecracker(FirecrackerBackendConfig {
                    binary_path: config.binary_path,
                    kernel_image_path: config.kernel_image_path,
                    base_image_path: config.base_image_path,
                    max_vcpu_count: config.max_vcpu_count,
                    max_memory_mib: config.max_memory_mib,
                    max_disk_mib: config.max_disk_mib,
                    vsock_guest_cid_base: config.vsock_guest_cid_base,
                    guest_agent_port: config.guest_agent_port,
                    readiness_timeout: std::time::Duration::from_millis(
                        config.readiness_timeout_ms,
                    ),
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_service_config() {
        let config = GatewayServiceConfig::from_slice(
            br#"{
                "work_root": "/tmp/llm-harness-sandbox-gateway",
                "backend": {"type": "bwrap"},
                "principals": [
                    {
                        "principal": "phone",
                        "token_env": "PHONE_TOKEN",
                        "max_trust_profile": "trusted_lan"
                    }
                ]
            }"#,
        )
        .expect("valid config");

        assert_eq!(config.bind_addr(), "127.0.0.1");
        assert_eq!(config.port(), 7080);
        assert_eq!(config.principals.len(), 1);
        assert_eq!(config.principals[0].token_env, "PHONE_TOKEN");
    }

    #[test]
    fn parses_audit_service_config() {
        let config = GatewayServiceConfig::from_slice(
            br#"{
                "work_root": "/tmp/llm-harness-sandbox-gateway",
                "backend": {"type": "bwrap"},
                "principals": [
                    {
                        "principal": "phone",
                        "token_env": "PHONE_TOKEN"
                    }
                ],
                "audit": {
                    "path": "/var/log/llm-harness-sandbox-gateway/audit.jsonl",
                    "failure_mode": "fail_open"
                }
            }"#,
        )
        .expect("valid config");

        assert_eq!(
            config
                .audit
                .as_ref()
                .and_then(|audit| audit.path.as_deref()),
            Some(PathBuf::from(
                "/var/log/llm-harness-sandbox-gateway/audit.jsonl"
            ))
            .as_deref()
        );
        assert_eq!(
            config.audit.and_then(|audit| audit.failure_mode),
            Some(crate::AuditFailureMode::FailOpen)
        );
    }
}
