use std::{collections::BTreeSet, sync::Arc};

use futures::future::BoxFuture;
use llm_harness_agent::session::{
    ListSessionOptions, SessionEntry, SessionEntryPayload, SessionMutation, SessionMutationKind,
    SessionMutationObserver, SessionRepo, SessionStorage,
};
use llm_harness_types::{AgentMessage, AssistantMessageKind, ContentBlock, SessionError};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{
    SessionRecallDocument, SessionRecallError, SessionRecallIndex, SessionRecallProjection,
    SessionRecallRef,
};

enum ProjectionSnapshot {
    Eligible(SessionRecallProjection),
    Ineligible { session_id: String, revision: u64 },
}

/// Builds and reconciles a disposable recall projection from Session authority.
pub struct SessionRecallProjector {
    repo: Arc<dyn SessionRepo>,
    index: Arc<dyn SessionRecallIndex>,
    operations: Mutex<()>,
}

impl SessionRecallProjector {
    pub fn new(repo: Arc<dyn SessionRepo>, index: Arc<dyn SessionRecallIndex>) -> Self {
        Self {
            repo,
            index,
            operations: Mutex::new(()),
        }
    }

    pub fn index(&self) -> &Arc<dyn SessionRecallIndex> {
        &self.index
    }

    pub async fn project_session(
        &self,
        session_id: &str,
        abort: CancellationToken,
    ) -> Result<(), SessionRecallError> {
        let _guard = self.operations.lock().await;
        self.project_session_locked(session_id, 0, abort).await
    }

    pub async fn reconcile(&self, abort: CancellationToken) -> Result<(), SessionRecallError> {
        let _guard = self.operations.lock().await;
        check_abort(&abort)?;
        let metadata = self
            .repo
            .list(ListSessionOptions::default())
            .await
            .map_err(map_session_error)?;
        let authoritative_ids = metadata
            .iter()
            .map(|metadata| metadata.id.clone())
            .collect::<BTreeSet<_>>();
        let projected = self.index.projected_revisions().await?;
        for (session_id, revision) in projected {
            check_abort(&abort)?;
            if !authoritative_ids.contains(&session_id) {
                self.index
                    .remove_session(session_id, revision, true)
                    .await?;
            }
        }
        for metadata in metadata {
            check_abort(&abort)?;
            self.project_session_locked(&metadata.id, metadata.recall_revision, abort.clone())
                .await?;
        }
        Ok(())
    }

    /// Build a fresh generation and atomically switch only after all reads succeed.
    pub async fn rebuild(&self, abort: CancellationToken) -> Result<(), SessionRecallError> {
        let _guard = self.operations.lock().await;
        check_abort(&abort)?;
        let metadata = self
            .repo
            .list(ListSessionOptions::default())
            .await
            .map_err(map_session_error)?;
        let mut projections = Vec::new();
        for metadata in metadata {
            check_abort(&abort)?;
            let storage = match self.repo.open(&metadata.id).await {
                Ok(storage) => storage,
                Err(SessionError::SessionNotFound(_)) => continue,
                Err(error) => return Err(map_session_error(error)),
            };
            if let ProjectionSnapshot::Eligible(projection) =
                snapshot(storage.as_ref(), &abort).await?
            {
                projections.push(projection);
            }
        }
        check_abort(&abort)?;
        self.index.replace_all(projections).await
    }

    async fn project_session_locked(
        &self,
        session_id: &str,
        revision_hint: u64,
        abort: CancellationToken,
    ) -> Result<(), SessionRecallError> {
        check_abort(&abort)?;
        if revision_hint > 0
            && self
                .index
                .projected_revisions()
                .await?
                .get(session_id)
                .is_some_and(|projected| *projected >= revision_hint)
        {
            return Ok(());
        }
        let storage = match self.repo.open(session_id).await {
            Ok(storage) => storage,
            Err(SessionError::SessionNotFound(_)) => {
                self.index
                    .remove_session(session_id.to_string(), revision_hint, true)
                    .await?;
                return Ok(());
            }
            Err(error) => return Err(map_session_error(error)),
        };
        match snapshot(storage.as_ref(), &abort).await? {
            ProjectionSnapshot::Eligible(projection) => {
                self.index.replace_session(projection).await?;
            }
            ProjectionSnapshot::Ineligible {
                session_id,
                revision,
            } => {
                self.index
                    .remove_session(session_id, revision, false)
                    .await?;
            }
        }
        Ok(())
    }
}

impl SessionMutationObserver for SessionRecallProjector {
    fn observe<'a>(&'a self, mutation: SessionMutation) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let _guard = self.operations.lock().await;
            let revision = mutation.recall_revision.unwrap_or(0);
            let session_id = mutation.session_id.clone();
            let result = if matches!(mutation.kind, SessionMutationKind::Deleted) {
                self.index
                    .remove_session(mutation.session_id, revision, true)
                    .await
                    .map(|_| ())
            } else {
                self.project_session_locked(
                    &mutation.session_id,
                    revision,
                    CancellationToken::new(),
                )
                .await
            };
            if let Err(error) = result {
                tracing::warn!(
                    session_id,
                    revision,
                    error = ?error,
                    "session recall projection update failed; reconciliation is required"
                );
            }
        })
    }
}

async fn snapshot(
    storage: &dyn SessionStorage,
    abort: &CancellationToken,
) -> Result<ProjectionSnapshot, SessionRecallError> {
    for _ in 0..3 {
        check_abort(abort)?;
        let before = storage.metadata().await.map_err(map_session_error)?;
        if !before.is_recallable() {
            return Ok(ProjectionSnapshot::Ineligible {
                session_id: before.id,
                revision: before.recall_revision,
            });
        }
        let Some(leaf) = before.active_cursor else {
            return Ok(ProjectionSnapshot::Eligible(SessionRecallProjection {
                session_id: before.id,
                revision: before.recall_revision,
                scope: before
                    .recall_scope
                    .expect("recallable metadata has a scope"),
                documents: vec![],
            }));
        };
        let path = storage
            .path_to_root(leaf)
            .await
            .map_err(map_session_error)?;
        check_abort(abort)?;
        let after = storage.metadata().await.map_err(map_session_error)?;
        if before.recall_revision != after.recall_revision
            || before.active_cursor != after.active_cursor
        {
            continue;
        }
        let scope = before
            .recall_scope
            .clone()
            .ok_or(SessionRecallError::ProjectionFailed)?;
        return Ok(ProjectionSnapshot::Eligible(SessionRecallProjection {
            session_id: before.id.clone(),
            revision: before.recall_revision,
            scope,
            documents: extract_turns(
                &before.id,
                before.name.as_deref(),
                leaf,
                before.recall_revision,
                &path,
            ),
        }));
    }
    Err(SessionRecallError::ProjectionFailed)
}

pub(crate) fn extract_turns(
    session_id: &str,
    title: Option<&str>,
    branch_leaf_id: llm_harness_types::EntryId,
    revision: u64,
    path: &[SessionEntry],
) -> Vec<SessionRecallDocument> {
    struct Pending {
        entry_id: llm_harness_types::EntryId,
        text: String,
        updated_at: chrono::DateTime<chrono::Utc>,
    }

    fn finish(
        documents: &mut Vec<SessionRecallDocument>,
        pending: Pending,
        assistant: Option<(
            llm_harness_types::EntryId,
            String,
            chrono::DateTime<chrono::Utc>,
        )>,
        session_id: &str,
        title: Option<&str>,
        branch_leaf_id: llm_harness_types::EntryId,
        revision: u64,
    ) {
        if pending.text.trim().is_empty() {
            return;
        }
        let (assistant_entry_id, assistant_text, updated_at) = match assistant {
            Some((id, text, timestamp)) => (
                Some(id),
                (!text.trim().is_empty()).then_some(text),
                timestamp,
            ),
            None => (None, None, pending.updated_at),
        };
        documents.push(SessionRecallDocument {
            reference: SessionRecallRef {
                session_id: session_id.to_string(),
                user_entry_id: pending.entry_id,
                assistant_entry_id,
                branch_leaf_id,
                revision,
            },
            title: title.map(str::to_string),
            user_text: pending.text,
            assistant_text,
            updated_at,
        });
    }

    let mut documents = Vec::new();
    let mut pending: Option<Pending> = None;
    for entry in path {
        let SessionEntryPayload::Message(message) = &entry.payload else {
            continue;
        };
        match message {
            AgentMessage::User(user) => {
                if let Some(previous) = pending.take() {
                    finish(
                        &mut documents,
                        previous,
                        None,
                        session_id,
                        title,
                        branch_leaf_id,
                        revision,
                    );
                }
                pending = Some(Pending {
                    entry_id: entry.id,
                    text: text_blocks(&user.content),
                    updated_at: user.timestamp,
                });
            }
            AgentMessage::Assistant(assistant)
                if assistant.kind == AssistantMessageKind::FinalAnswer =>
            {
                if let Some(user) = pending.take() {
                    finish(
                        &mut documents,
                        user,
                        Some((entry.id, assistant.text_content(), assistant.timestamp)),
                        session_id,
                        title,
                        branch_leaf_id,
                        revision,
                    );
                }
            }
            _ => {}
        }
    }
    if let Some(user) = pending {
        finish(
            &mut documents,
            user,
            None,
            session_id,
            title,
            branch_leaf_id,
            revision,
        );
    }
    documents
}

fn text_blocks(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn check_abort(abort: &CancellationToken) -> Result<(), SessionRecallError> {
    if abort.is_cancelled() {
        Err(SessionRecallError::Aborted)
    } else {
        Ok(())
    }
}

fn map_session_error(error: SessionError) -> SessionRecallError {
    match error {
        SessionError::SessionNotFound(_) | SessionError::EntryNotFound(_) => {
            SessionRecallError::AuthorityUnavailable
        }
        _ => SessionRecallError::ProjectionFailed,
    }
}
