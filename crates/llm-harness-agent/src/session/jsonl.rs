use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{EntryId, SessionError};
use tokio::sync::Mutex;

use super::repo::SessionRepo;
use super::storage::SessionStorage;
use super::tree_ops;
use super::types::*;

// ── JsonlSessionStorage ────────────────────────────────────────────────────────

/// Session storage backed by a `.jsonl` file plus a `.meta.json` sidecar.
///
/// Entries are appended as newline-delimited JSON. The in-memory tree cache
/// is built on first access and kept in sync via incremental updates.
pub(crate) struct JsonlSessionStorage {
    inner: Mutex<JsonlInner>,
}

struct JsonlInner {
    entries_path: PathBuf,
    meta_path: PathBuf,
    metadata: SessionMetadata,
    /// entry ID → entry (in-memory cache)
    entry_map: HashMap<EntryId, SessionEntry>,
    /// parent ID → child IDs (None key = root children)
    children_map: HashMap<Option<EntryId>, Vec<EntryId>>,
    loaded: bool,
}

impl JsonlSessionStorage {
    /// Open or create a JSONL-backed session storage.
    ///
    /// `dir` is the session directory (must already exist).
    /// Metadata is loaded from `dir/meta.json` if present; otherwise `metadata` is used.
    pub(crate) async fn open(dir: &Path, metadata: SessionMetadata) -> Result<Self, SessionError> {
        let entries_path = dir.join("entries.jsonl");
        let meta_path = dir.join("meta.json");

        // Persist initial metadata if not already present.
        let metadata = if meta_path.exists() {
            let bytes = tokio::fs::read(&meta_path).await?;
            serde_json::from_slice(&bytes)
                .map_err(|e| SessionError::Serialization(e.to_string()))?
        } else {
            let bytes = serde_json::to_vec_pretty(&metadata)
                .map_err(|e| SessionError::Serialization(e.to_string()))?;
            let tmp = meta_path.with_extension("tmp");
            tokio::fs::write(&tmp, &bytes).await?;
            tokio::fs::rename(&tmp, &meta_path).await?;
            metadata
        };

        Ok(Self {
            inner: Mutex::new(JsonlInner {
                entries_path,
                meta_path,
                metadata,
                entry_map: HashMap::new(),
                children_map: HashMap::new(),
                loaded: false,
            }),
        })
    }

    /// Load all entries from the JSONL file into the in-memory cache.
    async fn ensure_loaded(inner: &mut JsonlInner) -> Result<(), SessionError> {
        if inner.loaded {
            return Ok(());
        }
        if !inner.entries_path.exists() {
            inner.loaded = true;
            return Ok(());
        }
        let content = tokio::fs::read_to_string(&inner.entries_path).await?;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let entry: SessionEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(e) => {
                    // Skip malformed/unknown lines instead of failing the whole
                    // session. Forward-compat: a newer binary may have written
                    // an entry_type this binary doesn't recognize. The entry
                    // is lost to this binary but the session remains usable.
                    eprintln!(
                        "warn: session {:?}: skipping unparseable entry line: {e}",
                        inner.entries_path
                    );
                    continue;
                }
            };
            inner
                .children_map
                .entry(entry.parent_id)
                .or_default()
                .push(entry.id);
            inner.entry_map.insert(entry.id, entry);
        }
        inner.loaded = true;
        Ok(())
    }

    async fn persist_meta(inner: &JsonlInner) -> Result<(), SessionError> {
        let bytes = serde_json::to_vec_pretty(&inner.metadata)
            .map_err(|e| SessionError::Serialization(e.to_string()))?;
        Self::atomic_write(&inner.meta_path, &bytes).await?;
        Ok(())
    }

    /// Atomic write: write to a temp file then rename over the target.
    ///
    /// Avoids leaving a partially-written file if the process crashes
    /// mid-write (POSIX rename is atomic within the same filesystem).
    async fn atomic_write(path: &Path, content: &[u8]) -> Result<(), SessionError> {
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, content).await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }
}

impl SessionStorage for JsonlSessionStorage {
    fn metadata(&self) -> BoxFuture<'_, Result<SessionMetadata, SessionError>> {
        Box::pin(async move { Ok(self.inner.lock().await.metadata.clone()) })
    }

    fn create_entry_id(&self) -> EntryId {
        EntryId::new()
    }

    fn append_entry(&self, entry: SessionEntry) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            Self::ensure_loaded(&mut inner).await?;

            let line = serde_json::to_string(&entry)
                .map_err(|e| SessionError::Serialization(e.to_string()))?;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&inner.entries_path)
                .await?;
            use tokio::io::AsyncWriteExt;
            file.write_all(line.as_bytes()).await?;
            file.write_all(b"\n").await?;
            file.flush().await?;

            inner
                .children_map
                .entry(entry.parent_id)
                .or_default()
                .push(entry.id);
            inner.entry_map.insert(entry.id, entry);
            inner.metadata.updated_at = chrono::Utc::now();
            Ok(())
        })
    }

    fn get_entry(&self, id: EntryId) -> BoxFuture<'_, Result<Option<SessionEntry>, SessionError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            Self::ensure_loaded(&mut inner).await?;
            Ok(inner.entry_map.get(&id).cloned())
        })
    }

    fn children(&self, parent: EntryId) -> BoxFuture<'_, Result<Vec<SessionEntry>, SessionError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            Self::ensure_loaded(&mut inner).await?;
            Ok(tree_ops::children(
                &inner.entry_map,
                &inner.children_map,
                parent,
            ))
        })
    }

    fn all_leaves(&self) -> BoxFuture<'_, Result<Vec<EntryId>, SessionError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            Self::ensure_loaded(&mut inner).await?;
            Ok(tree_ops::all_leaves(&inner.entry_map, &inner.children_map))
        })
    }

    fn active_cursor(&self) -> BoxFuture<'_, Result<Option<EntryId>, SessionError>> {
        Box::pin(async move { Ok(self.inner.lock().await.metadata.active_cursor) })
    }

    fn set_active_cursor(&self, id: EntryId) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            inner.metadata.active_cursor = Some(id);
            Self::persist_meta(&inner).await
        })
    }

    fn path_to_root(
        &self,
        target: EntryId,
    ) -> BoxFuture<'_, Result<Vec<SessionEntry>, SessionError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            Self::ensure_loaded(&mut inner).await?;
            tree_ops::path_to_root(&inner.entry_map, target)
        })
    }

    fn common_ancestor(
        &self,
        a: EntryId,
        b: EntryId,
    ) -> BoxFuture<'_, Result<Option<EntryId>, SessionError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            Self::ensure_loaded(&mut inner).await?;
            tree_ops::common_ancestor(&inner.entry_map, a, b)
        })
    }

    fn label_at(&self, id: EntryId) -> BoxFuture<'_, Result<Option<String>, SessionError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            Self::ensure_loaded(&mut inner).await?;
            Ok(tree_ops::label_at(
                &inner.entry_map,
                &inner.children_map,
                id,
            ))
        })
    }

    fn paths_to_all_leaves(&self) -> BoxFuture<'_, Result<Vec<Vec<SessionEntry>>, SessionError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            Self::ensure_loaded(&mut inner).await?;
            tree_ops::paths_to_all_leaves(&inner.entry_map, &inner.children_map)
        })
    }

    fn find_entries_by_type(
        &self,
        kind: SessionEntryKind,
    ) -> BoxFuture<'_, Result<Vec<EntryId>, SessionError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            Self::ensure_loaded(&mut inner).await?;
            Ok(tree_ops::find_entries_by_type(&inner.entry_map, kind))
        })
    }

    fn update_metadata_name(
        &self,
        name: Option<String>,
    ) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            inner.metadata.name = name;
            Self::persist_meta(&inner).await
        })
    }

    fn update_metadata_model(
        &self,
        model: Option<String>,
    ) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            inner.metadata.model = model;
            Self::persist_meta(&inner).await
        })
    }

    fn delete_entries(&self, ids: Vec<EntryId>) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            let ids_set: std::collections::HashSet<EntryId> = ids.into_iter().collect();
            let mut inner = self.inner.lock().await;
            Self::ensure_loaded(&mut inner).await?;

            let mut cursor_reset = false;
            for id in &ids_set {
                if let Some(entry) = inner.entry_map.remove(id)
                    && let Some(children) = inner.children_map.get_mut(&entry.parent_id)
                {
                    children.retain(|c| c != id);
                }
                inner.children_map.remove(&Some(*id));
            }
            if let Some(cursor) = inner.metadata.active_cursor
                && ids_set.contains(&cursor)
            {
                inner.metadata.active_cursor = None;
                cursor_reset = true;
            }

            // Rewrite JSONL file without the deleted entries (sorted for determinism).
            let mut remaining: Vec<&SessionEntry> = inner.entry_map.values().collect();
            remaining.sort_by_key(|e| e.id);
            let mut content = String::new();
            for entry in remaining {
                let line = serde_json::to_string(entry)
                    .map_err(|e| SessionError::Serialization(e.to_string()))?;
                content.push_str(&line);
                content.push('\n');
            }
            Self::atomic_write(&inner.entries_path, content.as_bytes()).await?;

            if cursor_reset {
                Self::persist_meta(&inner).await?;
            }
            Ok(())
        })
    }
    fn all_entries(&self) -> BoxFuture<'_, Result<Vec<SessionEntry>, SessionError>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            Self::ensure_loaded(&mut inner).await?;
            Ok(inner.entry_map.values().cloned().collect())
        })
    }
}

// ── JsonlSessionRepo ───────────────────────────────────────────────────────────

/// File-system-backed session repository.
///
/// Each session is stored in its own subdirectory: `{root}/{session_id}/`.
pub struct JsonlSessionRepo {
    root_dir: PathBuf,
    /// Cache of open storages to avoid re-loading on repeated `open()`.
    cache: Mutex<HashMap<String, Arc<dyn SessionStorage>>>,
}

impl JsonlSessionRepo {
    /// Create a new repo rooted at `root_dir` (must exist).
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn session_dir(&self, id: &str) -> PathBuf {
        self.root_dir.join(id)
    }
}

impl SessionRepo for JsonlSessionRepo {
    fn create(
        &self,
        opts: CreateSessionOptions,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>> {
        Box::pin(async move {
            let id = uuid::Uuid::now_v7().to_string();
            let dir = self.session_dir(&id);
            tokio::fs::create_dir_all(&dir).await?;
            let now = chrono::Utc::now();
            let meta = SessionMetadata {
                id: id.clone(),
                name: opts.name,
                created_at: now,
                updated_at: now,
                model: opts.initial_model,
                active_cursor: None,
                parent_session_path: None,
            };
            let storage: Arc<dyn SessionStorage> =
                Arc::new(JsonlSessionStorage::open(&dir, meta).await?);
            self.cache.lock().await.insert(id, storage.clone());
            Ok(storage)
        })
    }

    fn open(&self, id: &str) -> BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>> {
        let id = id.to_owned();
        Box::pin(async move {
            {
                let cache = self.cache.lock().await;
                if let Some(s) = cache.get(&id) {
                    return Ok(s.clone());
                }
            }
            let dir = self.session_dir(&id);
            if !dir.exists() {
                return Err(SessionError::SessionNotFound(id));
            }
            // Load metadata from disk.
            let meta_path = dir.join("meta.json");
            if !meta_path.exists() {
                return Err(SessionError::SessionNotFound(id.clone()));
            }
            let bytes = tokio::fs::read(&meta_path).await?;
            let meta: SessionMetadata = serde_json::from_slice(&bytes)
                .map_err(|e| SessionError::Serialization(e.to_string()))?;
            let storage: Arc<dyn SessionStorage> =
                Arc::new(JsonlSessionStorage::open(&dir, meta).await?);
            self.cache.lock().await.insert(id, storage.clone());
            Ok(storage)
        })
    }

    fn list(
        &self,
        opts: ListSessionOptions,
    ) -> BoxFuture<'_, Result<Vec<SessionMetadata>, SessionError>> {
        Box::pin(async move {
            let mut read_dir = tokio::fs::read_dir(&self.root_dir).await?;
            let mut metas: Vec<SessionMetadata> = Vec::new();
            while let Some(entry) = read_dir.next_entry().await? {
                if !entry.file_type().await?.is_dir() {
                    continue;
                }
                let meta_path = entry.path().join("meta.json");
                if !meta_path.exists() {
                    continue;
                }
                let bytes = tokio::fs::read(&meta_path).await?;
                let meta: SessionMetadata = match serde_json::from_slice(&bytes) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if let Some(ref needle) = opts.name_contains
                    && !meta.name.as_deref().unwrap_or("").contains(needle.as_str())
                {
                    continue;
                }
                metas.push(meta);
            }
            metas.sort_by(|a, b| match opts.order {
                ListOrder::UpdatedDesc => b.updated_at.cmp(&a.updated_at),
                ListOrder::UpdatedAsc => a.updated_at.cmp(&b.updated_at),
                ListOrder::CreatedDesc => b.created_at.cmp(&a.created_at),
                ListOrder::CreatedAsc => a.created_at.cmp(&b.created_at),
            });
            let offset = opts.offset.unwrap_or(0);
            let metas = if offset < metas.len() {
                &metas[offset..]
            } else {
                &[]
            };
            let metas = if let Some(limit) = opts.limit {
                &metas[..limit.min(metas.len())]
            } else {
                metas
            };
            Ok(metas.to_vec())
        })
    }

    fn delete(&self, id: &str) -> BoxFuture<'_, Result<(), SessionError>> {
        let id = id.to_owned();
        Box::pin(async move {
            let dir = self.session_dir(&id);
            if !dir.exists() {
                return Err(SessionError::SessionNotFound(id.clone()));
            }
            tokio::fs::remove_dir_all(&dir).await?;
            self.cache.lock().await.remove(&id);
            Ok(())
        })
    }

    fn fork(
        &self,
        source_id: &str,
        from_entry: EntryId,
        opts: ForkOptions,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>> {
        let source_id = source_id.to_owned();
        Box::pin(async move {
            let source = self.open(&source_id).await?;
            let path = source.path_to_root(from_entry).await?;

            let new_id = uuid::Uuid::now_v7().to_string();
            let new_dir = self.session_dir(&new_id);
            tokio::fs::create_dir_all(&new_dir).await?;
            let now = chrono::Utc::now();
            let new_meta = SessionMetadata {
                id: new_id.clone(),
                name: opts.name,
                created_at: now,
                updated_at: now,
                model: None,
                active_cursor: None,
                parent_session_path: if !opts.copy_entries {
                    Some(source_id)
                } else {
                    None
                },
            };
            let new_storage = Arc::new(JsonlSessionStorage::open(&new_dir, new_meta).await?);

            let mut id_map: HashMap<EntryId, EntryId> = HashMap::new();
            let mut last_new_id: Option<EntryId> = None;
            for entry in path {
                let new_entry_id = new_storage.create_entry_id();
                id_map.insert(entry.id, new_entry_id);
                let new_parent = entry.parent_id.and_then(|p| id_map.get(&p)).copied();
                let new_entry = SessionEntry {
                    id: new_entry_id,
                    parent_id: new_parent,
                    timestamp: entry.timestamp,
                    payload: entry.payload,
                };
                new_storage.append_entry(new_entry).await?;
                last_new_id = Some(new_entry_id);
            }
            if let Some(cursor) = last_new_id {
                new_storage.set_active_cursor(cursor).await?;
            }

            let storage: Arc<dyn SessionStorage> = new_storage;
            self.cache.lock().await.insert(new_id, storage.clone());
            Ok(storage)
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::session::session::Session;
    use llm_harness_types::*;

    fn user_msg(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: chrono::Utc::now(),
        })
    }

    #[tokio::test]
    async fn jsonl_create_append_reload() {
        let tmp = TempDir::new().unwrap();
        let repo = JsonlSessionRepo::new(tmp.path());

        let storage = repo.create(CreateSessionOptions::default()).await.unwrap();
        let session = Session::new(storage.clone());
        session.append_message(user_msg("hello")).await.unwrap();
        session.append_message(user_msg("world")).await.unwrap();

        let meta = session.metadata().await.unwrap();
        let id = meta.id.clone();

        // Drop the cached storage, reload from disk.
        drop(session);
        repo.cache.lock().await.remove(&id);

        let storage2 = repo.open(&id).await.unwrap();
        let session2 = Session::new(storage2);
        let ctx = session2.build_context().await.unwrap();
        assert_eq!(ctx.messages.len(), 2);
    }

    #[tokio::test]
    async fn jsonl_delete_removes_directory() {
        let tmp = TempDir::new().unwrap();
        let repo = JsonlSessionRepo::new(tmp.path());
        let storage = repo.create(CreateSessionOptions::default()).await.unwrap();
        let id = storage.metadata().await.unwrap().id;
        repo.delete(&id).await.unwrap();
        assert!(repo.open(&id).await.is_err());
    }

    #[tokio::test]
    async fn jsonl_unknown_entry_type_skipped_not_fatal() {
        // Simulate a newer binary writing an entry_type this binary doesn't
        // know. The line should be skipped (warned), not abort the session.
        let tmp = TempDir::new().unwrap();
        let repo = JsonlSessionRepo::new(tmp.path());
        let storage = repo.create(CreateSessionOptions::default()).await.unwrap();
        let session = Session::new(storage.clone());
        session.append_message(user_msg("before")).await.unwrap();
        session.append_message(user_msg("after")).await.unwrap();

        let meta = session.metadata().await.unwrap();
        let id = meta.id.clone();
        drop(session);
        repo.cache.lock().await.remove(&id);

        // Inject a line with an unknown entry_type between the two valid ones.
        let entries_path = tmp.path().join(&id).join("entries.jsonl");
        let content = tokio::fs::read_to_string(&entries_path).await.unwrap();
        let mut lines: Vec<&str> = content.lines().collect();
        let unknown = serde_json::json!({
            "id": "019f2b12-61dc-7000-9e6a-ee60f3b64df9",
            "parent_id": null,
            "timestamp": "2026-07-04T00:00:00Z",
            "entry_type": "some_future_variant",
            "payload_field": 42,
        })
        .to_string();
        lines.insert(1, &unknown);
        tokio::fs::write(&entries_path, lines.join("\n"))
            .await
            .unwrap();

        let storage2 = repo.open(&id).await.unwrap();
        let session2 = Session::new(storage2);
        let ctx = session2.build_context().await.unwrap();
        // Two known messages survive; the unknown line is skipped.
        assert_eq!(ctx.messages.len(), 2);
    }

    #[tokio::test]
    async fn jsonl_malformed_line_skipped_not_fatal() {
        // A corrupt JSON line should not abort the whole session.
        let tmp = TempDir::new().unwrap();
        let repo = JsonlSessionRepo::new(tmp.path());
        let storage = repo.create(CreateSessionOptions::default()).await.unwrap();
        let session = Session::new(storage.clone());
        session.append_message(user_msg("ok")).await.unwrap();

        let meta = session.metadata().await.unwrap();
        let id = meta.id.clone();
        drop(session);
        repo.cache.lock().await.remove(&id);

        let entries_path = tmp.path().join(&id).join("entries.jsonl");
        let content = tokio::fs::read_to_string(&entries_path).await.unwrap();
        let corrupted = format!("{content}\nthis is not json\n");
        tokio::fs::write(&entries_path, corrupted).await.unwrap();

        let storage2 = repo.open(&id).await.unwrap();
        let session2 = Session::new(storage2);
        let ctx = session2.build_context().await.unwrap();
        assert_eq!(ctx.messages.len(), 1);
    }

    #[tokio::test]
    async fn jsonl_delete_uses_atomic_write_no_tmp_residue() {
        let tmp = TempDir::new().unwrap();
        let repo = JsonlSessionRepo::new(tmp.path());
        let storage = repo.create(CreateSessionOptions::default()).await.unwrap();
        let session = Session::new(storage.clone());
        let id1 = session.append_message(user_msg("keep")).await.unwrap();
        let id2 = session.append_message(user_msg("delete")).await.unwrap();
        let meta = session.metadata().await.unwrap();
        let id = meta.id.clone();
        drop(session);

        // delete_entries rewrites entries.jsonl atomically.
        storage.delete_entries(vec![id2]).await.unwrap();

        let dir = tmp.path().join(&id);
        // No leftover .tmp file in the session directory.
        let mut entries = tokio::fs::read_dir(&dir).await.unwrap();
        while let Some(e) = entries.next_entry().await.unwrap() {
            let name = e.file_name();
            assert_ne!(
                name.to_string_lossy(),
                "entries.tmp",
                "atomic write left a .tmp residue"
            );
        }
        // The surviving entry is still readable from disk (file not truncated).
        let storage2 = repo.open(&id).await.unwrap();
        let entry = storage2.get_entry(id1).await.unwrap();
        assert!(entry.is_some(), "surviving entry should still be on disk");
    }
}
