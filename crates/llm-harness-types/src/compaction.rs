use crate::{AgentMessage, EntryId};

/// Result returned by `compact()`; written to session by the caller (Harness).
///
/// `compact()` does **not** write to the session — the caller (Harness) appends
/// a `SessionEntryPayload::Compaction` entry on `Ok`.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// Compaction summary message injected at the start of the subsequent LLM context.
    pub summary_message: AgentMessage,
    /// ID of the first session entry that remains valid after compaction.
    pub first_kept_entry: EntryId,
    /// Estimated token count before compaction.
    pub tokens_before: usize,
    /// Estimated token count after compaction (summary + kept entries).
    pub tokens_after: usize,
}
