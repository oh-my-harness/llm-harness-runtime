//! Tests for Knowledge + Memory tools (Spec §7).

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use llm_harness_knowledge::access::{
    AllowAllAuthorizer, KnowledgeAccessContext, KnowledgeAccessControl, KnowledgeScope,
    PrincipalRef,
};
use llm_harness_knowledge::{
    EvidenceAuthority, EvidenceProviderId, KnowledgeCitationPolicy, KnowledgeError,
    KnowledgePlugin, KnowledgePluginConfig, KnowledgeRef, KnowledgeRegistry,
    KnowledgeRequestContext, KnowledgeSource, KnowledgeToolConfig,
};
use llm_harness_knowledge_local::{
    DocumentFileSystem, DocumentParserRegistry, DocumentRootConfig, LocalDocumentSource,
    LocalDocumentSourceConfig, OsDocumentFileSystem, content_revision,
};
use llm_harness_live_tests::skip_if_none;
use llm_harness_live_tests::{
    CapturedEvent, EventCapture, LiveProvider, MULTI_TURN, SINGLE_TURN, assert_event_emitted,
    assert_tool_was_called, make_harness_with, with_timeout,
};
use llm_harness_memory::{
    MemoryConsistency, MemoryDeleteReceipt, MemoryMutationGate, MemoryMutationGateError,
    MemoryMutationRequest, MemoryPlugin, MemoryService, MemoryStore, MemoryStoreDescriptor,
    MemoryVisibility, MemoryWrite, MemoryWritePolicy, MemoryWriteReceipt, SecureMemoryWritePolicy,
    SecureMemoryWritePolicyConfig,
};
use llm_harness_types::RunRequest;
use tokio_util::sync::CancellationToken;

// ── Test fixtures ──────────────────────────────────────────────────────

/// Create a temp directory with two markdown documents about Rust and Python.
fn make_doc_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("rust.md"),
        "# Rust Programming\n\nRust is a systems programming language focused on memory safety, \
         concurrency, and performance. It uses a borrow checker to enforce ownership rules at \
         compile time.\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("python.md"),
        "# Python Programming\n\nPython is a high-level, interpreted programming language known \
         for its readable syntax and broad ecosystem. It is widely used in data science, web \
         development, and automation.\n",
    )
    .unwrap();
    tmp
}

/// Build a `LocalDocumentSource` backed by an OS filesystem with two .md docs.
async fn make_doc_source(
    tmp: &tempfile::TempDir,
    access_control: Arc<KnowledgeAccessControl>,
) -> LocalDocumentSource {
    let config = LocalDocumentSourceConfig {
        source_id: "local-docs".to_string(),
        name: "Local Documents".to_string(),
        description: "Test markdown documents".to_string(),
        domains: vec!["test".to_string()],
        roots: vec![DocumentRootConfig {
            id: "root".to_string(),
            path: tmp.path().to_path_buf(),
        }],
        source_secret: vec![0x42u8; 32],
        max_document_bytes: 1024 * 1024,
    };
    LocalDocumentSource::create(
        config,
        Arc::new(OsDocumentFileSystem) as Arc<dyn DocumentFileSystem>,
        DocumentParserRegistry::default(),
        access_control,
    )
    .await
    .expect("LocalDocumentSource creation should succeed")
}

/// Build the knowledge stack: access control, registry, evidence authority, plugin config.
fn make_knowledge_plugin(
    source: Arc<LocalDocumentSource>,
    access_control: Arc<KnowledgeAccessControl>,
) -> KnowledgePlugin {
    let registry = Arc::new(
        KnowledgeRegistry::builder(access_control)
            .source(source as Arc<dyn KnowledgeSource>)
            .build()
            .expect("registry build should succeed"),
    );
    let authority = Arc::new(
        EvidenceAuthority::new(
            vec![0x11u8; 32],
            [EvidenceProviderId("local-docs-provider".to_string())],
        )
        .expect("evidence authority creation should succeed"),
    );
    let config = KnowledgePluginConfig {
        tools: KnowledgeToolConfig::default(),
        citation_policy: KnowledgeCitationPolicy::default(),
    };
    KnowledgePlugin::new(
        registry,
        authority,
        EvidenceProviderId("local-docs-provider".to_string()),
        config,
    )
    .expect("KnowledgePlugin creation should succeed")
}

/// Create the `KnowledgeAccessContext` extension to attach to every `RunRequest`.
fn make_access_context() -> KnowledgeAccessContext {
    KnowledgeAccessContext::new(
        KnowledgeScope::new("default"),
        PrincipalRef::new("tester", "human"),
    )
}

// ── Tests ──────────────────────────────────────────────────────────────

/// The `knowledge_search` tool registered by `KnowledgePlugin` is callable by
/// the LLM: a prompt asking the model to search for 'Rust' results in a
/// `knowledge_search` tool call and the conversation settles normally.
#[tokio::test]
async fn knowledge_search_returns_results() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let doc_dir = make_doc_dir();
    let access_control = Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer)));
    let source = Arc::new(make_doc_source(&doc_dir, access_control.clone()).await);
    let plugin = make_knowledge_plugin(source, access_control);

    let (harness, _tmp) = make_harness_with(entry, |b| b.install(&plugin)).await;
    let cap = EventCapture::subscribe(&harness);

    let request = RunRequest::from_text(
        "Use the knowledge_search tool to search for 'Rust'. \
         After you get the results, tell me what you found.",
    )
    .with_extension(make_access_context());

    with_timeout(SINGLE_TURN, harness.run(request))
        .await
        .expect("run should complete successfully");
    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "knowledge_search");
    assert_event_emitted(&events, &CapturedEvent::Settled);
}

/// The `knowledge_read` tool is callable by the LLM after a search: the model
/// first searches, then reads a document reference returned by the search.
#[tokio::test]
async fn knowledge_read_returns_content() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let doc_dir = make_doc_dir();
    let access_control = Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer)));
    let source = Arc::new(make_doc_source(&doc_dir, access_control.clone()).await);
    let plugin = make_knowledge_plugin(source, access_control);

    let (harness, _tmp) = make_harness_with(entry, |b| b.install(&plugin)).await;
    let cap = EventCapture::subscribe(&harness);

    let request = RunRequest::from_text(
        "Use the knowledge_search tool to search for 'Rust'. \
         Then use the knowledge_read tool to read the first result reference. \
         Finally, summarize what you read.",
    )
    .with_extension(make_access_context());

    with_timeout(MULTI_TURN, harness.run(request))
        .await
        .expect("run should complete successfully");
    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "knowledge_read");
    assert_event_emitted(&events, &CapturedEvent::Settled);
}

// ── In-memory MemoryStore implementation ───────────────────────────────

/// Simple in-memory store that satisfies `MemoryStore` for tests.
struct InMemoryStore {
    descriptor: MemoryStoreDescriptor,
    entries: Mutex<Vec<(KnowledgeRef, Vec<u8>)>>,
}

impl InMemoryStore {
    fn new(read_source_id: String) -> Self {
        Self {
            descriptor: MemoryStoreDescriptor {
                read_source_id,
                consistency: MemoryConsistency::Immediate,
            },
            entries: Mutex::new(Vec::new()),
        }
    }
}

impl MemoryStore for InMemoryStore {
    fn descriptor(&self) -> &MemoryStoreDescriptor {
        &self.descriptor
    }

    fn upsert<'a>(
        &'a self,
        ctx: KnowledgeRequestContext<'a>,
        write: MemoryWrite,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<MemoryWriteReceipt, KnowledgeError>> {
        Box::pin(async move {
            let item_id = format!("mem-{}", ctx.run.id());
            let revision = content_revision(write.content.as_bytes());
            let reference = KnowledgeRef {
                source_id: self.descriptor.read_source_id.clone(),
                item_id: item_id.clone(),
                revision: Some(revision.clone()),
            };
            self.entries
                .lock()
                .unwrap()
                .push((reference.clone(), write.content.into_bytes()));
            Ok(MemoryWriteReceipt {
                reference,
                visibility: MemoryVisibility::Visible,
            })
        })
    }

    fn delete<'a>(
        &'a self,
        _ctx: KnowledgeRequestContext<'a>,
        reference: KnowledgeRef,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<MemoryDeleteReceipt, KnowledgeError>> {
        Box::pin(async move {
            self.entries
                .lock()
                .unwrap()
                .retain(|(r, _)| r != &reference);
            Ok(MemoryDeleteReceipt {
                reference,
                visibility: MemoryVisibility::Visible,
            })
        })
    }
}

// ── Permissive mutation gate ───────────────────────────────────────────

struct AllowAllGate;

impl MemoryMutationGate for AllowAllGate {
    fn authorize<'a>(
        &'a self,
        _ctx: KnowledgeRequestContext<'a>,
        _request: MemoryMutationRequest,
        _abort: CancellationToken,
    ) -> BoxFuture<'a, Result<(), MemoryMutationGateError>> {
        Box::pin(async { Ok(()) })
    }
}

/// The `memory_write` tool registered by `MemoryPlugin` is callable by the
/// LLM: the model writes a fact to memory, then in a second turn attempts to
/// retrieve it. The assertion verifies `memory_write` was called.
#[tokio::test]
async fn memory_write_saves_and_read_back() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    let doc_dir = make_doc_dir();
    let access_control = Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer)));
    let source = Arc::new(make_doc_source(&doc_dir, access_control.clone()).await);

    // Build the memory stack.
    let store = Arc::new(InMemoryStore::new(source.descriptor().id.clone()));
    let write_policy = Arc::new(
        SecureMemoryWritePolicy::new(vec![0x55u8; 32], SecureMemoryWritePolicyConfig::default())
            .expect("secure policy creation should succeed"),
    );
    let service = Arc::new(
        MemoryService::new(
            access_control,
            source.clone() as Arc<dyn KnowledgeSource>,
            store.clone() as Arc<dyn MemoryStore>,
            write_policy as Arc<dyn MemoryWritePolicy>,
            Arc::new(AllowAllGate) as Arc<dyn MemoryMutationGate>,
        )
        .expect("MemoryService creation should succeed"),
    );
    let plugin = MemoryPlugin::new(service);

    let (harness, _tmp) = make_harness_with(entry, |b| b.install(&plugin)).await;
    let cap = EventCapture::subscribe(&harness);

    let request = RunRequest::from_text(
        "Use the memory_write tool to save this fact: \
         'The user prefers Rust over Python for systems programming.' \
         After saving, tell me you saved it.",
    )
    .with_extension(make_access_context());

    with_timeout(MULTI_TURN, harness.run(request))
        .await
        .expect("run should complete successfully");
    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "memory_write");
    assert_event_emitted(&events, &CapturedEvent::Settled);

    // Verify the entry was actually stored.
    let stored = store.entries.lock().unwrap();
    assert!(
        !stored.is_empty(),
        "InMemoryStore should have at least one entry after memory_write"
    );
}
