use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, OnceLock},
};

use chrono::Utc;
use futures::future::BoxFuture;
use llm_adapter::types::{Message, RequestContent};
use llm_harness_agent::{AgentHarness, AgentHarnessOptions, Plugin};
use llm_harness_loop::{
    LlmClient,
    test_utils::{MockLlmClient, MockResponse, NoOpEnv},
};
use llm_harness_runtime_knowledge::*;
use llm_harness_types::{AgentMessage, DataBlock, RunRequest, Tool, ToolProjectionKind};
use tokio_util::sync::CancellationToken;

struct FixtureSource;

impl KnowledgeSource for FixtureSource {
    fn descriptor(&self) -> &KnowledgeSourceDescriptor {
        static DESCRIPTOR: OnceLock<KnowledgeSourceDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| KnowledgeSourceDescriptor {
            id: "docs".into(),
            name: "Documents".into(),
            description: "Fixture documents".into(),
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
        _ctx: KnowledgeRequestContext<'a>,
        _request: SourceSearchRequest,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<SourceSearchPage, KnowledgeError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }
            Ok(SourceSearchPage {
                hits: vec![KnowledgeHit {
                    reference: reference(),
                    title: Some("Refund policy".into()),
                    snippet: "Thirty day refund window".into(),
                    suggested_selectors: vec![ContentSelector::Document],
                    uri: Some("knowledge://docs/refund".into()),
                    score: Some(1.0),
                    updated_at: None,
                    metadata: BTreeMap::new(),
                }],
                next_cursor: None,
            })
        })
    }

    fn read<'a>(
        &'a self,
        _ctx: KnowledgeRequestContext<'a>,
        request: KnowledgeReadRequest,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<KnowledgeContent, KnowledgeError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                return Err(KnowledgeError::Aborted);
            }
            if request.reference != reference() {
                return Err(KnowledgeError::NotFound);
            }
            Ok(KnowledgeContent {
                reference: request.reference,
                selector: request.selector,
                title: Some("Refund policy".into()),
                blocks: vec![DataBlock::text(
                    "PRIVATE POLICY BODY: refunds last thirty days",
                )],
                uri: Some("knowledge://docs/refund".into()),
                updated_at: None,
                obtained_at: Utc::now(),
                truncated: false,
                metadata: BTreeMap::new(),
            })
        })
    }
}

fn reference() -> KnowledgeRef {
    KnowledgeRef {
        source_id: "docs".into(),
        item_id: "refund".into(),
        revision: Some("rev-1".into()),
    }
}

fn access() -> KnowledgeAccessContext {
    KnowledgeAccessContext::new(
        KnowledgeScope::new("test"),
        PrincipalRef::new("user-1", "test"),
    )
}

#[tokio::test]
async fn agent_searches_reads_and_persists_only_evidence_projection() {
    let access_control = Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer)));
    let registry = Arc::new(
        KnowledgeRegistry::builder(access_control)
            .source(Arc::new(FixtureSource))
            .build()
            .unwrap(),
    );
    let provider = EvidenceProviderId("knowledge.registry".into());
    let authority = Arc::new(EvidenceAuthority::new(vec![5; 32], [provider.clone()]).unwrap());
    let plugin = KnowledgePlugin::new(
        registry,
        authority,
        provider,
        KnowledgeToolConfig::default(),
    )
    .expect("knowledge plugin should build");
    let mut tools: Vec<Arc<dyn Tool>> = vec![];
    plugin.register_tools(&mut tools);

    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "search-1",
            KNOWLEDGE_SEARCH_TOOL_NAME,
            r#"{"query":"refund","source_id":"docs"}"#,
        ),
        MockResponse::tool_use(
            "read-1",
            KNOWLEDGE_READ_TOOL_NAME,
            r#"{"reference":{"source_id":"docs","item_id":"refund","revision":"rev-1"}}"#,
        ),
        MockResponse::text("Refunds are available for thirty days."),
    ]));
    let mut options = AgentHarnessOptions::new("test-model");
    options.tools = tools;
    let harness = AgentHarness::new_in_memory(
        client.clone() as Arc<dyn LlmClient>,
        Arc::new(NoOpEnv),
        options,
    )
    .await;
    harness
        .run(RunRequest::from_text("What is the refund window?").with_extension(access()))
        .await
        .unwrap();

    {
        let requests = client.captured_requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].messages().iter().any(|message| {
            matches!(
                message,
                Message::Tool { content, .. }
                    if content.iter().any(|block| matches!(
                        block,
                        RequestContent::Text(text)
                            if text.contains("PRIVATE POLICY BODY")
                                || text.starts_with("Citation handle: [K:")
                    ))
            )
        }));
    }

    let context = harness.build_context().await.unwrap();
    let read_result = context.messages.iter().find_map(|message| match message {
        AgentMessage::ToolResult(result)
            if result.projection.kind == ToolProjectionKind::Ephemeral =>
        {
            Some(result)
        }
        _ => None,
    });
    let read_result = read_result.expect("read result must be persisted");
    let serialized = serde_json::to_string(read_result).unwrap();
    assert!(!serialized.contains("PRIVATE POLICY BODY"));
    assert!(serialized.contains("knowledge.evidence"));
    assert!(serialized.contains("rev-1"));
}
