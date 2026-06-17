use chrono::{DateTime, Utc};
use llm_harness_types::*;
use serde::{Deserialize, Serialize};

// ── SessionEntryPayload ────────────────────────────────────────────────────────

/// All possible payloads stored in a session log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "entry_type", rename_all = "snake_case")]
pub enum SessionEntryPayload {
    /// An agent message (user, assistant, tool-result, etc.). 90%+ of all entries.
    Message(AgentMessage),
    /// Model configuration change; replays via `build_context` to restore last model.
    ModelChange {
        /// Display name of the model (e.g., "gpt-4", "claude-3-opus").
        to: String,
        /// Provider name (e.g., "openai", "anthropic") if available.
        provider: Option<String>,
        /// Full model identifier (e.g., "gpt-4-0125-preview") if available.
        model_id: Option<String>,
    },
    /// Thinking-level change.
    ThinkingLevelChange { to: ThinkingLevel },
    /// Active tool list change.
    ActiveToolsChange { active: Vec<String> },
    /// A compaction event: summary message + first kept entry reference.
    Compaction(CompactionEntry),
    /// A named label marking an interesting point in the tree (for navigation).
    Label {
        /// Human-readable label name for this checkpoint.
        name: String,
    },
    /// A session naming event; latest one wins.
    SessionInfo {
        /// Human-readable name assigned to this session.
        name: String,
    },
    /// Application-layer custom entry.
    Custom {
        /// Application-defined sub-type tag.
        #[serde(rename = "type")]
        custom_type: String,
        /// Arbitrary application-defined data (any JSON-serializable value).
        data: serde_json::Value,
    },
    /// Semantic annotation that a branch was created here (optional; for UI).
    BranchPoint {
        /// The parent entry ID where the branch diverged.
        from: EntryId,
        /// Optional label for this branch point.
        label: Option<String>,
    },
    /// Records a cursor switch between two branches.
    BranchSwitch {
        /// The previous branch's leaf entry ID.
        from: EntryId,
        /// The new branch's leaf entry ID.
        to: EntryId,
        /// Optional human-provided summary of why this switch occurred.
        summary: Option<String>,
    },
    /// AI-generated summary of a branch.
    BranchSummary(BranchSummaryEntry),
}

/// Compaction data stored in a session entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionEntry {
    /// The summary message inserted into context after compaction.
    pub summary_message: AgentMessage,
    /// First entry still valid after compaction; everything before is covered by the summary.
    pub first_kept_entry: EntryId,
    /// Estimated token count before compaction (UI display).
    pub tokens_before: usize,
    /// True if the summary came from a `BeforeCompactHook` rather than the framework.
    pub from_hook: bool,
    /// Opaque extension data (e.g. modified files list).
    pub details: Option<serde_json::Value>,
}

/// AI-generated branch summary entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchSummaryEntry {
    /// The leaf entry ID of the summarized branch.
    pub leaf_id: EntryId,
    /// First entry covered by the summary (inclusive).
    pub from_entry: EntryId,
    /// AI-generated summary text.
    pub summary: String,
    /// Estimated token count of the summarized range.
    pub token_count: usize,
}

// ── SessionEntry ───────────────────────────────────────────────────────────────

/// A single node in the session tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    /// Unique entry ID (UUIDv7, time-ordered).
    pub id: EntryId,
    /// Parent entry ID; `None` means this is the tree root.
    pub parent_id: Option<EntryId>,
    /// Creation timestamp.
    pub timestamp: DateTime<Utc>,
    /// Typed payload.
    pub payload: SessionEntryPayload,
}

// ── Session metadata ───────────────────────────────────────────────────────────

/// Persistent metadata for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    /// Session identifier (stable across renames).
    pub id: String,
    /// Human-readable session name (most recent `SessionInfo` entry wins).
    pub name: Option<String>,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// When the session was last modified.
    pub updated_at: DateTime<Utc>,
    /// Last known model ID.
    pub model: Option<String>,
    /// Current write cursor; `None` = no entries yet.
    pub active_cursor: Option<EntryId>,
    /// Source session path for cross-session forks (reference mode, v1.x only).
    pub parent_session_path: Option<String>,
}

// ── Creation / list options ────────────────────────────────────────────────────

/// Options for creating a new session.
#[derive(Debug, Default)]
pub struct CreateSessionOptions {
    /// Human-readable name for the session (optional; can be set later via SessionInfo entry).
    pub name: Option<String>,
    /// Initial LLM model to use (optional; defaults to harness configuration).
    pub initial_model: Option<String>,
    /// Initial thinking level for the session (optional; defaults to disabled).
    pub initial_thinking_level: Option<ThinkingLevel>,
    /// List of tool identifiers initially available in this session.
    pub initial_tools: Vec<String>,
}

/// Options for listing sessions.
#[derive(Debug, Default)]
pub struct ListSessionOptions {
    /// Maximum number of sessions to return (None = no limit).
    pub limit: Option<usize>,
    /// Number of sessions to skip from the start (for pagination).
    pub offset: Option<usize>,
    /// Sort order for the results.
    pub order: ListOrder,
    /// Filter sessions by name substring (case-insensitive).
    pub name_contains: Option<String>,
}

/// Sort order for session listings.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ListOrder {
    /// Sort by last modification time, most recent first.
    #[default]
    UpdatedDesc,
    /// Sort by last modification time, oldest first.
    UpdatedAsc,
    /// Sort by creation time, most recent first.
    CreatedDesc,
    /// Sort by creation time, oldest first.
    CreatedAsc,
}

/// Options for cross-session fork.
#[derive(Debug)]
pub struct ForkOptions {
    /// Optional name for the forked session; uses parent name if not provided.
    pub name: Option<String>,
    /// v1 forces this to `true` (full entry copy with new IDs).
    pub copy_entries: bool,
}

impl Default for ForkOptions {
    fn default() -> Self {
        Self {
            name: None,
            copy_entries: true,
        }
    }
}

// ── Branch info ────────────────────────────────────────────────────────────────

/// Summary information about a single branch (leaf → root path).
#[derive(Debug, Clone)]
pub struct BranchInfo {
    /// The leaf entry ID of this branch.
    pub leaf_id: EntryId,
    /// Optional branch label.
    pub label: Option<String>,
    /// Number of entries from root to this leaf.
    pub message_count: usize,
    /// Timestamp of the most recent entry.
    pub last_activity: DateTime<Utc>,
    /// AI-generated branch summary, if available.
    pub summary: Option<String>,
}

// ── BuiltContext ───────────────────────────────────────────────────────────────

/// The "effective context" built from the active branch's entries.
///
/// Compaction entries are resolved: messages before the last compaction are
/// replaced with the compaction summary message.
#[derive(Debug)]
pub struct BuiltContext {
    /// Effective message list to pass to the LLM.
    pub messages: Vec<AgentMessage>,
    /// Last known model ID from the session log.
    pub last_model: Option<String>,
    /// Last known thinking level from the session log.
    pub last_thinking_level: Option<ThinkingLevel>,
    /// Last known active tools list from the session log.
    pub last_active_tools: Option<Vec<String>>,
}

// ── SessionEntryKind ───────────────────────────────────────────────────────────

/// Discriminant for `find_entries_by_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEntryKind {
    /// An agent message (user, assistant, tool-result, etc.).
    Message,
    /// A model configuration change.
    ModelChange,
    /// A thinking-level change.
    ThinkingLevelChange,
    /// An active tools list change.
    ActiveToolsChange,
    /// A compaction event.
    Compaction,
    /// A named label entry.
    Label,
    /// A session naming event.
    SessionInfo,
    /// An application-layer custom entry.
    Custom,
    /// A branch point annotation.
    BranchPoint,
    /// A branch switch record.
    BranchSwitch,
    /// An AI-generated branch summary.
    BranchSummary,
}

impl SessionEntry {
    /// Returns the `SessionEntryKind` for this entry.
    pub fn kind(&self) -> SessionEntryKind {
        match &self.payload {
            SessionEntryPayload::Message(_) => SessionEntryKind::Message,
            SessionEntryPayload::ModelChange { .. } => SessionEntryKind::ModelChange,
            SessionEntryPayload::ThinkingLevelChange { .. } => {
                SessionEntryKind::ThinkingLevelChange
            }
            SessionEntryPayload::ActiveToolsChange { .. } => SessionEntryKind::ActiveToolsChange,
            SessionEntryPayload::Compaction(_) => SessionEntryKind::Compaction,
            SessionEntryPayload::Label { .. } => SessionEntryKind::Label,
            SessionEntryPayload::SessionInfo { .. } => SessionEntryKind::SessionInfo,
            SessionEntryPayload::Custom { .. } => SessionEntryKind::Custom,
            SessionEntryPayload::BranchPoint { .. } => SessionEntryKind::BranchPoint,
            SessionEntryPayload::BranchSwitch { .. } => SessionEntryKind::BranchSwitch,
            SessionEntryPayload::BranchSummary(_) => SessionEntryKind::BranchSummary,
        }
    }
}
