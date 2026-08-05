use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use futures::future::BoxFuture;
use llm_harness_types::{EntryId, SessionError};

use super::storage::{InMemorySessionStorage, SessionStorage};
use super::types::{
    CreateSessionOptions, ForkOptions, ListOrder, ListSessionOptions, SessionDurability,
    SessionLifecycle, SessionMetadata, SessionRecallPolicy,
};

// ── SessionRepo trait ──────────────────────────────────────────────────────────

/// Repository managing multiple sessions' lifecycles.
pub trait SessionRepo: Send + Sync {
    /// Create a new session with the given options.
    fn create(
        &self,
        opts: CreateSessionOptions,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>>;

    /// Open an existing session by ID.
    fn open(&self, id: &str) -> BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>>;

    /// List sessions matching `opts`.
    fn list(
        &self,
        opts: ListSessionOptions,
    ) -> BoxFuture<'_, Result<Vec<SessionMetadata>, SessionError>>;

    /// Permanently delete the session with the given ID.
    fn delete(&self, id: &str) -> BoxFuture<'_, Result<(), SessionError>>;

    /// Cross-session fork: copy the path from root to `from_entry` into a new session.
    fn fork(
        &self,
        source_id: &str,
        from_entry: EntryId,
        opts: ForkOptions,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>>;
}

// ── InMemorySessionRepo ────────────────────────────────────────────────────────

struct RepoState {
    sessions: HashMap<String, Arc<dyn SessionStorage>>,
}

/// In-memory session repository for testing and ephemeral use.
pub struct InMemorySessionRepo {
    inner: Mutex<RepoState>,
}

impl InMemorySessionRepo {
    /// Create a new in-memory session repository.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RepoState {
                sessions: HashMap::new(),
            }),
        }
    }

    fn new_id() -> String {
        uuid::Uuid::now_v7().to_string()
    }
}

impl Default for InMemorySessionRepo {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionRepo for InMemorySessionRepo {
    fn create(
        &self,
        opts: CreateSessionOptions,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>> {
        Box::pin(async move {
            let recall = opts.recall_settings(SessionDurability::Temporary)?;
            let id = Self::new_id();
            let now = Utc::now();
            let meta = SessionMetadata {
                id: id.clone(),
                name: opts.name,
                created_at: now,
                updated_at: now,
                model: opts.initial_model,
                active_cursor: None,
                parent_session_path: None,
                durability: recall.durability,
                lifecycle: recall.lifecycle,
                recall_policy: recall.policy,
                recall_scope: recall.scope,
                recall_revision: if recall.policy == SessionRecallPolicy::Enabled {
                    1
                } else {
                    0
                },
            };
            let storage: Arc<dyn SessionStorage> = Arc::new(InMemorySessionStorage::new(meta));
            self.inner
                .lock()
                .unwrap()
                .sessions
                .insert(id, storage.clone());
            Ok(storage)
        })
    }

    fn open(&self, id: &str) -> BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>> {
        let id = id.to_owned();
        Box::pin(async move {
            self.inner
                .lock()
                .unwrap()
                .sessions
                .get(&id)
                .cloned()
                .ok_or(SessionError::SessionNotFound(id))
        })
    }

    fn list(
        &self,
        opts: ListSessionOptions,
    ) -> BoxFuture<'_, Result<Vec<SessionMetadata>, SessionError>> {
        Box::pin(async move {
            let sessions: Vec<Arc<dyn SessionStorage>> = self
                .inner
                .lock()
                .unwrap()
                .sessions
                .values()
                .cloned()
                .collect();

            let mut metas: Vec<SessionMetadata> = Vec::new();
            for s in sessions {
                let m = s.metadata().await?;
                if let Some(ref needle) = opts.name_contains
                    && !m.name.as_deref().unwrap_or("").contains(needle.as_str())
                {
                    continue;
                }
                metas.push(m);
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
            let removed = self.inner.lock().unwrap().sessions.remove(&id);
            if removed.is_none() {
                return Err(SessionError::SessionNotFound(id));
            }
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
            // Retrieve source storage.
            let source = {
                self.inner
                    .lock()
                    .unwrap()
                    .sessions
                    .get(&source_id)
                    .cloned()
                    .ok_or_else(|| SessionError::SessionNotFound(source_id.clone()))?
            };

            // Build the path from root to from_entry.
            let path = source.path_to_root(from_entry).await?;

            // Create a fresh session.
            let new_id = Self::new_id();
            let now = Utc::now();
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
                durability: SessionDurability::Temporary,
                lifecycle: SessionLifecycle::Active,
                recall_policy: SessionRecallPolicy::Disabled,
                recall_scope: None,
                recall_revision: 0,
            };
            let new_storage: Arc<dyn SessionStorage> =
                Arc::new(InMemorySessionStorage::new(new_meta));

            // Copy entries with new IDs (maintaining relative parent chain).
            let mut id_map: HashMap<EntryId, EntryId> = HashMap::new();
            let mut last_new_id: Option<EntryId> = None;
            for entry in path {
                let new_entry_id = new_storage.create_entry_id();
                id_map.insert(entry.id, new_entry_id);
                let new_parent = entry.parent_id.and_then(|p| id_map.get(&p)).copied();
                let new_entry = super::types::SessionEntry {
                    id: new_entry_id,
                    parent_id: new_parent,
                    timestamp: entry.timestamp,
                    payload: entry.payload,
                    compaction_policy: entry.compaction_policy,
                };
                new_storage.append_entry(new_entry).await?;
                last_new_id = Some(new_entry_id);
            }
            if let Some(cursor) = last_new_id {
                new_storage.set_active_cursor(cursor).await?;
            }

            self.inner
                .lock()
                .unwrap()
                .sessions
                .insert(new_id, new_storage.clone());
            Ok(new_storage)
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::session::Session;
    use crate::session::{
        SessionEntry, SessionEntryPayload, SessionRecallScope, SessionRecallSettings,
    };
    use llm_harness_types::*;

    fn user_msg(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: chrono::Utc::now(),
        })
    }

    #[tokio::test]
    async fn create_open_delete() {
        let repo = InMemorySessionRepo::new();
        let storage = repo.create(CreateSessionOptions::default()).await.unwrap();
        let meta = storage.metadata().await.unwrap();
        assert_eq!(meta.durability, SessionDurability::Temporary);
        assert_eq!(meta.lifecycle, SessionLifecycle::Active);
        assert_eq!(meta.recall_policy, SessionRecallPolicy::Disabled);
        assert_eq!(meta.recall_revision, 0);
        assert!(!meta.is_recallable());
        let id = meta.id.clone();

        repo.open(&id).await.unwrap();
        repo.delete(&id).await.unwrap();
        assert!(repo.open(&id).await.is_err());
    }

    #[tokio::test]
    async fn list_with_filter_and_order() {
        let repo = InMemorySessionRepo::new();
        repo.create(CreateSessionOptions {
            name: Some("alpha".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        repo.create(CreateSessionOptions {
            name: Some("beta".into()),
            ..Default::default()
        })
        .await
        .unwrap();

        let all = repo.list(ListSessionOptions::default()).await.unwrap();
        assert_eq!(all.len(), 2);

        let filtered = repo
            .list(ListSessionOptions {
                name_contains: Some("alph".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name.as_deref(), Some("alpha"));
    }

    #[tokio::test]
    async fn fork_copies_path() {
        let repo = InMemorySessionRepo::new();
        let storage = repo.create(CreateSessionOptions::default()).await.unwrap();
        let session = Session::new(storage);

        let id1 = session.append_message(user_msg("msg1")).await.unwrap();
        session.append_message(user_msg("msg2")).await.unwrap();

        // Fork at id1 — new session should only have msg1.
        let meta = session.metadata().await.unwrap();
        let forked = repo
            .fork(&meta.id, id1, ForkOptions::default())
            .await
            .unwrap();
        let forked_session = Session::new(forked);
        let forked_meta = forked_session.metadata().await.unwrap();
        assert_eq!(forked_meta.recall_policy, SessionRecallPolicy::Disabled);
        assert_eq!(forked_meta.recall_revision, 0);
        let ctx = forked_session.build_context().await.unwrap();
        assert_eq!(ctx.messages.len(), 1);
    }

    #[tokio::test]
    async fn in_memory_recall_is_explicit_and_revisioned() {
        let repo = InMemorySessionRepo::new();
        let storage = repo.create(CreateSessionOptions::default()).await.unwrap();
        let session = Session::new(storage);

        let invalid = SessionRecallSettings {
            durability: SessionDurability::Temporary,
            lifecycle: SessionLifecycle::Active,
            policy: SessionRecallPolicy::Enabled,
            scope: Some(SessionRecallScope::new("team-a")),
        };
        assert!(matches!(
            session.update_recall_settings(invalid).await,
            Err(SessionError::InvalidRecallConfiguration(_))
        ));
        assert_eq!(session.metadata().await.unwrap().recall_revision, 0);

        session
            .update_recall_settings(SessionRecallSettings {
                durability: SessionDurability::Durable,
                lifecycle: SessionLifecycle::Active,
                policy: SessionRecallPolicy::Enabled,
                scope: Some(SessionRecallScope::new("team-a")),
            })
            .await
            .unwrap();
        let enabled = session.metadata().await.unwrap();
        assert!(enabled.is_recallable());
        assert_eq!(enabled.recall_revision, 1);

        session.append_message(user_msg("recall me")).await.unwrap();
        assert_eq!(session.metadata().await.unwrap().recall_revision, 2);

        session
            .update_recall_settings(SessionRecallSettings {
                durability: SessionDurability::Durable,
                lifecycle: SessionLifecycle::Archived,
                policy: SessionRecallPolicy::Enabled,
                scope: Some(SessionRecallScope::new("team-a")),
            })
            .await
            .unwrap();
        let archived = session.metadata().await.unwrap();
        assert!(!archived.is_recallable());
        assert_eq!(archived.recall_revision, 3);
    }

    #[tokio::test]
    async fn in_memory_create_rejects_enabled_temporary_session() {
        let repo = InMemorySessionRepo::new();
        let result = repo
            .create(CreateSessionOptions {
                recall_policy: SessionRecallPolicy::Enabled,
                recall_scope: Some(SessionRecallScope::new("team-a")),
                ..Default::default()
            })
            .await;
        assert!(matches!(
            result,
            Err(SessionError::InvalidRecallConfiguration(_))
        ));
        assert!(
            repo.list(ListSessionOptions::default())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn recall_revision_tracks_visibility_not_detached_append() {
        let repo = InMemorySessionRepo::new();
        let storage = repo
            .create(CreateSessionOptions {
                durability: Some(SessionDurability::Durable),
                recall_policy: SessionRecallPolicy::Enabled,
                recall_scope: Some(SessionRecallScope::new("team-a")),
                ..Default::default()
            })
            .await
            .unwrap();
        let entry_id = storage.create_entry_id();
        storage
            .append_entry(SessionEntry {
                id: entry_id,
                parent_id: None,
                timestamp: chrono::Utc::now(),
                payload: SessionEntryPayload::Unknown,
                compaction_policy: None,
            })
            .await
            .unwrap();
        assert_eq!(storage.metadata().await.unwrap().recall_revision, 1);

        storage.set_active_cursor(entry_id).await.unwrap();
        assert_eq!(storage.metadata().await.unwrap().recall_revision, 2);
    }

    #[tokio::test]
    async fn in_memory_create_rejects_empty_scope_namespace() {
        let repo = InMemorySessionRepo::new();
        let result = repo
            .create(CreateSessionOptions {
                durability: Some(SessionDurability::Durable),
                recall_policy: SessionRecallPolicy::Enabled,
                recall_scope: Some(SessionRecallScope::new("  ")),
                ..Default::default()
            })
            .await;
        assert!(matches!(
            result,
            Err(SessionError::InvalidRecallConfiguration(_))
        ));
    }
}
