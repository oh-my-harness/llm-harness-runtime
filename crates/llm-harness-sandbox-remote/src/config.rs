use std::fmt;
use std::time::Duration;

use llm_harness_platform::sandbox::{NetRule, ResourceLimits, SandboxConfig};
use reqwest::Url;

use crate::error::RemoteSandboxError;

/// Configuration for a runtime-side remote-sandbox client.
#[derive(Clone)]
pub struct RemoteSandboxClientConfig {
    /// Gateway base URL, for example `https://sandbox.example.internal`.
    pub endpoint: Url,
    /// Bearer token used for gateway authentication.
    pub bearer_token: String,
    /// Explicitly allow plain HTTP to non-loopback endpoints.
    ///
    /// Default `false`: non-loopback endpoints must use HTTPS so the bearer
    /// token never crosses an untrusted network in clear text. Set `true`
    /// only for trusted LANs (e.g. a phone talking to a gateway on the same
    /// home WiFi) where TLS termination is not practical.
    pub allow_insecure_http: bool,
    /// TCP/TLS connection timeout.
    pub connect_timeout: Duration,
    /// Timeout for non-streaming control and file requests.
    pub request_timeout: Duration,
    /// Upper bound for a streaming shell execution.
    pub max_exec_timeout: Duration,
    /// Upper bound for a non-streaming JSON response body.
    pub max_response_bytes: usize,
}

impl fmt::Debug for RemoteSandboxClientConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteSandboxClientConfig")
            .field("endpoint", &self.endpoint)
            .field("bearer_token", &"<redacted>")
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("max_exec_timeout", &self.max_exec_timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

impl RemoteSandboxClientConfig {
    /// Create a validated client configuration. Non-loopback plain-HTTP
    /// endpoints are rejected; use [`Self::new_insecure`] to opt in.
    pub fn new(
        endpoint: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, RemoteSandboxError> {
        Self::build(endpoint, bearer_token, false)
    }

    fn build(
        endpoint: impl Into<String>,
        bearer_token: impl Into<String>,
        allow_insecure_http: bool,
    ) -> Result<Self, RemoteSandboxError> {
        let mut endpoint = Url::parse(&endpoint.into()).map_err(|error| {
            RemoteSandboxError::invalid_config(format!("invalid endpoint: {error}"))
        })?;
        let bearer_token = bearer_token.into();
        if !endpoint.path().ends_with('/') {
            let mut path = endpoint.path().to_owned();
            path.push('/');
            endpoint.set_path(&path);
        }
        Self::validate_endpoint_and_token(&endpoint, &bearer_token, allow_insecure_http)?;
        Self::validate_runtime_options(
            Duration::from_secs(5),
            Duration::from_secs(30),
            Duration::from_secs(60 * 60),
            64 * 1024 * 1024,
        )?;

        Ok(Self {
            endpoint,
            bearer_token,
            allow_insecure_http,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            max_exec_timeout: Duration::from_secs(60 * 60),
            max_response_bytes: 64 * 1024 * 1024,
        })
    }

    /// Create a client config that permits plain HTTP to non-loopback
    /// endpoints. Only for trusted LANs (e.g. a phone and the gateway on the
    /// same home WiFi) where the bearer token never crosses an untrusted
    /// network; the name makes the trade-off visible at call sites.
    pub fn new_insecure(
        endpoint: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, RemoteSandboxError> {
        Self::build(endpoint, bearer_token, true)
    }

    /// Explicitly permit plain HTTP to non-loopback endpoints.
    pub fn with_allow_insecure_http(mut self, allow: bool) -> Self {
        self.allow_insecure_http = allow;
        self
    }

    /// Override the default connection timeout.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Override the default non-streaming request timeout.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Override the default streaming execution upper bound.
    pub fn with_max_exec_timeout(mut self, timeout: Duration) -> Self {
        self.max_exec_timeout = timeout;
        self
    }

    /// Override the default non-streaming response-body limit.
    pub fn with_max_response_bytes(mut self, max_bytes: usize) -> Self {
        self.max_response_bytes = max_bytes;
        self
    }

    pub(crate) fn validate(&self) -> Result<(), RemoteSandboxError> {
        Self::validate_endpoint_and_token(
            &self.endpoint,
            &self.bearer_token,
            self.allow_insecure_http,
        )?;
        Self::validate_runtime_options(
            self.connect_timeout,
            self.request_timeout,
            self.max_exec_timeout,
            self.max_response_bytes,
        )
    }

    fn validate_endpoint_and_token(
        endpoint: &Url,
        bearer_token: &str,
        allow_insecure_http: bool,
    ) -> Result<(), RemoteSandboxError> {
        if !matches!(endpoint.scheme(), "http" | "https") {
            return Err(RemoteSandboxError::invalid_config(
                "remote sandbox endpoint must use http or https",
            ));
        }
        if endpoint.scheme() == "http"
            && !allow_insecure_http
            && !is_loopback_host(endpoint.host_str().unwrap_or_default())
        {
            return Err(RemoteSandboxError::invalid_config(
                "remote sandbox endpoint must use HTTPS for non-loopback hosts \
                 (set allow_insecure_http to explicitly permit plain HTTP on a \
                 trusted network)",
            ));
        }
        if endpoint.host_str().is_none() {
            return Err(RemoteSandboxError::invalid_config(
                "remote sandbox endpoint must contain a host",
            ));
        }
        if !endpoint.username().is_empty()
            || endpoint
                .password()
                .is_some_and(|password| !password.is_empty())
        {
            return Err(RemoteSandboxError::invalid_config(
                "remote sandbox endpoint must not contain credentials",
            ));
        }
        if endpoint.query().is_some() || endpoint.fragment().is_some() {
            return Err(RemoteSandboxError::invalid_config(
                "remote sandbox endpoint must not contain a query or fragment",
            ));
        }
        if bearer_token.is_empty() {
            return Err(RemoteSandboxError::invalid_config(
                "remote sandbox bearer token must not be empty",
            ));
        }
        if bearer_token
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(RemoteSandboxError::invalid_config(
                "remote sandbox bearer token must not contain control or whitespace characters",
            ));
        }
        Ok(())
    }

    fn validate_runtime_options(
        connect_timeout: Duration,
        request_timeout: Duration,
        max_exec_timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<(), RemoteSandboxError> {
        if connect_timeout.is_zero() {
            return Err(RemoteSandboxError::invalid_config(
                "remote sandbox connect timeout must be positive",
            ));
        }
        if request_timeout.is_zero() {
            return Err(RemoteSandboxError::invalid_config(
                "remote sandbox request timeout must be positive",
            ));
        }
        if max_exec_timeout.is_zero() {
            return Err(RemoteSandboxError::invalid_config(
                "remote sandbox max execution timeout must be positive",
            ));
        }
        if max_exec_timeout.as_millis() == 0 {
            return Err(RemoteSandboxError::invalid_config(
                "remote sandbox max execution timeout must be at least one millisecond",
            ));
        }
        if u64::try_from(max_exec_timeout.as_millis()).is_err() {
            return Err(RemoteSandboxError::invalid_config(
                "remote sandbox max execution timeout is too large",
            ));
        }
        if max_response_bytes == 0 {
            return Err(RemoteSandboxError::invalid_config(
                "remote sandbox max response bytes must be positive",
            ));
        }
        Ok(())
    }
}

fn is_loopback_host(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Wire representation of [`SandboxConfig`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteSandboxConfig {
    pub fs_allowlist: Vec<String>,
    pub fs_denylist: Vec<String>,
    pub net_allowlist: Vec<NetRuleWire>,
    pub resource_limits: ResourceLimitsWire,
    pub work_dir: Option<String>,
    /// Trust level requested for the sandbox. Defaults to `restricted` so
    /// clients that predate the field keep working unchanged.
    #[serde(default)]
    pub trust_profile: llm_harness_platform::trust::TrustProfile,
}

/// Wire representation of a network allowlist rule.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NetRuleWire {
    pub host: String,
    pub port_range: Option<(u16, u16)>,
}

/// Wire representation of resource limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResourceLimitsWire {
    pub max_cpus: Option<usize>,
    pub max_processes: Option<usize>,
    pub max_memory_mb: Option<usize>,
    pub max_disk_mb: Option<usize>,
    pub timeout_ms: Option<u64>,
}

impl TryFrom<SandboxConfig> for RemoteSandboxConfig {
    type Error = RemoteSandboxError;

    fn try_from(value: SandboxConfig) -> Result<Self, Self::Error> {
        let fs_allowlist = value
            .fs_allowlist
            .iter()
            .map(|path| crate::client::path_string(path))
            .collect::<Result<Vec<_>, _>>()?;
        let fs_denylist = value
            .fs_denylist
            .iter()
            .map(|path| crate::client::path_string(path))
            .collect::<Result<Vec<_>, _>>()?;
        let net_allowlist = value
            .net_allowlist
            .iter()
            .map(NetRuleWire::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let work_dir = match value.work_dir {
            Some(path) => Some(crate::client::path_string(&path)?),
            None => None,
        };
        let resource_limits = ResourceLimitsWire::try_from(value.resource_limits)?;
        Ok(Self {
            fs_allowlist,
            fs_denylist,
            net_allowlist,
            resource_limits,
            work_dir,
            trust_profile: value.trust_profile,
        })
    }
}

impl TryFrom<&NetRule> for NetRuleWire {
    type Error = RemoteSandboxError;

    fn try_from(value: &NetRule) -> Result<Self, Self::Error> {
        if value.host.trim().is_empty() {
            return Err(RemoteSandboxError::invalid_config(
                "network allowlist host must not be empty",
            ));
        }
        if value
            .host
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(RemoteSandboxError::invalid_config(
                "network allowlist host must not contain control or whitespace characters",
            ));
        }
        if let Some((low, high)) = value.port_range
            && low > high
        {
            return Err(RemoteSandboxError::invalid_config(
                "network allowlist port range must be ordered",
            ));
        }
        Ok(Self {
            host: value.host.clone(),
            port_range: value.port_range,
        })
    }
}

impl TryFrom<ResourceLimits> for ResourceLimitsWire {
    type Error = RemoteSandboxError;

    fn try_from(value: ResourceLimits) -> Result<Self, Self::Error> {
        if let Some(max_cpus) = value.max_cpus
            && max_cpus == 0
        {
            return Err(RemoteSandboxError::invalid_config(
                "max_cpus must be positive",
            ));
        }
        if let Some(max_processes) = value.max_processes
            && max_processes == 0
        {
            return Err(RemoteSandboxError::invalid_config(
                "max_processes must be positive",
            ));
        }
        if let Some(max_memory_mb) = value.max_memory_mb
            && max_memory_mb == 0
        {
            return Err(RemoteSandboxError::invalid_config(
                "max_memory_mb must be positive",
            ));
        }
        if let Some(max_disk_mb) = value.max_disk_mb
            && max_disk_mb == 0
        {
            return Err(RemoteSandboxError::invalid_config(
                "max_disk_mb must be positive",
            ));
        }
        if let Some(timeout) = value.timeout
            && timeout.is_zero()
        {
            return Err(RemoteSandboxError::invalid_config(
                "resource timeout must be positive",
            ));
        }
        Ok(Self {
            max_cpus: value.max_cpus,
            max_processes: value.max_processes,
            max_memory_mb: value.max_memory_mb,
            max_disk_mb: value.max_disk_mb,
            timeout_ms: value
                .timeout
                .map(|timeout| {
                    u64::try_from(timeout.as_millis()).map_err(|_| {
                        RemoteSandboxError::invalid_config("resource timeout is too large")
                    })
                })
                .transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_endpoint_with_credentials_query_or_fragment() {
        for endpoint in [
            "http://user:pass@example.internal",
            "https://example.internal?token=secret",
            "https://example.internal#section",
        ] {
            let error = RemoteSandboxClientConfig::new(endpoint, "token")
                .expect_err("endpoint should be rejected");
            assert!(matches!(error, RemoteSandboxError::InvalidConfig(_)));
        }
    }

    #[test]
    fn trust_profile_defaults_to_restricted_and_roundtrips() {
        let json = r#"{"fs_allowlist":[],"fs_denylist":[],"net_allowlist":[],"resource_limits":{"max_cpus":null,"max_processes":null,"max_memory_mb":null,"max_disk_mb":null,"timeout":null}}"#;
        let config: RemoteSandboxConfig =
            serde_json::from_str(json).expect("old clients without trust_profile parse");
        assert_eq!(
            config.trust_profile,
            llm_harness_platform::trust::TrustProfile::Restricted
        );

        let json_lan = json.replace(
            r#""net_allowlist":[]"#,
            r#""net_allowlist":[],"trust_profile":"trusted_lan""#,
        );
        let config: RemoteSandboxConfig =
            serde_json::from_str(&json_lan).expect("explicit trust_profile parses");
        assert_eq!(
            config.trust_profile,
            llm_harness_platform::trust::TrustProfile::TrustedLan
        );
        assert_eq!(
            serde_json::to_string(&config.trust_profile).unwrap(),
            "\"trusted_lan\""
        );
    }

    #[test]
    fn trust_profile_orders_restricted_lt_lan_lt_full() {
        use llm_harness_platform::trust::TrustProfile;
        assert!(TrustProfile::Restricted < TrustProfile::TrustedLan);
        assert!(TrustProfile::TrustedLan < TrustProfile::TrustedFull);
    }

    #[test]
    fn rejects_zero_runtime_limits() {
        let error = RemoteSandboxClientConfig::validate_runtime_options(
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::from_secs(1),
            1,
        )
        .expect_err("zero timeout should be rejected");
        assert!(matches!(error, RemoteSandboxError::InvalidConfig(_)));
    }

    #[test]
    fn rejects_relative_and_traversal_guest_paths() {
        for work_dir in [
            std::path::PathBuf::from("relative"),
            std::path::PathBuf::from("/a/../b"),
            std::path::PathBuf::from("/a\0b"),
        ] {
            let config = SandboxConfig {
                fs_allowlist: Vec::new(),
                fs_denylist: Vec::new(),
                net_allowlist: Vec::new(),
                resource_limits: ResourceLimits {
                    max_cpus: None,
                    max_processes: None,
                    max_memory_mb: None,
                    max_disk_mb: None,
                    timeout: None,
                },
                work_dir: Some(work_dir),
                trust_profile: Default::default(),
                host_binds: Vec::new(),
            };
            let error = RemoteSandboxConfig::try_from(config)
                .expect_err("invalid guest path should be rejected");
            assert!(matches!(error, RemoteSandboxError::InvalidConfig(_)));
        }
    }

    #[test]
    fn redacts_bearer_token_in_debug_output_and_requires_tls_off_loopback() {
        let config = RemoteSandboxClientConfig::new("http://127.0.0.1:8080", "secret-token")
            .expect("loopback HTTP is valid");
        assert!(!format!("{config:?}").contains("secret-token"));

        let error = RemoteSandboxClientConfig::new("http://sandbox.example.internal", "token")
            .expect_err("non-loopback HTTP should be rejected");
        assert!(matches!(error, RemoteSandboxError::InvalidConfig(_)));
    }

    #[test]
    fn allows_loopback_http_for_ipv6_addresses() {
        for endpoint in ["http://[::1]:8080", "http://[0:0:0:0:0:0:0:1]:8080"] {
            RemoteSandboxClientConfig::new(endpoint, "token")
                .expect("IPv6 loopback HTTP should be valid");
        }
    }

    #[test]
    fn allow_insecure_http_opt_in_permits_non_loopback_http() {
        // Default: `new` rejects non-loopback HTTP.
        let error = RemoteSandboxClientConfig::new("http://192.168.31.20:7080", "token")
            .expect_err("non-loopback HTTP must default to rejected");
        assert!(matches!(error, RemoteSandboxError::InvalidConfig(_)));

        // Opt-in via `new_insecure`: accepted and auditable.
        let config = RemoteSandboxClientConfig::new_insecure("http://192.168.31.20:7080", "token")
            .expect("opted-in insecure HTTP is valid");
        assert!(config.allow_insecure_http);
        config.validate().expect("flag consistent with validation");

        // HTTPS endpoints never need the flag.
        RemoteSandboxClientConfig::new("https://sandbox.example.internal", "token")
            .expect("HTTPS is always valid");
    }

    #[test]
    fn rejects_max_execution_timeout_below_the_wire_format_resolution() {
        let error = RemoteSandboxClientConfig::validate_runtime_options(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_micros(999),
            1,
        )
        .expect_err("sub-millisecond timeout should be rejected");
        assert!(matches!(error, RemoteSandboxError::InvalidConfig(_)));
    }
}
