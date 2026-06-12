use std::collections::HashMap;
use std::path::PathBuf;

use futures::future::BoxFuture;
use llm_harness_types::{AgentError, AuthHook, AuthInfo};
use serde::Deserialize;

/// Resolves API keys from a single environment variable.
///
/// Reads the env var set at construction time (e.g., `ANTHROPIC_API_KEY`).
pub struct EnvAuthHook {
    /// Name of the environment variable to read (e.g., `ANTHROPIC_API_KEY`).
    env_var: String,
}

impl EnvAuthHook {
    /// Create a hook that reads the named environment variable.
    pub fn new(env_var: impl Into<String>) -> Self {
        Self {
            env_var: env_var.into(),
        }
    }

    /// Convenience constructor: env var name becomes `{PROVIDER}_API_KEY`.
    pub fn for_provider(provider: &str) -> Self {
        Self {
            env_var: format!("{}_API_KEY", provider.to_uppercase()),
        }
    }
}

impl AuthHook for EnvAuthHook {
    fn resolve<'a>(&'a self) -> BoxFuture<'a, Result<AuthInfo, AgentError>> {
        Box::pin(async move {
            match std::env::var(&self.env_var) {
                Ok(key) => Ok(AuthInfo {
                    api_key: Some(key),
                    headers: vec![],
                }),
                Err(_) => Err(AgentError::Internal(format!(
                    "env var `{}` is not set",
                    self.env_var
                ))),
            }
        })
    }
}

/// JSON config file schema expected by `FileAuthHook`.
#[derive(Debug, Clone, Deserialize)]
struct AuthConfig {
    providers: HashMap<String, ProviderAuth>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProviderAuth {
    api_key: String,
    #[allow(dead_code)]
    base_url: Option<String>,
    headers: Option<HashMap<String, String>>,
}

/// Resolves API keys from a JSON configuration file.
///
/// Each instance is bound to one provider (set at construction) because
/// `AuthHook::resolve()` takes no parameters.
pub struct FileAuthHook {
    provider: String,
    config: AuthConfig,
}

impl FileAuthHook {
    /// Load auth config from a JSON file for the specified provider.
    pub fn from_json_file(
        path: impl Into<PathBuf>,
        provider: impl Into<String>,
    ) -> Result<Self, FileAuthError> {
        let path: PathBuf = path.into();
        let content = std::fs::read_to_string(&path)
            .map_err(|e| FileAuthError::ReadFailed { path: path.clone(), source: e })?;
        let config: AuthConfig = serde_json::from_str(&content)
            .map_err(|e| FileAuthError::ParseFailed { path: path.clone(), source: e })?;
        Ok(Self {
            provider: provider.into(),
            config,
        })
    }
}

impl AuthHook for FileAuthHook {
    fn resolve<'a>(&'a self) -> BoxFuture<'a, Result<AuthInfo, AgentError>> {
        Box::pin(async move {
            let key = self.provider.to_lowercase();
            match self.config.providers.get(&key) {
                Some(auth) => Ok(AuthInfo {
                    api_key: Some(auth.api_key.clone()),
                    headers: auth
                        .headers
                        .as_ref()
                        .map(|h| h.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                        .unwrap_or_default(),
                }),
                None => Err(AgentError::Internal(format!(
                    "provider `{}` not found in auth config",
                    self.provider
                ))),
            }
        })
    }
}

/// Errors loading auth configuration files.
#[derive(Debug, thiserror::Error)]
pub enum FileAuthError {
    #[error("failed to read auth file {path}: {source}")]
    ReadFailed {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse auth file {path}: {source}")]
    ParseFailed {
        path: PathBuf,
        source: serde_json::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn env_auth_hook_resolves_key() {
        // SAFETY: single-threaded test binary; no concurrent env access.
        unsafe { std::env::set_var("TEST_HARNESS_API_KEY_XYZ", "sk-ant-test-key") };
        let hook = EnvAuthHook::new("TEST_HARNESS_API_KEY_XYZ");
        let result = hook.resolve().await;
        // SAFETY: same single-threaded context.
        unsafe { std::env::remove_var("TEST_HARNESS_API_KEY_XYZ") };
        let info = result.unwrap();
        assert_eq!(info.api_key, Some("sk-ant-test-key".to_string()));
    }

    #[tokio::test]
    async fn env_auth_hook_missing_key_errors() {
        let hook = EnvAuthHook::new("DEFINITELY_NONEXISTENT_VAR_XYZ_12345");
        let result = hook.resolve().await;
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error but got Ok"),
        };
        assert!(err.to_string().contains("DEFINITELY_NONEXISTENT_VAR_XYZ_12345"));
    }

    #[tokio::test]
    async fn file_auth_hook_parses_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"providers": {"anthropic": {"api_key": "sk-ant-from-file"}}}"#,
        )
        .unwrap();

        let hook = FileAuthHook::from_json_file(path, "anthropic").unwrap();
        let result = hook.resolve().await.unwrap();
        assert_eq!(result.api_key, Some("sk-ant-from-file".to_string()));
    }

    #[tokio::test]
    async fn file_auth_hook_missing_provider_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"providers": {"openai": {"api_key": "sk-openai-key"}}}"#,
        )
        .unwrap();

        let hook = FileAuthHook::from_json_file(path, "anthropic").unwrap();
        let result = hook.resolve().await;
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error but got Ok"),
        };
        assert!(err.to_string().contains("anthropic"));
    }
}
