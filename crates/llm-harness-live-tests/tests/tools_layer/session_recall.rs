//! Tests for SessionRecall (Spec §7).
//!
//! - `session_recall_search_finds_history`: a prior conversation is projected
//!   into the recall index, registered as a `SessionRecallKnowledgeSource` in
//!   `KnowledgeRegistry`, and a *new* session harness calls `knowledge_search`
//!   to discover the prior conversation.
//! - `projector_rebuilds_index`: writing two sessions and calling
//!   `projector.rebuild()` projects both into the index.

use std::sync::Arc;

use chrono::Utc;
use llm_harness_agent::{
    JsonlSessionRepo, Session, SessionRecallScope, SessionRepo, session::CreateSessionOptions,
};
use llm_harness_knowledge::access::{
    AllowAllAuthorizer, KnowledgeAccessContext, KnowledgeAccessControl, KnowledgeScope,
    PrincipalRef,
};
use llm_harness_knowledge::{
    EvidenceAuthority, EvidenceProviderId, KnowledgeCitationPolicy, KnowledgePlugin,
    KnowledgePluginConfig, KnowledgeRegistry, KnowledgeToolConfig,
};
use llm_harness_live_tests::skip_if_none;
use llm_harness_live_tests::{
    CapturedEvent, EventCapture, LiveProvider, MULTI_TURN, SINGLE_TURN, assert_event_emitted,
    assert_tool_was_called, make_harness_with, with_timeout,
};
use llm_harness_session_recall::{
    InMemorySessionRecallIndex, SessionRecallAccessContext, SessionRecallIndex,
    SessionRecallKnowledgeSource, SessionRecallProjector, SessionRecallService,
};
use llm_harness_types::{
    AgentMessage, AssistantMessage, AssistantMessageKind, ContentBlock, RunRequest, UserMessage,
};
use tokio_util::sync::CancellationToken;

// ── Fixtures ───────────────────────────────────────────────────────────

/// Build a recall-enabled `JsonlSessionRepo` rooted at `dir`.
fn make_repo(dir: &std::path::Path) -> JsonlSessionRepo {
    JsonlSessionRepo::new(dir)
}

/// Create a recall-enabled session and return the `Session` plus its ID.
async fn make_recall_session(repo: &Arc<dyn SessionRepo>, name: &str) -> (Session, String) {
    let storage = repo
        .create(CreateSessionOptions {
            name: Some(name.to_string()),
            recall_policy: llm_harness_agent::SessionRecallPolicy::Enabled,
            recall_scope: Some(SessionRecallScope::new("test")),
            ..Default::default()
        })
        .await
        .expect("session creation should succeed");
    let meta = storage.metadata().await.expect("metadata should load");
    let session = Session::new(storage);
    (session, meta.id)
}

/// Write a user→assistant conversation into `session`.
async fn write_conversation(session: &Session, user_text: &str, assistant_text: &str) {
    session
        .append_message(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: user_text.to_string(),
            }],
            timestamp: Utc::now(),
        }))
        .await
        .expect("append user message");
    session
        .append_message(AgentMessage::Assistant(AssistantMessage {
            kind: AssistantMessageKind::FinalAnswer,
            message_id: format!("msg_{}", Utc::now().timestamp_millis()),
            turn_id: format!("turn_{}", Utc::now().timestamp_millis()),
            content: vec![ContentBlock::Text {
                text: assistant_text.to_string(),
            }],
            stop_reason: Some(llm_harness_types::StopReason::EndTurn),
            timestamp: Utc::now(),
            provider: None,
            api: None,
            model: None,
            usage: None,
            error_message: None,
        }))
        .await
        .expect("append assistant message");
}

/// Build the knowledge access context used by both the registry and runs.
fn make_access_context() -> KnowledgeAccessContext {
    KnowledgeAccessContext::new(
        KnowledgeScope::new("default"),
        PrincipalRef::new("tester", "human"),
    )
}

// ── Tests ──────────────────────────────────────────────────────────────

/// A prior conversation (user asks about Rust, assistant responds) is
/// projected into the recall index. A *new* harness with a fresh session
/// then calls `knowledge_search` and finds the prior conversation.
#[tokio::test]
async fn session_recall_search_finds_history() {
    let providers = LiveProvider::from_env();
    let entry = providers.all_providers().into_iter().next();
    skip_if_none!(entry);
    let entry = entry.unwrap();

    // ── Phase 1: create the recall store, project a prior session ──────
    let recall_dir = tempfile::tempdir().unwrap();
    let repo: Arc<dyn SessionRepo> = Arc::new(make_repo(recall_dir.path()));
    let index = Arc::new(InMemorySessionRecallIndex::new());
    let projector = Arc::new(SessionRecallProjector::new(repo.clone(), index.clone()));

    let (prior_session, prior_id) = make_recall_session(&repo, "Rust Q&A").await;
    write_conversation(
        &prior_session,
        "What is Rust's borrow checker?",
        "The borrow checker enforces ownership rules at compile time, \
         preventing data races and use-after-free bugs.",
    )
    .await;
    drop(prior_session);

    // Project the prior session into the index.
    projector
        .project_session(&prior_id, CancellationToken::new())
        .await
        .expect("project_session should succeed");

    // ── Phase 2: build the knowledge stack with SessionRecallSource ────
    let access_control = Arc::new(KnowledgeAccessControl::new(Arc::new(AllowAllAuthorizer)));
    let service = Arc::new(SessionRecallService::new(
        repo.clone(),
        index.clone(),
        access_control.clone(),
    ));
    let source = Arc::new(SessionRecallKnowledgeSource::new(service));

    let registry = Arc::new(
        KnowledgeRegistry::builder(access_control.clone())
            .source(source.clone() as Arc<dyn llm_harness_knowledge::KnowledgeSource>)
            .build()
            .expect("registry build should succeed"),
    );
    let authority = Arc::new(
        EvidenceAuthority::new(
            vec![0x22u8; 32],
            [EvidenceProviderId("session-recall-provider".to_string())],
        )
        .expect("evidence authority creation should succeed"),
    );
    let plugin_config = KnowledgePluginConfig {
        tools: KnowledgeToolConfig::default(),
        citation_policy: KnowledgeCitationPolicy::default(),
    };
    let plugin = KnowledgePlugin::new(
        registry,
        authority,
        EvidenceProviderId("session-recall-provider".to_string()),
        plugin_config,
    )
    .expect("KnowledgePlugin creation should succeed");

    // ── Phase 3: new harness with a DIFFERENT session ──────────────────
    let (harness, _harness_tmp) = make_harness_with(entry, |b| b.install(&plugin)).await;
    let cap = EventCapture::subscribe(&harness);

    let access = make_access_context();
    let recall_ctx = SessionRecallAccessContext::new(SessionRecallScope::new("test"));

    let request = RunRequest::from_text(
        "Use the knowledge_search tool to search for 'Rust borrow checker'. \
         After you get the results, tell me what you found.",
    )
    .with_extension(access)
    .with_extension(recall_ctx);

    with_timeout(MULTI_TURN, harness.run(request))
        .await
        .expect("run should complete successfully");
    cap.settle().await;

    let events = cap.events();
    assert_tool_was_called(&events, "knowledge_search");
    assert_event_emitted(&events, &CapturedEvent::Settled);
}

/// Writing two sessions and calling `projector.rebuild()` projects both
/// into the index: `projected_revisions()` returns both session IDs.
#[tokio::test]
async fn projector_rebuilds_index() {
    let tmp = tempfile::tempdir().unwrap();
    let repo: Arc<dyn SessionRepo> = Arc::new(make_repo(tmp.path()));
    let index = Arc::new(InMemorySessionRecallIndex::new());
    let projector = SessionRecallProjector::new(repo.clone(), index.clone());

    // Session 1: Rust
    let (s1, id1) = make_recall_session(&repo, "Rust discussion").await;
    write_conversation(
        &s1,
        "Tell me about Rust ownership.",
        "Rust ownership ensures memory safety without a garbage collector.",
    )
    .await;
    drop(s1);

    // Session 2: Python
    let (s2, id2) = make_recall_session(&repo, "Python discussion").await;
    write_conversation(
        &s2,
        "What is Python good for?",
        "Python is great for data science, web development, and automation.",
    )
    .await;
    drop(s2);

    // Rebuild the entire index from authority.
    projector
        .rebuild(CancellationToken::new())
        .await
        .expect("rebuild should succeed");

    let revisions = index
        .projected_revisions()
        .await
        .expect("projected_revisions should succeed");

    assert!(
        revisions.contains_key(&id1),
        "index should contain session 1 after rebuild; got keys: {:?}",
        revisions.keys().collect::<Vec<_>>()
    );
    assert!(
        revisions.contains_key(&id2),
        "index should contain session 2 after rebuild; got keys: {:?}",
        revisions.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        revisions.len(),
        2,
        "index should have exactly 2 sessions after rebuild"
    );

    // Suppress unused-variable warning for MULTI_TURN (imported for parity).
    let _ = MULTI_TURN;
    let _ = SINGLE_TURN;
}
