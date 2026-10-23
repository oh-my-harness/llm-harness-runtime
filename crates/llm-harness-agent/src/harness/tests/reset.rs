//! Regression tests for the run-boundary reset surface:
//! `AgentHarness::reset_conversation`, `Session::clear`, and
//! `SessionStorage::clear`.
//!
//! A reset is the isolation boundary between controlled pipeline runs
//! (Agent Team V0 per-run isolation). It must:
//!
//! 1. erase the transcript (and only the transcript — never live config),
//! 2. erase the per-run replay signals (`replay_tool_started`,
//!    `replay_tool_names`, `replay_has_progress`), because inheriting them
//!    either leaks tool names across runs or re-enables replay of a
//!    generation that already performed side effects,
//! 3. fail closed when the harness is not idle (no partial clearing), and
//! 4. propagate the same semantics through every storage backend, with
//!    third-party/opaque backends rejecting `clear` rather than silently
//!    pretending to succeed.
//!
//! Everything here is deterministic: the mock LLM client is driven by
//! pre-scripted responses and non-idle phases are set directly, so no test
//! depends on timing or a live provider.

use super::*;

use crate::session::{
    CreateSessionOptions, ForkOptions, JsonlSessionRepo, ListSessionOptions, ObservedSessionRepo,
    SessionMetadata, SessionMutation, SessionMutationKind, SessionMutationObserver,
    SessionRecallPolicy, SessionRecallScope, SessionRepo, SessionStorage,
};
use tempfile::TempDir;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn user_msg(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        content: vec![ContentBlock::Text { text: text.into() }],
        timestamp: chrono::Utc::now(),
    })
}

fn block_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn message_text(message: &AgentMessage) -> String {
    match message {
        AgentMessage::User(message) => block_text(&message.content),
        AgentMessage::Assistant(message) => block_text(&message.content),
        _ => String::new(),
    }
}

/// Text of every message on the active branch, as the model would see it.
async fn context_texts(harness: &AgentHarness) -> Vec<String> {
    harness
        .build_context()
        .await
        .unwrap()
        .messages
        .iter()
        .map(message_text)
        .collect()
}

fn state_tool_names(state: &HarnessState) -> Vec<String> {
    state
        .tools
        .iter()
        .map(|tool| tool.name().to_owned())
        .collect()
}

fn path_ids(path: &[SessionEntry]) -> Vec<EntryId> {
    path.iter().map(|entry| entry.id).collect()
}

// ── 1. Transcript clearing ───────────────────────────────────────────────────

#[tokio::test]
async fn reset_conversation_clears_transcript_and_next_turn_starts_from_root() {
    let harness = make_harness(vec![MockResponse::text("first answer")]);
    harness.prompt("first question").await.unwrap();

    assert!(
        !harness.read_active_path().await.unwrap().is_empty(),
        "precondition: a prompt must persist transcript entries"
    );
    assert!(
        !harness.build_context().await.unwrap().messages.is_empty(),
        "precondition: the model must have seen at least one message"
    );
    assert!(
        harness
            .session_metadata()
            .await
            .unwrap()
            .active_cursor
            .is_some()
    );

    harness.reset_conversation().await.unwrap();

    assert!(
        harness.read_active_path().await.unwrap().is_empty(),
        "reset must leave no entry on the active path"
    );
    assert!(
        harness.build_context().await.unwrap().messages.is_empty(),
        "reset must leave no message in the model context"
    );
    assert_eq!(
        harness.session_metadata().await.unwrap().active_cursor,
        None,
        "the cursor must return to the empty root state"
    );
    assert_eq!(harness.state().phase, HarnessPhase::Idle);

    // A fresh turn after reset must attach at the root, not under stale entries.
    harness
        .append_message(user_msg("after reset"))
        .await
        .unwrap();
    let path = harness.read_active_path().await.unwrap();
    assert_eq!(path.len(), 1, "the new turn must be a single root entry");
    assert!(
        matches!(
            &path[0].payload,
            SessionEntryPayload::Message(AgentMessage::User(_))
        ),
        "the first entry after reset must be the new user message"
    );
    assert_eq!(path[0].parent_id, None);
    let context = harness.build_context().await.unwrap();
    assert_eq!(context.messages.len(), 1);
    assert_eq!(message_text(&context.messages[0]), "after reset");
}

// ── 2. Replay signals ────────────────────────────────────────────────────────

#[tokio::test]
async fn reset_conversation_clears_all_replay_signals() {
    let harness = make_harness(vec![MockResponse::text("answer")]);
    harness.prompt("go").await.unwrap();

    // Simulate a generation that started tools and was then interrupted:
    // exactly the state a new pipeline run must never inherit.
    {
        let mut inner = harness.inner.lock().unwrap();
        inner.state.replay_tool_started = true;
        inner.state.replay_tool_names.push("read_file".into());
        inner.state.replay_tool_names.push("write_file".into());
        inner.state.replay_has_progress = true;
    }
    let before = harness.state();
    assert!(before.replay_tool_started);
    assert_eq!(before.replay_tool_names.len(), 2);
    assert!(before.replay_has_progress);

    harness.reset_conversation().await.unwrap();

    let after = harness.state();
    assert!(
        !after.replay_tool_started,
        "the fail-closed replay gate must be cleared at a run boundary"
    );
    assert!(
        after.replay_tool_names.is_empty(),
        "stale tool names must not survive a run boundary: {:?}",
        after.replay_tool_names
    );
    assert!(
        !after.replay_has_progress,
        "a new run must be allowed to replay an untouched assignment"
    );
    assert_eq!(after.phase, HarnessPhase::Idle);
    assert!(harness.read_active_path().await.unwrap().is_empty());
}

// ── 3. Non-idle rejection is side-effect free ───────────────────────────────

#[tokio::test]
async fn reset_conversation_rejected_when_not_idle_without_side_effects() {
    let harness = make_harness(vec![MockResponse::text("kept answer")]);
    harness.prompt("kept question").await.unwrap();

    let before_ids = path_ids(&harness.read_active_path().await.unwrap());
    assert!(!before_ids.is_empty());

    // `phase` is driven by the loop; forcing it here reproduces exactly the
    // state the guard observes mid-run, without racing a real generation.
    {
        let mut inner = harness.inner.lock().unwrap();
        inner.state.phase = HarnessPhase::Turning;
        inner.state.replay_tool_started = true;
        inner.state.replay_tool_names.push("read_file".into());
        inner.state.replay_has_progress = true;
    }

    let error = harness.reset_conversation().await.unwrap_err();
    match error {
        HarnessError::NotIdle(phase) => assert_eq!(phase, HarnessPhase::Turning),
        other => panic!("expected HarnessError::NotIdle, got {other:?}"),
    }

    // A rejected reset must not clear or partially mutate anything.
    assert_eq!(
        path_ids(&harness.read_active_path().await.unwrap()),
        before_ids,
        "a rejected reset must leave the transcript untouched"
    );
    let state = harness.state();
    assert_eq!(state.phase, HarnessPhase::Turning);
    assert!(state.replay_tool_started);
    assert_eq!(state.replay_tool_names, vec!["read_file".to_string()]);
    assert!(state.replay_has_progress);
}

// ── 3b. Reset claims the harness atomically ────────────────────────────────

/// Storage that parks inside `clear`, so the test can observe the harness
/// while the transcript is being erased.
struct GatedClearStorage {
    inner: Arc<dyn SessionStorage>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl SessionStorage for GatedClearStorage {
    fn metadata(&self) -> futures::future::BoxFuture<'_, Result<SessionMetadata, SessionError>> {
        self.inner.metadata()
    }

    fn create_entry_id(&self) -> EntryId {
        self.inner.create_entry_id()
    }

    fn append_entry(
        &self,
        entry: SessionEntry,
    ) -> futures::future::BoxFuture<'_, Result<(), SessionError>> {
        self.inner.append_entry(entry)
    }

    fn get_entry(
        &self,
        id: EntryId,
    ) -> futures::future::BoxFuture<'_, Result<Option<SessionEntry>, SessionError>> {
        self.inner.get_entry(id)
    }

    fn children(
        &self,
        parent: EntryId,
    ) -> futures::future::BoxFuture<'_, Result<Vec<SessionEntry>, SessionError>> {
        self.inner.children(parent)
    }

    fn all_leaves(&self) -> futures::future::BoxFuture<'_, Result<Vec<EntryId>, SessionError>> {
        self.inner.all_leaves()
    }

    fn active_cursor(
        &self,
    ) -> futures::future::BoxFuture<'_, Result<Option<EntryId>, SessionError>> {
        self.inner.active_cursor()
    }

    fn set_active_cursor(
        &self,
        id: EntryId,
    ) -> futures::future::BoxFuture<'_, Result<(), SessionError>> {
        self.inner.set_active_cursor(id)
    }

    fn path_to_root(
        &self,
        target: EntryId,
    ) -> futures::future::BoxFuture<'_, Result<Vec<SessionEntry>, SessionError>> {
        self.inner.path_to_root(target)
    }

    fn common_ancestor(
        &self,
        a: EntryId,
        b: EntryId,
    ) -> futures::future::BoxFuture<'_, Result<Option<EntryId>, SessionError>> {
        self.inner.common_ancestor(a, b)
    }

    fn label_at(
        &self,
        id: EntryId,
    ) -> futures::future::BoxFuture<'_, Result<Option<String>, SessionError>> {
        self.inner.label_at(id)
    }

    fn paths_to_all_leaves(
        &self,
    ) -> futures::future::BoxFuture<'_, Result<Vec<Vec<SessionEntry>>, SessionError>> {
        self.inner.paths_to_all_leaves()
    }

    fn find_entries_by_type(
        &self,
        kind: SessionEntryKind,
    ) -> futures::future::BoxFuture<'_, Result<Vec<EntryId>, SessionError>> {
        self.inner.find_entries_by_type(kind)
    }

    fn update_metadata_name(
        &self,
        name: Option<String>,
    ) -> futures::future::BoxFuture<'_, Result<(), SessionError>> {
        self.inner.update_metadata_name(name)
    }

    fn update_metadata_model(
        &self,
        model: Option<String>,
    ) -> futures::future::BoxFuture<'_, Result<(), SessionError>> {
        self.inner.update_metadata_model(model)
    }

    fn update_recall_settings(
        &self,
        settings: crate::session::SessionRecallSettings,
    ) -> futures::future::BoxFuture<'_, Result<(), SessionError>> {
        self.inner.update_recall_settings(settings)
    }

    fn delete_entries(
        &self,
        ids: Vec<EntryId>,
    ) -> futures::future::BoxFuture<'_, Result<(), SessionError>> {
        self.inner.delete_entries(ids)
    }

    fn all_entries(
        &self,
    ) -> futures::future::BoxFuture<'_, Result<Vec<SessionEntry>, SessionError>> {
        self.inner.all_entries()
    }

    fn clear(&self) -> futures::future::BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            self.inner.clear().await
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_claims_the_harness_against_concurrent_runs() {
    let inner_repo = InMemorySessionRepo::new();
    let inner = inner_repo
        .create(CreateSessionOptions::default())
        .await
        .unwrap();
    Session::new(inner.clone())
        .append_message(user_msg("run one"))
        .await
        .unwrap();

    let gated = Arc::new(GatedClearStorage {
        inner: inner.clone(),
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    });
    let harness = Arc::new(AgentHarness::with_session(
        Arc::new(MockLlmClient::new(vec![MockResponse::text("must not run")]))
            as Arc<dyn LlmClient>,
        Arc::new(NoOpEnv),
        Session::new(gated.clone() as Arc<dyn SessionStorage>),
        AgentHarnessOptions::new("test-model"),
    ));

    let reset_harness = harness.clone();
    let reset = tokio::spawn(async move { reset_harness.reset_conversation().await });
    gated.entered.notified().await;

    // The clear is still in flight: the harness must already advertise a
    // non-idle phase, and a concurrent run must be rejected *before* it can
    // write into the transcript being erased.
    assert_eq!(harness.state().phase, HarnessPhase::Resetting);
    match harness.prompt("run two").await.unwrap_err() {
        HarnessError::NotIdle(phase) => assert_eq!(phase, HarnessPhase::Resetting),
        other => panic!("expected HarnessError::NotIdle, got {other:?}"),
    }
    assert_eq!(
        harness.read_active_path().await.unwrap().len(),
        1,
        "a rejected run must not append while the reset is in flight"
    );

    gated.release.notify_one();
    reset.await.unwrap().unwrap();

    assert!(harness.read_active_path().await.unwrap().is_empty());
    assert_eq!(harness.state().phase, HarnessPhase::Idle);
    assert!(!harness.state().replay_tool_started);
}

// ── 3b2. Queued cross-run state is cleared ────────────────────────────────

#[tokio::test]
async fn reset_clears_queued_next_turn_and_salvaged_steer_state() {
    let harness = make_harness(vec![
        MockResponse::text("first answer"),
        MockResponse::text("second answer"),
    ]);
    harness.prompt("first question").await.unwrap();

    // Emulate the two channels the run teardown leaves behind: a next-turn
    // buffer (delivered at the next `run()`) and the salvage flag the
    // scheduling layer reads to requeue inbox work.
    harness.next_turn("LEFTOVER-FROM-RUN-ONE");
    harness.inner.lock().unwrap().state.salvaged_steers = true;
    assert!(harness.has_queued_messages());
    assert!(harness.state().salvaged_steers);

    harness.reset_conversation().await.unwrap();

    assert!(
        !harness.has_queued_messages(),
        "a run boundary must not carry queued next-turn messages"
    );
    assert!(
        !harness.state().salvaged_steers,
        "a run boundary must not report a stale salvage"
    );
    assert!(
        !harness.take_salvaged_steers(),
        "the salvage flag must already be consumed by the reset"
    );
    assert!(harness.build_context().await.unwrap().messages.is_empty());

    harness.prompt("second question").await.unwrap();
    let texts: Vec<String> = harness
        .build_context()
        .await
        .unwrap()
        .messages
        .iter()
        .map(message_text)
        .collect();
    assert!(
        texts.iter().any(|text| text.contains("second question")),
        "the next run must still see its own input: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .all(|text| !text.contains("LEFTOVER-FROM-RUN-ONE")),
        "queued state from the previous run leaked into the next run: {texts:?}"
    );
}

// ── 3c. Unsupported storage rejects reset without changes ─────────────────

#[tokio::test]
async fn unsupported_reset_leaves_transcript_and_replay_state_untouched() {
    // `FailAfterNStorage` forwards every storage method except `clear`, so the
    // trait's fail-closed default is what the reset hits here.
    let harness =
        make_harness_failing_storage(usize::MAX, vec![MockResponse::text("first answer")]);
    harness.prompt("first question").await.unwrap();
    let before_ids = path_ids(&harness.read_active_path().await.unwrap());
    assert!(!before_ids.is_empty(), "precondition: transcript exists");
    {
        let mut inner = harness.inner.lock().unwrap();
        inner.state.replay_tool_started = true;
        inner.state.replay_tool_names.push("read_file".into());
        inner.state.replay_has_progress = true;
    }

    let error = harness.reset_conversation().await.unwrap_err();
    assert!(
        matches!(error, HarnessError::Session(SessionError::ClearUnsupported)),
        "expected the storage's fail-closed error, got {error:?}"
    );

    assert_eq!(
        path_ids(&harness.read_active_path().await.unwrap()),
        before_ids,
        "an unsupported clear must not remove or truncate transcript entries"
    );
    let state = harness.state();
    assert_eq!(
        state.phase,
        HarnessPhase::Idle,
        "the phase guard must release the harness after a failed reset"
    );
    assert!(
        state.replay_tool_started,
        "a failed reset must not open the replay gate"
    );
    assert_eq!(state.replay_tool_names, vec!["read_file".to_string()]);
    assert!(state.replay_has_progress);
}

// ── 4. Idempotency and configuration preservation ───────────────────────────

#[tokio::test]
async fn repeated_reset_is_idempotent_and_preserves_configuration() {
    let harness = make_harness(vec![MockResponse::text("answer")]);
    harness
        .set_model("configured-model".into(), None)
        .await
        .unwrap();
    harness.set_system_prompt(Some("configured system prompt".into()));
    harness
        .set_tools(vec![Arc::new(DummyTool::new("dummy-tool"))])
        .await
        .unwrap();
    harness.prompt("hello").await.unwrap();

    let before = harness.state();
    let before_config = (
        before.model.clone(),
        before.system_prompt.clone(),
        state_tool_names(&before),
        before.temperature,
    );
    assert_eq!(before_config.0, "configured-model");
    assert_eq!(before_config.1.as_deref(), Some("configured system prompt"));
    assert_eq!(before_config.2, vec!["dummy-tool".to_string()]);

    harness.reset_conversation().await.unwrap();
    harness.reset_conversation().await.unwrap();

    assert!(harness.read_active_path().await.unwrap().is_empty());
    assert!(harness.build_context().await.unwrap().messages.is_empty());
    assert_eq!(harness.state().phase, HarnessPhase::Idle);

    let after = harness.state();
    let after_config = (
        after.model.clone(),
        after.system_prompt.clone(),
        state_tool_names(&after),
        after.temperature,
    );
    assert_eq!(
        after_config, before_config,
        "reset must not touch model, system prompt, tools, or sampling config"
    );
}

// ── 5. Session::clear on in-memory storage ─────────────────────────────────

#[tokio::test]
async fn session_clear_empties_in_memory_storage_and_advances_recall_revision() {
    let repo = InMemorySessionRepo::new();
    // Recall must be explicitly enabled to observe revision advancement.
    let storage = repo
        .create(CreateSessionOptions {
            durability: Some(SessionDurability::Durable),
            recall_policy: SessionRecallPolicy::Enabled,
            recall_scope: Some(SessionRecallScope::new("reset-tests")),
            ..Default::default()
        })
        .await
        .unwrap();
    let session = Session::new(storage.clone());

    let first = session.append_message(user_msg("one")).await.unwrap();
    session.append_message(user_msg("two")).await.unwrap();

    assert_eq!(storage.all_entries().await.unwrap().len(), 2);
    assert_eq!(storage.children(first).await.unwrap().len(), 1);
    assert!(storage.active_cursor().await.unwrap().is_some());
    let before = session.metadata().await.unwrap();

    session.clear().await.unwrap();

    assert!(
        storage.all_entries().await.unwrap().is_empty(),
        "entries must be gone"
    );
    assert!(
        storage.children(first).await.unwrap().is_empty(),
        "the children index must be gone"
    );
    assert_eq!(storage.active_cursor().await.unwrap(), None);
    assert!(session.build_context().await.unwrap().messages.is_empty());

    let after = session.metadata().await.unwrap();
    assert_eq!(after.active_cursor, None);
    assert!(
        after.recall_revision > before.recall_revision,
        "clearing recall-visible content must advance the recall revision \
         (before {}, after {})",
        before.recall_revision,
        after.recall_revision
    );

    // The cleared session remains usable.
    session.append_message(user_msg("fresh")).await.unwrap();
    let context = session.build_context().await.unwrap();
    assert_eq!(context.messages.len(), 1);
    assert_eq!(message_text(&context.messages[0]), "fresh");
}

// ── 6. JSONL durability of clear ───────────────────────────────────────────

#[tokio::test]
async fn jsonl_clear_truncates_disk_and_survives_reload() {
    let tmp = TempDir::new().unwrap();
    let repo = JsonlSessionRepo::new(tmp.path());
    let storage = repo.create(CreateSessionOptions::default()).await.unwrap();
    let session = Session::new(storage);

    session.append_message(user_msg("disk one")).await.unwrap();
    session.append_message(user_msg("disk two")).await.unwrap();

    let id = session.metadata().await.unwrap().id.clone();
    let entries_path = tmp.path().join(&id).join("entries.jsonl");
    let meta_path = tmp.path().join(&id).join("meta.json");
    let raw_before = tokio::fs::read_to_string(&entries_path).await.unwrap();
    assert!(
        raw_before
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
            == 2,
        "precondition: two entries on disk"
    );

    session.clear().await.unwrap();

    // (a) The entries file must be truncated, not merely de-indexed.
    let raw_after = tokio::fs::read_to_string(&entries_path)
        .await
        .expect("clear must keep the entries file in place for later appends");
    assert!(
        raw_after.trim().is_empty(),
        "clear must truncate entries.jsonl, found {raw_after:?}"
    );

    // (b) The cursor must be persisted as None.
    let meta_bytes = tokio::fs::read(&meta_path).await.unwrap();
    let meta: SessionMetadata = serde_json::from_slice(&meta_bytes).unwrap();
    assert_eq!(meta.active_cursor, None);

    // (c) A fresh repo instance (cold cache) must load an empty session.
    let repo2 = JsonlSessionRepo::new(tmp.path());
    let reloaded = Session::new(repo2.open(&id).await.unwrap());
    assert!(reloaded.read_active_path().await.unwrap().is_empty());
    assert!(reloaded.build_context().await.unwrap().messages.is_empty());

    // (d) Appending after clear must be visible after another reload.
    reloaded
        .append_message(user_msg("after disk clear"))
        .await
        .unwrap();
    let repo3 = JsonlSessionRepo::new(tmp.path());
    let reloaded_again = Session::new(repo3.open(&id).await.unwrap());
    let context = reloaded_again.build_context().await.unwrap();
    assert_eq!(context.messages.len(), 1);
    assert_eq!(message_text(&context.messages[0]), "after disk clear");
}

#[tokio::test]
async fn jsonl_reset_io_failure_requires_successful_retry_before_next_run() {
    let tmp = TempDir::new().unwrap();
    let repo = JsonlSessionRepo::new(tmp.path());
    let storage = repo.create(CreateSessionOptions::default()).await.unwrap();
    let harness = AgentHarness::with_session(
        Arc::new(MockLlmClient::new(vec![
            MockResponse::text("first answer"),
            MockResponse::text("second answer"),
        ])) as Arc<dyn LlmClient>,
        Arc::new(NoOpEnv),
        Session::new(storage.clone()),
        AgentHarnessOptions::new("test-model"),
    );
    harness.prompt("first question").await.unwrap();
    let before_ids = path_ids(&storage.all_entries().await.unwrap());
    assert!(!before_ids.is_empty(), "precondition: transcript exists");
    let before_meta = storage.metadata().await.unwrap();
    assert!(before_meta.active_cursor.is_some());
    let entries_path = tmp.path().join(&before_meta.id).join("entries.jsonl");
    let raw_before = tokio::fs::read(&entries_path).await.unwrap();
    {
        let mut inner = harness.inner.lock().unwrap();
        inner.state.replay_tool_started = true;
        inner.state.replay_tool_names.push("read_file".into());
        inner.state.replay_has_progress = true;
    }

    // A directory at the temporary-file path fails the entries write on all
    // supported hosts, including privileged runners where chmod would not.
    // Metadata persistence still succeeds before this deterministic I/O error.
    let blocked_tmp = entries_path.with_extension("tmp");
    tokio::fs::create_dir(&blocked_tmp).await.unwrap();
    let error = harness.reset_conversation().await.unwrap_err();
    assert!(matches!(error, HarnessError::Session(SessionError::Io(_))));
    assert_eq!(harness.state().phase, HarnessPhase::Idle);
    assert!(harness.read_active_path().await.unwrap().is_empty());
    assert!(storage.metadata().await.unwrap().active_cursor.is_none());
    assert_eq!(
        tokio::fs::read(&entries_path).await.unwrap(),
        raw_before,
        "an empty active path must not be mistaken for removed disk entries"
    );
    let mut remaining_ids = path_ids(&storage.all_entries().await.unwrap());
    let mut expected_ids = before_ids;
    remaining_ids.sort();
    expected_ids.sort();
    assert_eq!(remaining_ids, expected_ids);
    let state = harness.state();
    assert!(state.replay_tool_started);
    assert!(state.replay_has_progress);
    assert_eq!(state.replay_tool_names, vec!["read_file".to_string()]);

    // The caller must stop on Err. Remove the I/O obstruction and successfully
    // retry the reset before authorizing another run.
    tokio::fs::remove_dir(&blocked_tmp).await.unwrap();
    harness.reset_conversation().await.unwrap();
    assert!(storage.all_entries().await.unwrap().is_empty());
    assert!(tokio::fs::read(&entries_path).await.unwrap().is_empty());
    assert!(!harness.state().replay_tool_started);
    let reloaded_repo = JsonlSessionRepo::new(tmp.path());
    let reloaded = reloaded_repo.open(&before_meta.id).await.unwrap();
    assert!(reloaded.all_entries().await.unwrap().is_empty());
    assert!(reloaded.active_cursor().await.unwrap().is_none());

    harness.prompt("second question").await.unwrap();
    let texts = context_texts(&harness).await;
    assert!(texts.iter().any(|text| text.contains("second question")));
    assert!(texts.iter().all(|text| !text.contains("first question")));
}

// ── 7. Observed storage forwards clear to its inner storage ─────────────────

#[derive(Default)]
struct RecordingObserver {
    mutations: std::sync::Mutex<Vec<SessionMutationKind>>,
}

impl SessionMutationObserver for RecordingObserver {
    fn observe<'a>(&'a self, mutation: SessionMutation) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            self.mutations.lock().unwrap().push(mutation.kind);
        })
    }
}

/// Repo stub that always hands out one pre-built storage, so a test can hold a
/// direct handle to the "inner" storage behind `ObservedSessionRepo` and assert
/// what actually happened to it.
struct FixedStorageRepo {
    storage: Arc<dyn SessionStorage>,
}

impl SessionRepo for FixedStorageRepo {
    fn create(
        &self,
        _opts: CreateSessionOptions,
    ) -> futures::future::BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>> {
        let storage = self.storage.clone();
        Box::pin(async move { Ok(storage) })
    }

    fn open(
        &self,
        id: &str,
    ) -> futures::future::BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>> {
        let id = id.to_owned();
        Box::pin(async move { Err(SessionError::SessionNotFound(id)) })
    }

    fn list(
        &self,
        _opts: ListSessionOptions,
    ) -> futures::future::BoxFuture<'_, Result<Vec<SessionMetadata>, SessionError>> {
        Box::pin(async { Ok(vec![]) })
    }

    fn delete(&self, _id: &str) -> futures::future::BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async { Ok(()) })
    }

    fn fork(
        &self,
        source_id: &str,
        _from_entry: EntryId,
        _opts: ForkOptions,
    ) -> futures::future::BoxFuture<'_, Result<Arc<dyn SessionStorage>, SessionError>> {
        let id = source_id.to_owned();
        Box::pin(async move { Err(SessionError::SessionNotFound(id)) })
    }
}

#[tokio::test]
async fn observed_storage_forwards_clear_to_inner_storage() {
    let inner_repo = InMemorySessionRepo::new();
    let inner = inner_repo
        .create(CreateSessionOptions::default())
        .await
        .unwrap();
    let observer = Arc::new(RecordingObserver::default());
    let repo = ObservedSessionRepo::new(
        Arc::new(FixedStorageRepo {
            storage: inner.clone(),
        }),
        observer.clone(),
    );
    let storage = repo.create(CreateSessionOptions::default()).await.unwrap();
    let session = Session::new(storage.clone());

    session
        .append_message(user_msg("observed one"))
        .await
        .unwrap();
    session
        .append_message(user_msg("observed two"))
        .await
        .unwrap();
    assert_eq!(storage.all_entries().await.unwrap().len(), 2);
    assert_eq!(inner.all_entries().await.unwrap().len(), 2);
    observer.mutations.lock().unwrap().clear();

    // Must not panic and must reach the wrapped storage.
    session.clear().await.unwrap();

    assert!(
        inner.all_entries().await.unwrap().is_empty(),
        "ObservedSessionStorage must forward clear to the wrapped storage"
    );
    assert_eq!(inner.active_cursor().await.unwrap(), None);
    assert!(storage.all_entries().await.unwrap().is_empty());
    assert_eq!(storage.active_cursor().await.unwrap(), None);
    assert!(session.build_context().await.unwrap().messages.is_empty());
    assert_eq!(
        observer.mutations.lock().unwrap().as_slice(),
        &[SessionMutationKind::Cleared],
        "a clear must invalidate derived projections exactly once"
    );
}

// ── 8. Opaque storage fails closed ─────────────────────────────────────────

#[tokio::test]
async fn storage_without_clear_support_fails_closed_and_keeps_entries() {
    let repo = InMemorySessionRepo::new();
    let inner = repo.create(CreateSessionOptions::default()).await.unwrap();
    let inner_session = Session::new(inner.clone());
    inner_session
        .append_message(user_msg("must survive"))
        .await
        .unwrap();
    let before_ids = path_ids(&inner_session.read_active_path().await.unwrap());
    assert_eq!(before_ids.len(), 1);

    // Forwards every method except `clear`, so the trait's fail-closed default
    // is the one under test. This mirrors a third-party backend that predates
    // the reset feature.
    let guarded: Arc<dyn SessionStorage> =
        Arc::new(super::FailAfterNStorage::new(inner.clone(), usize::MAX));
    let session = Session::new(guarded);

    let error = session.clear().await.unwrap_err();
    assert!(
        matches!(&error, SessionError::ClearUnsupported),
        "expected SessionError::ClearUnsupported, got {error:?}"
    );

    assert_eq!(
        path_ids(&session.read_active_path().await.unwrap()),
        before_ids,
        "a failed clear must not delete anything"
    );
    assert_eq!(
        path_ids(&inner_session.read_active_path().await.unwrap()),
        before_ids,
        "the backing storage must still hold the original entries"
    );
}

// ── 9. Multi-run isolation, with a non-vacuous control ─────────────────────

#[tokio::test]
async fn reset_isolates_runs_while_control_harness_retains_history() {
    // Isolated harness: reset between the two runs.
    let isolated = make_harness(vec![
        MockResponse::text("answer one"),
        MockResponse::text("answer two"),
    ]);
    isolated.prompt("run one question").await.unwrap();
    let run1_texts = context_texts(&isolated).await;
    assert!(
        run1_texts
            .iter()
            .any(|text| text.contains("run one question")),
        "precondition: run 1 must reach the context"
    );

    isolated.reset_conversation().await.unwrap();
    isolated.prompt("run two question").await.unwrap();

    let run2_texts = context_texts(&isolated).await;
    assert!(
        run2_texts
            .iter()
            .any(|text| text.contains("run two question")),
        "run 2 must be present"
    );
    assert!(
        run2_texts
            .iter()
            .all(|text| !text.contains("run one question")),
        "run 1 user content leaked into run 2: {run2_texts:?}"
    );
    assert!(
        run2_texts.iter().all(|text| !text.contains("answer one")),
        "run 1 assistant content leaked into run 2: {run2_texts:?}"
    );

    // Control: identical prompts without a reset, proving the assertions above
    // are not vacuously true because history is dropped for other reasons.
    let control = make_harness(vec![
        MockResponse::text("answer one"),
        MockResponse::text("answer two"),
    ]);
    control.prompt("run one question").await.unwrap();
    control.prompt("run two question").await.unwrap();

    let control_texts = context_texts(&control).await;
    assert!(
        control_texts
            .iter()
            .any(|text| text.contains("run one question")),
        "control: history must be retained when no reset happens"
    );
    assert!(
        control_texts
            .iter()
            .any(|text| text.contains("run two question")),
        "control: the second run must also be present"
    );
    assert!(
        control_texts.iter().any(|text| text.contains("answer one")),
        "control: the first assistant answer must also be retained"
    );
}
