//! Reusable semantic contract checks for every `KnowledgeSource`.
//!
//! Downstream source crates can enable the `test-utils` feature and call
//! [`verify_source_contract`] from their own integration tests.

use std::collections::BTreeSet;

use llm_harness_types::RunContext;
use tokio_util::sync::CancellationToken;

use crate::{
    ContentSelector, KnowledgeAccessContext, KnowledgeCapability, KnowledgeError,
    KnowledgeErrorCode, KnowledgeReadRequest, KnowledgeRef, KnowledgeRequestContext,
    KnowledgeSource, SourceSearchRequest,
};

/// Source-specific fixture values used by the shared semantic checks.
#[derive(Clone)]
pub struct SourceContractCase {
    pub allowed_access: KnowledgeAccessContext,
    pub denied_access: KnowledgeAccessContext,
    pub search: SourceSearchRequest,
    pub expected_reference: KnowledgeRef,
    pub selector: ContentSelector,
    pub max_bytes: usize,
    pub missing_reference: KnowledgeRef,
    /// Required when the descriptor advertises `Revisioned`.
    pub stale_reference: Option<KnowledgeRef>,
}

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[error("knowledge source contract check `{check}` failed: {message}")]
pub struct SourceContractViolation {
    pub check: &'static str,
    pub message: String,
}

fn violation(
    check: &'static str,
    message: impl Into<String>,
) -> Result<(), SourceContractViolation> {
    Err(SourceContractViolation {
        check,
        message: message.into(),
    })
}

fn expect_error_code(
    check: &'static str,
    result: Result<impl Sized, KnowledgeError>,
    expected: KnowledgeErrorCode,
) -> Result<(), SourceContractViolation> {
    match result {
        Err(error) if error.code() == expected => Ok(()),
        Err(error) => violation(
            check,
            format!("expected {expected:?}, received {:?}", error.code()),
        ),
        Ok(_) => violation(check, format!("expected {expected:?}, received success")),
    }
}

/// Verify source identity, exact reads, authorization, cancellation, and errors.
pub async fn verify_source_contract(
    source: &dyn KnowledgeSource,
    run: &RunContext,
    case: &SourceContractCase,
) -> Result<(), SourceContractViolation> {
    let descriptor = source.descriptor();
    if descriptor.id.trim().is_empty() {
        return violation("descriptor.id", "source ID must not be empty");
    }
    let required = BTreeSet::from([KnowledgeCapability::Search, KnowledgeCapability::Read]);
    if !required.is_subset(&descriptor.capabilities) {
        return violation(
            "descriptor.capabilities",
            "source must advertise Search and Read",
        );
    }
    if case.expected_reference.source_id != descriptor.id
        || case.missing_reference.source_id != descriptor.id
        || case
            .stale_reference
            .as_ref()
            .is_some_and(|reference| reference.source_id != descriptor.id)
    {
        return violation(
            "fixture.references",
            "all fixture references must belong to the source descriptor",
        );
    }

    let allowed = KnowledgeRequestContext {
        run,
        access: &case.allowed_access,
    };
    let page = source
        .search(allowed, case.search.clone(), CancellationToken::new())
        .await
        .map_err(|error| SourceContractViolation {
            check: "search.success",
            message: format!("search failed with {:?}", error.code()),
        })?;
    if page.hits.len() > case.search.limit {
        return violation(
            "search.limit",
            format!(
                "source returned {} hits for limit {}",
                page.hits.len(),
                case.search.limit
            ),
        );
    }
    if page
        .hits
        .iter()
        .any(|hit| hit.reference.source_id != descriptor.id)
    {
        return violation(
            "search.reference_source",
            "a hit reference belongs to another source",
        );
    }
    if !page
        .hits
        .iter()
        .any(|hit| hit.reference == case.expected_reference)
    {
        return violation(
            "search.expected_reference",
            "configured reference was not returned",
        );
    }

    let read_request = KnowledgeReadRequest {
        reference: case.expected_reference.clone(),
        selector: case.selector.clone(),
        max_bytes: case.max_bytes,
    };
    let content = source
        .read(allowed, read_request.clone(), CancellationToken::new())
        .await
        .map_err(|error| SourceContractViolation {
            check: "read.success",
            message: format!("read failed with {:?}", error.code()),
        })?;
    if content.reference != read_request.reference {
        return violation(
            "read.exact_reference",
            "read silently changed the requested reference or revision",
        );
    }
    if content.selector != read_request.selector {
        return violation(
            "read.selector",
            "read returned content for a different selector",
        );
    }
    if content.blocks.is_empty() {
        return violation("read.content", "successful read returned no data blocks");
    }
    if content.obtained_at < *run.started_at() {
        return violation("read.obtained_at", "obtained_at predates the current run");
    }

    let missing = source
        .read(
            allowed,
            KnowledgeReadRequest {
                reference: case.missing_reference.clone(),
                selector: case.selector.clone(),
                max_bytes: case.max_bytes,
            },
            CancellationToken::new(),
        )
        .await;
    expect_error_code("read.not_found", missing, KnowledgeErrorCode::NotFound)?;

    let revisioned = descriptor
        .capabilities
        .contains(&KnowledgeCapability::Revisioned);
    match (revisioned, &case.stale_reference) {
        (true, Some(reference)) => {
            let stale = source
                .read(
                    allowed,
                    KnowledgeReadRequest {
                        reference: reference.clone(),
                        selector: case.selector.clone(),
                        max_bytes: case.max_bytes,
                    },
                    CancellationToken::new(),
                )
                .await;
            expect_error_code(
                "read.stale_revision",
                stale,
                KnowledgeErrorCode::StaleReference,
            )?;
        }
        (true, None) => {
            return violation(
                "fixture.stale_reference",
                "revisioned source requires a stale reference fixture",
            );
        }
        (false, Some(_)) => {
            return violation(
                "fixture.stale_reference",
                "non-revisioned source must not provide a stale reference fixture",
            );
        }
        (false, None) => {}
    }

    let denied = KnowledgeRequestContext {
        run,
        access: &case.denied_access,
    };
    let denied_search = source
        .search(denied, case.search.clone(), CancellationToken::new())
        .await;
    expect_error_code(
        "search.unauthorized",
        denied_search,
        KnowledgeErrorCode::Unauthorized,
    )?;
    let denied_read = source
        .read(denied, read_request.clone(), CancellationToken::new())
        .await;
    expect_error_code(
        "read.unauthorized",
        denied_read,
        KnowledgeErrorCode::Unauthorized,
    )?;

    let search_abort = CancellationToken::new();
    search_abort.cancel();
    let aborted_search = source
        .search(allowed, case.search.clone(), search_abort)
        .await;
    expect_error_code(
        "search.aborted",
        aborted_search,
        KnowledgeErrorCode::Aborted,
    )?;

    let read_abort = CancellationToken::new();
    read_abort.cancel();
    let aborted_read = source.read(allowed, read_request, read_abort).await;
    expect_error_code("read.aborted", aborted_read, KnowledgeErrorCode::Aborted)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::OnceLock,
    };

    use chrono::Utc;
    use futures::future::BoxFuture;
    use llm_harness_types::{DataBlock, RunContext, RunRequest};

    use super::*;
    use crate::{
        FreshnessClass, KnowledgeContent, KnowledgeHit, KnowledgeSourceDescriptor, PrincipalRef,
        SourceSearchPage,
    };

    struct ContractSource;

    impl ContractSource {
        fn reference(revision: &str) -> KnowledgeRef {
            KnowledgeRef {
                source_id: "contract-docs".into(),
                item_id: "policy".into(),
                revision: Some(revision.into()),
            }
        }

        fn check(
            ctx: KnowledgeRequestContext<'_>,
            abort: &CancellationToken,
        ) -> Result<(), KnowledgeError> {
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }
            if ctx.access.principal.subject != "allowed" {
                return Err(KnowledgeError::Unauthorized);
            }
            Ok(())
        }
    }

    impl KnowledgeSource for ContractSource {
        fn descriptor(&self) -> &KnowledgeSourceDescriptor {
            static DESCRIPTOR: OnceLock<KnowledgeSourceDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| KnowledgeSourceDescriptor {
                id: "contract-docs".into(),
                name: "Contract documents".into(),
                description: "Fixture source for the shared contract".into(),
                domains: vec!["policy".into()],
                capabilities: BTreeSet::from([
                    KnowledgeCapability::Search,
                    KnowledgeCapability::Read,
                    KnowledgeCapability::Revisioned,
                ]),
                freshness: FreshnessClass::Static,
                filter_fields: vec![],
            })
        }

        fn search<'a>(
            &'a self,
            ctx: KnowledgeRequestContext<'a>,
            request: SourceSearchRequest,
            abort: CancellationToken,
        ) -> BoxFuture<'a, Result<SourceSearchPage, KnowledgeError>> {
            Box::pin(async move {
                Self::check(ctx, &abort)?;
                let hits = if request.limit == 0 {
                    vec![]
                } else {
                    vec![KnowledgeHit {
                        reference: Self::reference("rev-1"),
                        title: Some("Policy".into()),
                        snippet: "Refund policy".into(),
                        suggested_selectors: vec![ContentSelector::Document],
                        uri: None,
                        score: Some(1.0),
                        updated_at: None,
                        metadata: BTreeMap::new(),
                    }]
                };
                Ok(SourceSearchPage {
                    hits,
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
                Self::check(ctx, &abort)?;
                if request.reference.source_id != "contract-docs"
                    || request.reference.item_id != "policy"
                {
                    return Err(KnowledgeError::NotFound);
                }
                if request.reference.revision.as_deref() != Some("rev-1") {
                    return Err(KnowledgeError::StaleReference {
                        latest: Some(Self::reference("rev-1")),
                    });
                }
                Ok(KnowledgeContent {
                    reference: request.reference,
                    selector: request.selector,
                    title: Some("Policy".into()),
                    blocks: vec![DataBlock::text("Refunds are available for 30 days.")],
                    uri: None,
                    updated_at: None,
                    obtained_at: Utc::now(),
                    truncated: false,
                    metadata: BTreeMap::new(),
                })
            })
        }
    }

    fn access(subject: &str) -> KnowledgeAccessContext {
        KnowledgeAccessContext::new(
            crate::KnowledgeScope::new("contract"),
            PrincipalRef::new(subject, "test"),
        )
    }

    #[tokio::test]
    async fn fixture_source_passes_shared_contract() {
        let run =
            RunContext::new(RunRequest::from_text("refunds").with_extension(access("allowed")));
        let case = SourceContractCase {
            allowed_access: access("allowed"),
            denied_access: access("denied"),
            search: SourceSearchRequest {
                query: "refund".into(),
                filters: vec![],
                limit: 10,
                cursor: None,
            },
            expected_reference: ContractSource::reference("rev-1"),
            selector: ContentSelector::Document,
            max_bytes: 4096,
            missing_reference: KnowledgeRef {
                source_id: "contract-docs".into(),
                item_id: "missing".into(),
                revision: Some("rev-1".into()),
            },
            stale_reference: Some(ContractSource::reference("rev-0")),
        };

        verify_source_contract(&ContractSource, &run, &case)
            .await
            .unwrap();
    }
}
