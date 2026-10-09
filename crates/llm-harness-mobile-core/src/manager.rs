//! Multi-session management: persisted sessions via `JsonlSessionRepo` plus a
//! per-session state sidecar (`state.json`) tracking sandbox id, trust profile
//! and last activity for resume.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;

use crate::{MobileConfig, MobileError, MobileSession, run_on_runtime};
use llm_harness_agent::session::SessionRepo;
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

/// Map persisted agent messages to mobile replay bubbles. Progress assistant
/// messages are skipped (replay shows durable turns); non-text blocks are
/// ignored; tool results map to a one-line `tool` entry with an error flag.
pub fn history_to_mobile(
    messages: &[llm_harness_types::AgentMessage],
) -> Vec<MobileHistoryMessage> {
    use llm_harness_types::{AgentMessage, AssistantMessageKind, ContentBlock, DataBlock};
    let mut out = Vec::new();
    for message in messages {
        match message {
            AgentMessage::User(user) => {
                let text = user
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    out.push(MobileHistoryMessage {
                        role: "user".to_owned(),
                        text,
                        is_error: false,
                    });
                }
            }
            AgentMessage::Assistant(assistant) => {
                if assistant.kind == AssistantMessageKind::Progress {
                    continue;
                }
                let text = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    out.push(MobileHistoryMessage {
                        role: "assistant".to_owned(),
                        text,
                        is_error: false,
                    });
                }
            }
            AgentMessage::ToolResult(result) => {
                let text = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        DataBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push(MobileHistoryMessage {
                    role: "tool".to_owned(),
                    text: text.lines().next().unwrap_or_default().to_owned(),
                    is_error: result.is_error,
                });
            }
            _ => {}
        }
    }
    out
}

/// One replayed history message for the resume screen.
#[derive(Debug, Clone, uniffi::Record)]
pub struct MobileHistoryMessage {
    /// `user` | `assistant` | `tool`.
    pub role: String,
    /// Text content (first line for tool results).
    pub text: String,
    /// Tool failures only.
    pub is_error: bool,
}

/// Creates, lists and resumes persisted mobile sessions.
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

/// Process-wide root for persisted sessions. Set once by the first
/// `MobileSessionManager::new` (or by `OHM_MOBILE_DATA_DIR` on desktop).
static SESSION_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

fn session_repo() -> anyhow::Result<Arc<llm_harness_agent::session::JsonlSessionRepo>> {
    static REPO: std::sync::OnceLock<Arc<llm_harness_agent::session::JsonlSessionRepo>> =
        std::sync::OnceLock::new();
    if let Some(repo) = REPO.get() {
        return Ok(repo.clone());
    }
    let root = dirs_data_root()?;
    let sessions_dir = root.join("sessions");
    std::fs::create_dir_all(&sessions_dir)?;
    let repo = Arc::new(llm_harness_agent::session::JsonlSessionRepo::new(
        sessions_dir,
    ));
    // A single repo instance shares the open-storage cache: create→send→open
    // must observe the same in-memory storage or the send never lands in the
    // entries.jsonl the resume path reads.
    Ok(REPO.get_or_init(|| repo).clone())
}

fn dirs_data_root() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("OHM_MOBILE_DATA_DIR") {
        let path = PathBuf::from(dir);
        let _ = SESSION_ROOT.set(path.clone());
        return Ok(path);
    }
    if let Some(path) = SESSION_ROOT.get() {
        return Ok(path.clone());
    }
    #[cfg(target_os = "android")]
    {
        anyhow::bail!(
            "session data dir was never initialized (MobileSessionManager::new not called)"
        )
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
        let path = PathBuf::from(&data_dir);
        let _ = SESSION_ROOT.set(path.clone());
        Arc::new(Self { data_dir: path })
    }

    ///
    /// Deliberately NOT uniffi-async: foreign-side polling of a Rust future
    /// that needs the tokio reactor (network I/O) deadlocks — nothing drives
    /// the reactor. We block the calling (Kotlin dispatch) thread instead,
    /// exactly like the top-level `connect`.
    pub fn create_session(
        &self,
        config: MobileConfig,
        name: String,
    ) -> Result<Arc<MobileSession>, MobileError> {
        run_on_runtime(self.create_session_inner(config, name)).map_err(MobileError::from)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>, MobileError> {
        run_on_runtime(self.list_sessions_inner()).map_err(MobileError::from)
    }

    /// Read a session's persisted history WITHOUT connecting to a gateway
    /// or building a harness. Powers the lazy-connect chat screen.
    pub fn read_history(
        &self,
        session_id: String,
    ) -> Result<Vec<MobileHistoryMessage>, MobileError> {
        run_on_runtime(async move {
            validate_session_id(&session_id)?;
            let storage = open_storage(&session_id).await?;
            let session = llm_harness_agent::session::Session::new(storage);
            let context = session
                .build_context()
                .await
                .map_err(|e| MobileError::Internal {
                    detail: format!("read session history: {e}"),
                })?;
            Ok::<Vec<MobileHistoryMessage>, MobileError>(history_to_mobile(&context.messages))
        })
    }

    pub fn open_session(
        &self,
        config: MobileConfig,
        session_id: String,
    ) -> Result<Arc<MobileSession>, MobileError> {
        run_on_runtime(self.open_session_inner(config, session_id)).map_err(MobileError::from)
    }

    pub fn delete_session(&self, session_id: String) -> Result<(), MobileError> {
        run_on_runtime(self.delete_session_inner(session_id)).map_err(MobileError::from)
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
        if config.gateway_url.is_empty() {
            // Sandbox-free session: no gateway, no tools. LLM access only.
            self.persist_state(&session_id, "", &config)?;
            MobileSession::assemble(
                config,
                crate::session::SandboxHandle::Local,
                Some(session_id),
                false,
            )
            .await
        } else {
            let sandbox = crate::session::connect_sandbox(&config).await?;
            self.persist_state(&session_id, sandbox.sandbox_id(), &config)?;
            MobileSession::assemble(
                config,
                crate::session::SandboxHandle::Remote(sandbox),
                Some(session_id),
                false,
            )
            .await
        }
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
        if config.gateway_url.is_empty() {
            self.persist_state(&session_id, "", &config)?;
            MobileSession::assemble(
                config,
                crate::session::SandboxHandle::Local,
                Some(session_id),
                false,
            )
            .await
        } else {
            let sandbox = crate::session::connect_sandbox(&config).await?;
            self.persist_state(&session_id, sandbox.sandbox_id(), &config)?;
            MobileSession::assemble(
                config,
                crate::session::SandboxHandle::Remote(sandbox),
                Some(session_id),
                true,
            )
            .await
        }
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
        sandbox_id: &str,
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
                sandbox_id: sandbox_id.to_owned(),
                trust_profile: config.trust_profile.clone(),
                last_active_at_ms: previous.last_active_at_ms,
            },
        )?;
        Ok(())
    }
}
