use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, OnceLock},
};

use chrono::Utc;
use futures::future::BoxFuture;
use llm_adapter::types::{Message, RequestContent};
use llm_harness_agent::{AgentHarness, AgentHarnessOptions, HarnessHooks, Plugin};
use llm_harness_loop::{
    LlmClient,
    test_utils::{MockLlmClient, MockResponse, NoOpEnv},
};
use llm_harness_runtime_knowledge::*;
use llm_harness_types::{AgentMessage, DataBlock, RunRequest, Tool, ToolProjectionKind};
use tokio_util::sync::CancellationToken;

struct FixtureSource;
struct MemoryFixtureSource;
struct ReferenceOnlyFixtureSource;

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

impl KnowledgeSource for MemoryFixtureSource {
    fn descriptor(&self) -> &KnowledgeSourceDescriptor {
        static DESCRIPTOR: OnceLock<KnowledgeSourceDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| KnowledgeSourceDescriptor {
            id: "memory".into(),
            name: "Learner memory".into(),
            description: "Fixture learner memory".into(),
            domains: vec!["personalization".into()],
            capabilities: BTreeSet::from([
                KnowledgeCapability::Search,
                KnowledgeCapability::Read,
                KnowledgeCapability::Revisioned,
            ]),
            freshness: FreshnessClass::Live,
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
                    reference: memory_reference(),
                    title: Some("Explanation preference".into()),
                    snippet: "Prefers concise explanations".into(),
                    suggested_selectors: vec![ContentSelector::Document],
                    uri: None,
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
            if request.reference != memory_reference() {
                return Err(KnowledgeError::NotFound);
            }
            Ok(KnowledgeContent {
                reference: request.reference,
                selector: request.selector,
                title: Some("Explanation preference".into()),
                blocks: vec![DataBlock::text("Prefers concise explanations")],
                uri: None,
                updated_at: None,
                obtained_at: Utc::now(),
                truncated: false,
                metadata: BTreeMap::new(),
            })
        })
    }
}

impl KnowledgeSource for ReferenceOnlyFixtureSource {
    fn descriptor(&self) -> &KnowledgeSourceDescriptor {
        static DESCRIPTOR: OnceLock<KnowledgeSourceDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| KnowledgeSourceDescriptor {
            id: "history-fixture".into(),
            name: "History fixture".into(),
            description: "Reference-only search projection fixture".into(),
            domains: vec!["history".into()],
            capabilities: BTreeSet::from([KnowledgeCapability::Search, KnowledgeCapability::Read]),
            freshness: FreshnessClass::Live,
            filter_fields: vec![],
        })
    }

    fn search_projection(&self) -> KnowledgeSearchProjection {
        KnowledgeSearchProjection::ReferenceOnly
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
                    reference: KnowledgeRef {
                        source_id: "history-fixture".into(),
                        item_id: "turn-1".into(),
                        revision: Some("1".into()),
                    },
                    title: Some("Old session".into()),
                    snippet: "PRIVATE HISTORY SNIPPET".into(),
                    suggested_selectors: vec![ContentSelector::Document],
                    uri: None,
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
        _request: KnowledgeReadRequest,
        abort: CancellationToken,
    ) -> BoxFuture<'a, Result<KnowledgeContent, KnowledgeError>> {
        Box::pin(async move {
            if abort.is_cancelled() {
                Err(KnowledgeError::Aborted)
            } else {
                Err(KnowledgeError::NotFound)
            }
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

fn memory_reference() -> KnowledgeRef {
    KnowledgeRef {
        source_id: "memory".into(),
        item_id: "preference".into(),
        revision: Some("rev-1".into()),
    }
}

fn access() -> KnowledgeAccessContext {
    KnowledgeAccessContext::new(
        KnowledgeScope::new("test"),
        PrincipalRef::new("user-1", "test"),
    )
}

fn mixed_citation_policy() -> KnowledgeCitationPolicy {
    KnowledgeCitationPolicy::builder()
        .source("docs", KnowledgeCitationRequirement::Required)
        .source("memory", KnowledgeCitationRequirement::Optional)
        .build()
        .unwrap()
}

fn knowledge_plugin(config: KnowledgePluginConfig) -> KnowledgePlugin {
    let access_control = Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer)));
    let registry = Arc::new(
        KnowledgeRegistry::builder(access_control)
            .source(Arc::new(FixtureSource))
            .source(Arc::new(MemoryFixtureSource))
            .build()
            .unwrap(),
    );
    let provider = EvidenceProviderId("knowledge.registry".into());
    let authority = Arc::new(EvidenceAuthority::new(vec![5; 32], [provider.clone()]).unwrap());
    KnowledgePlugin::new(registry, authority, provider, config)
        .expect("knowledge plugin should build")
}

fn reference_only_plugin() -> KnowledgePlugin {
    let access_control = Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer)));
    let registry = Arc::new(
        KnowledgeRegistry::builder(access_control)
            .source(Arc::new(ReferenceOnlyFixtureSource))
            .build()
            .unwrap(),
    );
    let provider = EvidenceProviderId("knowledge.registry".into());
    let authority = Arc::new(EvidenceAuthority::new(vec![7; 32], [provider.clone()]).unwrap());
    KnowledgePlugin::new(
        registry,
        authority,
        provider,
        KnowledgePluginConfig::default(),
    )
    .unwrap()
}

fn mixed_projection_plugin() -> KnowledgePlugin {
    let access_control = Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer)));
    let registry = Arc::new(
        KnowledgeRegistry::builder(access_control)
            .source(Arc::new(FixtureSource))
            .source(Arc::new(ReferenceOnlyFixtureSource))
            .build()
            .unwrap(),
    );
    let provider = EvidenceProviderId("knowledge.registry".into());
    let authority = Arc::new(EvidenceAuthority::new(vec![9; 32], [provider.clone()]).unwrap());
    KnowledgePlugin::new(
        registry,
        authority,
        provider,
        KnowledgePluginConfig::default(),
    )
    .unwrap()
}

#[tokio::test]
async fn reference_only_search_does_not_persist_snippets() {
    let plugin = reference_only_plugin();
    let mut tools: Vec<Arc<dyn Tool>> = vec![];
    plugin.register_tools(&mut tools);
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "search-1",
            KNOWLEDGE_SEARCH_TOOL_NAME,
            r#"{"query":"old","source_id":"history-fixture"}"#,
        ),
        MockResponse::text("done"),
    ]));
    let mut options = AgentHarnessOptions::new("test-model");
    options.tools = tools;
    let harness =
        AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, Arc::new(NoOpEnv), options).await;
    harness
        .run(RunRequest::from_text("search history").with_extension(access()))
        .await
        .unwrap();

    let context = harness.build_context().await.unwrap();
    let result = context.messages.iter().find_map(|message| match message {
        AgentMessage::ToolResult(result) => Some(result),
        _ => None,
    });
    let result = result.expect("search result must be projected");
    assert_eq!(result.projection.kind, ToolProjectionKind::Ephemeral);
    let serialized = serde_json::to_string(result).unwrap();
    assert!(!serialized.contains("PRIVATE HISTORY SNIPPET"));
    assert!(serialized.contains("turn-1"));
}

#[tokio::test]
async fn unscoped_search_federates_visible_sources() {
    let plugin = knowledge_plugin(KnowledgePluginConfig::default());
    let mut tools: Vec<Arc<dyn Tool>> = vec![];
    plugin.register_tools(&mut tools);
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "search-1",
            KNOWLEDGE_SEARCH_TOOL_NAME,
            r#"{"query":"help"}"#,
        ),
        MockResponse::text("done"),
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
        .run(RunRequest::from_text("search all knowledge").with_extension(access()))
        .await
        .unwrap();

    let requests = client.captured_requests.lock().unwrap();
    let result_text = requests[1]
        .messages()
        .iter()
        .filter_map(|message| match message {
            Message::Tool { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|block| match block {
                        RequestContent::Text(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect::<String>();
    assert!(result_text.contains("refund"));
    assert!(result_text.contains("preference"));
    assert!(result_text.contains("source_failures"));
}

#[tokio::test]
async fn federated_search_uses_the_most_restrictive_session_projection() {
    let plugin = mixed_projection_plugin();
    let mut tools: Vec<Arc<dyn Tool>> = vec![];
    plugin.register_tools(&mut tools);
    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use("search-1", KNOWLEDGE_SEARCH_TOOL_NAME, r#"{"query":"old"}"#),
        MockResponse::text("done"),
    ]));
    let mut options = AgentHarnessOptions::new("test-model");
    options.tools = tools;
    let harness =
        AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, Arc::new(NoOpEnv), options).await;
    harness
        .run(RunRequest::from_text("search mixed knowledge").with_extension(access()))
        .await
        .unwrap();

    let context = harness.build_context().await.unwrap();
    let result = context.messages.iter().find_map(|message| match message {
        AgentMessage::ToolResult(result) => Some(result),
        _ => None,
    });
    let result = result.expect("federated search result must be projected");
    assert_eq!(result.projection.kind, ToolProjectionKind::Ephemeral);
    let serialized = serde_json::to_string(result).unwrap();
    assert!(!serialized.contains("PRIVATE HISTORY SNIPPET"));
    assert!(!serialized.contains("Thirty day refund window"));
    assert!(serialized.contains("turn-1"));
    assert!(serialized.contains("refund"));
}

#[tokio::test]
async fn agent_searches_reads_and_persists_only_evidence_projection() {
    let plugin = knowledge_plugin(KnowledgePluginConfig::default());
    let mut tools: Vec<Arc<dyn Tool>> = vec![];
    let mut hooks = HarnessHooks::none();
    plugin.register_tools(&mut tools);
    plugin.register_hooks(&mut hooks);

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
    options.hooks = hooks;
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

#[tokio::test]
async fn strict_policy_rejects_an_uncited_answer_after_reading_evidence() {
    let plugin = knowledge_plugin(KnowledgePluginConfig {
        citation_policy: mixed_citation_policy(),
        ..KnowledgePluginConfig::default()
    });
    let mut tools: Vec<Arc<dyn Tool>> = vec![];
    let mut hooks = HarnessHooks::none();
    plugin.register_tools(&mut tools);
    plugin.register_hooks(&mut hooks);

    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "read-1",
            KNOWLEDGE_READ_TOOL_NAME,
            r#"{"reference":{"source_id":"docs","item_id":"refund","revision":"rev-1"}}"#,
        ),
        MockResponse::text("Refunds are available for thirty days."),
    ]));
    let mut options = AgentHarnessOptions::new("test-model");
    options.tools = tools;
    options.hooks = hooks;
    let harness =
        AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, Arc::new(NoOpEnv), options).await;

    harness
        .run(RunRequest::from_text("What is the refund window?").with_extension(access()))
        .await
        .unwrap();

    assert!(
        harness
            .state()
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("missing_knowledge_citation"))
    );
    let context = harness.build_context().await.unwrap();
    assert!(
        !context.messages.iter().any(|message| matches!(
            message,
            AgentMessage::Assistant(message)
                if message.kind == llm_harness_types::AssistantMessageKind::FinalAnswer
        )),
        "the uncited candidate must not be persisted as a final answer"
    );
}

#[tokio::test]
async fn optional_memory_evidence_allows_a_silent_personalized_answer() {
    let plugin = knowledge_plugin(KnowledgePluginConfig {
        citation_policy: mixed_citation_policy(),
        ..KnowledgePluginConfig::default()
    });
    let mut tools: Vec<Arc<dyn Tool>> = vec![];
    let mut hooks = HarnessHooks::none();
    plugin.register_tools(&mut tools);
    plugin.register_hooks(&mut hooks);

    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "read-memory",
            KNOWLEDGE_READ_TOOL_NAME,
            r#"{"reference":{"source_id":"memory","item_id":"preference","revision":"rev-1"}}"#,
        ),
        MockResponse::text("Here is a concise explanation."),
    ]));
    let mut options = AgentHarnessOptions::new("test-model");
    options.tools = tools;
    options.hooks = hooks;
    let harness =
        AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, Arc::new(NoOpEnv), options).await;

    harness
        .run(RunRequest::from_text("Explain this briefly.").with_extension(access()))
        .await
        .unwrap();

    assert!(harness.state().error_message.is_none());
    let context = harness.build_context().await.unwrap();
    assert!(context.messages.iter().any(|message| matches!(
        message,
        AgentMessage::Assistant(message)
            if message.kind == llm_harness_types::AssistantMessageKind::FinalAnswer
    )));
}

#[tokio::test]
async fn mixed_sources_still_require_the_strict_source_citation() {
    let plugin = knowledge_plugin(KnowledgePluginConfig {
        citation_policy: mixed_citation_policy(),
        ..KnowledgePluginConfig::default()
    });
    let mut tools: Vec<Arc<dyn Tool>> = vec![];
    let mut hooks = HarnessHooks::none();
    plugin.register_tools(&mut tools);
    plugin.register_hooks(&mut hooks);

    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::tool_use(
            "read-memory",
            KNOWLEDGE_READ_TOOL_NAME,
            r#"{"reference":{"source_id":"memory","item_id":"preference","revision":"rev-1"}}"#,
        ),
        MockResponse::tool_use(
            "read-course",
            KNOWLEDGE_READ_TOOL_NAME,
            r#"{"reference":{"source_id":"docs","item_id":"refund","revision":"rev-1"}}"#,
        ),
        MockResponse::text("Refunds are available for thirty days."),
    ]));
    let mut options = AgentHarnessOptions::new("test-model");
    options.tools = tools;
    options.hooks = hooks;
    let harness =
        AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, Arc::new(NoOpEnv), options).await;

    harness
        .run(RunRequest::from_text("Answer concisely.").with_extension(access()))
        .await
        .unwrap();

    assert!(
        harness
            .state()
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("missing_knowledge_citation"))
    );
}
