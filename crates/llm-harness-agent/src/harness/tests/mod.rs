use super::*;

use crate::session::storage::SessionStorage;
use crate::session::types::{SessionEntry, SessionEntryKind, SessionMetadata};
use futures::future::BoxFuture;
use llm_harness_loop::test_utils::{MockLlmClient, MockResponse, NoOpEnv};

// Sub-module declarations:
mod branches;
mod compaction;
mod config;
mod cost;
mod lifecycle;
mod metadata;
mod phase_guard;
mod prepare_next_turn;
mod queues;
mod reset;
mod tools;

// ── Shared helpers used by multiple test files ───────────────────────────────

/// Test helper: wraps any SessionStorage and fails `append_entry` after `fail_after` successes.
struct FailAfterNStorage {
    inner: Arc<dyn SessionStorage>,
    fail_after: usize,
    call_count: std::sync::Mutex<usize>,
}

impl FailAfterNStorage {
    fn new(inner: Arc<dyn SessionStorage>, fail_after: usize) -> Self {
        Self {
            inner,
            fail_after,
            call_count: std::sync::Mutex::new(0),
        }
    }
}

impl SessionStorage for FailAfterNStorage {
    fn append_entry(
        &self,
        entry: SessionEntry,
    ) -> BoxFuture<'_, Result<(), llm_harness_types::SessionError>> {
        Box::pin(async move {
            {
                let mut n = self.call_count.lock().unwrap();
                if *n >= self.fail_after {
                    return Err(llm_harness_types::SessionError::ConcurrentModification);
                }
                *n += 1;
            } // lock dropped here before await
            self.inner.append_entry(entry).await
        })
    }
    fn metadata(&self) -> BoxFuture<'_, Result<SessionMetadata, llm_harness_types::SessionError>> {
        self.inner.metadata()
    }
    fn create_entry_id(&self) -> llm_harness_types::EntryId {
        self.inner.create_entry_id()
    }
    fn get_entry(
        &self,
        id: llm_harness_types::EntryId,
    ) -> BoxFuture<'_, Result<Option<SessionEntry>, llm_harness_types::SessionError>> {
        self.inner.get_entry(id)
    }
    fn children(
        &self,
        parent: llm_harness_types::EntryId,
    ) -> BoxFuture<'_, Result<Vec<SessionEntry>, llm_harness_types::SessionError>> {
        self.inner.children(parent)
    }
    fn all_leaves(
        &self,
    ) -> BoxFuture<'_, Result<Vec<llm_harness_types::EntryId>, llm_harness_types::SessionError>>
    {
        self.inner.all_leaves()
    }
    fn active_cursor(
        &self,
    ) -> BoxFuture<'_, Result<Option<llm_harness_types::EntryId>, llm_harness_types::SessionError>>
    {
        self.inner.active_cursor()
    }
    fn set_active_cursor(
        &self,
        id: llm_harness_types::EntryId,
    ) -> BoxFuture<'_, Result<(), llm_harness_types::SessionError>> {
        self.inner.set_active_cursor(id)
    }
    fn path_to_root(
        &self,
        target: llm_harness_types::EntryId,
    ) -> BoxFuture<'_, Result<Vec<SessionEntry>, llm_harness_types::SessionError>> {
        self.inner.path_to_root(target)
    }
    fn common_ancestor(
        &self,
        a: llm_harness_types::EntryId,
        b: llm_harness_types::EntryId,
    ) -> BoxFuture<'_, Result<Option<llm_harness_types::EntryId>, llm_harness_types::SessionError>>
    {
        self.inner.common_ancestor(a, b)
    }
    fn label_at(
        &self,
        id: llm_harness_types::EntryId,
    ) -> BoxFuture<'_, Result<Option<String>, llm_harness_types::SessionError>> {
        self.inner.label_at(id)
    }
    fn paths_to_all_leaves(
        &self,
    ) -> BoxFuture<'_, Result<Vec<Vec<SessionEntry>>, llm_harness_types::SessionError>> {
        self.inner.paths_to_all_leaves()
    }
    fn find_entries_by_type(
        &self,
        kind: SessionEntryKind,
    ) -> BoxFuture<'_, Result<Vec<llm_harness_types::EntryId>, llm_harness_types::SessionError>>
    {
        self.inner.find_entries_by_type(kind)
    }
    fn update_metadata_name(
        &self,
        name: Option<String>,
    ) -> BoxFuture<'_, Result<(), llm_harness_types::SessionError>> {
        self.inner.update_metadata_name(name)
    }
    fn update_metadata_model(
        &self,
        model: Option<String>,
    ) -> BoxFuture<'_, Result<(), llm_harness_types::SessionError>> {
        self.inner.update_metadata_model(model)
    }
    fn delete_entries(
        &self,
        ids: Vec<llm_harness_types::EntryId>,
    ) -> BoxFuture<'_, Result<(), llm_harness_types::SessionError>> {
        self.inner.delete_entries(ids)
    }
    fn all_entries(
        &self,
    ) -> BoxFuture<'_, Result<Vec<SessionEntry>, llm_harness_types::SessionError>> {
        self.inner.all_entries()
    }
}

fn make_harness_failing_storage(fail_after: usize, responses: Vec<MockResponse>) -> AgentHarness {
    let repo = crate::session::repo::InMemorySessionRepo::new();
    let storage = futures::executor::block_on(
        repo.create(crate::session::types::CreateSessionOptions::default()),
    )
    .unwrap();
    let failing = Arc::new(FailAfterNStorage::new(storage, fail_after));
    let session = crate::session::session::Session::new(failing as Arc<dyn SessionStorage>);
    let client = Arc::new(MockLlmClient::new(responses));
    let env = Arc::new(NoOpEnv);
    AgentHarness::with_session(
        client as Arc<dyn LlmClient>,
        env,
        session,
        AgentHarnessOptions::new("test-model"),
    )
}

fn make_harness(responses: Vec<MockResponse>) -> AgentHarness {
    let client = Arc::new(MockLlmClient::new(responses));
    let env = Arc::new(NoOpEnv);
    let opts = AgentHarnessOptions::new("test-model");
    futures::executor::block_on(AgentHarness::new_in_memory(
        client as Arc<dyn LlmClient>,
        env,
        opts,
    ))
}

struct DummyTool {
    name: String,
    schema: serde_json::Value,
}
impl DummyTool {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            schema: serde_json::json!({"type":"object","properties":{}}),
        }
    }
}
impl llm_harness_types::Tool for DummyTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "test tool"
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        &self.schema
    }
    fn execute<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: &'a llm_harness_types::ToolContext,
    ) -> futures::future::BoxFuture<
        'a,
        Result<llm_harness_types::ToolResult, llm_harness_types::ToolFailure>,
    > {
        Box::pin(async {
            Ok(llm_harness_types::ToolResult::full(
                vec![llm_harness_types::DataBlock::text("ok")],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

struct BuiltInLevelTool {
    name: String,
}
impl llm_harness_types::Tool for BuiltInLevelTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn level(&self) -> llm_harness_types::ToolLevel {
        llm_harness_types::ToolLevel::BuiltIn
    }
    fn description(&self) -> &str {
        "harness tool"
    }
    fn parameters_schema(&self) -> &serde_json::Value {
        static S: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        S.get_or_init(|| serde_json::json!({}))
    }
    fn execute<'a>(
        &'a self,
        _: serde_json::Value,
        _: &'a llm_harness_types::ToolContext,
    ) -> futures::future::BoxFuture<
        'a,
        Result<llm_harness_types::ToolResult, llm_harness_types::ToolFailure>,
    > {
        Box::pin(async {
            Ok(llm_harness_types::ToolResult::full(
                vec![],
                serde_json::Value::Null,
                false,
            ))
        })
    }
}

/// LLM client that never returns from `chat` — for testing abort paths.
struct BlockingClient;

#[async_trait::async_trait]
impl LlmClient for BlockingClient {
    fn capabilities(&self) -> llm_harness_loop::ProviderCapabilities {
        llm_harness_loop::ProviderCapabilities::new(false, false, false)
    }
    async fn chat(
        &self,
        _: &llm_harness_loop::ChatRequest,
    ) -> std::result::Result<llm_harness_loop::ChatResponse, llm_harness_loop::LlmError> {
        std::future::pending().await
    }
    async fn chat_stream(
        &self,
        _: &llm_harness_loop::ChatRequest,
    ) -> std::result::Result<llm_harness_loop::StreamHandle, llm_harness_loop::LlmError> {
        std::future::pending().await
    }
}
