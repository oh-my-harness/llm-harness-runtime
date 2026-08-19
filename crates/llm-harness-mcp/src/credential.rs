use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use futures::FutureExt;
use futures::future::BoxFuture;
/// A credential declaration with optional tool-scope restriction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedCredential {
    pub value: String,
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
}

/// Credential declarations for a single MCP server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialDeclarations {
    #[serde(default)]
    pub scoped_env: HashMap<String, ScopedCredential>,
}

/// Mechanism-layer trait: resolve a credential reference to its value.
/// Concrete backends (env, file, vault) are provided by the consumer.
pub trait CredentialProvider: Send + Sync {
    /// Resolve a credential reference. Returns None if not found.
    fn resolve(&self, reference: &str) -> BoxFuture<'_, Option<String>>;
}

/// Default provider: resolves `${ENV:VAR}` and `${ENV:VAR:-default}` from
/// process environment. Does not handle `vault://` references.
pub struct EnvCredentialProvider;

impl CredentialProvider for EnvCredentialProvider {
    fn resolve(&self, reference: &str) -> BoxFuture<'_, Option<String>> {
        let result = if let Some(rest) = reference.strip_prefix("${ENV:") {
            if let Some(close) = rest.find('}') {
                let inner = &rest[..close];
                if let Some((var, default)) = inner.split_once(":-") {
                    Some(std::env::var(var).unwrap_or_else(|_| default.to_owned()))
                } else {
                    std::env::var(inner).ok()
                }
            } else {
                None
            }
        } else if reference.starts_with("vault://") {
            tracing::warn!(
                reference = %reference,
                "vault:// reference encountered but no vault CredentialProvider is configured; \
                 configure one via McpManager::with_credential_provider"
            );
            None
        } else {
            None
        };
        async move { result }.boxed()
    }
}

/// Parses a credential value string. Returns:
/// - `Some(value)` for pure text (no reference syntax) — value is the text itself
/// - `Some(value)` for `${ENV:...}` resolved via the provider
/// - `Some(value)` for `vault://...` resolved via the provider
/// - `None` if a reference cannot be resolved
pub async fn resolve_credential_value(
    raw: &str,
    provider: &dyn CredentialProvider,
) -> Option<String> {
    if raw.starts_with("${ENV:") || raw.starts_with("vault://") {
        provider.resolve(raw).await
    } else {
        Some(raw.to_owned())
    }
}

/// Resolves all values in an env/headers map, returning a new map with
/// concrete values. Returns Err with the first unresolved reference.
pub async fn resolve_map(
    input: &HashMap<String, String>,
    provider: &dyn CredentialProvider,
) -> Result<HashMap<String, String>, String> {
    let mut output = HashMap::with_capacity(input.len());
    for (key, raw) in input {
        match resolve_credential_value(raw, provider).await {
            Some(value) => {
                output.insert(key.clone(), value);
            }
            None => return Err(raw.clone()),
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pure_text_passthrough() {
        let provider = EnvCredentialProvider;
        let result = resolve_credential_value("application/json", &provider).await;
        assert_eq!(result, Some("application/json".to_owned()));
    }

    #[tokio::test]
    async fn env_var_resolved() {
        unsafe { std::env::set_var("TEST_MCP_CRED_TOKEN", "secret123") };
        let provider = EnvCredentialProvider;
        let result = resolve_credential_value("${ENV:TEST_MCP_CRED_TOKEN}", &provider).await;
        assert_eq!(result, Some("secret123".to_owned()));
        unsafe { std::env::remove_var("TEST_MCP_CRED_TOKEN") };
    }

    #[tokio::test]
    async fn env_var_with_default() {
        unsafe { std::env::remove_var("TEST_MCP_CRED_MISSING") };
        let provider = EnvCredentialProvider;
        let result =
            resolve_credential_value("${ENV:TEST_MCP_CRED_MISSING:-fallback}", &provider).await;
        assert_eq!(result, Some("fallback".to_owned()));
    }

    #[tokio::test]
    async fn env_var_missing_no_default_returns_none() {
        unsafe { std::env::remove_var("TEST_MCP_CRED_MISSING2") };
        let provider = EnvCredentialProvider;
        let result = resolve_credential_value("${ENV:TEST_MCP_CRED_MISSING2}", &provider).await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn vault_reference_without_provider_returns_none() {
        let provider = EnvCredentialProvider;
        let result = resolve_credential_value("vault://prod/github/token", &provider).await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn resolve_map_returns_error_on_first_unresolved() {
        unsafe { std::env::remove_var("TEST_MCP_CRED_MISSING3") };
        let provider = EnvCredentialProvider;
        let mut input = HashMap::new();
        input.insert("GOOD".to_owned(), "plain".to_owned());
        input.insert("BAD".to_owned(), "${ENV:TEST_MCP_CRED_MISSING3}".to_owned());
        let result = resolve_map(&input, &provider).await;
        assert_eq!(result, Err("${ENV:TEST_MCP_CRED_MISSING3}".to_owned()));
    }
    #[test]
    fn scoped_credential_deserializes() {
        let json = r#"{
            "value": "${ENV:GITHUB_TOKEN}",
            "allowed_tools": ["search_repos", "get_file_contents"]
        }"#;
        let cred: ScopedCredential = serde_json::from_str(json).unwrap();
        assert_eq!(cred.value, "${ENV:GITHUB_TOKEN}");
        assert_eq!(
            cred.allowed_tools.as_ref().unwrap(),
            &vec!["search_repos".to_owned(), "get_file_contents".to_owned()]
        );
    }

    #[test]
    fn scoped_credential_without_allowed_tools() {
        let json = r#"{"value": "plain_text"}"#;
        let cred: ScopedCredential = serde_json::from_str(json).unwrap();
        assert!(cred.allowed_tools.is_none());
    }
    #[tokio::test]
    async fn text_containing_env_substring_passthrough() {
        let provider = EnvCredentialProvider;
        let result = resolve_credential_value("prefix-${ENV:VAR}", &provider).await;
        assert_eq!(result, Some("prefix-${ENV:VAR}".to_owned()));
    }
}
