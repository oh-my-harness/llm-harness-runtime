use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

const CONFIG_CANDIDATES: [&str; 2] = [".mcp.json", "mcp.json"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpConfigFile {
    #[serde(rename = "mcpServers")]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        timeout: Option<u64>,
        #[serde(default)]
        version_policy: VersionPolicy,
        #[serde(default)]
        credentials: Option<crate::credential::CredentialDeclarations>,
    },
    HttpOrSse {
        url: String,
        #[serde(default, rename = "type")]
        transport_type: Option<String>,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        timeout: Option<u64>,
        #[serde(default)]
        version_policy: VersionPolicy,
        #[serde(default)]
        credentials: Option<crate::credential::CredentialDeclarations>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionPolicy {
    #[serde(default)]
    pub lock_file: Option<PathBuf>,
    #[serde(default)]
    pub on_mismatch: MismatchPolicy,
    #[serde(default)]
    pub on_tool_change: ToolChangePolicy,
}

impl Default for VersionPolicy {
    fn default() -> Self {
        Self {
            lock_file: None,
            on_mismatch: MismatchPolicy::FailClosed,
            on_tool_change: ToolChangePolicy::FailClosed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MismatchPolicy {
    #[serde(rename = "fail_closed")]
    #[default]
    FailClosed,
    #[serde(rename = "allow_with_warning")]
    AllowWithWarning,
    #[serde(rename = "allow_and_update")]
    AllowAndUpdate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolChangePolicy {
    #[serde(rename = "fail_closed")]
    #[default]
    FailClosed,
    #[serde(rename = "allow_added")]
    AllowAdded,
    #[serde(rename = "allow_all")]
    AllowAll,
}

impl McpServerConfig {
    pub fn is_stdio(&self) -> bool {
        matches!(self, Self::Stdio { .. })
    }

    pub fn is_sse(&self) -> bool {
        matches!(
            self,
            Self::HttpOrSse {
                transport_type: Some(kind),
                ..
            } if kind.eq_ignore_ascii_case("sse")
        )
    }

    pub fn is_http(&self) -> bool {
        matches!(self, Self::HttpOrSse { .. }) && !self.is_sse()
    }

    pub fn timeout_ms(&self) -> Option<u64> {
        match self {
            Self::Stdio { timeout, .. } | Self::HttpOrSse { timeout, .. } => *timeout,
        }
    }
    pub fn version_policy(&self) -> &VersionPolicy {
        match self {
            Self::Stdio { version_policy, .. } | Self::HttpOrSse { version_policy, .. } => {
                version_policy
            }
        }
    }
    pub fn credentials(&self) -> Option<&crate::credential::CredentialDeclarations> {
        match self {
            Self::Stdio { credentials, .. } | Self::HttpOrSse { credentials, .. } => {
                credentials.as_ref()
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Stdio {
                command,
                cwd,
                env,
                credentials,
                ..
            } => {
                if command.trim().is_empty() {
                    bail!("stdio MCP server command must not be empty");
                }
                if matches!(cwd, Some(path) if path.as_os_str().is_empty()) {
                    bail!("stdio MCP server cwd must not be empty");
                }
                check_env_credential_mutex(env, credentials)?;
            }
            Self::HttpOrSse {
                url,
                transport_type,
                headers,
                credentials,
                ..
            } => {
                let parsed =
                    Url::parse(url).with_context(|| format!("invalid MCP server URL '{url}'"))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    bail!("MCP server URL must use http or https: {url}");
                }
                if let Some(kind) = transport_type
                    && !matches!(
                        kind.to_ascii_lowercase().as_str(),
                        "http" | "streamable-http" | "sse"
                    )
                {
                    bail!("unsupported MCP transport type '{kind}'");
                }
                check_env_credential_mutex(headers, credentials)?;
            }
        }
        Ok(())
    }
}

impl McpConfigFile {
    pub fn from_json(text: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(text).context("invalid MCP configuration JSON")?;
        validate_transport_discriminators(&value)?;
        let config: Self =
            serde_json::from_value(value).context("invalid MCP configuration shape")?;
        config.validate()?;
        Ok(config)
    }

    pub async fn from_file(path: &Path) -> Result<Self> {
        let text = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to read MCP config '{}'", path.display()))?;
        Self::from_json(&text)
            .with_context(|| format!("failed to parse MCP config '{}'", path.display()))
    }

    /// Discover `.mcp.json` or `mcp.json` in `dir`.
    ///
    /// `.mcp.json` takes precedence when both files exist.
    pub async fn discover(dir: &Path) -> Result<Option<Self>> {
        for candidate in CONFIG_CANDIDATES {
            let path = dir.join(candidate);
            match tokio::fs::metadata(&path).await {
                Ok(metadata) if metadata.is_file() => {
                    return Self::from_file(&path).await.map(Some);
                }
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to inspect MCP config '{}'", path.display())
                    });
                }
            }
        }
        Ok(None)
    }

    pub fn validate(&self) -> Result<()> {
        for (name, server) in &self.mcp_servers {
            if name.trim().is_empty() {
                bail!("MCP server name must not be empty");
            }
            server
                .validate()
                .with_context(|| format!("invalid MCP server '{name}'"))?;
        }
        Ok(())
    }
}

/// Ensures credential keys declared in `credentials.scoped_env` do not also
/// appear in the plain `env`/`headers` map. A key in both places would be
/// ambiguous — is it scoped or unscoped? The mutex invariant enforces that
/// each credential key has exactly one declaration site.
fn check_env_credential_mutex(
    plain: &HashMap<String, String>,
    credentials: &Option<crate::credential::CredentialDeclarations>,
) -> Result<()> {
    if let Some(creds) = credentials {
        for key in creds.scoped_env.keys() {
            if plain.contains_key(key) {
                bail!(
                    "credential key '{key}' must not appear in both env/headers and credentials.scoped_env"
                );
            }
        }
    }
    Ok(())
}
fn validate_transport_discriminators(value: &Value) -> Result<()> {
    let servers = value
        .get("mcpServers")
        .and_then(Value::as_object)
        .context("MCP configuration must contain an 'mcpServers' object")?;

    for (name, server) in servers {
        let server = server
            .as_object()
            .with_context(|| format!("MCP server '{name}' must be an object"))?;
        let has_command = server.contains_key("command");
        let has_url = server.contains_key("url");
        if has_command == has_url {
            bail!("MCP server '{name}' must specify exactly one of 'command' or 'url'");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_stdio_http_and_legacy_sse_servers() {
        let config = McpConfigFile::from_json(
            r#"{
                "mcpServers": {
                    "filesystem": {
                        "command": "npx",
                        "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
                        "env": {"FOO": "bar"},
                        "cwd": "/tmp",
                        "timeout": 1000
                    },
                    "remote": {
                        "url": "https://example.com/mcp",
                        "headers": {"Authorization": "Bearer token"}
                    },
                    "legacy": {
                        "type": "sse",
                        "url": "https://example.com/sse"
                    }
                }
            }"#,
        )
        .unwrap();

        assert!(config.mcp_servers["filesystem"].is_stdio());
        assert!(config.mcp_servers["remote"].is_http());
        assert!(config.mcp_servers["legacy"].is_sse());
        assert_eq!(config.mcp_servers["filesystem"].timeout_ms(), Some(1000));
    }

    #[test]
    fn rejects_invalid_server_config() {
        let error = McpConfigFile::from_json(
            r#"{
                "mcpServers": {
                    "broken": {"command": ""}
                }
            }"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("broken"));
    }

    #[test]
    fn rejects_unsupported_remote_scheme_and_transport() {
        let scheme_error =
            McpConfigFile::from_json(r#"{"mcpServers":{"remote":{"url":"file:///tmp/socket"}}}"#)
                .unwrap_err();
        let transport_error = McpConfigFile::from_json(
            r#"{"mcpServers":{"remote":{"url":"https://example.com","type":"websocket"}}}"#,
        )
        .unwrap_err();

        assert!(scheme_error.to_string().contains("remote"));
        assert!(transport_error.to_string().contains("remote"));
    }

    #[test]
    fn rejects_ambiguous_or_missing_transport_discriminator() {
        let ambiguous = McpConfigFile::from_json(
            r#"{"mcpServers":{"broken":{"command":"server","url":"https://example.com"}}}"#,
        )
        .unwrap_err();
        let missing =
            McpConfigFile::from_json(r#"{"mcpServers":{"broken":{"args":[]}}}"#).unwrap_err();

        assert!(ambiguous.to_string().contains("exactly one"));
        assert!(missing.to_string().contains("exactly one"));
    }

    #[tokio::test]
    async fn loads_and_discovers_config_file() {
        let temp = tempfile::tempdir().unwrap();
        let visible = temp.path().join("mcp.json");
        fs::write(
            &visible,
            r#"{"mcpServers":{"visible":{"command":"visible"}}}"#,
        )
        .unwrap();

        let loaded = McpConfigFile::from_file(&visible).await.unwrap();
        assert!(loaded.mcp_servers.contains_key("visible"));

        let discovered = McpConfigFile::discover(temp.path()).await.unwrap().unwrap();
        assert!(discovered.mcp_servers.contains_key("visible"));
    }

    #[tokio::test]
    async fn hidden_config_takes_precedence() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("mcp.json"),
            r#"{"mcpServers":{"visible":{"command":"visible"}}}"#,
        )
        .unwrap();
        fs::write(
            temp.path().join(".mcp.json"),
            r#"{"mcpServers":{"hidden":{"command":"hidden"}}}"#,
        )
        .unwrap();

        let discovered = McpConfigFile::discover(temp.path()).await.unwrap().unwrap();
        assert!(discovered.mcp_servers.contains_key("hidden"));
        assert!(!discovered.mcp_servers.contains_key("visible"));
    }

    #[tokio::test]
    async fn discover_returns_none_without_config() {
        let temp = tempfile::tempdir().unwrap();
        assert!(
            McpConfigFile::discover(temp.path())
                .await
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn version_policy_defaults_to_fail_closed() {
        let policy = VersionPolicy::default();
        assert_eq!(policy.on_mismatch, MismatchPolicy::FailClosed);
        assert_eq!(policy.on_tool_change, ToolChangePolicy::FailClosed);
        assert!(policy.lock_file.is_none());
    }

    #[test]
    fn version_policy_deserializes_from_json() {
        let json = r#"{
            "lock_file": ".mcp-lock.json",
            "on_mismatch": "allow_with_warning",
            "on_tool_change": "allow_added"
        }"#;
        let policy: VersionPolicy = serde_json::from_str(json).unwrap();
        assert_eq!(policy.on_mismatch, MismatchPolicy::AllowWithWarning);
        assert_eq!(policy.on_tool_change, ToolChangePolicy::AllowAdded);
        assert_eq!(
            policy.lock_file.as_deref(),
            Some(std::path::Path::new(".mcp-lock.json"))
        );
    }
    #[test]
    fn credentials_and_env_mutual_exclusion_rejected() {
        let json = r#"{
            "mcpServers": {
                "github": {
                    "command": "github-server",
                    "env": {"TOKEN": "val1"},
                    "credentials": {
                        "scoped_env": {
                            "TOKEN": {"value": "val2"}
                        }
                    }
                }
            }
        }"#;
        let result: Result<McpConfigFile, _> = serde_json::from_str(json);
        if let Ok(config) = result {
            assert!(
                config.validate().is_err(),
                "env and scoped_env with same key should be rejected"
            );
        }
    }
}
