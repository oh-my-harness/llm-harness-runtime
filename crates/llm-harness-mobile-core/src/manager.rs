//! Multi-session management: persisted sessions via `JsonlSessionRepo` plus a
//! per-session state sidecar (`state.json`) tracking sandbox id, trust profile
//! and last activity for resume.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;

use crate::{MobileConfig, MobileError, MobileSession};
use llm_harness_agent::session::SessionRepo;
use llm_harness_sandbox_remote::RemoteSandbox;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SessionState {
    pub sandbox_id: String,
    pub trust_profile: String,
    pub last_active_at_ms: u64,
}

impl SessionState {
    pub fn touch(&mut self, now_ms: u64) {
        self.last_active_at_ms = now_ms;
    }
}

pub mod state {
    pub use super::SessionState;
    use super::{Path, PathBuf};

    fn state_path(dir: &Path) -> PathBuf {
        dir.join("state.json")
    }

    pub fn write_state(dir: &Path, state: &SessionState) -> anyhow::Result<()> {
        let json = serde_json::to_vec_pretty(state)?;
        std::fs::write(state_path(dir), json)?;
        Ok(())
    }

    pub fn read_state(dir: &Path) -> anyhow::Result<SessionState> {
        let path = state_path(dir);
        if !path.exists() {
            return Ok(SessionState::default());
        }
        let bytes = std::fs::read(&path)?;
        Ok(serde_json::from_slice(&bytes).unwrap_or_default())
    }
}

/// Summary of a persisted session for the sessions list screen.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionSummary {
    pub id: String,
    pub name: String,
    pub created_at_ms: u64,
    pub last_active_at_ms: u64,
}

/// Creates, lists and resumes persisted mobile sessions.
///
/// Session entries live under `<data_dir>/<session-id>/entries.jsonl`
/// (`JsonlSessionRepo` layout); the sidecar `state.json` in the same directory
/// tracks sandbox identity for resume.
#[derive(uniffi::Object)]
pub struct MobileSessionManager {
    data_dir: PathBuf,
}

pub(crate) async fn open_storage(
    session_id: &str,
) -> anyhow::Result<Arc<dyn llm_harness_agent::session::SessionStorage>> {
    validate_session_id(session_id)?;
    let repo = session_repo()?;
    let storage = repo
        .open(session_id)
        .await
        .map_err(|e| anyhow::anyhow!("open session {session_id}: {e}"))?;
    Ok(storage)
}

fn validate_session_id(session_id: &str) -> anyhow::Result<()> {
    // Session ids become directory names; reject traversal and separators.
    if session_id.is_empty()
        || session_id.contains(['/', '\\', '\0'])
        || session_id.starts_with('.')
    {
        return Err(anyhow::anyhow!("invalid session id {session_id:?}"));
    }
    Ok(())
}

fn session_repo() -> anyhow::Result<Arc<llm_harness_agent::session::JsonlSessionRepo>> {
    let root = dirs_data_root()?;
    let sessions_dir = root.join("sessions");
    std::fs::create_dir_all(&sessions_dir)?;
    Ok(Arc::new(llm_harness_agent::session::JsonlSessionRepo::new(
        sessions_dir,
    )))
}

/// Root for persisted state: `dirs::data_dir()/ohmyharness-mobile` on desktop,
/// app-private internal storage equivalent on Android via `XDG`-less fallback.
fn dirs_data_root() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("OHM_MOBILE_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    #[cfg(target_os = "android")]
    {
        // On Android the FFI host passes OHM_MOBILE_DATA_DIR; without it we
        // cannot know the app-private dir.
        anyhow::bail!("OHM_MOBILE_DATA_DIR must be set on Android")
    }
    #[cfg(not(target_os = "android"))]
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory"))?;
        Ok(home.join(".local/share/ohmyharness-mobile"))
    }
}

#[uniffi::export]
impl MobileSessionManager {
    #[uniffi::constructor]
    pub fn new(data_dir: String) -> Arc<Self> {
        Arc::new(Self {
            data_dir: PathBuf::from(data_dir),
        })
    }

    pub async fn create_session(
        &self,
        config: MobileConfig,
        name: String,
    ) -> Result<Arc<MobileSession>, MobileError> {
        self.create_session_inner(config, name)
            .await
            .map_err(MobileError::from)
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, MobileError> {
        self.list_sessions_inner().await.map_err(MobileError::from)
    }

    pub async fn open_session(
        &self,
        config: MobileConfig,
        session_id: String,
    ) -> Result<Arc<MobileSession>, MobileError> {
        self.open_session_inner(config, session_id)
            .await
            .map_err(MobileError::from)
    }

    pub async fn delete_session(&self, session_id: String) -> Result<(), MobileError> {
        self.delete_session_inner(session_id)
            .await
            .map_err(MobileError::from)
    }
}

impl MobileSessionManager {
    async fn create_session_inner(
        &self,
        config: MobileConfig,
        name: String,
    ) -> anyhow::Result<Arc<MobileSession>> {
        let repo = session_repo()?;
        let storage = repo
            .create(llm_harness_agent::session::types::CreateSessionOptions {
                name: if name.trim().is_empty() {
                    None
                } else {
                    Some(name)
                },
                initial_model: Some(config.model.clone()),
                ..Default::default()
            })
            .await?;
        let session_id = storage.metadata().await?.id;
        let sandbox = crate::connect_sandbox(&config).await?;
        self.persist_state(&session_id, &sandbox, &config)?;
        MobileSession::assemble(config, sandbox, Some(session_id)).await
    }

    async fn list_sessions_inner(&self) -> anyhow::Result<Vec<SessionSummary>> {
        let repo = session_repo()?;
        let sessions = repo
            .list(Default::default())
            .await
            .map_err(|e| anyhow::anyhow!("list sessions: {e}"))?;
        let mut out = Vec::with_capacity(sessions.len());
        for meta in sessions {
            let dir = self.session_dir(&meta.id);
            let state = state::read_state(&dir).unwrap_or_default();
            out.push(SessionSummary {
                id: meta.id.clone(),
                name: meta.name.clone().unwrap_or_else(|| meta.id.clone()),
                created_at_ms: u64::try_from(meta.created_at.timestamp_millis()).unwrap_or(0),
                last_active_at_ms: state.last_active_at_ms,
            });
        }
        out.sort_by_key(|summary| std::cmp::Reverse(summary.last_active_at_ms));
        Ok(out)
    }

    async fn open_session_inner(
        &self,
        config: MobileConfig,
        session_id: String,
    ) -> anyhow::Result<Arc<MobileSession>> {
        // Validate the session exists before spending a sandbox.
        open_storage(&session_id).await?;
        let sandbox = crate::connect_sandbox(&config).await?;
        self.persist_state(&session_id, &sandbox, &config)?;
        MobileSession::assemble(config, sandbox, Some(session_id)).await
    }

    async fn delete_session_inner(&self, session_id: String) -> anyhow::Result<()> {
        validate_session_id(&session_id)?;
        let repo = session_repo()?;
        repo.delete(&session_id)
            .await
            .map_err(|e| anyhow::anyhow!("delete session: {e}"))?;
        let dir = self.session_dir(&session_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.data_dir.join("sessions").join(session_id)
    }

    fn persist_state(
        &self,
        session_id: &str,
        sandbox: &RemoteSandbox,
        config: &MobileConfig,
    ) -> anyhow::Result<()> {
        validate_session_id(session_id)?;
        let dir = self.session_dir(session_id);
        std::fs::create_dir_all(&dir)?;
        // Preserve last_active across reconnects.
        let previous = state::read_state(&dir).unwrap_or_default();
        state::write_state(
            &dir,
            &SessionState {
                sandbox_id: sandbox.sandbox_id().to_owned(),
                trust_profile: config.trust_profile.clone(),
                last_active_at_ms: previous.last_active_at_ms,
            },
        )?;
        Ok(())
    }
}
