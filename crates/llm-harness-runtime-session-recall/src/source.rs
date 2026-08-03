use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use chrono::Utc;
use futures::future::BoxFuture;
use llm_harness_agent::{SessionRecallScope, SessionRepo};
use llm_harness_runtime_knowledge::{
    ContentSelector, FreshnessClass, KnowledgeAccessContext, KnowledgeAccessControl,
    KnowledgeAction, KnowledgeCapability, KnowledgeContent, KnowledgeError, KnowledgeHit,
    KnowledgeReadRequest, KnowledgeRef, KnowledgeRequestContext, KnowledgeResourceRef,
    KnowledgeSearchProjection, KnowledgeSource, KnowledgeSourceDescriptor, SourceSearchPage,
    SourceSearchRequest,
};
use llm_harness_types::{DataBlock, SessionError};
use tokio_util::sync::CancellationToken;

use crate::{
    HistoryRecallRequest, SESSION_RECALL_SOURCE_ID, SessionRecallCandidate, SessionRecallDocument,
    SessionRecallError, SessionRecallIndex, SessionRecallIndexQuery, SessionRecallRef,
    SessionRecallSearchRequest, truncate_utf8,
};

/// Search/read facade that revalidates every candidate against Session authority.
pub struct SessionRecallService {
    repo: Arc<dyn SessionRepo>,
    index: Arc<dyn SessionRecallIndex>,
    access_control: Arc<KnowledgeAccessControl>,
}

impl SessionRecallService {
    pub fn new(
        repo: Arc<dyn SessionRepo>,
        index: Arc<dyn SessionRecallIndex>,
        access_control: Arc<KnowledgeAccessControl>,
    ) -> Self {
        Self {
            repo,
            index,
            access_control,
        }
    }

    pub async fn search(
        &self,
        access: &KnowledgeAccessContext,
        request: SessionRecallSearchRequest,
        abort: CancellationToken,
    ) -> Result<Vec<SessionRecallCandidate>, KnowledgeError> {
        check_abort(&abort)?;
        if request.scope != scope_from_access(access) {
            return Err(KnowledgeError::Unauthorized);
        }
        self.authorize_source(access, KnowledgeAction::Search)
            .await?;
        if request.limit == 0 {
            return Ok(vec![]);
        }
        let bounded_limit = request.limit.min(100);
        let candidate_limit = bounded_limit.saturating_mul(4).min(100);
        let indexed = self
            .index
            .search(
                SessionRecallIndexQuery {
                    query: request.query,
                    scope: request.scope,
                    exclude_session_id: request.exclude_session_id,
                    limit: candidate_limit,
                    max_snippet_bytes: request.max_snippet_bytes,
                },
                abort.clone(),
            )
            .await
            .map_err(map_recall_error)?;
        let mut validated = Vec::new();
        for candidate in indexed {
            check_abort(&abort)?;
            match self
                .authoritative_document(access, &candidate.reference, abort.clone())
                .await
            {
                Ok(document) => {
                    let (snippet, _) =
                        truncate_utf8(&document.formatted_content(), request.max_snippet_bytes);
                    validated.push(SessionRecallCandidate {
                        reference: document.reference,
                        title: document.title,
                        snippet,
                        score: candidate.score,
                        updated_at: document.updated_at,
                    });
                    if validated.len() == request.limit {
                        break;
                    }
                }
                Err(KnowledgeError::Unauthorized)
                | Err(KnowledgeError::NotFound)
                | Err(KnowledgeError::StaleReference { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(validated)
    }

    pub async fn read(
        &self,
        access: &KnowledgeAccessContext,
        reference: &SessionRecallRef,
        max_bytes: usize,
        abort: CancellationToken,
    ) -> Result<(SessionRecallDocument, String, bool), KnowledgeError> {
        if max_bytes == 0 {
            return Err(KnowledgeError::InvalidFilter(
                "max_bytes must be greater than zero".into(),
            ));
        }
        let document = self
            .authoritative_document(access, reference, abort)
            .await?;
        let (content, truncated) = truncate_utf8(&document.formatted_content(), max_bytes);
        Ok((document, content, truncated))
    }

    async fn authoritative_document(
        &self,
        access: &KnowledgeAccessContext,
        reference: &SessionRecallRef,
        abort: CancellationToken,
    ) -> Result<SessionRecallDocument, KnowledgeError> {
        check_abort(&abort)?;
        let knowledge_ref = to_knowledge_ref(reference)?;
        self.access_control
            .authorize(
                access,
                KnowledgeAction::Read,
                KnowledgeResourceRef::Item(&knowledge_ref),
            )
            .await?;
        let storage = self
            .repo
            .open(&reference.session_id)
            .await
            .map_err(map_authority_error)?;
        let before = storage.metadata().await.map_err(map_authority_error)?;
        let expected_scope = scope_from_access(access);
        if before.recall_scope.as_ref() != Some(&expected_scope) {
            return Err(KnowledgeError::Unauthorized);
        }
        if !before.is_recallable() {
            return Err(KnowledgeError::NotFound);
        }
        if before.recall_revision != reference.revision
            || before.active_cursor != Some(reference.branch_leaf_id)
        {
            return Err(KnowledgeError::StaleReference { latest: None });
        }
        let path = storage
            .path_to_root(reference.branch_leaf_id)
            .await
            .map_err(map_authority_error)?;
        check_abort(&abort)?;
        let after = storage.metadata().await.map_err(map_authority_error)?;
        if before.recall_revision != after.recall_revision
            || before.active_cursor != after.active_cursor
        {
            return Err(KnowledgeError::StaleReference { latest: None });
        }
        super::projector::extract_turns(
            &before.id,
            before.name.as_deref(),
            reference.branch_leaf_id,
            before.recall_revision,
            &path,
        )
        .into_iter()
        .find(|document| &document.reference == reference)
        .ok_or(KnowledgeError::StaleReference { latest: None })
    }

    async fn authorize_source(
        &self,
        access: &KnowledgeAccessContext,
        action: KnowledgeAction,
    ) -> Result<(), KnowledgeError> {
        self.access_control
            .authorize(
                access,
                action,
                KnowledgeResourceRef::Source {
                    source_id: SESSION_RECALL_SOURCE_ID,
                    domains: &["session_history".to_string()],
                },
            )
            .await
    }
}

pub struct SessionRecallKnowledgeSource {
    service: Arc<SessionRecallService>,
    descriptor: KnowledgeSourceDescriptor,
    max_snippet_bytes: usize,
}

impl SessionRecallKnowledgeSource {
    pub fn new(service: Arc<SessionRecallService>) -> Self {
        Self {
            service,
            descriptor: KnowledgeSourceDescriptor {
                id: SESSION_RECALL_SOURCE_ID.into(),
                name: "Session history".into(),
                description: "Authorized, active-branch conversational history".into(),
                domains: vec!["session_history".into()],
                capabilities: BTreeSet::from([
                    KnowledgeCapability::Search,
                    KnowledgeCapability::Read,
                    KnowledgeCapability::Revisioned,
                ]),
                freshness: FreshnessClass::NearRealTime,
                filter_fields: vec![],
            },
            max_snippet_bytes: 1024,
        }
    }

    pub fn with_max_snippet_bytes(mut self, max_snippet_bytes: usize) -> Self {
        self.max_snippet_bytes = max_snippet_bytes.max(1);
        self
    }

    pub fn service(&self) -> &Arc<SessionRecallService> {
        &self.service
    }
}

impl KnowledgeSource for SessionRecallKnowledgeSource {
    fn descriptor(&self) -> &KnowledgeSourceDescriptor {
        &self.descriptor
    }

    fn search_projection(&self) -> KnowledgeSearchProjection {
        KnowledgeSearchProjection::ReferenceOnly
    }

    fn search<'a>(
        &'a self,
        ctx: KnowledgeRequestContext<'a>,
        request: SourceSearchRequest,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<SourceSearchPage, KnowledgeError>> {
        Box::pin(async move {
            check_abort(&abort)?;
            if request.cursor.is_some() {
                return Err(KnowledgeError::InvalidCursor);
            }
            if !request.filters.is_empty() {
                return Err(KnowledgeError::InvalidFilter(
                    "session recall does not expose model-controlled filters".into(),
                ));
            }
            let current_session_id = ctx
                .run
                .extension::<HistoryRecallRequest>()
                .map(|request| request.current_session_id.clone());
            let candidates = self
                .service
                .search(
                    ctx.access,
                    SessionRecallSearchRequest {
                        query: request.query,
                        scope: scope_from_access(ctx.access),
                        exclude_session_id: current_session_id,
                        limit: request.limit,
                        max_snippet_bytes: self.max_snippet_bytes,
                    },
                    abort,
                )
                .await?;
            Ok(SourceSearchPage {
                hits: candidates
                    .into_iter()
                    .map(|candidate| KnowledgeHit {
                        reference: to_knowledge_ref(&candidate.reference)
                            .expect("typed recall references are serializable"),
                        title: candidate.title,
                        snippet: candidate.snippet,
                        suggested_selectors: vec![ContentSelector::Document],
                        uri: None,
                        score: Some(candidate.score),
                        updated_at: Some(candidate.updated_at),
                        metadata: reference_metadata(&candidate.reference),
                    })
                    .collect(),
                next_cursor: None,
            })
        })
    }

    fn read<'a>(
        &'a self,
        ctx: KnowledgeRequestContext<'a>,
        request: KnowledgeReadRequest,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<KnowledgeContent, KnowledgeError>> {
        Box::pin(async move {
            if request.reference.source_id != SESSION_RECALL_SOURCE_ID {
                return Err(KnowledgeError::NotFound);
            }
            if request.selector != ContentSelector::Document {
                return Err(KnowledgeError::UnsupportedCapability(
                    "session recall supports document reads only".into(),
                ));
            }
            let reference = from_knowledge_ref(&request.reference)?;
            let (document, content, truncated) = self
                .service
                .read(ctx.access, &reference, request.max_bytes, abort)
                .await?;
            Ok(KnowledgeContent {
                reference: request.reference,
                selector: ContentSelector::Document,
                title: document.title,
                blocks: vec![DataBlock::text(content)],
                uri: None,
                updated_at: Some(document.updated_at),
                obtained_at: Utc::now(),
                truncated,
                metadata: reference_metadata(&document.reference),
            })
        })
    }
}

pub fn to_knowledge_ref(reference: &SessionRecallRef) -> Result<KnowledgeRef, KnowledgeError> {
    Ok(KnowledgeRef {
        source_id: SESSION_RECALL_SOURCE_ID.into(),
        item_id: reference.encode().map_err(|_| KnowledgeError::NotFound)?,
        revision: Some(reference.revision.to_string()),
    })
}

pub fn from_knowledge_ref(reference: &KnowledgeRef) -> Result<SessionRecallRef, KnowledgeError> {
    if reference.source_id != SESSION_RECALL_SOURCE_ID {
        return Err(KnowledgeError::NotFound);
    }
    let decoded =
        SessionRecallRef::decode(&reference.item_id).map_err(|_| KnowledgeError::NotFound)?;
    if reference.revision.as_deref() != Some(decoded.revision.to_string().as_str()) {
        return Err(KnowledgeError::StaleReference { latest: None });
    }
    Ok(decoded)
}

fn scope_from_access(access: &KnowledgeAccessContext) -> SessionRecallScope {
    SessionRecallScope {
        namespace: access.scope.namespace.clone(),
        tenant: access.scope.tenant.clone(),
        project: access.scope.project.clone(),
        attributes: access.scope.attributes.clone(),
    }
}

fn reference_metadata(reference: &SessionRecallRef) -> BTreeMap<String, serde_json::Value> {
    BTreeMap::from([
        ("session_id".into(), reference.session_id.clone().into()),
        (
            "user_entry_id".into(),
            reference.user_entry_id.to_string().into(),
        ),
        (
            "assistant_entry_id".into(),
            reference
                .assistant_entry_id
                .map(|id| serde_json::Value::String(id.to_string()))
                .unwrap_or(serde_json::Value::Null),
        ),
        (
            "branch_leaf_id".into(),
            reference.branch_leaf_id.to_string().into(),
        ),
        ("recall_revision".into(), reference.revision.into()),
    ])
}

fn check_abort(abort: &CancellationToken) -> Result<(), KnowledgeError> {
    if abort.is_cancelled() {
        Err(KnowledgeError::Aborted)
    } else {
        Ok(())
    }
}

fn map_recall_error(error: SessionRecallError) -> KnowledgeError {
    match error {
        SessionRecallError::Aborted => KnowledgeError::Aborted,
        _ => KnowledgeError::Backend("session recall projection unavailable".into()),
    }
}

fn map_authority_error(error: SessionError) -> KnowledgeError {
    match error {
        SessionError::SessionNotFound(_) | SessionError::EntryNotFound(_) => {
            KnowledgeError::NotFound
        }
        _ => KnowledgeError::Backend("session authority unavailable".into()),
    }
}
