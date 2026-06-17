use std::sync::Arc;

use chrono::Utc;
use llm_harness_types::*;

use super::storage::SessionStorage;
use super::types::*;

/// High-level session interface: appends entries, builds effective LLM context,
/// and manages branch operations.
///
/// Wraps a `SessionStorage` and provides the tree semantics on top.
/// All methods take `&self` — concurrent access is serialized inside the storage.
pub struct Session {
    storage: Arc<dyn SessionStorage>,
}

impl Session {
    /// Wrap a storage backend in the high-level `Session` interface.
    pub fn new(storage: Arc<dyn SessionStorage>) -> Self {
        Self { storage }
    }

    /// Return the underlying storage (for Harness-level writes).
    pub fn storage(&self) -> &Arc<dyn SessionStorage> {
        &self.storage
    }

    // ── Raw path reads ─────────────────────────────────────────────────────

    /// Return all entries on the active cursor's path, ordered root-first.
    pub async fn read_active_path(&self) -> Result<Vec<SessionEntry>, SessionError> {
        match self.storage.active_cursor().await? {
            Some(cursor) => self.storage.path_to_root(cursor).await,
            None => Ok(vec![]),
        }
    }

    /// Return all entries on the path from root to `leaf`, ordered root-first.
    pub async fn read_path_of(&self, leaf: EntryId) -> Result<Vec<SessionEntry>, SessionError> {
        self.storage.path_to_root(leaf).await
    }

    // ── Context building ───────────────────────────────────────────────────

    /// Build the effective LLM context from the active path.
    ///
    /// Resolves compaction: messages before the last compaction are replaced
    /// with the compaction summary. Configuration changes (model, thinking level,
    /// tools) are applied in order, and the last value of each is returned.
    pub async fn build_context(&self) -> Result<BuiltContext, SessionError> {
        let entries = self.read_active_path().await?;
        Ok(build_context_from_entries(&entries))
    }

    // ── Append ────────────────────────────────────────────────────────────

    /// Append an `AgentMessage` under the current active cursor and advance it.
    pub async fn append_message(&self, msg: AgentMessage) -> Result<EntryId, SessionError> {
        self.append(SessionEntryPayload::Message(msg)).await
    }

    /// Append any payload under the current active cursor and advance it.
    pub async fn append(&self, payload: SessionEntryPayload) -> Result<EntryId, SessionError> {
        let id = self.storage.create_entry_id();
        let parent_id = self.storage.active_cursor().await?;

        // Update model metadata when a ModelChange is appended.
        if let SessionEntryPayload::ModelChange { ref to, .. } = payload {
            self.storage.update_metadata_model(Some(to.clone())).await?;
        }

        let entry = SessionEntry {
            id,
            parent_id,
            timestamp: Utc::now(),
            payload,
        };
        self.storage.append_entry(entry).await?;
        self.storage.set_active_cursor(id).await?;
        Ok(id)
    }

    // ── Branch operations ─────────────────────────────────────────────────

    /// Switch the active cursor to `target` and write a `BranchSwitch` record.
    pub async fn navigate_to(&self, target: EntryId) -> Result<(), SessionError> {
        let current = self.storage.active_cursor().await?;
        // Only write a BranchSwitch entry if we're actually moving to a different location.
        if let Some(from) = current
            && from != target
        {
            let switch_id = self.storage.create_entry_id();
            let entry = SessionEntry {
                id: switch_id,
                parent_id: Some(from),
                timestamp: Utc::now(),
                payload: SessionEntryPayload::BranchSwitch {
                    from,
                    to: target,
                    summary: None,
                },
            };
            self.storage.append_entry(entry).await?;
        }
        self.storage.set_active_cursor(target).await
    }

    /// Fork at `from_entry`: set cursor to that entry and write a `BranchPoint` annotation.
    ///
    /// Returns the `BranchPoint` entry ID. Subsequent `append` calls create children
    /// of `from_entry`, forming a new branch.
    pub async fn fork_branch(
        &self,
        from_entry: EntryId,
        label: Option<String>,
    ) -> Result<EntryId, SessionError> {
        self.storage.set_active_cursor(from_entry).await?;
        let bp_id = self.storage.create_entry_id();
        let entry = SessionEntry {
            id: bp_id,
            parent_id: Some(from_entry),
            timestamp: Utc::now(),
            payload: SessionEntryPayload::BranchPoint {
                from: from_entry,
                label,
            },
        };
        self.storage.append_entry(entry).await?;
        self.storage.set_active_cursor(bp_id).await?;
        Ok(bp_id)
    }

    /// Delete the branch ending at `leaf`.
    ///
    /// Walks up via `parent_id`, collecting entries to delete until it hits a
    /// parent that has other children (shared ancestor). The shared ancestor is
    /// preserved. If the active cursor was on a deleted entry, it is reset to
    /// the nearest surviving ancestor.
    pub async fn delete_branch(&self, leaf: EntryId) -> Result<(), SessionError> {
        let mut to_delete: Vec<EntryId> = Vec::new();
        let mut current = leaf;

        loop {
            let entry = self
                .storage
                .get_entry(current)
                .await?
                .ok_or(SessionError::EntryNotFound(current))?;
            to_delete.push(current);
            match entry.parent_id {
                None => break, // reached root
                Some(parent_id) => {
                    let siblings = self.storage.children(parent_id).await?;
                    if siblings.len() > 1 {
                        // Parent has other children — stop before deleting it.
                        break;
                    }
                    current = parent_id;
                }
            }
        }

        // Determine new cursor: the parent of the topmost entry we delete.
        let top_entry = self.storage.get_entry(*to_delete.last().unwrap()).await?;
        let new_cursor = top_entry.and_then(|e| e.parent_id);

        let old_cursor = self.storage.active_cursor().await?;
        let cursor_deleted = old_cursor.is_some_and(|c| to_delete.contains(&c));

        self.storage.delete_entries(to_delete).await?;

        // Restore cursor to surviving ancestor if it was deleted.
        if cursor_deleted && let Some(c) = new_cursor {
            self.storage.set_active_cursor(c).await?;
        }

        Ok(())
    }

    /// Return a list of all branches (one per leaf).
    pub async fn list_branches(&self) -> Result<Vec<BranchInfo>, SessionError> {
        let all_paths = self.storage.paths_to_all_leaves().await?;
        let mut branches = Vec::with_capacity(all_paths.len());
        for path in all_paths {
            let Some(leaf_entry) = path.last() else {
                continue;
            };
            let leaf_id = leaf_entry.id;
            let message_count = path.len();
            let last_activity = leaf_entry.timestamp;
            let label = self.storage.label_at(leaf_id).await?;
            let summary = path.iter().rev().find_map(|e| {
                if let SessionEntryPayload::BranchSummary(bs) = &e.payload
                    && bs.leaf_id == leaf_id
                {
                    Some(bs.summary.clone())
                } else {
                    None
                }
            });
            branches.push(BranchInfo {
                leaf_id,
                label,
                message_count,
                last_activity,
                summary,
            });
        }
        Ok(branches)
    }

    /// Session metadata snapshot.
    pub async fn metadata(&self) -> Result<SessionMetadata, SessionError> {
        self.storage.metadata().await
    }
}

// ── build_context_from_entries ────────────────────────────────────────────────

/// Pure function: build effective context from a root-first entry list.
///
/// Finds the last `Compaction` entry, skips all entries before its
/// `first_kept_entry`, inserts the compaction summary, then walks the
/// remaining entries collecting messages and configuration changes.
pub fn build_context_from_entries(entries: &[SessionEntry]) -> BuiltContext {
    let mut last_model: Option<String> = None;
    let mut last_thinking_level: Option<ThinkingLevel> = None;
    let mut last_active_tools: Option<Vec<String>> = None;

    // Locate the most recent compaction entry.
    let last_compaction = entries.iter().rev().find_map(|e| {
        if let SessionEntryPayload::Compaction(c) = &e.payload {
            Some(c.clone())
        } else {
            None
        }
    });

    // Determine the first entry index to include.
    let start_idx = if let Some(ref c) = last_compaction {
        entries
            .iter()
            .position(|e| e.id == c.first_kept_entry)
            .unwrap_or(0)
    } else {
        0
    };

    let mut messages: Vec<AgentMessage> = Vec::new();

    // Prepend the compaction summary message if there was a compaction.
    if let Some(ref c) = last_compaction {
        messages.push(c.summary_message.clone());
    }

    // Walk entries from start_idx onward, collecting messages and config changes.
    for entry in &entries[start_idx..] {
        match &entry.payload {
            SessionEntryPayload::Message(msg) => messages.push(msg.clone()),
            SessionEntryPayload::ModelChange { to, .. } => {
                last_model = Some(to.clone());
            }
            SessionEntryPayload::ThinkingLevelChange { to } => {
                last_thinking_level = Some(*to);
            }
            SessionEntryPayload::ActiveToolsChange { active } => {
                last_active_tools = Some(active.clone());
            }
            // Compaction already handled above; skip duplicate summary insertion.
            SessionEntryPayload::Compaction(_) => {}
            // Branch metadata, labels, custom entries: skip.
            _ => {}
        }
    }

    BuiltContext {
        messages,
        last_model,
        last_thinking_level,
        last_active_tools,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {

    use llm_harness_types::*;

    use crate::session::{
        SessionRepo,
        repo::InMemorySessionRepo,
        types::{CreateSessionOptions, SessionEntryPayload},
    };

    use super::Session;

    async fn make_session() -> Session {
        let repo = InMemorySessionRepo::new();
        let storage = repo.create(CreateSessionOptions::default()).await.unwrap();
        Session::new(storage)
    }

    fn user_msg(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: chrono::Utc::now(),
        })
    }

    fn assistant_msg(text: &str) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: Some(StopReason::EndTurn),
            timestamp: chrono::Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        })
    }

    #[tokio::test]
    async fn append_and_build_context() {
        let session = make_session().await;
        session.append_message(user_msg("hello")).await.unwrap();
        session
            .append_message(assistant_msg("hi there"))
            .await
            .unwrap();

        let ctx = session.build_context().await.unwrap();
        assert_eq!(ctx.messages.len(), 2);
        assert!(matches!(ctx.messages[0], AgentMessage::User(_)));
        assert!(matches!(ctx.messages[1], AgentMessage::Assistant(_)));
    }

    #[tokio::test]
    async fn model_change_tracked_in_context() {
        let session = make_session().await;
        session
            .append(SessionEntryPayload::ModelChange {
                to: "claude-opus-4-7".into(),
                provider: None,
                model_id: None,
            })
            .await
            .unwrap();
        let ctx = session.build_context().await.unwrap();
        assert_eq!(ctx.last_model.as_deref(), Some("claude-opus-4-7"));
    }

    #[tokio::test]
    async fn navigate_to_switches_branch() {
        let session = make_session().await;
        let id1 = session.append_message(user_msg("first")).await.unwrap();
        session
            .append_message(assistant_msg("reply 1"))
            .await
            .unwrap();

        // Navigate back to id1 and append a different response.
        session.navigate_to(id1).await.unwrap();
        session
            .append_message(assistant_msg("reply 2"))
            .await
            .unwrap();

        // The active path should now end with "reply 2".
        let ctx = session.build_context().await.unwrap();
        let last_msg = ctx.messages.last().unwrap();
        if let AgentMessage::Assistant(a) = last_msg {
            let text = a.content.iter().find_map(|c| {
                if let ContentBlock::Text { text } = c {
                    Some(text.as_str())
                } else {
                    None
                }
            });
            assert_eq!(text, Some("reply 2"));
        } else {
            panic!("expected assistant message");
        }
    }

    #[tokio::test]
    async fn list_branches_returns_all_leaves() {
        let session = make_session().await;
        let root_id = session.append_message(user_msg("common")).await.unwrap();

        // Branch A
        session.navigate_to(root_id).await.unwrap();
        session
            .append_message(assistant_msg("branch A"))
            .await
            .unwrap();

        // Branch B
        session.navigate_to(root_id).await.unwrap();
        session
            .append_message(assistant_msg("branch B"))
            .await
            .unwrap();

        let branches = session.list_branches().await.unwrap();
        assert_eq!(branches.len(), 2);
    }

    #[tokio::test]
    async fn build_context_after_compaction() {
        let session = make_session().await;
        session.append_message(user_msg("old 1")).await.unwrap();
        session
            .append_message(assistant_msg("old reply"))
            .await
            .unwrap();
        let kept_id = session
            .append_message(user_msg("new question"))
            .await
            .unwrap();

        let summary = AgentMessage::CompactionSummary(CompactionSummaryMessage {
            summary: "Summary of old messages".into(),
            timestamp: chrono::Utc::now(),
        });
        use crate::session::types::CompactionEntry;
        session
            .append(SessionEntryPayload::Compaction(CompactionEntry {
                summary_message: summary,
                first_kept_entry: kept_id,
                tokens_before: 500,
                from_hook: false,
                details: None,
            }))
            .await
            .unwrap();
        session
            .append_message(assistant_msg("new reply"))
            .await
            .unwrap();

        let ctx = session.build_context().await.unwrap();
        // Should have: [compaction summary, new question, new reply]
        assert_eq!(ctx.messages.len(), 3);
        assert!(matches!(
            ctx.messages[0],
            AgentMessage::CompactionSummary(_)
        ));
    }
}
