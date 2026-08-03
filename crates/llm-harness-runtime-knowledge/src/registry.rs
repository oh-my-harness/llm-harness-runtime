use std::{collections::BTreeMap, sync::Arc};

use llm_harness_types::RunContext;
use tokio_util::sync::CancellationToken;

use crate::{
    FederatedKnowledgePage, KnowledgeAccessContext, KnowledgeAccessControl, KnowledgeAction,
    KnowledgeError, KnowledgeReadRequest, KnowledgeRequestContext, KnowledgeResourceRef,
    KnowledgeSearchRequest, KnowledgeSource, KnowledgeSourceDescriptor, SourceSearchRequest,
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
        if request.source_ids.len() > 1 {
            return Err(KnowledgeError::UnsupportedCapability(
                "a search request may select at most one source".into(),
            ));
        }
        let candidates = if let Some(source_id) = request.source_ids.first() {
            let source = self
                .sources
                .get(source_id)
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
            vec![descriptor.clone()]
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
        if candidates.len() != 1 {
            return Err(KnowledgeError::UnsupportedCapability(
                "search request must resolve to exactly one visible source".into(),
            ));
        }
        let descriptor = &candidates[0];
        let source = self
            .sources
            .get(&descriptor.id)
            .ok_or(KnowledgeError::NotFound)?;
        self.access_control
            .authorize(
                access,
                KnowledgeAction::Search,
                KnowledgeResourceRef::Source {
                    source_id: &descriptor.id,
                    domains: &descriptor.domains,
                },
            )
            .await?;
        let filters = request
            .filters
            .into_iter()
            .map(|filter| {
                if filter.source_id != descriptor.id {
                    Err(KnowledgeError::InvalidFilter(
                        "filter source does not match selected source".into(),
                    ))
                } else {
                    Ok(filter.expression)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let page = source
            .search(
                KnowledgeRequestContext { run, access },
                SourceSearchRequest {
                    query: request.query,
                    filters,
                    limit: request.limit,
                    cursor: None,
                },
                abort,
            )
            .await?;
        Ok(FederatedKnowledgePage {
            hits: page.hits,
            next_cursor: None,
            source_failures: vec![],
            search_projection: source.search_projection(),
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
