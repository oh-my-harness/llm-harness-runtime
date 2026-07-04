use std::collections::HashMap;
use std::sync::Mutex;

use futures::future::BoxFuture;
use llm_harness_types::{EntryId, SessionError};

use super::tree_ops;
use super::types::{SessionEntry, SessionEntryKind, SessionMetadata};

// ── SessionStorage trait ───────────────────────────────────────────────────────

/// Low-level storage backend for a single session.
///
/// Implementations must serialize all writes internally so callers can share
/// `Arc<dyn SessionStorage>` freely.
pub trait SessionStorage: Send + Sync {
    /// Return the current session metadata snapshot.
    fn metadata(&self) -> BoxFuture<'_, Result<SessionMetadata, SessionError>>;

    /// Generate a new time-ordered entry ID (sync — no I/O needed).
    fn create_entry_id(&self) -> EntryId;

    /// Append a pre-built entry. Does NOT advance `active_cursor`; the caller
    /// is responsible for calling `set_active_cursor` afterwards if needed.
    fn append_entry(&self, entry: SessionEntry) -> BoxFuture<'_, Result<(), SessionError>>;

    /// Get an entry by ID.
    fn get_entry(&self, id: EntryId) -> BoxFuture<'_, Result<Option<SessionEntry>, SessionError>>;

    /// Get all child entries of the given parent.
    fn children(&self, parent: EntryId) -> BoxFuture<'_, Result<Vec<SessionEntry>, SessionError>>;

    /// Get all leaf entry IDs (entries with no children).
    fn all_leaves(&self) -> BoxFuture<'_, Result<Vec<EntryId>, SessionError>>;

    /// Get the current active cursor position.
    fn active_cursor(&self) -> BoxFuture<'_, Result<Option<EntryId>, SessionError>>;

    /// Set the active cursor to a new position.
    fn set_active_cursor(&self, id: EntryId) -> BoxFuture<'_, Result<(), SessionError>>;

    /// Return all entries from `target` to the root, ordered root-first.
    fn path_to_root(
        &self,
        target: EntryId,
    ) -> BoxFuture<'_, Result<Vec<SessionEntry>, SessionError>>;

    /// Find the lowest common ancestor of two entries.
    fn common_ancestor(
        &self,
        a: EntryId,
        b: EntryId,
    ) -> BoxFuture<'_, Result<Option<EntryId>, SessionError>>;

    /// Return the label name for the entry at `id`, if any.
    fn label_at(&self, id: EntryId) -> BoxFuture<'_, Result<Option<String>, SessionError>>;

    /// Return all leaf-to-root paths in a single traversal.
    ///
    /// Each inner `Vec` is ordered root-first (same convention as `path_to_root`).
    /// Replaces L calls to `path_to_root` in `Session::list_branches`.
    fn paths_to_all_leaves(&self) -> BoxFuture<'_, Result<Vec<Vec<SessionEntry>>, SessionError>>;

    /// Find all entries of a given kind.
    fn find_entries_by_type(
        &self,
        kind: SessionEntryKind,
    ) -> BoxFuture<'_, Result<Vec<EntryId>, SessionError>>;

    /// Update the session name.
    fn update_metadata_name(&self, name: Option<String>)
    -> BoxFuture<'_, Result<(), SessionError>>;

    /// Update the session model.
    fn update_metadata_model(
        &self,
        model: Option<String>,
    ) -> BoxFuture<'_, Result<(), SessionError>>;

    /// Delete the given entries and remove them from the children index.
    ///
    /// If the active cursor is among the deleted IDs it is reset to `None`.
    fn delete_entries(&self, ids: Vec<EntryId>) -> BoxFuture<'_, Result<(), SessionError>>;
    /// Return all entries in the session (unordered).
    ///
    /// UI uses this to reconstruct the full branch tree for ASCII rendering.
    fn all_entries(&self) -> BoxFuture<'_, Result<Vec<SessionEntry>, SessionError>>;
}

// ── InMemorySessionStorage ─────────────────────────────────────────────────────

struct InMemoryState {
    metadata: SessionMetadata,
    entries: HashMap<EntryId, SessionEntry>,
    /// parent → list of child IDs (insertion order)
    children: HashMap<Option<EntryId>, Vec<EntryId>>,
}

/// In-memory session storage; useful for tests and ephemeral sessions.
pub struct InMemorySessionStorage {
    inner: Mutex<InMemoryState>,
}

impl InMemorySessionStorage {
    /// Create a new in-memory session storage with the given initial metadata.
    pub fn new(metadata: SessionMetadata) -> Self {
        Self {
            inner: Mutex::new(InMemoryState {
                metadata,
                entries: HashMap::new(),
                children: HashMap::new(),
            }),
        }
    }
}

impl SessionStorage for InMemorySessionStorage {
    fn metadata(&self) -> BoxFuture<'_, Result<SessionMetadata, SessionError>> {
        Box::pin(async move { Ok(self.inner.lock().unwrap().metadata.clone()) })
    }

    fn create_entry_id(&self) -> EntryId {
        EntryId::new()
    }

    fn append_entry(&self, entry: SessionEntry) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            let mut st = self.inner.lock().unwrap();
            st.children
                .entry(entry.parent_id)
                .or_default()
                .push(entry.id);
            st.entries.insert(entry.id, entry);
            st.metadata.updated_at = chrono::Utc::now();
            Ok(())
        })
    }

    fn get_entry(&self, id: EntryId) -> BoxFuture<'_, Result<Option<SessionEntry>, SessionError>> {
        Box::pin(async move { Ok(self.inner.lock().unwrap().entries.get(&id).cloned()) })
    }

    fn children(&self, parent: EntryId) -> BoxFuture<'_, Result<Vec<SessionEntry>, SessionError>> {
        Box::pin(async move {
            let st = self.inner.lock().unwrap();
            Ok(tree_ops::children(&st.entries, &st.children, parent))
        })
    }

    fn all_leaves(&self) -> BoxFuture<'_, Result<Vec<EntryId>, SessionError>> {
        Box::pin(async move {
            let st = self.inner.lock().unwrap();
            Ok(tree_ops::all_leaves(&st.entries, &st.children))
        })
    }

    fn active_cursor(&self) -> BoxFuture<'_, Result<Option<EntryId>, SessionError>> {
        Box::pin(async move { Ok(self.inner.lock().unwrap().metadata.active_cursor) })
    }

    fn set_active_cursor(&self, id: EntryId) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            let mut st = self.inner.lock().unwrap();
            st.metadata.active_cursor = Some(id);
            st.metadata.updated_at = chrono::Utc::now();
            Ok(())
        })
    }

    fn path_to_root(
        &self,
        target: EntryId,
    ) -> BoxFuture<'_, Result<Vec<SessionEntry>, SessionError>> {
        Box::pin(async move {
            let st = self.inner.lock().unwrap();
            tree_ops::path_to_root(&st.entries, target)
        })
    }

    fn common_ancestor(
        &self,
        a: EntryId,
        b: EntryId,
    ) -> BoxFuture<'_, Result<Option<EntryId>, SessionError>> {
        Box::pin(async move {
            let st = self.inner.lock().unwrap();
            tree_ops::common_ancestor(&st.entries, a, b)
        })
    }

    fn label_at(&self, id: EntryId) -> BoxFuture<'_, Result<Option<String>, SessionError>> {
        Box::pin(async move {
            let st = self.inner.lock().unwrap();
            Ok(tree_ops::label_at(&st.entries, &st.children, id))
        })
    }

    fn paths_to_all_leaves(&self) -> BoxFuture<'_, Result<Vec<Vec<SessionEntry>>, SessionError>> {
        Box::pin(async move {
            let st = self.inner.lock().unwrap();
            tree_ops::paths_to_all_leaves(&st.entries, &st.children)
        })
    }

    fn find_entries_by_type(
        &self,
        kind: SessionEntryKind,
    ) -> BoxFuture<'_, Result<Vec<EntryId>, SessionError>> {
        Box::pin(async move {
            let st = self.inner.lock().unwrap();
            Ok(tree_ops::find_entries_by_type(&st.entries, kind))
        })
    }

    fn update_metadata_name(
        &self,
        name: Option<String>,
    ) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            self.inner.lock().unwrap().metadata.name = name;
            Ok(())
        })
    }

    fn update_metadata_model(
        &self,
        model: Option<String>,
    ) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            self.inner.lock().unwrap().metadata.model = model;
            Ok(())
        })
    }

    fn delete_entries(&self, ids: Vec<EntryId>) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            let ids_set: std::collections::HashSet<EntryId> = ids.into_iter().collect();
            let mut st = self.inner.lock().unwrap();
            for id in &ids_set {
                if let Some(entry) = st.entries.remove(id)
                    && let Some(children) = st.children.get_mut(&entry.parent_id)
                {
                    children.retain(|c| c != id);
                }
                st.children.remove(&Some(*id));
            }
            if let Some(cursor) = st.metadata.active_cursor
                && ids_set.contains(&cursor)
            {
                st.metadata.active_cursor = None;
            }
            Ok(())
        })
    }

    fn all_entries(&self) -> BoxFuture<'_, Result<Vec<SessionEntry>, SessionError>> {
        Box::pin(async move {
            Ok(self
                .inner
                .lock()
                .unwrap()
                .entries
                .values()
                .cloned()
                .collect())
        })
    }
}
