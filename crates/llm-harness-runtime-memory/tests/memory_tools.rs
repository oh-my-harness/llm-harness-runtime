use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use futures::future::BoxFuture;
use llm_harness_agent::{AgentHarness, AgentHarnessOptions, Plugin};
use llm_harness_loop::{
    LlmClient,
    test_utils::{MockLlmClient, MockResponse, NoOpEnv},
};
use llm_harness_runtime_knowledge::{
    AllowAllAuthorizer, FreshnessClass, KnowledgeAccessContext, KnowledgeAccessControl,
    KnowledgeCapability, KnowledgeContent, KnowledgeError, KnowledgeReadRequest, KnowledgeRef,
    KnowledgeRequestContext, KnowledgeScope, KnowledgeSource, KnowledgeSourceDescriptor,
    PrincipalRef, SourceSearchPage, SourceSearchRequest,
};
use llm_harness_runtime_memory::{
    MEMORY_FORGET_TOOL_NAME, MEMORY_WRITE_TOOL_NAME, MemoryConsistency, MemoryDeleteReceipt,
    MemoryMutationGate, MemoryMutationGateError, MemoryMutationRequest, MemoryPlugin,
    MemoryService, MemoryStore, MemoryStoreDescriptor, MemoryVisibility, MemoryWrite,
    MemoryWriteReceipt, SecureMemoryWritePolicy, SecureMemoryWritePolicyConfig,
};
use llm_harness_types::{AgentMessage, RunRequest, Tool};
use tokio_util::sync::CancellationToken;

struct MemoryReadSource {
    descriptor: KnowledgeSourceDescriptor,
}

#[derive(Default)]
struct ApprovingMutationGate {
    calls: AtomicUsize,
}

impl MemoryMutationGate for ApprovingMutationGate {
    fn authorize<'a>(
        &'a self,
        _ctx: KnowledgeRequestContext<'a>,
        _request: MemoryMutationRequest,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), MemoryMutationGateError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                return Err(MemoryMutationGateError::Aborted);
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

impl MemoryReadSource {
    fn new() -> Self {
        Self {
            descriptor: KnowledgeSourceDescriptor {
                id: "memory".into(),
                name: "Memory".into(),
                description: "Fixture memory read model".into(),
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

impl KnowledgeSource for MemoryReadSource {
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

#[derive(Default)]
struct StoreState {
    receipts: HashMap<String, MemoryWriteReceipt>,
    writes: Vec<MemoryWrite>,
    deletes: Vec<KnowledgeRef>,
}

struct FixtureStore {
    descriptor: MemoryStoreDescriptor,
    state: Mutex<StoreState>,
}

impl FixtureStore {
    fn new() -> Self {
        Self {
            descriptor: MemoryStoreDescriptor {
                read_source_id: "memory".into(),
                consistency: MemoryConsistency::Immediate,
            },
            state: Mutex::new(StoreState::default()),
        }
    }
}

impl MemoryStore for FixtureStore {
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
            let mut state = self.state.lock().unwrap();
            if let Some(receipt) = state.receipts.get(&write.idempotency_key) {
                return Ok(receipt.clone());
            }
            let receipt = MemoryWriteReceipt {
                reference: KnowledgeRef {
                    source_id: "memory".into(),
                    item_id: "preference-1".into(),
                    revision: Some("rev-1".into()),
                },
                visibility: MemoryVisibility::Visible,
            };
            state
                .receipts
                .insert(write.idempotency_key.clone(), receipt.clone());
            state.writes.push(write);
            Ok(receipt)
        })
    }

    fn delete<'a>(
        &'a self,
        _ctx: KnowledgeRequestContext<'a>,
        reference: KnowledgeRef,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<MemoryDeleteReceipt, KnowledgeError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }
            self.state.lock().unwrap().deletes.push(reference.clone());
            Ok(MemoryDeleteReceipt {
                reference,
                visibility: MemoryVisibility::Visible,
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

#[tokio::test]
async fn agent_writes_and_forgets_memory_through_projected_tools() {
    let access_control = Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer)));
    let store = Arc::new(FixtureStore::new());
    let policy = Arc::new(
        SecureMemoryWritePolicy::new(vec![8; 32], SecureMemoryWritePolicyConfig::default())
            .unwrap(),
    );
    let mutation_gate = Arc::new(ApprovingMutationGate::default());
    let service = Arc::new(
        MemoryService::new(
            access_control,
            Arc::new(MemoryReadSource::new()),
            store.clone(),
            policy,
            mutation_gate.clone(),
        )
        .unwrap(),
    );
    let plugin = MemoryPlugin::new(service);
    let mut tools: Vec<Arc<dyn Tool>> = vec![];
    plugin.register_tools(&mut tools);

    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "write-1",
            MEMORY_WRITE_TOOL_NAME,
            r#"{"content":"  Prefers concise explanations  ","kind":"preference"}"#,
        ),
        MockResponse::tool_use(
            "forget-1",
            MEMORY_FORGET_TOOL_NAME,
            r#"{"reference":{"source_id":"memory","item_id":"preference-1","revision":"rev-1"}}"#,
        ),
        MockResponse::text("I remembered the preference and then removed it as requested."),
    ]));
    let mut options = AgentHarnessOptions::new("test-model");
    options.tools = tools;
    let harness =
        AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, Arc::new(NoOpEnv), options).await;

    harness
        .run(
            RunRequest::from_text(
                "Remember that I prefer concise explanations, then forget that preference.",
            )
            .with_extension(access()),
        )
        .await
        .unwrap();

    {
        let state = store.state.lock().unwrap();
        assert_eq!(state.writes.len(), 1);
        assert_eq!(state.writes[0].content, "Prefers concise explanations");
        assert_eq!(state.deletes.len(), 1);
        assert_eq!(state.deletes[0].item_id, "preference-1");
    }
    assert_eq!(mutation_gate.calls.load(Ordering::SeqCst), 2);

    let context = harness.build_context().await.unwrap();
    let results = context
        .messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    let serialized = serde_json::to_string(&results).unwrap();
    assert!(serialized.contains("memory.write"));
    assert!(serialized.contains("memory.delete"));
    assert!(serialized.contains("rev-1"));
    assert!(!serialized.contains("Prefers concise explanations"));
}
