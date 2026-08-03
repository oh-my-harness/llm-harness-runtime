use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use llm_harness_agent::SessionRecallScope;
use llm_harness_types::EntryId;
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
    pub scope: SessionRecallScope,
    pub exclude_session_id: Option<String>,
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

/// Explicit per-run opt-in. Absence of this extension disables the plugin.
#[derive(Clone, PartialEq, Eq)]
pub struct HistoryRecallRequest {
    pub current_session_id: String,
    pub query: Option<String>,
}

impl HistoryRecallRequest {
    pub fn new(current_session_id: impl Into<String>) -> Self {
        Self {
            current_session_id: current_session_id.into(),
            query: None,
        }
    }

    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.query = Some(query.into());
        self
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
