use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Tracks sandbox IDs created by this control-service instance so that a
/// restart can clean up any orphaned remote sandboxes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistedSessions {
    pub sandbox_ids: Vec<String>,
}

#[derive(Clone)]
pub struct SessionPersistence {
    path: std::path::PathBuf,
}

impl SessionPersistence {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> anyhow::Result<PersistedSessions> {
        if !self.path.exists() {
            return Ok(PersistedSessions::default());
        }
        let bytes = std::fs::read(&self.path)?;
        serde_json::from_slice(&bytes)
            .map_err(|error| anyhow::anyhow!("invalid persistence file: {error}"))
    }

    pub fn save(&self, sessions: &PersistedSessions) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(sessions)?;
        std::fs::write(&self.path, bytes)?;
        Ok(())
    }

    pub fn add_sandbox(&self, sandbox_id: &str) -> anyhow::Result<()> {
        let mut sessions = self.load()?;
        if !sessions.sandbox_ids.iter().any(|id| id == sandbox_id) {
            sessions.sandbox_ids.push(sandbox_id.to_owned());
        }
        self.save(&sessions)
    }

    pub fn remove_sandbox(&self, sandbox_id: &str) -> anyhow::Result<()> {
        let mut sessions = self.load()?;
        sessions.sandbox_ids.retain(|id| id != sandbox_id);
        self.save(&sessions)
    }

    pub fn clear(&self) -> anyhow::Result<()> {
        self.save(&PersistedSessions::default())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// On startup, clean up any orphaned sandboxes from a previous run by
/// sending DELETE requests to the gateway for each persisted sandbox ID.
pub async fn reconcile_orphaned_sandboxes(
    persistence: &SessionPersistence,
    gateway_base_url: &str,
    gateway_token: &str,
    allow_insecure_http: bool,
) -> anyhow::Result<Vec<String>> {
    let persisted = persistence.load()?;
    if persisted.sandbox_ids.is_empty() {
        return Ok(Vec::new());
    }
    let client_config = if allow_insecure_http {
        llm_harness_sandbox_remote::RemoteSandboxClientConfig::new_insecure(
            gateway_base_url.to_owned(),
            gateway_token.to_owned(),
        )?
    } else {
        llm_harness_sandbox_remote::RemoteSandboxClientConfig::new(
            gateway_base_url.to_owned(),
            gateway_token.to_owned(),
        )?
    };
    let client = Arc::new(llm_harness_sandbox_remote::RemoteSandboxClient::new(
        client_config,
    )?);

    let mut cleaned = Vec::new();
    let mut failed = Vec::new();
    for sandbox_id in &persisted.sandbox_ids {
        match client.delete_sandbox(sandbox_id).await {
            Ok(()) => cleaned.push(sandbox_id.clone()),
            Err(error) => {
                tracing::warn!(sandbox_id = %sandbox_id, error = %error, "failed to clean up orphaned sandbox");
                failed.push(sandbox_id.clone());
            }
        }
    }

    if failed.is_empty() {
        persistence.clear()?;
        tracing::info!(count = cleaned.len(), "cleaned up all orphaned sandboxes");
    } else {
        let failed_count = failed.len();
        let remaining = PersistedSessions {
            sandbox_ids: failed,
        };
        persistence.save(&remaining)?;
        tracing::warn!(
            count = failed_count,
            "some orphaned sandboxes could not be cleaned; will retry on next start"
        );
    }

    Ok(cleaned)
}
