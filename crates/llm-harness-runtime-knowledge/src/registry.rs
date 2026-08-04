use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};

use llm_harness_types::RunContext;
use tokio_util::sync::CancellationToken;

use crate::{
    FederatedKnowledgePage, KnowledgeAccessContext, KnowledgeAccessControl, KnowledgeAction,
    KnowledgeError, KnowledgeReadRequest, KnowledgeRequestContext, KnowledgeResourceRef,
    KnowledgeSearchProjection, KnowledgeSearchRequest, KnowledgeSource, KnowledgeSourceDescriptor,
    SourceSearchRequest,
};

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum KnowledgeRegistryBuildError {
    #[error("knowledge source ID must not be empty")]
    EmptySourceId,
    #[error("duplicate knowledge source ID: {0}")]
    DuplicateSourceId(String),
    #[error("knowledge registry requires at least one source")]
    NoSources,
}

pub struct KnowledgeRegistryBuilder {
    access_control: Arc<KnowledgeAccessControl>,
    sources: Vec<Arc<dyn KnowledgeSource>>,
}

impl KnowledgeRegistryBuilder {
    pub fn source(mut self, source: Arc<dyn KnowledgeSource>) -> Self {
        self.sources.push(source);
        self
    }

    pub fn build(self) -> Result<KnowledgeRegistry, KnowledgeRegistryBuildError> {
        if self.sources.is_empty() {
            return Err(KnowledgeRegistryBuildError::NoSources);
        }
        let mut sources = BTreeMap::new();
        for source in self.sources {
            let id = source.descriptor().id.clone();
            if id.is_empty() {
                return Err(KnowledgeRegistryBuildError::EmptySourceId);
            }
            if sources.insert(id.clone(), source).is_some() {
                return Err(KnowledgeRegistryBuildError::DuplicateSourceId(id));
            }
        }
        Ok(KnowledgeRegistry {
            access_control: self.access_control,
            sources,
        })
    }
}

pub struct KnowledgeRegistry {
    access_control: Arc<KnowledgeAccessControl>,
    sources: BTreeMap<String, Arc<dyn KnowledgeSource>>,
}

impl KnowledgeRegistry {
    pub fn builder(access_control: Arc<KnowledgeAccessControl>) -> KnowledgeRegistryBuilder {
        KnowledgeRegistryBuilder {
            access_control,
            sources: vec![],
        }
    }

    pub(crate) fn source_descriptor(&self, source_id: &str) -> Option<&KnowledgeSourceDescriptor> {
        self.sources
            .get(source_id)
            .map(|source| source.descriptor())
    }

    pub(crate) fn source_descriptors(&self) -> impl Iterator<Item = &KnowledgeSourceDescriptor> {
        self.sources.values().map(|source| source.descriptor())
    }

    pub async fn visible_sources(
        &self,
        access: &KnowledgeAccessContext,
    ) -> Result<Vec<KnowledgeSourceDescriptor>, KnowledgeError> {
        let mut visible = Vec::new();
        for source in self.sources.values() {
            let descriptor = source.descriptor();
            match self
                .access_control
                .authorize(
                    access,
                    KnowledgeAction::Discover,
                    KnowledgeResourceRef::Source {
                        source_id: &descriptor.id,
                        domains: &descriptor.domains,
                    },
                )
                .await
            {
                Ok(()) => visible.push(descriptor.clone()),
                Err(KnowledgeError::Unauthorized) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(visible)
    }

    pub async fn search(
        &self,
        run: &RunContext,
        access: &KnowledgeAccessContext,
        request: KnowledgeSearchRequest,
        abort: CancellationToken,
    ) -> Result<FederatedKnowledgePage, KnowledgeError> {
        if request.cursor.is_some() {
            return Err(KnowledgeError::InvalidCursor);
        }
        let explicit_source_ids = request.source_ids.iter().cloned().collect::<BTreeSet<_>>();
        if explicit_source_ids.len() != request.source_ids.len() {
            return Err(KnowledgeError::InvalidFilter(
                "source selection contains duplicate IDs".into(),
            ));
        }
        let strict_single_source = request.source_ids.len() == 1;
        let candidates = if !explicit_source_ids.is_empty() {
            let mut selected = Vec::with_capacity(explicit_source_ids.len());
            for source_id in explicit_source_ids {
                let source = self
                    .sources
                    .get(&source_id)
                    .ok_or(KnowledgeError::NotFound)?;
                let descriptor = source.descriptor();
                self.access_control
                    .authorize(
                        access,
                        KnowledgeAction::Discover,
                        KnowledgeResourceRef::Source {
                            source_id: &descriptor.id,
                            domains: &descriptor.domains,
                        },
                    )
                    .await?;
                if !request.domains.is_empty()
                    && !descriptor
                        .domains
                        .iter()
                        .any(|domain| request.domains.contains(domain))
                {
                    return Err(KnowledgeError::NotFound);
                }
                selected.push(descriptor.clone());
            }
            selected
        } else {
            self.visible_sources(access)
                .await?
                .into_iter()
                .filter(|descriptor| {
                    request.domains.is_empty()
                        || descriptor
                            .domains
                            .iter()
                            .any(|domain| request.domains.contains(domain))
                })
                .collect::<Vec<_>>()
        };
        let candidate_ids = candidates
            .iter()
            .map(|descriptor| descriptor.id.as_str())
            .collect::<BTreeSet<_>>();
        if request
            .filters
            .iter()
            .any(|filter| !candidate_ids.contains(filter.source_id.as_str()))
        {
            return Err(KnowledgeError::InvalidFilter(
                "filter source does not match a selected source".into(),
            ));
        }

        let mut source_pages = Vec::new();
        let mut source_failures = Vec::new();
        let mut search_projection = KnowledgeSearchProjection::Projected;
        for descriptor in candidates {
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }
            let source = self
                .sources
                .get(&descriptor.id)
                .ok_or(KnowledgeError::NotFound)?;
            if source.search_projection() == KnowledgeSearchProjection::ReferenceOnly {
                search_projection = KnowledgeSearchProjection::ReferenceOnly;
            }
            if let Err(error) = self
                .access_control
                .authorize(
                    access,
                    KnowledgeAction::Search,
                    KnowledgeResourceRef::Source {
                        source_id: &descriptor.id,
                        domains: &descriptor.domains,
                    },
                )
                .await
            {
                if strict_single_source || error == KnowledgeError::Aborted {
                    return Err(error);
                }
                if error != KnowledgeError::Unauthorized {
                    source_failures.push(error.source_failure(descriptor.id));
                }
                continue;
            }
            let filters = request
                .filters
                .iter()
                .filter(|filter| filter.source_id == descriptor.id)
                .map(|filter| filter.expression.clone())
                .collect();
            let result = source
                .search(
                    KnowledgeRequestContext { run, access },
                    SourceSearchRequest {
                        query: request.query.clone(),
                        filters,
                        limit: request.limit,
                        cursor: None,
                    },
                    abort.clone(),
                )
                .await;
            match result {
                Ok(page)
                    if page
                        .hits
                        .iter()
                        .all(|hit| hit.reference.source_id == descriptor.id) =>
                {
                    source_pages.push(VecDeque::from(page.hits));
                }
                Ok(_) => {
                    let error = KnowledgeError::Backend(
                        "knowledge source returned a reference in another source namespace".into(),
                    );
                    if strict_single_source {
                        return Err(error);
                    }
                    source_failures.push(error.source_failure(descriptor.id));
                }
                Err(error) if strict_single_source || error == KnowledgeError::Aborted => {
                    return Err(error);
                }
                Err(KnowledgeError::Unauthorized) => {}
                Err(error) => source_failures.push(error.source_failure(descriptor.id)),
            }
        }

        let hits = merge_federated_hits(source_pages, request.limit);
        Ok(FederatedKnowledgePage {
            hits,
            next_cursor: None,
            source_failures,
            search_projection,
        })
    }

    pub async fn read(
        &self,
        run: &RunContext,
        access: &KnowledgeAccessContext,
        request: KnowledgeReadRequest,
        abort: CancellationToken,
    ) -> Result<crate::KnowledgeContent, KnowledgeError> {
        let source = self
            .sources
            .get(&request.reference.source_id)
            .ok_or(KnowledgeError::NotFound)?;
        let expected_source_id = source.descriptor().id.clone();
        self.access_control
            .authorize(
                access,
                KnowledgeAction::Read,
                KnowledgeResourceRef::Item(&request.reference),
            )
            .await?;
        let content = source
            .read(KnowledgeRequestContext { run, access }, request, abort)
            .await?;
        if content.reference.source_id != expected_source_id {
            return Err(KnowledgeError::Backend(
                "knowledge source returned a reference in another source namespace".into(),
            ));
        }
        Ok(content)
    }
}

fn merge_federated_hits(
    mut source_pages: Vec<VecDeque<crate::KnowledgeHit>>,
    limit: usize,
) -> Vec<crate::KnowledgeHit> {
    let mut hits = Vec::with_capacity(limit);
    let mut seen = BTreeSet::new();
    while hits.len() < limit {
        let mut progressed = false;
        for page in &mut source_pages {
            let Some(hit) = page.pop_front() else {
                continue;
            };
            progressed = true;
            if seen.insert(hit.reference.clone()) {
                hits.push(hit);
                if hits.len() == limit {
                    break;
                }
            }
        }
        if !progressed {
            break;
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::{
            OnceLock,
            atomic::{AtomicBool, Ordering},
        },
    };

    use futures::future::BoxFuture;
    use llm_harness_types::{RunContext, RunRequest};

    use super::*;
    use crate::{
        AllowAllAuthorizer, ContentSelector, DenyAllAuthorizer, FreshnessClass,
        KnowledgeCapability, KnowledgeContent, KnowledgeHit, KnowledgeRef, KnowledgeScope,
        PrincipalRef, SourceSearchPage,
    };

    struct GuardedSource {
        called: AtomicBool,
    }

    struct ForeignReferenceSource;
    struct FailingSource;

    impl GuardedSource {
        fn new() -> Self {
            Self {
                called: AtomicBool::new(false),
            }
        }
    }

    impl KnowledgeSource for GuardedSource {
        fn descriptor(&self) -> &KnowledgeSourceDescriptor {
            static DESCRIPTOR: OnceLock<KnowledgeSourceDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| KnowledgeSourceDescriptor {
                id: "docs".into(),
                name: "Docs".into(),
                description: "Docs".into(),
                domains: vec![],
                capabilities: BTreeSet::from([
                    KnowledgeCapability::Search,
                    KnowledgeCapability::Read,
                ]),
                freshness: FreshnessClass::Static,
                filter_fields: vec![],
            })
        }

        fn search<'a>(
            &'a self,
            _ctx: KnowledgeRequestContext<'a>,
            _request: SourceSearchRequest,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<SourceSearchPage, KnowledgeError>> {
            self.called.store(true, Ordering::SeqCst);
            Box::pin(async {
                Ok(SourceSearchPage {
                    hits: Vec::<KnowledgeHit>::new(),
                    next_cursor: None,
                })
            })
        }

        fn read<'a>(
            &'a self,
            _ctx: KnowledgeRequestContext<'a>,
            _request: KnowledgeReadRequest,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<KnowledgeContent, KnowledgeError>> {
            self.called.store(true, Ordering::SeqCst);
            Box::pin(async { Err(KnowledgeError::NotFound) })
        }
    }

    impl KnowledgeSource for ForeignReferenceSource {
        fn descriptor(&self) -> &KnowledgeSourceDescriptor {
            static DESCRIPTOR: OnceLock<KnowledgeSourceDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| KnowledgeSourceDescriptor {
                id: "optional".into(),
                name: "Optional".into(),
                description: "Optional".into(),
                domains: vec![],
                capabilities: BTreeSet::from([
                    KnowledgeCapability::Search,
                    KnowledgeCapability::Read,
                ]),
                freshness: FreshnessClass::Static,
                filter_fields: vec![],
            })
        }

        fn search<'a>(
            &'a self,
            _ctx: KnowledgeRequestContext<'a>,
            _request: SourceSearchRequest,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<SourceSearchPage, KnowledgeError>> {
            Box::pin(async {
                Ok(SourceSearchPage {
                    hits: vec![],
                    next_cursor: None,
                })
            })
        }

        fn read<'a>(
            &'a self,
            _ctx: KnowledgeRequestContext<'a>,
            request: KnowledgeReadRequest,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<KnowledgeContent, KnowledgeError>> {
            Box::pin(async move {
                Ok(KnowledgeContent {
                    reference: KnowledgeRef {
                        source_id: "strict".into(),
                        ..request.reference
                    },
                    selector: request.selector,
                    title: None,
                    blocks: vec![],
                    uri: None,
                    updated_at: None,
                    obtained_at: chrono::Utc::now(),
                    truncated: false,
                    metadata: BTreeMap::new(),
                })
            })
        }
    }

    impl KnowledgeSource for FailingSource {
        fn descriptor(&self) -> &KnowledgeSourceDescriptor {
            static DESCRIPTOR: OnceLock<KnowledgeSourceDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| KnowledgeSourceDescriptor {
                id: "broken".into(),
                name: "Broken".into(),
                description: "Broken".into(),
                domains: vec![],
                capabilities: BTreeSet::from([KnowledgeCapability::Search]),
                freshness: FreshnessClass::Live,
                filter_fields: vec![],
            })
        }

        fn search<'a>(
            &'a self,
            _ctx: KnowledgeRequestContext<'a>,
            _request: SourceSearchRequest,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<SourceSearchPage, KnowledgeError>> {
            Box::pin(async { Err(KnowledgeError::Backend("private backend detail".into())) })
        }

        fn read<'a>(
            &'a self,
            _ctx: KnowledgeRequestContext<'a>,
            _request: KnowledgeReadRequest,
            _abort: CancellationToken,
        ) -> BoxFuture<'a, Result<KnowledgeContent, KnowledgeError>> {
            Box::pin(async { Err(KnowledgeError::NotFound) })
        }
    }

    fn access() -> KnowledgeAccessContext {
        KnowledgeAccessContext::new(
            KnowledgeScope::new("test"),
            PrincipalRef::new("user", "test"),
        )
    }

    #[test]
    fn duplicate_source_ids_are_rejected_at_build_time() {
        let control = Arc::new(KnowledgeAccessControl::default());
        let error = KnowledgeRegistry::builder(control)
            .source(Arc::new(GuardedSource::new()))
            .source(Arc::new(GuardedSource::new()))
            .build()
            .err()
            .expect("duplicate source IDs must fail");
        assert_eq!(
            error,
            KnowledgeRegistryBuildError::DuplicateSourceId("docs".into())
        );
    }

    #[tokio::test]
    async fn registry_denial_happens_before_source_execution() {
        let source = Arc::new(GuardedSource::new());
        let registry = KnowledgeRegistry::builder(Arc::new(KnowledgeAccessControl::new(Arc::new(
            DenyAllAuthorizer,
        ))))
        .source(source.clone())
        .build()
        .unwrap();
        let run = RunContext::new(RunRequest::from_text("question"));
        let error = registry
            .search(
                &run,
                &access(),
                KnowledgeSearchRequest {
                    query: "policy".into(),
                    domains: vec![],
                    source_ids: vec!["docs".into()],
                    filters: vec![],
                    limit: 5,
                    cursor: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error, KnowledgeError::Unauthorized);
        assert!(!source.called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn federated_search_sanitizes_partial_failures_but_explicit_search_stays_strict() {
        let registry = KnowledgeRegistry::builder(Arc::new(KnowledgeAccessControl::new(Arc::new(
            AllowAllAuthorizer,
        ))))
        .source(Arc::new(GuardedSource::new()))
        .source(Arc::new(FailingSource))
        .build()
        .unwrap();
        let run = RunContext::new(RunRequest::from_text("question"));
        let page = registry
            .search(
                &run,
                &access(),
                KnowledgeSearchRequest {
                    query: "policy".into(),
                    domains: vec![],
                    source_ids: vec![],
                    filters: vec![],
                    limit: 5,
                    cursor: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(page.hits.is_empty());
        assert_eq!(
            page.source_failures,
            vec![crate::SourceFailure {
                source_id: "broken".into(),
                code: crate::KnowledgeErrorCode::Backend,
                retryable: true,
            }]
        );

        let error = registry
            .search(
                &run,
                &access(),
                KnowledgeSearchRequest {
                    query: "policy".into(),
                    domains: vec![],
                    source_ids: vec!["broken".into()],
                    filters: vec![],
                    limit: 5,
                    cursor: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), crate::KnowledgeErrorCode::Backend);
        assert_eq!(error.diagnostic(), Some("private backend detail"));
    }

    #[tokio::test]
    async fn read_rejects_a_source_namespace_substitution() {
        let registry = KnowledgeRegistry::builder(Arc::new(KnowledgeAccessControl::new(Arc::new(
            AllowAllAuthorizer,
        ))))
        .source(Arc::new(ForeignReferenceSource))
        .build()
        .unwrap();
        let run = RunContext::new(RunRequest::from_text("question"));

        let error = registry
            .read(
                &run,
                &access(),
                KnowledgeReadRequest {
                    reference: KnowledgeRef {
                        source_id: "optional".into(),
                        item_id: "item".into(),
                        revision: Some("rev-1".into()),
                    },
                    selector: ContentSelector::Document,
                    max_bytes: 1024,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert_eq!(error.code(), crate::KnowledgeErrorCode::Backend);
        assert_eq!(error.to_string(), "knowledge backend failed");
    }
}
