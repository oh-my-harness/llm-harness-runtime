use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use llm_harness_agent::SessionRecallScope;
use llm_harness_runtime_knowledge::KnowledgeRef;
use llm_harness_types::{EntryId, RunId};
use serde::{Deserialize, Serialize};

pub const SESSION_RECALL_SOURCE_ID: &str = "session_recall";

/// Stable reference to one turn on one authoritative Session branch.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionRecallRef {
    pub session_id: String,
    pub user_entry_id: EntryId,
    pub assistant_entry_id: Option<EntryId>,
    pub branch_leaf_id: EntryId,
    pub revision: u64,
}

impl SessionRecallRef {
    pub fn encode(&self) -> Result<String, SessionRecallError> {
        let bytes = serde_json::to_vec(self).map_err(|_| SessionRecallError::InvalidReference)?;
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn decode(encoded: &str) -> Result<Self, SessionRecallError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| SessionRecallError::InvalidReference)?;
        serde_json::from_slice(&bytes).map_err(|_| SessionRecallError::InvalidReference)
    }

    pub fn navigation_target(&self) -> SessionRecallNavigationTarget {
        SessionRecallNavigationTarget {
            session_id: self.session_id.clone(),
            entry_id: self.user_entry_id,
        }
    }
}

/// Product-neutral target for opening the authoritative Session turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecallNavigationTarget {
    pub session_id: String,
    pub entry_id: EntryId,
}

/// One derived, searchable conversational turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecallDocument {
    pub reference: SessionRecallRef,
    pub title: Option<String>,
    pub user_text: String,
    pub assistant_text: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl SessionRecallDocument {
    pub fn searchable_text(&self) -> String {
        let mut text = String::new();
        if let Some(title) = &self.title {
            text.push_str(title);
            text.push('\n');
        }
        text.push_str(&self.user_text);
        if let Some(assistant) = &self.assistant_text {
            text.push('\n');
            text.push_str(assistant);
        }
        text
    }

    pub fn formatted_content(&self) -> String {
        let mut text = format!("User:\n{}", self.user_text);
        if let Some(assistant) = &self.assistant_text {
            text.push_str("\n\nAssistant:\n");
            text.push_str(assistant);
        }
        text
    }
}

/// Atomic contents of one Session in a recall index generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecallProjection {
    pub session_id: String,
    pub revision: u64,
    pub scope: SessionRecallScope,
    pub documents: Vec<SessionRecallDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecallIndexQuery {
    pub query: String,
    pub scope: SessionRecallScope,
    pub exclude_session_id: Option<String>,
    pub limit: usize,
    pub max_snippet_bytes: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecallCandidate {
    pub reference: SessionRecallRef,
    pub title: Option<String>,
    pub snippet: String,
    pub score: f32,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecallSearchRequest {
    pub query: String,
    pub limit: usize,
    pub max_snippet_bytes: usize,
}

/// Bounds for automatic context recall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecallBudget {
    pub max_hits: usize,
    pub max_bytes_per_hit: usize,
    pub max_total_bytes: usize,
    pub max_tokens_per_hit: usize,
    pub max_total_tokens: usize,
}

impl Default for SessionRecallBudget {
    fn default() -> Self {
        Self {
            max_hits: 3,
            max_bytes_per_hit: 4 * 1024,
            max_total_bytes: 8 * 1024,
            // The default estimator uses UTF-8 bytes as a conservative upper bound.
            max_tokens_per_hit: 4 * 1024,
            max_total_tokens: 8 * 1024,
        }
    }
}

impl SessionRecallBudget {
    pub fn validate(&self) -> Result<(), SessionRecallError> {
        if self.max_hits == 0
            || self.max_bytes_per_hit == 0
            || self.max_total_bytes == 0
            || self.max_tokens_per_hit == 0
            || self.max_total_tokens == 0
        {
            return Err(SessionRecallError::InvalidConfiguration);
        }
        Ok(())
    }
}

/// Trusted, per-run authorization partition for Session Recall.
///
/// This type intentionally implements neither `Debug` nor serialization so it
/// cannot be logged or persisted accidentally. It is independent of
/// `KnowledgeAccessContext.scope`: products may use Knowledge attributes for
/// document routing without partitioning conversation history by those values.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionRecallAccessContext {
    pub scope: SessionRecallScope,
    pub current_session_id: Option<String>,
}

impl SessionRecallAccessContext {
    pub fn new(scope: SessionRecallScope) -> Self {
        Self {
            scope,
            current_session_id: None,
        }
    }

    pub fn with_current_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.current_session_id = Some(session_id.into());
        self
    }
}

/// Explicit per-run automatic-recall opt-in.
///
/// Tool-driven Session Recall needs `SessionRecallAccessContext` but does not
/// need this extension. Absence of this extension disables the automatic
/// History Recall plugin.
#[derive(Clone, PartialEq, Eq)]
pub struct HistoryRecallRequest {
    pub query: Option<String>,
}

impl HistoryRecallRequest {
    pub fn new() -> Self {
        Self { query: None }
    }

    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.query = Some(query.into());
        self
    }
}

impl Default for HistoryRecallRequest {
    fn default() -> Self {
        Self::new()
    }
}

/// One reference accepted by the automatic plugin after an authoritative read.
///
/// This observation contains navigation/provenance only, never recalled text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRecallAcceptedReference {
    pub reference: KnowledgeRef,
    pub navigation_target: SessionRecallNavigationTarget,
    pub uri: Option<String>,
}

/// Serializable snapshot published by an explicitly observed History Recall run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRecallObservationSnapshot {
    pub run_id: Option<RunId>,
    pub accepted_references: Vec<HistoryRecallAcceptedReference>,
}

/// Optional per-run observation handle for automatic History Recall.
///
/// Attach a fresh clone to `RunRequest` as a typed extension and retain the
/// original handle to inspect `snapshot()`. The handle itself is intentionally
/// not serializable and is never copied into Session messages.
#[derive(Clone, Default)]
pub struct HistoryRecallObservation {
    state: std::sync::Arc<std::sync::RwLock<HistoryRecallObservationSnapshot>>,
}

impl HistoryRecallObservation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> HistoryRecallObservationSnapshot {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn publish(
        &self,
        run_id: RunId,
        accepted_references: Vec<HistoryRecallAcceptedReference>,
    ) {
        *self
            .state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = HistoryRecallObservationSnapshot {
            run_id: Some(run_id),
            accepted_references,
        };
    }
}

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum SessionRecallError {
    #[error("session recall reference is invalid")]
    InvalidReference,
    #[error("session recall configuration is invalid")]
    InvalidConfiguration,
    #[error("session recall operation was aborted")]
    Aborted,
    #[error("session recall authority is unavailable")]
    AuthorityUnavailable,
    #[error("session recall projection failed")]
    ProjectionFailed,
}

pub(crate) fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}
