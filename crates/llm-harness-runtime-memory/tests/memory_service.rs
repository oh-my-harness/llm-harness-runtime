use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use chrono::Utc;
use futures::future::BoxFuture;
use llm_harness_runtime_knowledge::{
    AuthorizationDecision, FreshnessClass, KnowledgeAccessContext, KnowledgeAccessControl,
    KnowledgeAction, KnowledgeCapability, KnowledgeContent, KnowledgeError, KnowledgeReadRequest,
    KnowledgeRequestContext, KnowledgeResourceRef, KnowledgeScope, KnowledgeSource,
    KnowledgeSourceDescriptor, PrincipalRef, SourceSearchPage, SourceSearchRequest,
};
use llm_harness_runtime_memory::{
    MemoryConsistency, MemoryDeleteReceipt, MemoryPolicyError, MemoryProvenance,
    MemoryReceiptViolation, MemoryService, MemoryServiceBuildError, MemoryServiceError,
    MemorySessionId, MemoryStore, MemoryStoreDescriptor, MemoryVisibility, MemoryWrite,
    MemoryWriteIntent, MemoryWriteOrigin, MemoryWritePolicy, MemoryWriteReceipt,
    MemoryWriteViolation,
};
use llm_harness_types::{RunContext, RunRequest};
use tokio_util::sync::CancellationToken;

struct TestSource {
    descriptor: KnowledgeSourceDescriptor,
}

impl TestSource {
    fn memory() -> Self {
        Self {
            descriptor: KnowledgeSourceDescriptor {
                id: "memory".into(),
                name: "Memory".into(),
                description: "Test memory read source".into(),
                domains: vec!["memory".into()],
                capabilities: BTreeSet::from([
                    KnowledgeCapability::Search,
                    KnowledgeCapability::Read,
                    KnowledgeCapability::Revisioned,
                ]),
                freshness: FreshnessClass::NearRealTime,
                filter_fields: vec![],
            },
        }
    }
}

impl KnowledgeSource for TestSource {
    fn descriptor(&self) -> &KnowledgeSourceDescriptor {
        &self.descriptor
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
        _request: KnowledgeReadRequest,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<KnowledgeContent, KnowledgeError>> {
        Box::pin(async { Err(KnowledgeError::NotFound) })
    }
}

struct TestAuthorizer {
    decision: AuthorizationDecision,
    calls: Arc<Mutex<Vec<KnowledgeAction>>>,
}

impl llm_harness_runtime_knowledge::KnowledgeAuthorizer for TestAuthorizer {
    fn authorize<'a>(
        &'a self,
        _access: &'a KnowledgeAccessContext,
        action: KnowledgeAction,
        _resource: KnowledgeResourceRef<'a>,
    ) -> BoxFuture<'a, Result<AuthorizationDecision, KnowledgeError>> {
        self.calls.lock().unwrap().push(action);
        let decision = self.decision;
        Box::pin(async move { Ok(decision) })
    }
}

struct TestPolicy {
    calls: AtomicUsize,
    provenance: Mutex<Option<MemoryProvenance>>,
    tamper_provenance: bool,
}

impl TestPolicy {
    fn valid() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            provenance: Mutex::new(None),
            tamper_provenance: false,
        }
    }
}

impl MemoryWritePolicy for TestPolicy {
    fn prepare<'a>(
        &'a self,
        _ctx: KnowledgeRequestContext<'a>,
        intent: MemoryWriteIntent,
        provenance: MemoryProvenance,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<MemoryWrite, MemoryPolicyError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                return Err(MemoryPolicyError::Aborted);
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.provenance.lock().unwrap() = Some(provenance.clone());
            let mut write_provenance = provenance;
            if self.tamper_provenance {
                write_provenance.run_id = RunContext::new(RunRequest::from_text("other")).id();
            }
            Ok(MemoryWrite {
                content: intent.content,
                kind: intent.kind,
                metadata: BTreeMap::new(),
                provenance: write_provenance,
                idempotency_key: "idem-1".into(),
                expires_at: intent
                    .requested_ttl
                    .and_then(|ttl| chrono::Duration::from_std(ttl).ok())
                    .map(|ttl| Utc::now() + ttl),
            })
        })
    }
}

struct TestStore {
    descriptor: MemoryStoreDescriptor,
    write_receipt: MemoryWriteReceipt,
    upserts: AtomicUsize,
    deletes: AtomicUsize,
    writes: Mutex<Vec<MemoryWrite>>,
}

impl TestStore {
    fn immediate() -> Self {
        Self {
            descriptor: MemoryStoreDescriptor {
                read_source_id: "memory".into(),
                consistency: MemoryConsistency::Immediate,
            },
            write_receipt: MemoryWriteReceipt {
                reference: llm_harness_runtime_knowledge::KnowledgeRef {
                    source_id: "memory".into(),
                    item_id: "item-1".into(),
                    revision: Some("rev-1".into()),
                },
                visibility: MemoryVisibility::Visible,
            },
            upserts: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
            writes: Mutex::new(vec![]),
        }
    }
}

impl MemoryStore for TestStore {
    fn descriptor(&self) -> &MemoryStoreDescriptor {
        &self.descriptor
    }

    fn upsert<'a>(
        &'a self,
        _ctx: KnowledgeRequestContext<'a>,
        write: MemoryWrite,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<MemoryWriteReceipt, KnowledgeError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }
            self.upserts.fetch_add(1, Ordering::SeqCst);
            self.writes.lock().unwrap().push(write);
            Ok(self.write_receipt.clone())
        })
    }

    fn delete<'a>(
        &'a self,
        _ctx: KnowledgeRequestContext<'a>,
        reference: llm_harness_runtime_knowledge::KnowledgeRef,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<MemoryDeleteReceipt, KnowledgeError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }
            self.deletes.fetch_add(1, Ordering::SeqCst);
            Ok(MemoryDeleteReceipt {
                reference,
                visibility: match self.descriptor.consistency {
                    MemoryConsistency::Immediate => MemoryVisibility::Visible,
                    MemoryConsistency::Eventual => MemoryVisibility::Pending {
                        checkpoint: "checkpoint-1".into(),
                    },
                },
            })
        })
    }
}

fn access() -> KnowledgeAccessContext {
    KnowledgeAccessContext::new(
        KnowledgeScope::new("memory"),
        PrincipalRef::new("user-1", "user"),
    )
}

fn context(run: &RunContext) -> KnowledgeRequestContext<'_> {
    KnowledgeRequestContext {
        run,
        access: run.extension::<KnowledgeAccessContext>().unwrap(),
    }
}

fn control(
    decision: AuthorizationDecision,
) -> (
    Arc<KnowledgeAccessControl>,
    Arc<Mutex<Vec<KnowledgeAction>>>,
) {
    let calls = Arc::new(Mutex::new(vec![]));
    let authorizer = TestAuthorizer {
        decision,
        calls: calls.clone(),
    };
    (
        Arc::new(KnowledgeAccessControl::new(Arc::new(authorizer))),
        calls,
    )
}

fn intent() -> MemoryWriteIntent {
    MemoryWriteIntent {
        content: "Prefers concise explanations".into(),
        kind: Some("preference".into()),
        requested_ttl: None,
    }
}

#[test]
fn build_rejects_read_write_source_mismatch() {
    let source = Arc::new(TestSource::memory());
    let mut store = TestStore::immediate();
    store.descriptor.read_source_id = "other".into();
    let (control, _) = control(AuthorizationDecision::Allow);
    let error = match MemoryService::new(
        control,
        source,
        Arc::new(store),
        Arc::new(TestPolicy::valid()),
    ) {
        Ok(_) => panic!("mismatched read source must fail"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        MemoryServiceBuildError::ReadSourceMismatch { .. }
    ));
}

#[test]
fn build_requires_revisioned_read_source() {
    let mut source = TestSource::memory();
    source
        .descriptor
        .capabilities
        .remove(&KnowledgeCapability::Revisioned);
    let (control, _) = control(AuthorizationDecision::Allow);
    let error = match MemoryService::new(
        control,
        Arc::new(source),
        Arc::new(TestStore::immediate()),
        Arc::new(TestPolicy::valid()),
    ) {
        Ok(_) => panic!("non-revisioned source must fail"),
        Err(error) => error,
    };
    assert_eq!(
        error,
        MemoryServiceBuildError::MissingReadCapability(KnowledgeCapability::Revisioned)
    );
}

#[tokio::test]
async fn denied_write_stops_before_policy_and_store() {
    let (control, calls) = control(AuthorizationDecision::Deny);
    let policy = Arc::new(TestPolicy::valid());
    let store = Arc::new(TestStore::immediate());
    let service = MemoryService::new(
        control,
        Arc::new(TestSource::memory()),
        store.clone(),
        policy.clone(),
    )
    .unwrap();
    let run = RunContext::new(RunRequest::from_text("remember").with_extension(access()));

    let error = service
        .write(
            context(&run),
            intent(),
            MemoryWriteOrigin::ExplicitTool {
                tool_use_id: "tool-1".into(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        MemoryServiceError::Knowledge(KnowledgeError::Unauthorized)
    ));
    assert_eq!(*calls.lock().unwrap(), vec![KnowledgeAction::Write]);
    assert_eq!(policy.calls.load(Ordering::SeqCst), 0);
    assert_eq!(store.upserts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn write_builds_server_provenance_and_returns_exact_receipt() {
    let (control, _) = control(AuthorizationDecision::Allow);
    let policy = Arc::new(TestPolicy::valid());
    let store = Arc::new(TestStore::immediate());
    let expected = store.write_receipt.clone();
    let service = MemoryService::new(
        control,
        Arc::new(TestSource::memory()),
        store.clone(),
        policy.clone(),
    )
    .unwrap();
    let session = MemorySessionId::new("session-1").unwrap();
    let run = RunContext::new(
        RunRequest::from_text("remember")
            .with_extension(access())
            .with_extension(session),
    );

    let receipt = service
        .write(
            context(&run),
            intent(),
            MemoryWriteOrigin::ExplicitTool {
                tool_use_id: "tool-1".into(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(receipt, expected);
    let provenance = policy.provenance.lock().unwrap().clone().unwrap();
    assert_eq!(provenance.run_id, run.id());
    assert_eq!(provenance.session_id.as_deref(), Some("session-1"));
    assert_eq!(
        provenance.origin,
        MemoryWriteOrigin::ExplicitTool {
            tool_use_id: "tool-1".into()
        }
    );
    assert_eq!(store.writes.lock().unwrap()[0].provenance, provenance);
}

#[tokio::test]
async fn policy_cannot_replace_server_provenance() {
    let (control, _) = control(AuthorizationDecision::Allow);
    let policy = Arc::new(TestPolicy {
        tamper_provenance: true,
        ..TestPolicy::valid()
    });
    let store = Arc::new(TestStore::immediate());
    let service = MemoryService::new(
        control,
        Arc::new(TestSource::memory()),
        store.clone(),
        policy,
    )
    .unwrap();
    let run = RunContext::new(RunRequest::from_text("remember").with_extension(access()));

    let error = service
        .write(
            context(&run),
            intent(),
            MemoryWriteOrigin::Application {
                operation_id: "request-1".into(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        MemoryServiceError::InvalidWrite(MemoryWriteViolation::ProvenanceMismatch)
    ));
    assert_eq!(store.upserts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn receipt_must_match_declared_consistency() {
    let (control, _) = control(AuthorizationDecision::Allow);
    let mut store = TestStore::immediate();
    store.write_receipt.visibility = MemoryVisibility::Pending {
        checkpoint: "unexpected".into(),
    };
    let service = MemoryService::new(
        control,
        Arc::new(TestSource::memory()),
        Arc::new(store),
        Arc::new(TestPolicy::valid()),
    )
    .unwrap();
    let run = RunContext::new(RunRequest::from_text("remember").with_extension(access()));

    let error = service
        .write(
            context(&run),
            intent(),
            MemoryWriteOrigin::Application {
                operation_id: "request-1".into(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        MemoryServiceError::InvalidReceipt(MemoryReceiptViolation::ConsistencyMismatch)
    ));
}

#[tokio::test]
async fn delete_rejects_reference_from_another_read_source() {
    let (control, calls) = control(AuthorizationDecision::Allow);
    let store = Arc::new(TestStore::immediate());
    let service = MemoryService::new(
        control,
        Arc::new(TestSource::memory()),
        store.clone(),
        Arc::new(TestPolicy::valid()),
    )
    .unwrap();
    let run = RunContext::new(RunRequest::from_text("forget").with_extension(access()));

    let error = service
        .delete(
            context(&run),
            llm_harness_runtime_knowledge::KnowledgeRef {
                source_id: "other".into(),
                item_id: "item-1".into(),
                revision: Some("rev-1".into()),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        MemoryServiceError::Knowledge(KnowledgeError::NotFound)
    ));
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(store.deletes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pre_cancelled_write_has_no_side_effects() {
    let (control, calls) = control(AuthorizationDecision::Allow);
    let policy = Arc::new(TestPolicy::valid());
    let store = Arc::new(TestStore::immediate());
    let service = MemoryService::new(
        control,
        Arc::new(TestSource::memory()),
        store.clone(),
        policy.clone(),
    )
    .unwrap();
    let run = RunContext::new(RunRequest::from_text("remember").with_extension(access()));
    let abort = CancellationToken::new();
    abort.cancel();

    let error = service
        .write(
            context(&run),
            intent(),
            MemoryWriteOrigin::Application {
                operation_id: "request-1".into(),
            },
            abort,
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        MemoryServiceError::Knowledge(KnowledgeError::Aborted)
    ));
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(policy.calls.load(Ordering::SeqCst), 0);
    assert_eq!(store.upserts.load(Ordering::SeqCst), 0);
}

#[test]
fn memory_session_id_rejects_empty_values() {
    assert!(MemorySessionId::new("  ").is_err());
}
