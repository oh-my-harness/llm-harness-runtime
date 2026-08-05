use std::sync::Arc;

use chrono::Utc;
use llm_harness_types::*;

use super::storage::SessionStorage;
use super::types::*;

/// High-level session interface: appends entries, builds effective LLM context,
/// and manages branch operations.
///
/// Wraps a `SessionStorage` and provides the tree semantics on top.
/// All methods take `&self`. Single storage calls are thread-safe, but
/// multi-step operations like [`Session::append`] and [`Session::fork_branch`]
/// are NOT atomic — concurrent callers must serialize themselves (e.g. via
/// `AgentHarness`'s phase guard). See `SessionStorage` docs.
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
    ///
    /// Equivalent to `append_with_policy(payload, None)`.
    pub async fn append(&self, payload: SessionEntryPayload) -> Result<EntryId, SessionError> {
        self.append_with_policy(payload, None).await
    }

    /// Append any payload with a compaction policy under the current active
    /// cursor and advance it.
    ///
    /// Pass `Some(CompactionPolicy::Keep)` to mark the entry for verbatim
    /// preservation during compaction. Its content is appended to the
    /// compaction summary (in a "Preserved Content" section) rather than
    /// being summarized by the LLM.
    pub async fn append_with_policy(
        &self,
        payload: SessionEntryPayload,
        compaction_policy: Option<CompactionPolicy>,
    ) -> Result<EntryId, SessionError> {
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
            compaction_policy,
        };
        self.storage.append_entry(entry).await?;
        self.storage.set_active_cursor(id).await?;
        Ok(id)
    }

    // ── Branch operations ─────────────────────────────────────────────────

    /// Switch the active cursor to `target`.
    pub async fn navigate_to(&self, target: EntryId) -> Result<(), SessionError> {
        let current = self.storage.active_cursor().await?;
        if current != Some(target) {
            self.storage.set_active_cursor(target).await?;
        }
        Ok(())
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
            compaction_policy: None,
        };
        self.storage.append_entry(entry).await?;
        self.storage.set_active_cursor(bp_id).await?;
        Ok(bp_id)
    }

    /// Delete the branch rooted at `leaf` and its entire subtree.
    ///
    /// First collects all descendants of `leaf` (subtree), then walks up via
    /// `parent_id`, collecting ancestor entries until it hits a parent that
    /// has other children (shared ancestor). The shared ancestor is preserved.
    /// If the active cursor was on a deleted entry, it is reset to the nearest
    /// surviving ancestor.
    pub async fn delete_branch(&self, leaf: EntryId) -> Result<(), SessionError> {
        let mut to_delete: Vec<EntryId> = Vec::new();

        // 1. Collect all descendants of `leaf` (subtree, top-down).
        let mut stack = vec![leaf];
        while let Some(id) = stack.pop() {
            to_delete.push(id);
            let children = self.storage.children(id).await?;
            for child in children {
                stack.push(child.id);
            }
        }

        // 2. Walk up from `leaf`, collecting ancestor chain until a shared ancestor.
        let mut current = leaf;
        loop {
            let entry = self
                .storage
                .get_entry(current)
                .await?
                .ok_or(SessionError::EntryNotFound(current))?;
            match entry.parent_id {
                None => break, // reached root
                Some(parent_id) => {
                    if to_delete.contains(&parent_id) {
                        break; // already collected as a descendant
                    }
                    let siblings = self.storage.children(parent_id).await?;
                    if siblings.len() > 1 {
                        // Parent has other children — stop before deleting it.
                        break;
                    }
                    to_delete.push(parent_id);
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

    /// Atomically replace cross-session recall eligibility and authorization scope.
    pub async fn update_recall_settings(
        &self,
        settings: SessionRecallSettings,
    ) -> Result<(), SessionError> {
        self.storage.update_recall_settings(settings).await
    }
}

// ── build_context_from_entries ────────────────────────────────────────────────

/// Pure function: build effective context from a root-first entry list.
///
/// Finds the last `Compaction` entry, skips all entries before its
/// `first_kept_entry`, inserts the compaction summary, then walks the
/// remaining entries collecting messages and configuration changes.
pub(crate) fn build_context_from_entries(entries: &[SessionEntry]) -> BuiltContext {
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

#[cfg(all(test, feature = "test-utils"))]
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
        let mut m =
            llm_harness_loop::test_utils::test_assistant_message(vec![ContentBlock::Text {
                text: text.into(),
            }]);
        m.stop_reason = Some(StopReason::EndTurn);
        AgentMessage::Assistant(m)
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

    /// After BranchSwitch entry removal: root has two real children (A and B),
    /// both are true leaves — no BranchSwitch pollution inflating or deflating
    /// the leaf count.
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

    /// Tree shape (via fork_branch, no BranchSwitch orphans):
    ///   root
    ///   ├── A_mid → A_leaf
    ///   └── B_only
    /// Deleting A_leaf removes A_leaf + A_mid (unique chain), keeps root + B_only.
    #[tokio::test]
    async fn delete_branch_removes_only_unique_chain() {
        let session = make_session().await;
        let root = session.append_message(user_msg("root")).await.unwrap();

        // Branch A: root → A_mid → A_leaf
        session.fork_branch(root, None).await.unwrap();
        let _a_mid = session
            .append_message(assistant_msg("A mid"))
            .await
            .unwrap();
        let a_leaf = session
            .append_message(assistant_msg("A leaf"))
            .await
            .unwrap();

        // Branch B: root → B_only
        session.fork_branch(root, None).await.unwrap();
        let _b_leaf = session
            .append_message(assistant_msg("B only"))
            .await
            .unwrap();

        // Delete A_leaf — A_leaf's parent is A_mid; A_mid has only A_leaf, so continue.
        // A_mid's parent is root; root has BranchPoint_A + BranchPoint_B (2 > 1), stop.
        // Deletes: A_leaf + A_mid + BranchPoint_A. Keeps: root, BranchPoint_B, B_only.
        session.delete_branch(a_leaf).await.unwrap();

        // Active path (cursor on B branch) should be root → BranchPoint_B → B_only.
        let ctx = session.build_context().await.unwrap();
        // Active path is root → BranchPoint_B → B_only; BranchPoint is skipped by
        // build_context, so 2 messages remain.
        assert_eq!(
            ctx.messages.len(),
            2,
            "root + B_only should remain on active path"
        );
    }

    /// Tree shape:
    ///   root
    ///   └── A_mid → A_leaf
    /// Deleting A_leaf walks up through A_mid (only child) to root (no parent),
    /// deleting the entire chain including root.
    #[tokio::test]
    async fn delete_branch_to_root_removes_entire_chain() {
        let session = make_session().await;
        let _root = session.append_message(user_msg("root")).await.unwrap();
        let _a_mid = session
            .append_message(assistant_msg("A mid"))
            .await
            .unwrap();
        let a_leaf = session
            .append_message(assistant_msg("A leaf"))
            .await
            .unwrap();

        // No siblings anywhere — entire chain root + A_mid + A_leaf is deleted.
        session.delete_branch(a_leaf).await.unwrap();

        let branches = session.list_branches().await.unwrap();
        assert!(branches.is_empty(), "no branches should remain");

        let ctx = session.build_context().await.unwrap();
        assert!(
            ctx.messages.is_empty(),
            "context should be empty after deleting the only branch to root"
        );
    }

    /// Tree shape (via fork_branch):
    ///   root
    ///   ├── BP_A → A_mid → A_leaf
    ///   └── BP_B → B_only
    /// Cursor on A_leaf; deleting A_leaf walks up to root (shared ancestor),
    /// restoring cursor there.
    #[tokio::test]
    async fn delete_branch_restores_cursor_to_ancestor() {
        let session = make_session().await;
        let root = session.append_message(user_msg("root")).await.unwrap();

        // Branch A: root → BP_A → A_mid → A_leaf
        session.fork_branch(root, None).await.unwrap();
        let _a_mid = session
            .append_message(assistant_msg("A mid"))
            .await
            .unwrap();
        let a_leaf = session
            .append_message(assistant_msg("A leaf"))
            .await
            .unwrap();
        // cursor is now on a_leaf.

        // Branch B: root → BP_B → B_only (ensures root is a shared ancestor)
        session.fork_branch(root, None).await.unwrap();
        let _b_leaf = session
            .append_message(assistant_msg("B only"))
            .await
            .unwrap();

        // Move cursor back to a_leaf, then delete it.
        session.navigate_to(a_leaf).await.unwrap();
        session.delete_branch(a_leaf).await.unwrap();

        // delete_branch walks: a_leaf → A_mid (only child) → BP_A (only child)
        // → root (has BP_A + BP_B, 2 > 1, stop). Deletes a_leaf + A_mid + BP_A.
        // cursor was on a_leaf (deleted) → restored to root.
        let ctx = session.build_context().await.unwrap();
        assert_eq!(
            ctx.messages.len(),
            1,
            "cursor should be at root after deleting branch A"
        );
    }

    /// Tree shape:
    ///   root
    ///   ├── A_only
    ///   └── B_only
    /// Cursor on B_only; deleting A_only should not move the cursor.
    #[tokio::test]
    async fn delete_branch_preserves_cursor_when_not_on_deleted() {
        let session = make_session().await;
        let root = session.append_message(user_msg("root")).await.unwrap();

        // Branch A (will be deleted)
        session.navigate_to(root).await.unwrap();
        let a_leaf = session.append_message(assistant_msg("A")).await.unwrap();

        // Branch B (active)
        session.navigate_to(root).await.unwrap();
        let _b_leaf = session.append_message(assistant_msg("B")).await.unwrap();

        // cursor is on b_leaf; deleting a_leaf should not move it.
        session.delete_branch(a_leaf).await.unwrap();

        let ctx = session.build_context().await.unwrap();
        assert_eq!(ctx.messages.len(), 2, "root + B should remain");
        let last = ctx.messages.last().unwrap();
        if let AgentMessage::Assistant(a) = last {
            let text = a.content.iter().find_map(|c| {
                if let ContentBlock::Text { text } = c {
                    Some(text.as_str())
                } else {
                    None
                }
            });
            assert_eq!(text, Some("B"), "cursor should still be on branch B");
        } else {
            panic!("expected assistant message");
        }
    }

    /// Regression: navigate_to followed by delete_branch must not leave orphan entries.
    ///
    /// Before fix: navigate_to wrote a BranchSwitch entry as child of the old leaf.
    /// delete_branch deleted the old leaf but not the BranchSwitch child, leaving an
    /// orphan whose parent_id pointed to a deleted entry. list_branches then failed
    /// with EntryNotFound (jsonl backend) or returned a bogus branch (in-memory).
    ///
    /// Tree shape after fix:
    ///   root
    ///   ├── A   (deleted)
    ///   └── B   (survives)
    #[tokio::test]
    async fn navigate_then_delete_branch_no_orphan() {
        let session = make_session().await;
        let root = session.append_message(user_msg("root")).await.unwrap();

        // Branch A
        session.navigate_to(root).await.unwrap();
        let a_leaf = session.append_message(assistant_msg("A")).await.unwrap();

        // Navigate back to root and create branch B.
        session.navigate_to(root).await.unwrap();
        let b_leaf = session.append_message(assistant_msg("B")).await.unwrap();

        // Delete branch A — must not leave orphan BranchSwitch under A.
        session.delete_branch(a_leaf).await.unwrap();

        let branches = session.list_branches().await.unwrap();
        assert_eq!(branches.len(), 1, "only branch B should remain");
        assert_eq!(branches[0].leaf_id, b_leaf);
    }

    /// Regression: delete_branch on a node with children must delete the
    /// entire subtree, not just the node itself. Without the fix, children
    /// become orphans (parent_id points to a deleted entry), causing
    /// list_branches to fail with EntryNotFound.
    ///
    /// Tree shape:
    ///   root → turn1 → turn2_main (main_leaf)
    ///                ↘ fork → alt_leaf → turn3_alt
    ///
    /// delete_branch(alt_leaf) must delete [alt_leaf, turn3_alt] + the
    /// unique ancestor chain up to turn1 (which is shared, so preserved).
    /// After deletion, only the main branch should remain.
    #[tokio::test]
    async fn delete_branch_with_subtree_no_orphan() {
        let session = make_session().await;
        let root = session.append_message(user_msg("root")).await.unwrap();
        let turn1 = session
            .append_message(assistant_msg("turn1"))
            .await
            .unwrap();
        let main_leaf = session.append_message(assistant_msg("main")).await.unwrap();

        // Fork from turn1, then extend the forked branch.
        let alt_leaf = session.fork_branch(turn1, None).await.unwrap();
        session.navigate_to(alt_leaf).await.unwrap();
        let turn3_alt = session
            .append_message(assistant_msg("alt_turn3"))
            .await
            .unwrap();

        // Navigate back to main branch, then delete the alt branch root.
        // alt_leaf is NOT a true leaf — it has child turn3_alt.
        session.navigate_to(main_leaf).await.unwrap();
        session.delete_branch(alt_leaf).await.unwrap();

        // list_branches must not fail with EntryNotFound.
        let branches = session.list_branches().await.unwrap();
        assert_eq!(branches.len(), 1, "only main branch should remain");
        assert_eq!(branches[0].leaf_id, main_leaf);

        // Verify the orphan entry is gone.
        let entries = session.storage.all_entries().await.unwrap();
        assert!(
            entries.iter().any(|e| e.id == root),
            "shared root must remain after deleting the alternate subtree"
        );
        assert!(
            !entries.iter().any(|e| e.id == turn3_alt),
            "turn3_alt (child of deleted alt_leaf) must be removed"
        );
        assert!(
            !entries.iter().any(|e| e.id == alt_leaf),
            "alt_leaf must be removed"
        );
    }
}
