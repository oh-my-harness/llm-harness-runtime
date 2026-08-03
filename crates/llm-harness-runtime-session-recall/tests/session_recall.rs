use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use chrono::Utc;
use futures::future::BoxFuture;
use llm_adapter::types::{Message, RequestContent};
use llm_harness_agent::{
    AgentHarness, AgentHarnessOptions, HarnessHooks, InMemorySessionRepo, ObservedSessionRepo,
    Plugin, Session, SessionDurability, SessionLifecycle, SessionRecallPolicy, SessionRecallScope,
    SessionRecallSettings, SessionRepo, session::ForkOptions,
};
use llm_harness_loop::{
    LlmClient,
    test_utils::{MockLlmClient, MockResponse, NoOpEnv},
};
use llm_harness_runtime_knowledge::contract::{SourceContractCase, verify_source_contract};
use llm_harness_runtime_knowledge::{
    AuthorizationDecision, ContentSelector, KnowledgeAccessContext, KnowledgeAccessControl,
    KnowledgeAction, KnowledgeAuthorizer, KnowledgeError, KnowledgeErrorCode, KnowledgeReadRequest,
    KnowledgeRequestContext, KnowledgeResourceRef, KnowledgeScope, KnowledgeSearchProjection,
    KnowledgeSource, PrincipalRef, SourceSearchRequest,
};
use llm_harness_runtime_session_recall::*;
use llm_harness_types::{
    AgentContext, AgentError, AgentMessage, AssistantMessage, AssistantMessageKind, ContentBlock,
    DataBlock, EntryId, RunContext, RunRequest, StopReason, TransformContextCtx, UserMessage,
};
use tokio_util::sync::CancellationToken;

struct SubjectAuthorizer;

struct RevocableAuthorizer {
    allowed: Arc<AtomicBool>,
}

impl KnowledgeAuthorizer for SubjectAuthorizer {
    fn authorize<'a>(
        &'a self,
        access: &'a KnowledgeAccessContext,
        _action: KnowledgeAction,
        _resource: KnowledgeResourceRef<'a>,
    ) -> BoxFuture<'a, Result<AuthorizationDecision, KnowledgeError>> {
        Box::pin(async move {
            Ok(if access.principal.subject == "allowed" {
                AuthorizationDecision::Allow
            } else {
                AuthorizationDecision::Deny
            })
        })
    }
}

impl KnowledgeAuthorizer for RevocableAuthorizer {
    fn authorize<'a>(
        &'a self,
        _access: &'a KnowledgeAccessContext,
        _action: KnowledgeAction,
        _resource: KnowledgeResourceRef<'a>,
    ) -> BoxFuture<'a, Result<AuthorizationDecision, KnowledgeError>> {
        Box::pin(async move {
            Ok(if self.allowed.load(Ordering::SeqCst) {
                AuthorizationDecision::Allow
            } else {
                AuthorizationDecision::Deny
            })
        })
    }
}

fn recall_scope(namespace: &str) -> SessionRecallScope {
    SessionRecallScope::new(namespace)
}

fn access(namespace: &str, subject: &str) -> KnowledgeAccessContext {
    KnowledgeAccessContext::new(
        KnowledgeScope::new(namespace),
        PrincipalRef::new(subject, "user"),
    )
}

fn partitioned_recall_scope() -> SessionRecallScope {
    SessionRecallScope {
        namespace: "history".into(),
        tenant: Some("tenant-a".into()),
        project: Some("project-a".into()),
        attributes: BTreeMap::from([("region".into(), "apac".into())]),
    }
}

fn partitioned_access(subject: &str) -> KnowledgeAccessContext {
    let mut scope = KnowledgeScope::new("history");
    scope.tenant = Some("tenant-a".into());
    scope.project = Some("project-a".into());
    scope.attributes = BTreeMap::from([("region".into(), "apac".into())]);
    KnowledgeAccessContext::new(scope, PrincipalRef::new(subject, "user"))
}

fn user(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        content: vec![
            ContentBlock::Text { text: text.into() },
            ContentBlock::Thinking {
                thinking: "private-user-reasoning".into(),
                signature: None,
            },
        ],
        timestamp: Utc::now(),
    })
}

fn assistant(text: &str) -> AgentMessage {
    AgentMessage::Assistant(AssistantMessage {
        kind: AssistantMessageKind::FinalAnswer,
        message_id: "message-1".into(),
        turn_id: "turn-1".into(),
        content: vec![
            ContentBlock::Text { text: text.into() },
            ContentBlock::Thinking {
                thinking: "private-assistant-reasoning".into(),
                signature: None,
            },
            ContentBlock::ToolUse {
                id: "tool-1".into(),
                name: "secret-tool".into(),
                input: serde_json::json!({"secret": "private-tool-input"}),
            },
        ],
        stop_reason: Some(StopReason::EndTurn),
        timestamp: Utc::now(),
        provider: None,
        api: None,
        model: None,
        usage: None,
        error_message: None,
    })
}

async fn create_history(
    repo: &InMemorySessionRepo,
    namespace: &str,
    question: &str,
    answer: &str,
) -> (Session, String) {
    let storage = repo
        .create(llm_harness_agent::session::CreateSessionOptions {
            name: Some("Aurora planning".into()),
            durability: Some(SessionDurability::Durable),
            recall_policy: SessionRecallPolicy::Enabled,
            recall_scope: Some(recall_scope(namespace)),
            ..Default::default()
        })
        .await
        .unwrap();
    let session = Session::new(storage);
    session.append_message(user(question)).await.unwrap();
    session.append_message(assistant(answer)).await.unwrap();
    let id = session.metadata().await.unwrap().id;
    (session, id)
}

fn stack(
    repo: Arc<InMemorySessionRepo>,
) -> (
    Arc<InMemorySessionRecallIndex>,
    SessionRecallProjector,
    Arc<SessionRecallKnowledgeSource>,
) {
    let index = Arc::new(InMemorySessionRecallIndex::new());
    let control = Arc::new(KnowledgeAccessControl::new(Arc::new(SubjectAuthorizer)));
    let projector = SessionRecallProjector::new(repo.clone(), index.clone());
    let service = Arc::new(SessionRecallService::new(repo, index.clone(), control));
    let source = Arc::new(SessionRecallKnowledgeSource::new(service));
    (index, projector, source)
}

fn data_text(blocks: &[DataBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            DataBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn search_refs(
    source: &SessionRecallKnowledgeSource,
    access: &KnowledgeAccessContext,
    query: &str,
) -> Vec<SessionRecallRef> {
    let run = RunContext::new(RunRequest::from_text(query));
    source
        .search(
            KnowledgeRequestContext { run: &run, access },
            SourceSearchRequest {
                query: query.into(),
                filters: vec![],
                limit: 20,
                cursor: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .hits
        .iter()
        .map(|hit| from_knowledge_ref(&hit.reference).unwrap())
        .collect()
}

#[test]
fn typed_reference_round_trips_as_opaque_id() {
    let reference = SessionRecallRef {
        session_id: "session-a".into(),
        user_entry_id: EntryId::new(),
        assistant_entry_id: Some(EntryId::new()),
        branch_leaf_id: EntryId::new(),
        revision: 9,
    };
    let encoded = reference.encode().unwrap();
    assert_eq!(SessionRecallRef::decode(&encoded).unwrap(), reference);
    assert!(!encoded.contains("session-a"));

    let knowledge = to_knowledge_ref(&reference).unwrap();
    assert_eq!(from_knowledge_ref(&knowledge).unwrap(), reference);
}

#[tokio::test]
async fn source_search_and_read_revalidate_authority_and_exclude_private_blocks() {
    let repo = Arc::new(InMemorySessionRepo::new());
    let (session, _) = create_history(
        &repo,
        "team-a",
        "What is the Aurora launch schedule?",
        "The launch deadline is Friday.",
    )
    .await;
    let (_, projector, source) = stack(repo);
    projector.rebuild(CancellationToken::new()).await.unwrap();

    assert_eq!(
        source.search_projection(),
        KnowledgeSearchProjection::ReferenceOnly
    );
    let run = RunContext::new(RunRequest::from_text("Aurora deadline"));
    let allowed = access("team-a", "allowed");
    let page = source
        .search(
            KnowledgeRequestContext {
                run: &run,
                access: &allowed,
            },
            SourceSearchRequest {
                query: "Aurora deadline".into(),
                filters: vec![],
                limit: 5,
                cursor: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(page.hits.len(), 1);
    assert!(!page.hits[0].snippet.contains("private-"));
    let metadata = serde_json::to_string(&page.hits[0].metadata).unwrap();
    assert!(!metadata.contains("deadline is Friday"));

    let old_reference = page.hits[0].reference.clone();
    let content = source
        .read(
            KnowledgeRequestContext {
                run: &run,
                access: &allowed,
            },
            KnowledgeReadRequest {
                reference: old_reference.clone(),
                selector: ContentSelector::Document,
                max_bytes: 4096,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let text = data_text(&content.blocks);
    assert!(text.contains("Aurora launch schedule"));
    assert!(text.contains("deadline is Friday"));
    assert!(!text.contains("private-"));

    session
        .storage()
        .update_metadata_name(Some("Renamed".into()))
        .await
        .unwrap();
    let stale = source
        .read(
            KnowledgeRequestContext {
                run: &run,
                access: &allowed,
            },
            KnowledgeReadRequest {
                reference: old_reference,
                selector: ContentSelector::Document,
                max_bytes: 4096,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(stale.code(), KnowledgeErrorCode::StaleReference);
}

#[tokio::test]
async fn stale_index_cannot_leak_archived_deleted_or_cross_scope_sessions() {
    let repo = Arc::new(InMemorySessionRepo::new());
    let (session, session_id) =
        create_history(&repo, "team-a", "Aurora security review", "Review complete").await;
    let (_, projector, source) = stack(repo.clone());
    projector.rebuild(CancellationToken::new()).await.unwrap();
    let run = RunContext::new(RunRequest::from_text("Aurora"));

    let other_scope = source
        .search(
            KnowledgeRequestContext {
                run: &run,
                access: &access("team-b", "allowed"),
            },
            SourceSearchRequest {
                query: "Aurora".into(),
                filters: vec![],
                limit: 5,
                cursor: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(other_scope.hits.is_empty());

    let denied = source
        .search(
            KnowledgeRequestContext {
                run: &run,
                access: &access("team-a", "denied"),
            },
            SourceSearchRequest {
                query: "Aurora".into(),
                filters: vec![],
                limit: 5,
                cursor: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(denied.code(), KnowledgeErrorCode::Unauthorized);

    session
        .update_recall_settings(SessionRecallSettings {
            durability: SessionDurability::Durable,
            lifecycle: SessionLifecycle::Archived,
            policy: SessionRecallPolicy::Enabled,
            scope: Some(recall_scope("team-a")),
        })
        .await
        .unwrap();
    let archived = source
        .search(
            KnowledgeRequestContext {
                run: &run,
                access: &access("team-a", "allowed"),
            },
            SourceSearchRequest {
                query: "Aurora".into(),
                filters: vec![],
                limit: 5,
                cursor: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(archived.hits.is_empty());

    repo.delete(&session_id).await.unwrap();
    let deleted = source
        .search(
            KnowledgeRequestContext {
                run: &run,
                access: &access("team-a", "allowed"),
            },
            SourceSearchRequest {
                query: "Aurora".into(),
                filters: vec![],
                limit: 5,
                cursor: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(deleted.hits.is_empty());
}

fn projection(session_id: &str, revision: u64, text: &str) -> SessionRecallProjection {
    let leaf = EntryId::new();
    SessionRecallProjection {
        session_id: session_id.into(),
        revision,
        scope: recall_scope("team-a"),
        documents: vec![SessionRecallDocument {
            reference: SessionRecallRef {
                session_id: session_id.into(),
                user_entry_id: EntryId::new(),
                assistant_entry_id: None,
                branch_leaf_id: leaf,
                revision,
            },
            title: None,
            user_text: text.into(),
            assistant_text: None,
            updated_at: Utc::now(),
        }],
    }
}

#[tokio::test]
async fn index_rejects_out_of_order_tasks_and_rebuild_abort_keeps_generation() {
    let index = Arc::new(InMemorySessionRecallIndex::new());
    assert!(
        index
            .replace_session(projection("s", 3, "new"))
            .await
            .unwrap()
    );
    assert!(index.remove_session("s".into(), 3, false).await.unwrap());
    assert!(
        !index
            .replace_session(projection("s", 2, "old"))
            .await
            .unwrap()
    );
    assert!(
        index
            .replace_session(projection("s", 4, "newer"))
            .await
            .unwrap()
    );
    assert!(index.remove_session("s".into(), 4, true).await.unwrap());
    assert!(
        !index
            .replace_session(projection("s", 5, "resurrected"))
            .await
            .unwrap()
    );

    index
        .replace_all(vec![projection("stable", 1, "stable marker")])
        .await
        .unwrap();
    let repo = Arc::new(InMemorySessionRepo::new());
    let projector = SessionRecallProjector::new(repo, index.clone());
    let abort = CancellationToken::new();
    abort.cancel();
    assert_eq!(
        projector.rebuild(abort).await.unwrap_err(),
        SessionRecallError::Aborted
    );
    assert_eq!(
        index.projected_revisions().await.unwrap().get("stable"),
        Some(&1)
    );
}

#[tokio::test]
async fn history_plugin_is_explicit_ephemeral_bounded_and_current_session_safe() {
    let repo = Arc::new(InMemorySessionRepo::new());
    let (_, historical_id) =
        create_history(&repo, "team-a", "Aurora launch owner", "The owner is Mei.").await;
    let (_, projector, source) = stack(repo);
    projector.rebuild(CancellationToken::new()).await.unwrap();

    let mut config = HistoryRecallPluginConfig::default();
    config.budget.max_total_bytes = 700;
    config.budget.max_total_tokens = 700;
    let plugin = HistoryRecallPlugin::new(source, config).unwrap();
    let mut hooks = HarnessHooks::none();
    plugin.register_hooks(&mut hooks);
    let hook = hooks.transform_context.pop().unwrap();

    let run = RunContext::new(
        RunRequest::from_text("Who owns Aurora?")
            .with_extension(access("team-a", "allowed"))
            .with_extension(HistoryRecallRequest::new("current-session")),
    );
    let transformed = hook
        .transform(TransformContextCtx {
            run: &run,
            turn_index: 0,
            abort: CancellationToken::new(),
            context: AgentContext {
                system_prompt: None,
                messages: run.initial_messages().to_vec(),
            },
        })
        .await
        .unwrap();
    assert_eq!(transformed.messages.len(), 2);
    let injected = match &transformed.messages[0] {
        AgentMessage::User(user) => user
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>(),
        _ => panic!("history injection must be an ephemeral user data message"),
    };
    assert!(injected.contains("untrusted data"));
    assert!(injected.contains("owner is Mei"));
    assert!(injected.len() <= 700);
    assert_eq!(run.initial_messages().len(), 1);

    let transformed_again = hook
        .transform(TransformContextCtx {
            run: &run,
            turn_index: 1,
            abort: CancellationToken::new(),
            context: transformed,
        })
        .await
        .unwrap();
    assert_eq!(transformed_again.messages.len(), 2);

    let excluded_run = RunContext::new(
        RunRequest::from_text("Who owns Aurora?")
            .with_extension(access("team-a", "allowed"))
            .with_extension(HistoryRecallRequest::new(historical_id)),
    );
    let excluded = hook
        .transform(TransformContextCtx {
            run: &excluded_run,
            turn_index: 0,
            abort: CancellationToken::new(),
            context: AgentContext {
                system_prompt: None,
                messages: excluded_run.initial_messages().to_vec(),
            },
        })
        .await
        .unwrap();
    assert_eq!(excluded.messages.len(), 1);

    let disabled_run = RunContext::new(
        RunRequest::from_text("Who owns Aurora?").with_extension(access("team-a", "allowed")),
    );
    let disabled = hook
        .transform(TransformContextCtx {
            run: &disabled_run,
            turn_index: 0,
            abort: CancellationToken::new(),
            context: AgentContext {
                system_prompt: None,
                messages: disabled_run.initial_messages().to_vec(),
            },
        })
        .await
        .unwrap();
    assert_eq!(disabled.messages.len(), 1);
}

#[tokio::test]
async fn history_plugin_observes_run_cancellation() {
    let repo = Arc::new(InMemorySessionRepo::new());
    let (_, projector, source) = stack(repo);
    projector.rebuild(CancellationToken::new()).await.unwrap();
    let plugin = HistoryRecallPlugin::new(
        source,
        HistoryRecallPluginConfig {
            failure_mode: HistoryRecallFailureMode::FailRun,
            ..Default::default()
        },
    )
    .unwrap();
    let mut hooks = HarnessHooks::none();
    plugin.register_hooks(&mut hooks);
    let run = RunContext::new(
        RunRequest::from_text("Aurora")
            .with_extension(access("team-a", "allowed"))
            .with_extension(HistoryRecallRequest::new("current")),
    );
    let abort = CancellationToken::new();
    abort.cancel();
    let result = hooks.transform_context[0]
        .transform(TransformContextCtx {
            run: &run,
            turn_index: 0,
            abort,
            context: AgentContext {
                system_prompt: None,
                messages: run.initial_messages().to_vec(),
            },
        })
        .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("cancelled recall must fail in strict mode"),
    };
    assert!(matches!(error, AgentError::Aborted));
}

#[tokio::test]
async fn observed_repo_updates_and_invalidates_projection_incrementally() {
    let authority = Arc::new(InMemorySessionRepo::new());
    let index = Arc::new(InMemorySessionRecallIndex::new());
    let control = Arc::new(KnowledgeAccessControl::new(Arc::new(SubjectAuthorizer)));
    let projector = Arc::new(SessionRecallProjector::new(
        authority.clone(),
        index.clone(),
    ));
    let observed = ObservedSessionRepo::new(authority.clone(), projector);
    let storage = observed
        .create(llm_harness_agent::session::CreateSessionOptions {
            durability: Some(SessionDurability::Durable),
            recall_policy: SessionRecallPolicy::Enabled,
            recall_scope: Some(recall_scope("team-a")),
            ..Default::default()
        })
        .await
        .unwrap();
    let session = Session::new(storage);
    session
        .append_message(user("incremental comet marker"))
        .await
        .unwrap();
    session
        .append_message(assistant("indexed immediately"))
        .await
        .unwrap();

    let service = Arc::new(SessionRecallService::new(authority, index, control));
    let source = SessionRecallKnowledgeSource::new(service);
    let run = RunContext::new(RunRequest::from_text("comet"));
    let allowed = access("team-a", "allowed");
    let search = || async {
        source
            .search(
                KnowledgeRequestContext {
                    run: &run,
                    access: &allowed,
                },
                SourceSearchRequest {
                    query: "comet".into(),
                    filters: vec![],
                    limit: 5,
                    cursor: None,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap()
    };
    assert_eq!(search().await.hits.len(), 1);

    session
        .update_recall_settings(SessionRecallSettings {
            durability: SessionDurability::Durable,
            lifecycle: SessionLifecycle::Archived,
            policy: SessionRecallPolicy::Enabled,
            scope: Some(recall_scope("team-a")),
        })
        .await
        .unwrap();
    assert!(search().await.hits.is_empty());
}

#[tokio::test]
async fn session_source_passes_common_knowledge_contract() {
    let repo = Arc::new(InMemorySessionRepo::new());
    let (session, _) = create_history(
        &repo,
        "team-a",
        "contract nebula marker",
        "contract response",
    )
    .await;
    let (_, projector, source) = stack(repo);
    projector.rebuild(CancellationToken::new()).await.unwrap();
    let run = RunContext::new(RunRequest::from_text("nebula"));
    let allowed = access("team-a", "allowed");
    let page = source
        .search(
            KnowledgeRequestContext {
                run: &run,
                access: &allowed,
            },
            SourceSearchRequest {
                query: "nebula".into(),
                filters: vec![],
                limit: 5,
                cursor: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let expected = page.hits[0].reference.clone();
    let typed = from_knowledge_ref(&expected).unwrap();
    let missing = to_knowledge_ref(&SessionRecallRef {
        session_id: "missing-session".into(),
        user_entry_id: EntryId::new(),
        assistant_entry_id: None,
        branch_leaf_id: EntryId::new(),
        revision: 1,
    })
    .unwrap();
    let stale = to_knowledge_ref(&SessionRecallRef {
        revision: typed.revision.saturating_sub(1),
        ..typed
    })
    .unwrap();
    verify_source_contract(
        source.as_ref(),
        &run,
        &SourceContractCase {
            allowed_access: allowed,
            denied_access: access("team-a", "denied"),
            search: SourceSearchRequest {
                query: "nebula".into(),
                filters: vec![],
                limit: 5,
                cursor: None,
            },
            expected_reference: expected,
            selector: ContentSelector::Document,
            max_bytes: 4096,
            missing_reference: missing,
            stale_reference: Some(stale),
        },
    )
    .await
    .unwrap();

    // Keep authority alive for the full contract invocation.
    assert!(session.metadata().await.unwrap().is_recallable());
}

#[tokio::test]
async fn active_branch_switch_reprojects_only_visible_turns() {
    let repo = Arc::new(InMemorySessionRepo::new());
    let storage = repo
        .create(llm_harness_agent::session::CreateSessionOptions {
            durability: Some(SessionDurability::Durable),
            recall_policy: SessionRecallPolicy::Enabled,
            recall_scope: Some(recall_scope("team-a")),
            ..Default::default()
        })
        .await
        .unwrap();
    let session = Session::new(storage);
    session
        .append_message(user("first visible turn"))
        .await
        .unwrap();
    let first_leaf = session
        .append_message(assistant("first answer"))
        .await
        .unwrap();
    session
        .append_message(user("second hidden-after-switch marker"))
        .await
        .unwrap();
    session
        .append_message(assistant("second answer"))
        .await
        .unwrap();
    let session_id = session.metadata().await.unwrap().id;
    let index = Arc::new(InMemorySessionRecallIndex::new());
    let projector = SessionRecallProjector::new(repo, index.clone());
    projector.rebuild(CancellationToken::new()).await.unwrap();
    let query = || SessionRecallIndexQuery {
        query: "hidden after switch".into(),
        scope: recall_scope("team-a"),
        exclude_session_id: None,
        limit: 5,
        max_snippet_bytes: 512,
    };
    assert_eq!(
        index
            .search(query(), CancellationToken::new())
            .await
            .unwrap()
            .len(),
        1
    );

    session.navigate_to(first_leaf).await.unwrap();
    projector
        .project_session(&session_id, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        index
            .search(query(), CancellationToken::new())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn authorization_is_checked_again_between_search_and_read() {
    let repo = Arc::new(InMemorySessionRepo::new());
    create_history(
        &repo,
        "team-a",
        "revocable quasar marker",
        "authorized once",
    )
    .await;
    let index = Arc::new(InMemorySessionRecallIndex::new());
    let projector = SessionRecallProjector::new(repo.clone(), index.clone());
    projector.rebuild(CancellationToken::new()).await.unwrap();
    let allowed_flag = Arc::new(AtomicBool::new(true));
    let control = Arc::new(KnowledgeAccessControl::new(Arc::new(RevocableAuthorizer {
        allowed: allowed_flag.clone(),
    })));
    let service = Arc::new(SessionRecallService::new(repo, index, control));
    let source = SessionRecallKnowledgeSource::new(service);
    let run = RunContext::new(RunRequest::from_text("quasar"));
    let access = access("team-a", "allowed");
    let page = source
        .search(
            KnowledgeRequestContext {
                run: &run,
                access: &access,
            },
            SourceSearchRequest {
                query: "quasar".into(),
                filters: vec![],
                limit: 5,
                cursor: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let reference = page.hits[0].reference.clone();
    allowed_flag.store(false, Ordering::SeqCst);
    let error = source
        .read(
            KnowledgeRequestContext {
                run: &run,
                access: &access,
            },
            KnowledgeReadRequest {
                reference,
                selector: ContentSelector::Document,
                max_bytes: 1024,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), KnowledgeErrorCode::Unauthorized);
}

#[tokio::test]
async fn every_configured_scope_dimension_and_principal_isolated() {
    let repo = Arc::new(InMemorySessionRepo::new());
    let storage = repo
        .create(llm_harness_agent::session::CreateSessionOptions {
            durability: Some(SessionDurability::Durable),
            recall_policy: SessionRecallPolicy::Enabled,
            recall_scope: Some(partitioned_recall_scope()),
            ..Default::default()
        })
        .await
        .unwrap();
    let session = Session::new(storage);
    session
        .append_message(user("partitioned helios marker"))
        .await
        .unwrap();
    session
        .append_message(assistant("visible only in the exact partition"))
        .await
        .unwrap();
    let (_, projector, source) = stack(repo);
    projector.rebuild(CancellationToken::new()).await.unwrap();

    assert_eq!(
        search_refs(&source, &partitioned_access("allowed"), "helios")
            .await
            .len(),
        1
    );

    let mut wrong_namespace = partitioned_access("allowed");
    wrong_namespace.scope.namespace = "other-history".into();
    let mut wrong_tenant = partitioned_access("allowed");
    wrong_tenant.scope.tenant = Some("tenant-b".into());
    let mut wrong_project = partitioned_access("allowed");
    wrong_project.scope.project = Some("project-b".into());
    let mut wrong_attributes = partitioned_access("allowed");
    wrong_attributes
        .scope
        .attributes
        .insert("region".into(), "emea".into());
    for mismatched in [
        wrong_namespace,
        wrong_tenant,
        wrong_project,
        wrong_attributes,
    ] {
        assert!(search_refs(&source, &mismatched, "helios").await.is_empty());
    }

    let run = RunContext::new(RunRequest::from_text("helios"));
    let denied = partitioned_access("another-user");
    let error = source
        .search(
            KnowledgeRequestContext {
                run: &run,
                access: &denied,
            },
            SourceSearchRequest {
                query: "helios".into(),
                filters: vec![],
                limit: 5,
                cursor: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), KnowledgeErrorCode::Unauthorized);
}

#[tokio::test]
async fn temporary_and_recall_disabled_sessions_never_enter_projection() {
    let repo = Arc::new(InMemorySessionRepo::new());
    let temporary_storage = repo
        .create(llm_harness_agent::session::CreateSessionOptions::default())
        .await
        .unwrap();
    let temporary = Session::new(temporary_storage);
    temporary
        .append_message(user("excluded orion temporary marker"))
        .await
        .unwrap();
    temporary
        .append_message(assistant("must not be recalled"))
        .await
        .unwrap();
    assert!(
        temporary
            .update_recall_settings(SessionRecallSettings {
                durability: SessionDurability::Temporary,
                lifecycle: SessionLifecycle::Active,
                policy: SessionRecallPolicy::Enabled,
                scope: Some(recall_scope("team-a")),
            })
            .await
            .is_err()
    );

    let disabled_storage = repo
        .create(llm_harness_agent::session::CreateSessionOptions {
            durability: Some(SessionDurability::Durable),
            ..Default::default()
        })
        .await
        .unwrap();
    let disabled = Session::new(disabled_storage);
    disabled
        .append_message(user("excluded orion durable marker"))
        .await
        .unwrap();
    disabled
        .append_message(assistant("recall remains disabled"))
        .await
        .unwrap();

    let (index, projector, source) = stack(repo);
    projector.rebuild(CancellationToken::new()).await.unwrap();
    assert!(index.projected_revisions().await.unwrap().is_empty());
    assert!(
        search_refs(&source, &access("team-a", "allowed"), "orion")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn rebuild_is_equivalent_and_reconcile_repairs_missed_notifications() {
    let repo = Arc::new(InMemorySessionRepo::new());
    let (_, first_session_id) =
        create_history(&repo, "team-a", "rebuild atlas marker", "first answer").await;
    let (index, projector, source) = stack(repo.clone());
    let allowed = access("team-a", "allowed");

    projector.rebuild(CancellationToken::new()).await.unwrap();
    let before = search_refs(&source, &allowed, "atlas").await;
    assert_eq!(before.len(), 1);
    index.reset().await.unwrap();
    assert!(search_refs(&source, &allowed, "atlas").await.is_empty());
    projector.rebuild(CancellationToken::new()).await.unwrap();
    assert_eq!(search_refs(&source, &allowed, "atlas").await, before);

    let (_, missed_session_id) =
        create_history(&repo, "team-a", "missed zephyr marker", "second answer").await;
    assert!(search_refs(&source, &allowed, "zephyr").await.is_empty());
    projector.reconcile(CancellationToken::new()).await.unwrap();
    let repaired = search_refs(&source, &allowed, "zephyr").await;
    assert_eq!(repaired.len(), 1);
    assert_eq!(repaired[0].session_id, missed_session_id);

    repo.delete(&first_session_id).await.unwrap();
    projector.reconcile(CancellationToken::new()).await.unwrap();
    assert!(
        !index
            .projected_revisions()
            .await
            .unwrap()
            .contains_key(&first_session_id)
    );
}

#[tokio::test]
async fn observed_fork_and_entry_deletion_update_projection() {
    let authority = Arc::new(InMemorySessionRepo::new());
    let index = Arc::new(InMemorySessionRecallIndex::new());
    let projector = Arc::new(SessionRecallProjector::new(
        authority.clone(),
        index.clone(),
    ));
    let observed = ObservedSessionRepo::new(authority.clone(), projector);
    let original_storage = observed
        .create(llm_harness_agent::session::CreateSessionOptions {
            durability: Some(SessionDurability::Durable),
            recall_policy: SessionRecallPolicy::Enabled,
            recall_scope: Some(recall_scope("team-a")),
            ..Default::default()
        })
        .await
        .unwrap();
    let original = Session::new(original_storage.clone());
    original
        .append_message(user("forkable nova marker"))
        .await
        .unwrap();
    let assistant_id = original
        .append_message(assistant("copied authoritative answer"))
        .await
        .unwrap();
    let original_id = original.metadata().await.unwrap().id;

    let forked_storage = observed
        .fork(&original_id, assistant_id, ForkOptions::default())
        .await
        .unwrap();
    forked_storage
        .update_recall_settings(SessionRecallSettings {
            durability: SessionDurability::Durable,
            lifecycle: SessionLifecycle::Active,
            policy: SessionRecallPolicy::Enabled,
            scope: Some(recall_scope("team-a")),
        })
        .await
        .unwrap();
    let forked_id = forked_storage.metadata().await.unwrap().id;

    let control = Arc::new(KnowledgeAccessControl::new(Arc::new(SubjectAuthorizer)));
    let service = Arc::new(SessionRecallService::new(authority, index, control));
    let source = SessionRecallKnowledgeSource::new(service);
    let allowed = access("team-a", "allowed");
    let mut ids = search_refs(&source, &allowed, "nova")
        .await
        .into_iter()
        .map(|reference| reference.session_id)
        .collect::<Vec<_>>();
    ids.sort();
    let mut expected = vec![original_id.clone(), forked_id.clone()];
    expected.sort();
    assert_eq!(ids, expected);

    original_storage
        .delete_entries(vec![assistant_id])
        .await
        .unwrap();
    let remaining = search_refs(&source, &allowed, "nova").await;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].session_id, forked_id);
}

#[tokio::test]
async fn harness_uses_recall_context_without_persisting_it_to_current_session() {
    let repo = Arc::new(InMemorySessionRepo::new());
    create_history(
        &repo,
        "team-a",
        "persistent history-only phrase",
        "Mei owns the launch.",
    )
    .await;
    let (_, projector, source) = stack(repo);
    projector.rebuild(CancellationToken::new()).await.unwrap();
    let plugin = HistoryRecallPlugin::new(source, HistoryRecallPluginConfig::default()).unwrap();
    let mut hooks = HarnessHooks::none();
    plugin.register_hooks(&mut hooks);
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text(
        "current answer",
    )]));
    let mut options = AgentHarnessOptions::new("test-model");
    options.hooks = hooks;
    let harness = AgentHarness::new_in_memory(
        client.clone() as Arc<dyn LlmClient>,
        Arc::new(NoOpEnv),
        options,
    )
    .await;
    harness
        .run(
            RunRequest::from_text("Who owns the launch?")
                .with_extension(access("team-a", "allowed"))
                .with_extension(HistoryRecallRequest::new("current-session")),
        )
        .await
        .unwrap();

    {
        let requests = client.captured_requests.lock().unwrap();
        assert!(requests[0].messages().iter().any(|message| {
            matches!(
                message,
                Message::User(content)
                    if content.iter().any(|block| matches!(
                        block,
                        RequestContent::Text(text) if text.contains("Mei owns the launch")
                    ))
            )
        }));
    }
    let persisted =
        serde_json::to_string(&harness.build_context().await.unwrap().messages).unwrap();
    assert!(!persisted.contains("Mei owns the launch"));
    assert!(!persisted.contains("runtime:history-recall"));
    assert!(persisted.contains("Who owns the launch"));
    assert!(persisted.contains("current answer"));
}
