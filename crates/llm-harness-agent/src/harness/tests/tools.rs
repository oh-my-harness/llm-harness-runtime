use super::*;

#[tokio::test]
async fn tool_call_end_event_carries_tool_name() {
    use std::sync::Mutex as StdMutex;

    let h = make_harness(vec![
        MockResponse::tool_use("id-1", "my_tool", "{}"),
        MockResponse::text("done"),
    ]);

    let tool: Arc<dyn llm_harness_types::Tool> = Arc::new(DummyTool::new("my_tool"));
    h.set_tools(vec![tool]).await.unwrap();

    let received_name: Arc<StdMutex<Option<String>>> = Arc::new(StdMutex::new(None));
    let received_name_clone = received_name.clone();

    let mut rx = h.subscribe();
    let handle = tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            if let AgentHarnessEvent::ToolCallEnd { tool_name, .. } = event.as_ref() {
                *received_name_clone.lock().unwrap() = Some(tool_name.clone());
                break;
            }
        }
    });

    h.prompt("use tool").await.unwrap();
    handle.await.unwrap();

    assert_eq!(
        received_name.lock().unwrap().as_deref(),
        Some("my_tool"),
        "ToolCallEnd must carry the correct tool_name"
    );
}

struct NoOpBeforeToolCall;
impl BeforeToolCallHook for NoOpBeforeToolCall {
    fn on_call<'a>(
        &'a self,
        _ctx: BeforeToolCallCtx<'a>,
    ) -> futures::future::BoxFuture<'a, BeforeToolCallDecision> {
        Box::pin(async { BeforeToolCallDecision::Allow })
    }
}

struct SetToolsOnTurn {
    harness: Arc<std::sync::OnceLock<Arc<AgentHarness>>>,
    done: Arc<std::sync::atomic::AtomicBool>,
}
impl BeforeTurnHook for SetToolsOnTurn {
    fn before_turn<'a>(&'a self, _ctx: BeforeTurnCtx<'a>) -> futures::future::BoxFuture<'a, ()> {
        let harness_cell = self.harness.clone();
        let done = self.done.clone();
        Box::pin(async move {
            if done.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let h = harness_cell.get().unwrap().clone();
            let new_tool: Arc<dyn llm_harness_types::Tool> =
                Arc::new(DummyTool::new("mid_run_tool"));
            h.set_tools(vec![new_tool]).await.unwrap();
        })
    }
}

/// `set_tools` called mid-run (via a `before_turn` hook) must survive the
/// cleanup that unwraps `HookedTool`. The new tool must still be present
/// after the run ends.
#[tokio::test]
async fn set_tools_mid_run_survives_cleanup() {
    use std::sync::OnceLock;

    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
    let env = Arc::new(NoOpEnv);
    let harness_cell: Arc<OnceLock<Arc<AgentHarness>>> = Arc::new(OnceLock::new());
    let opts = AgentHarnessOptions {
        hooks: HarnessHooks {
            before_turn: vec![Arc::new(SetToolsOnTurn {
                harness: harness_cell.clone(),
                done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            })],
            // A no-op before_tool_call hook forces HookedTool wrapping.
            before_tool_call: vec![Arc::new(NoOpBeforeToolCall)],
            ..HarnessHooks::none()
        },
        ..AgentHarnessOptions::new("test-model")
    };
    let h = Arc::new(AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, env, opts).await);
    harness_cell.get_or_init(|| h.clone());

    h.prompt("hi").await.unwrap();

    let names: Vec<String> = h
        .state()
        .tools
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "mid_run_tool"),
        "mid-run set_tools must survive cleanup; tools = {names:?}"
    );
    assert!(
        !h.state().tools_overwritten_mid_run,
        "flag must be reset after cleanup"
    );
}

/// Without mid-run `set_tools`, cleanup must still unwrap HookedTool so the
/// next `build_loop_config` does not double-wrap. Two consecutive prompts
/// with a before_tool_call hook must succeed.
#[tokio::test]
async fn cleanup_unwraps_hooked_tool_without_mid_run_set() {
    struct NoOpBeforeToolCall;
    impl BeforeToolCallHook for NoOpBeforeToolCall {
        fn on_call<'a>(
            &'a self,
            _ctx: BeforeToolCallCtx<'a>,
        ) -> futures::future::BoxFuture<'a, BeforeToolCallDecision> {
            Box::pin(async { BeforeToolCallDecision::Allow })
        }
    }

    let client = Arc::new(MockLlmClient::new(vec![
        MockResponse::text("first"),
        MockResponse::text("second"),
    ]));
    let env = Arc::new(NoOpEnv);
    let opts = AgentHarnessOptions {
        hooks: HarnessHooks {
            before_tool_call: vec![Arc::new(NoOpBeforeToolCall)],
            ..HarnessHooks::none()
        },
        ..AgentHarnessOptions::new("test-model")
    };
    let h = AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, env, opts).await;

    // Two prompts: the second `build_loop_config` would double-wrap if
    // cleanup failed to unwrap. No panic / no error => pass.
    h.prompt("one").await.unwrap();
    h.prompt("two").await.unwrap();
    assert!(
        !h.state().tools_overwritten_mid_run,
        "flag must remain false without mid-run set_tools"
    );
}

/// `set_tools` in Idle phase must not set the mid-run flag (it takes the
/// direct `session.append` path, not the pending path).
#[tokio::test]
async fn set_tools_in_idle_does_not_set_flag() {
    let h = make_harness(vec![]);
    let tool: Arc<dyn llm_harness_types::Tool> = Arc::new(DummyTool::new("idle_tool"));
    h.set_tools(vec![tool]).await.unwrap();
    assert!(
        !h.state().tools_overwritten_mid_run,
        "Idle set_tools must not set the mid-run flag"
    );
}

/// On an LLM error (`AgentEvent::Error`), the run ends but the mid-run
/// `set_tools` change is NOT rolled back — LLM errors are part of the run,
/// not a run-level failure. Only storage/session errors (which make
/// `drive_loop` return `Err`) trigger rollback.
#[tokio::test]
async fn set_tools_mid_run_preserved_on_llm_error() {
    use std::sync::OnceLock;

    struct SetToolsOnTurn {
        harness: Arc<OnceLock<Arc<AgentHarness>>>,
        done: Arc<std::sync::atomic::AtomicBool>,
    }
    impl BeforeTurnHook for SetToolsOnTurn {
        fn before_turn<'a>(
            &'a self,
            _ctx: BeforeTurnCtx<'a>,
        ) -> futures::future::BoxFuture<'a, ()> {
            let harness_cell = self.harness.clone();
            let done = self.done.clone();
            Box::pin(async move {
                if done.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let h = harness_cell.get().unwrap().clone();
                let new_tool: Arc<dyn llm_harness_types::Tool> =
                    Arc::new(DummyTool::new("error_run_tool"));
                h.set_tools(vec![new_tool]).await.unwrap();
            })
        }
    }

    // First response errors; before_turn fires first and calls set_tools.
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::rate_limit_error()]));
    let env = Arc::new(NoOpEnv);
    let harness_cell: Arc<OnceLock<Arc<AgentHarness>>> = Arc::new(OnceLock::new());
    let opts = AgentHarnessOptions {
        hooks: HarnessHooks {
            before_turn: vec![Arc::new(SetToolsOnTurn {
                harness: harness_cell.clone(),
                done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            })],
            ..HarnessHooks::none()
        },
        ..AgentHarnessOptions::new("test-model")
    };
    let h = Arc::new(AgentHarness::new_in_memory(client as Arc<dyn LlmClient>, env, opts).await);
    harness_cell.get_or_init(|| h.clone());

    // The run errors; prompt returns Ok (LLM error is an event, not a Result).
    h.prompt("hi").await.unwrap();

    let names: Vec<String> = h
        .state()
        .tools
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "error_run_tool"),
        "mid-run set_tools must survive LLM error; tools = {names:?}"
    );
    assert!(
        !h.state().tools_overwritten_mid_run,
        "flag must be reset after cleanup"
    );
}

/// On a storage/session error (`drive_loop` returns `Err`), the run is
/// discarded: mid-run `set_tools` must be rolled back to `original_tools`
/// and the flag reset. This is the counterpart to
/// `set_tools_mid_run_preserved_on_llm_error` — LLM errors are events
/// (preserve), storage errors are `Result::Err` (rollback).
#[tokio::test]
async fn set_tools_mid_run_rolled_back_on_storage_error() {
    use std::sync::OnceLock;

    struct SetToolsOnTurn {
        harness: Arc<OnceLock<Arc<AgentHarness>>>,
        done: Arc<std::sync::atomic::AtomicBool>,
    }
    impl BeforeTurnHook for SetToolsOnTurn {
        fn before_turn<'a>(
            &'a self,
            _ctx: BeforeTurnCtx<'a>,
        ) -> futures::future::BoxFuture<'a, ()> {
            let harness_cell = self.harness.clone();
            let done = self.done.clone();
            Box::pin(async move {
                if done.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let h = harness_cell.get().unwrap().clone();
                let new_tool: Arc<dyn llm_harness_types::Tool> =
                    Arc::new(DummyTool::new("storage_err_tool"));
                h.set_tools(vec![new_tool]).await.unwrap();
            })
        }
    }

    // fail_after=0: the first flush_pending_writes at TurnEnd fails,
    // making drive_loop return Err — the storage-error path.
    let repo = crate::session::repo::InMemorySessionRepo::new();
    let storage = futures::executor::block_on(
        repo.create(crate::session::types::CreateSessionOptions::default()),
    )
    .unwrap();
    let failing = Arc::new(FailAfterNStorage::new(storage, 0));
    let session = crate::session::session::Session::new(failing as Arc<dyn SessionStorage>);
    let client = Arc::new(MockLlmClient::new(vec![MockResponse::text("ok")]));
    let env = Arc::new(NoOpEnv);
    let harness_cell: Arc<OnceLock<Arc<AgentHarness>>> = Arc::new(OnceLock::new());
    let opts = AgentHarnessOptions {
        hooks: HarnessHooks {
            before_turn: vec![Arc::new(SetToolsOnTurn {
                harness: harness_cell.clone(),
                done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            })],
            ..HarnessHooks::none()
        },
        ..AgentHarnessOptions::new("test-model")
    };
    let h = Arc::new(AgentHarness::with_session(
        client as Arc<dyn LlmClient>,
        env,
        session,
        opts,
    ));
    harness_cell.get_or_init(|| h.clone());

    // Storage error makes prompt return Err.
    let result = h.prompt("hi").await;
    assert!(result.is_err(), "prompt must return Err when storage fails");

    let names: Vec<String> = h
        .state()
        .tools
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(
        !names.iter().any(|n| n == "storage_err_tool"),
        "mid-run set_tools must be rolled back on storage error; tools = {names:?}"
    );
    assert!(
        !h.state().tools_overwritten_mid_run,
        "flag must be reset after storage-error rollback"
    );
}
