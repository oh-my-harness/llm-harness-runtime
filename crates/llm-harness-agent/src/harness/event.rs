use std::collections::HashSet;

use crate::skills::SkillDiagnostic;
use llm_harness_types::*;

// ── AgentHarnessEvent ─────────────────────────────────────────────────────────

/// Compaction statistics carried by `AgentHarnessEvent::CompactionEnd`.
#[derive(Debug, Clone)]
pub struct CompactionStats {
    /// Estimated token count before compaction.
    pub tokens_before: usize,
    /// Estimated token count after compaction.
    pub tokens_after: usize,
    /// Number of entries compressed.
    pub compressed_entries: usize,
}

/// Tool call result carried by `AgentHarnessEvent::ToolCallEnd`.
#[derive(Debug, Clone)]
pub struct HarnessToolCallResult {
    /// Result content blocks.
    pub content: Vec<ContentBlock>,
    /// Extension data.
    pub details: serde_json::Value,
    /// Whether the tool returned an error.
    pub is_error: bool,
}

/// Events emitted by [`AgentHarness`].
#[derive(Debug)]
pub enum AgentHarnessEvent {
    /// Wraps a raw `AgentEvent` from the underlying loop.
    Agent(AgentEvent),
    /// Harness phase changed.
    PhaseChange {
        /// Previous phase.
        from: HarnessPhase,
        /// New phase.
        to: HarnessPhase,
    },
    /// Active model changed.
    ModelUpdate {
        /// Previous model ID.
        from: String,
        /// New model ID.
        to: String,
    },
    /// Thinking level changed.
    ThinkingLevelUpdate {
        /// Previous thinking level.
        from: ThinkingLevel,
        /// New thinking level.
        to: ThinkingLevel,
    },
    /// Tool list changed.
    ToolsUpdate {
        /// Tool names added.
        added: Vec<String>,
        /// Tool names removed.
        removed: Vec<String>,
    },
    /// Active tool subset changed.
    ActiveToolsUpdate {
        /// Active tools; `None` means all tools are active.
        active: Option<HashSet<String>>,
    },
    /// Loaded resources changed.
    ResourcesUpdate {
        /// Number of loaded skills.
        skills: usize,
        /// Number of loaded prompt templates.
        templates: usize,
        /// Diagnostics from resource loading.
        diagnostics: Vec<SkillDiagnostic>,
    },
    /// Session name changed.
    SessionInfoUpdate {
        /// New session name.
        name: String,
    },
    /// Compaction started.
    CompactionStart {
        /// Estimated token count before compaction.
        estimated_tokens: usize,
    },
    /// Compaction completed.
    CompactionEnd {
        /// Compaction statistics if successful.
        stats: Option<CompactionStats>,
        /// Error message if failed.
        error: Option<String>,
    },
    /// Steer/follow-up queue length changed.
    QueueUpdate {
        /// Number of steer messages queued.
        steer_len: usize,
        /// Number of follow-up messages queued.
        follow_up_len: usize,
    },
    /// Session writes flushed.
    SavePoint {
        /// Number of entries flushed.
        entries_flushed: usize,
    },
    /// A branch was forked.
    BranchForked {
        /// Source entry ID.
        from: EntryId,
        /// New leaf entry ID.
        new_leaf: EntryId,
        /// Optional branch label.
        label: Option<String>,
    },
    /// Branch cursor was switched.
    BranchSwitched {
        /// Previous entry ID.
        from: EntryId,
        /// New entry ID.
        to: EntryId,
    },
    /// A branch was deleted.
    BranchDeleted {
        /// Leaf entry ID of the deleted branch.
        leaf: EntryId,
    },
    /// A branch summary was generated and persisted.
    BranchSummarized {
        /// Leaf entry ID of the branch.
        leaf: EntryId,
        /// Branch summary text.
        summary: String,
    },
    /// A tool call started.
    ToolCallStart {
        /// Tool call ID.
        tool_use_id: String,
        /// Tool name.
        tool_name: String,
        /// Tool arguments (JSON).
        args: serde_json::Value,
    },
    /// A tool call completed.
    ToolCallEnd {
        /// Tool call ID.
        tool_use_id: String,
        /// Tool name.
        tool_name: String,
        /// Tool call result.
        result: HarnessToolCallResult,
    },
    /// All queued activity finished.
    Settled,
    /// Harness was aborted.
    Aborted,
}
