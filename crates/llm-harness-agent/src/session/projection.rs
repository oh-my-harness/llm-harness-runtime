use std::sync::Arc;

use futures::future::BoxFuture;
use llm_harness_types::{EntryId, SessionError};

use super::{
    SessionEntry, SessionEntryKind, SessionMetadata, SessionRecallSettings, SessionRepo,
    SessionStorage,
};
use crate::session::{CreateSessionOptions, ForkOptions, ListSessionOptions};

/// Reference-only mutation signal for optional derived Session projections.
///
/// Events deliberately contain no Session entry payload, message text, tool
/// data, or authorization credential. A projector must reopen Session
/// authority to extract currently permitted content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMutation {
    pub session_id: String,
    pub recall_revision: Option<u64>,
    pub kind: SessionMutationKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMutationKind {
    Created,
    Forked { source_session_id: String },
    EntryAppended { entry_id: EntryId },
    EntriesDeleted { entry_ids: Vec<EntryId> },
    ActiveBranchChanged { entry_id: EntryId },
    MetadataChanged,
    RecallSettingsChanged,
    Deleted,
}

/// Opt-in sink for derived projection invalidation/update work.
///
/// Observation is intentionally infallible from the authority caller's point
/// of view: projection failure cannot roll back a Session mutation that has
/// already committed. Implementations record/retry failures and reconcile
/// from Session authority on startup.
pub trait SessionMutationObserver: Send + Sync {
    fn observe<'a>(&'a self, mutation: SessionMutation) -> BoxFuture<'a, ()>;
}

/// Optional `SessionRepo` decorator that emits reference-only mutation events.
///
/// Applications that do not install this wrapper execute the original repo and
/// storage implementations directly and pay no observation cost.
pub struct ObservedSessionRepo {
    inner: Arc<dyn SessionRepo>,
    observer: Arc<dyn SessionMutationObserver>,
}

impl ObservedSessionRepo {
    pub fn new(inner: Arc<dyn SessionRepo>, observer: Arc<dyn SessionMutationObserver>) -> Self {
        Self { inner, observer }
    }

    async fn wrap_storage(
        &self,
        storage: Arc<dyn SessionStorage>,
    ) -> Result<Arc<dyn SessionStorage>, SessionError> {
        let session_id = storage.metadata().await?.id;
        Ok(Arc::new(ObservedSessionStorage {
            inner: storage,
            observer: Arc::clone(&self.observer),
            session_id,
        }))
    }

    async fn emit_for_storage(&self, storage: &Arc<dyn SessionStorage>, kind: SessionMutationKind) {
        let metadata = storage.metadata().await.ok();
        let Some(metadata) = metadata else {
            return;
        };
        self.observer
            .observe(SessionMutation {
                session_id: metadata.id,
                recall_revision: Some(metadata.recall_revision),
                kind,
            })
            .await;
    }
}

impl SessionRepo for ObservedSessionRepo {
    fn create(
        &self,
        opts: CreateSessionOptions,
    ) -> BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>> {
        Box::pin(async move {
            let storage = self.inner.create(opts).await?;
            self.emit_for_storage(&storage, SessionMutationKind::Created)
                .await;
            self.wrap_storage(storage).await
        })
    }

    fn open(&self, id: &str) -> BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>> {
        let id = id.to_owned();
        Box::pin(async move {
            let storage = self.inner.open(&id).await?;
            self.wrap_storage(storage).await
        })
    }

    fn list(
        &self,
        opts: ListSessionOptions,
    ) -> BoxFuture<'_, Result<Vec<SessionMetadata>, SessionError>> {
        self.inner.list(opts)
    }

    fn delete(&self, id: &str) -> BoxFuture<'_, Result<(), SessionError>> {
        let id = id.to_owned();
        Box::pin(async move {
            let recall_revision = match self.inner.open(&id).await {
                Ok(storage) => storage
                    .metadata()
                    .await
                    .ok()
                    .map(|metadata| metadata.recall_revision),
                Err(_) => None,
            };
            self.inner.delete(&id).await?;
            self.observer
                .observe(SessionMutation {
                    session_id: id,
                    recall_revision,
                    kind: SessionMutationKind::Deleted,
                })
                .await;
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
            let storage = self.inner.fork(&source_id, from_entry, opts).await?;
            self.emit_for_storage(
                &storage,
                SessionMutationKind::Forked {
                    source_session_id: source_id,
                },
            )
            .await;
            self.wrap_storage(storage).await
        })
    }
}

struct ObservedSessionStorage {
    inner: Arc<dyn SessionStorage>,
    observer: Arc<dyn SessionMutationObserver>,
    session_id: String,
}

impl ObservedSessionStorage {
    async fn emit(&self, kind: SessionMutationKind) {
        let recall_revision = self
            .inner
            .metadata()
            .await
            .ok()
            .map(|metadata| metadata.recall_revision);
        self.observer
            .observe(SessionMutation {
                session_id: self.session_id.clone(),
                recall_revision,
                kind,
            })
            .await;
    }
}

impl SessionStorage for ObservedSessionStorage {
    fn metadata(&self) -> BoxFuture<'_, Result<SessionMetadata, SessionError>> {
        self.inner.metadata()
    }

    fn create_entry_id(&self) -> EntryId {
        self.inner.create_entry_id()
    }

    fn append_entry(&self, entry: SessionEntry) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            let entry_id = entry.id;
            self.inner.append_entry(entry).await?;
            self.emit(SessionMutationKind::EntryAppended { entry_id })
                .await;
            Ok(())
        })
    }

    fn get_entry(&self, id: EntryId) -> BoxFuture<'_, Result<Option<SessionEntry>, SessionError>> {
        self.inner.get_entry(id)
    }

    fn children(&self, parent: EntryId) -> BoxFuture<'_, Result<Vec<SessionEntry>, SessionError>> {
        self.inner.children(parent)
    }

    fn all_leaves(&self) -> BoxFuture<'_, Result<Vec<EntryId>, SessionError>> {
        self.inner.all_leaves()
    }

    fn active_cursor(&self) -> BoxFuture<'_, Result<Option<EntryId>, SessionError>> {
        self.inner.active_cursor()
    }

    fn set_active_cursor(&self, id: EntryId) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            self.inner.set_active_cursor(id).await?;
            self.emit(SessionMutationKind::ActiveBranchChanged { entry_id: id })
                .await;
            Ok(())
        })
    }

    fn path_to_root(
        &self,
        target: EntryId,
    ) -> BoxFuture<'_, Result<Vec<SessionEntry>, SessionError>> {
        self.inner.path_to_root(target)
    }

    fn common_ancestor(
        &self,
        a: EntryId,
        b: EntryId,
    ) -> BoxFuture<'_, Result<Option<EntryId>, SessionError>> {
        self.inner.common_ancestor(a, b)
    }

    fn label_at(&self, id: EntryId) -> BoxFuture<'_, Result<Option<String>, SessionError>> {
        self.inner.label_at(id)
    }

    fn paths_to_all_leaves(&self) -> BoxFuture<'_, Result<Vec<Vec<SessionEntry>>, SessionError>> {
        self.inner.paths_to_all_leaves()
    }

    fn find_entries_by_type(
        &self,
        kind: SessionEntryKind,
    ) -> BoxFuture<'_, Result<Vec<EntryId>, SessionError>> {
        self.inner.find_entries_by_type(kind)
    }

    fn update_metadata_name(
        &self,
        name: Option<String>,
    ) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            self.inner.update_metadata_name(name).await?;
            self.emit(SessionMutationKind::MetadataChanged).await;
            Ok(())
        })
    }

    fn update_metadata_model(
        &self,
        model: Option<String>,
    ) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            self.inner.update_metadata_model(model).await?;
            self.emit(SessionMutationKind::MetadataChanged).await;
            Ok(())
        })
    }

    fn update_recall_settings(
        &self,
        settings: SessionRecallSettings,
    ) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            self.inner.update_recall_settings(settings).await?;
            self.emit(SessionMutationKind::RecallSettingsChanged).await;
            Ok(())
        })
    }

    fn delete_entries(&self, ids: Vec<EntryId>) -> BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            let event_ids = ids.clone();
            self.inner.delete_entries(ids).await?;
            self.emit(SessionMutationKind::EntriesDeleted {
                entry_ids: event_ids,
            })
            .await;
            Ok(())
        })
    }

    fn all_entries(&self) -> BoxFuture<'_, Result<Vec<SessionEntry>, SessionError>> {
        self.inner.all_entries()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::Utc;

    use super::*;
    use crate::session::{InMemorySessionRepo, SessionEntryPayload, SessionRecallPolicy};

    #[derive(Default)]
    struct RecordingObserver {
        events: Mutex<Vec<SessionMutation>>,
    }

    impl SessionMutationObserver for RecordingObserver {
        fn observe<'a>(&'a self, mutation: SessionMutation) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                self.events.lock().unwrap().push(mutation);
            })
        }
    }

    #[tokio::test]
    async fn observed_repo_emits_reference_only_mutations() {
        let observer = Arc::new(RecordingObserver::default());
        let repo = ObservedSessionRepo::new(Arc::new(InMemorySessionRepo::new()), observer.clone());
        let storage = repo.create(CreateSessionOptions::default()).await.unwrap();
        let entry_id = storage.create_entry_id();
        storage
            .append_entry(SessionEntry {
                id: entry_id,
                parent_id: None,
                timestamp: Utc::now(),
                payload: SessionEntryPayload::SessionInfo {
                    name: "body-must-not-enter-event".into(),
                },
                compaction_policy: None,
            })
            .await
            .unwrap();
        storage.set_active_cursor(entry_id).await.unwrap();

        let events = observer.events.lock().unwrap().clone();
        assert!(matches!(events[0].kind, SessionMutationKind::Created));
        assert!(matches!(
            events[1].kind,
            SessionMutationKind::EntryAppended { entry_id: id } if id == entry_id
        ));
        assert!(matches!(
            events[2].kind,
            SessionMutationKind::ActiveBranchChanged { entry_id: id } if id == entry_id
        ));
        let serialized = format!("{events:?}");
        assert!(!serialized.contains("body-must-not-enter-event"));
    }

    #[tokio::test]
    async fn no_wrapper_means_no_observer_activity() {
        let observer = Arc::new(RecordingObserver::default());
        let repo = InMemorySessionRepo::new();
        let storage = repo.create(CreateSessionOptions::default()).await.unwrap();
        let id = storage.create_entry_id();
        storage
            .append_entry(SessionEntry {
                id,
                parent_id: None,
                timestamp: Utc::now(),
                payload: SessionEntryPayload::Unknown,
                compaction_policy: None,
            })
            .await
            .unwrap();
        assert!(observer.events.lock().unwrap().is_empty());
        assert_eq!(
            storage.metadata().await.unwrap().recall_policy,
            SessionRecallPolicy::Disabled
        );
    }
}
