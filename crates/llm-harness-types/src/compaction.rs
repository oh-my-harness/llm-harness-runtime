use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{AgentMessage, EntryId};

/// Whether a file was read or modified during the compacted period.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FileOpKind {
    /// File was read.
    Read,
    /// File was created or modified.
    Modify,
}

/// A file operation recorded for inclusion in the compaction summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOperation {
    /// Absolute path of the file.
    pub path: PathBuf,
    /// Whether the file was read or modified.
    pub kind: FileOpKind,
    /// Session entry ID at which this operation occurred.
    pub at_entry: EntryId,
}

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
    /// File operations recorded during the compacted period.
    pub file_operations: Vec<FileOperation>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EntryId;

    #[test]
    fn file_op_kind_serde_roundtrip() {
        let k = FileOpKind::Modify;
        let json = serde_json::to_string(&k).unwrap();
        let k2: FileOpKind = serde_json::from_str(&json).unwrap();
        assert!(matches!(k2, FileOpKind::Modify));
    }

    #[test]
    fn file_operation_fields() {
        let op = FileOperation {
            path: PathBuf::from("/foo/bar.rs"),
            kind: FileOpKind::Read,
            at_entry: EntryId::new(),
        };
        assert!(op.path.ends_with("bar.rs"));
        assert!(matches!(op.kind, FileOpKind::Read));
    }
}
